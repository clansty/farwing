//! Notification area integration through the freedesktop StatusNotifierItem
//! protocol, with the same entries as the Windows tray: activating the icon
//! or "打开 OpenUUYC" shows the control center, "退出" ends the run.
//!
//! GNOME shows these icons through the AppIndicator extension (enabled on
//! Ubuntu); KDE, Xfce and most other panels show them natively. Without a
//! watcher the tray cannot be created, and the caller keeps the window's
//! ordinary close behaviour instead of hiding it somewhere unreachable.
use anyhow::{Context, Result};
use ksni::blocking::TrayMethods;
use std::sync::atomic::{AtomicBool, Ordering};

pub(super) struct Tray {
    handle: ksni::blocking::Handle<Item>,
    /// 图标合成要解码整张 PNG，所以只有状态翻转时才重建。
    controlled: AtomicBool,
}

struct Item {
    icon: Vec<ksni::Icon>,
}

fn send(request: super::window_manager::Request) {
    let _ = super::window_manager::send(request);
}

impl ksni::Tray for Item {
    fn id(&self) -> String {
        "openuuyc".into()
    }
    fn title(&self) -> String {
        "OpenUUYC".into()
    }
    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        self.icon.clone()
    }
    fn activate(&mut self, _x: i32, _y: i32) {
        send(super::window_manager::Request::ShowMain);
    }
    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::StandardItem;
        vec![
            StandardItem {
                label: "打开 OpenUUYC".into(),
                activate: Box::new(|_| send(super::window_manager::Request::ShowMain)),
                ..Default::default()
            }
            .into(),
            ksni::MenuItem::Separator,
            StandardItem {
                label: "退出".into(),
                activate: Box::new(|_| send(super::window_manager::Request::Exit)),
                ..Default::default()
            }
            .into(),
        ]
    }
}

/// The application icon as SNI wants it: ARGB32, big-endian.
fn icon(controlled: bool) -> ksni::Icon {
    let image = crate::ui::branding::tray_pixmap(64, controlled);
    let (width, height) = image.dimensions();
    let mut data = image.into_raw();
    for pixel in data.chunks_exact_mut(4) {
        pixel.rotate_right(1);
    }
    ksni::Icon {
        width: width as i32,
        height: height as i32,
        data,
    }
}

impl Tray {
    pub fn new() -> Result<Self> {
        let handle = Item {
            icon: vec![icon(false)],
        }
        .spawn()
        .context("桌面没有可用的托盘（StatusNotifierItem）")?;
        Ok(Self {
            handle,
            controlled: AtomicBool::new(false),
        })
    }

    /// 被控角标：这台机器正被别人控制时，托盘图标右上角多一个红三角。
    pub(super) fn set_controlled(&self, controlled: bool) {
        if self.controlled.load(Ordering::Relaxed) == controlled {
            return;
        }
        let icon = icon(controlled);
        // 跟上 Windows 侧同一个原则：只有托盘真的换上了才记状态，没换上就留着重试。
        if self.handle.update(|item| item.icon = vec![icon]).is_some() {
            self.controlled.store(controlled, Ordering::Relaxed);
        } else {
            tracing::debug!(controlled, "托盘已关闭，角标留到下次再换");
        }
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        self.handle.shutdown().wait();
    }
}
