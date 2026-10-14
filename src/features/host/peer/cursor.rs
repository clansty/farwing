//! The capture owner samples the actual desktop; this task only publishes changes.
use super::{ReportRoutes, screens::Reports};
use crate::features::{
    host::lock,
    remote_cursor::{self, CursorImage},
};
use base64::Engine;
use std::{
    sync::{Arc, atomic::Ordering},
    time::Duration,
};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

pub(super) async fn run(
    reports: Arc<Reports>,
    mut route: watch::Receiver<ReportRoutes>,
    cancel: CancellationToken,
    lease: crate::features::host::Lease,
    kcp: crate::transport::uu_kcp::UuKcpControl,
) {
    let mut timer = tokio::time::interval(Duration::from_millis(33));
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut revision = None;
    let mut sent = None;
    loop {
        tokio::select! {_=cancel.cancelled()=>return,_=timer.tick()=>{},r=route.changed()=>{if r.is_err(){return}}}
        if !lease.requested() {
            return;
        }
        let routes = route.borrow_and_update().clone();
        let Some(channel) = routes.control.as_ref().and_then(std::sync::Weak::upgrade) else {
            continue;
        };
        if revision != Some(routes.revision) {
            revision = Some(routes.revision);
            sent = None;
        }
        let media = reports.media();
        let current = reports.current.load(Ordering::Acquire);
        let pointer = media
            .iter()
            .filter(|(_, m)| m.capturing && m.visible)
            .max_by_key(|(_, m)| m.screen.id == current)
            .and_then(|(index, _)| lock(&reports.pointers[*index]).clone());
        // Which screen the pointer is on is only known while it is on one this
        // connection is streaming: those are the coordinates the sampler's
        // report is in, and the screen the controller draws the pointer on.
        // A session that cannot say where its pointer is -- a Wayland one, see
        // `cursor_shape::reported_position` -- reports no screen instead of a
        // made-up one, and the controller keeps its own pointer where it is.
        //
        // The shape does not depend on that. A controller that draws the
        // pointer itself -- which is what it asks for when it turns capture
        // composition off -- draws it where it is aiming, and needs the shape
        // to draw; a pointer that cannot be placed, because it is on a monitor
        // nobody is watching or because the sampler is an X11 one that cannot
        // see a Wayland compositor's pointer, is still the shape the controller
        // should be showing. Reporting it as hidden instead leaves the picture
        // with no pointer at all.
        let at = pointer.as_ref().and_then(|p| p.position);
        let screen = at.and_then(|(x, y)| {
            lock(&reports.catalog)
                .iter()
                .find(|info| {
                    let s = &info.screen;
                    media
                        .iter()
                        .any(|(_, m)| m.capturing && m.visible && m.screen.id == s.id)
                        && i64::from(x) >= i64::from(s.left)
                        && i64::from(x) < i64::from(s.left) + i64::from(s.width)
                        && i64::from(y) >= i64::from(s.top)
                        && i64::from(y) < i64::from(s.top) + i64::from(s.height)
                })
                .map(|info| info.screen.clone())
        });
        let visible = pointer
            .as_ref()
            .is_some_and(|p| p.showing && p.image.is_some());
        let image = if visible {
            pointer.as_ref().and_then(|p| p.image.clone())
        } else {
            None
        };
        let key = (
            image.clone(),
            screen.as_ref().map_or(-1, |s| s.id),
            screen
                .as_ref()
                .map(|s| (s.dpi_scale, s.width, s.height, s.left, s.top)),
        );
        if sent.as_ref() == Some(&key) {
            continue;
        }
        let image = match image {
            Some(image) => match base64::engine::general_purpose::STANDARD.decode(&image.png) {
                Ok(png) => Some(CursorImage {
                    png,
                    width: image.width,
                    height: image.height,
                    hotspot: image.hotspot,
                    system_type: image.kind,
                }),
                Err(error) => {
                    tracing::warn!(%error,"invalid sampled cursor image");
                    continue;
                }
            },
            None => None,
        };
        let position = match (at, screen.as_ref()) {
            (Some((x, y)), Some(s)) => [
                (f64::from(x) - f64::from(s.left)) / f64::from(s.width),
                (f64::from(y) - f64::from(s.top)) / f64::from(s.height),
            ],
            _ => [0.0, 0.0],
        };
        let payload = remote_cursor::encode_shape(image.as_ref(), key.1, position);
        let bytes = crate::features::stream_control::publisher::cursor_report(payload);
        if bytes.len() > 512 * 1024 {
            tracing::warn!(bytes = bytes.len(), "cursor exceeds signal message limit");
            continue;
        }
        let result =
            tokio::select! {_=cancel.cancelled()=>return,r=kcp.send_control(&channel,bytes)=>r};
        match result {
            Ok(_) => {
                tracing::debug!(screen=key.1,visible,size=?image.as_ref().map(|i|(i.width,i.height)),"host cursor CONTROL state published");
                sent = Some(key);
            }
            Err(error) => tracing::debug!(%error,"host cursor CONTROL publication failed"),
        }
    }
}
