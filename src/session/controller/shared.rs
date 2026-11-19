//! One network owner per device; viewing and TCP mapping hold separate leases.
use super::*;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock, Weak};
use tokio::sync::watch;

#[derive(Clone)]
struct End {
    message: String,
    signal: Option<SignalFailure>,
}

pub(super) struct Session {
    account: String,
    pub peer: Arc<NativePeer>,
    forwarder: Mutex<Option<RtpForwarder>>,
    shutdown: Mutex<Option<oneshot::Sender<()>>>,
    ended: watch::Receiver<Option<Option<End>>>,
    /// 自己那一路摘掉就算结束，不必等整条 socket 收尾（房间里可能还开着
    /// 别的会话，socket 会继续跑）。
    ended_sender: Mutex<Option<watch::Sender<Option<Option<End>>>>>,
    viewing: AtomicBool,
    owner: Mutex<Option<CancellationToken>>,
    /// 房间只有这一个 socket，终端之类的第二路会话要交给它去开。
    side: tokio::sync::mpsc::UnboundedSender<crate::transport::signal::RoomRequest>,
}
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}
fn pool() -> &'static Mutex<HashMap<String, Weak<Session>>> {
    static POOL: OnceLock<Mutex<HashMap<String, Weak<Session>>>> = OnceLock::new();
    POOL.get_or_init(Mutex::default)
}
pub(super) fn key(controller: &str, target: &str) -> String {
    format!("{controller}:{target}")
}
pub(super) fn get(key: &str) -> Option<Arc<Session>> {
    let mut pool = lock(pool());
    pool.retain(|_, s| s.strong_count() > 0);
    pool.get(key).and_then(Weak::upgrade).filter(|s| {
        s.ended.borrow().is_none()
            && lock(&s.owner)
                .as_ref()
                .is_none_or(|owner| !owner.is_cancelled())
    })
}
pub(super) fn register(key: String, session: &Arc<Session>, owner: CancellationToken) {
    *lock(&session.owner) = Some(owner);
    lock(pool()).insert(key, Arc::downgrade(session));
}
pub(super) fn connection_gate(key: &str) -> Arc<tokio::sync::Mutex<()>> {
    static GATES: OnceLock<Mutex<HashMap<String, Weak<tokio::sync::Mutex<()>>>>> = OnceLock::new();
    let mut gates = lock(GATES.get_or_init(Mutex::default));
    gates.retain(|_, gate| gate.strong_count() > 0);
    if let Some(gate) = gates.get(key).and_then(Weak::upgrade) {
        return gate;
    }
    let gate = Arc::new(tokio::sync::Mutex::new(()));
    gates.insert(key.into(), Arc::downgrade(&gate));
    gate
}
pub(super) async fn acquire_connection(
    key: &str,
    cancel: &CancellationToken,
) -> Result<tokio::sync::OwnedMutexGuard<()>> {
    cancellable(cancel, async {
        Ok(connection_gate(key).lock_owned().await)
    })
    .await
}
/// 本机在这台设备上的房间。房间按设备分配，一个房间只有一条信令 socket：
/// 观看与终端是这条 socket 上的两路会话，谁先连谁是主人。
pub(super) struct Room {
    key: String,
    pub requests: tokio::sync::mpsc::UnboundedSender<crate::transport::signal::RoomRequest>,
}
impl Drop for Room {
    fn drop(&mut self) {
        let mut rooms = lock(rooms());
        if rooms
            .get(&self.key)
            .is_some_and(|room| room.strong_count() == 0)
        {
            rooms.remove(&self.key);
        }
    }
}
fn rooms() -> &'static Mutex<HashMap<String, Weak<Room>>> {
    static ROOMS: OnceLock<Mutex<HashMap<String, Weak<Room>>>> = OnceLock::new();
    ROOMS.get_or_init(Mutex::default)
}
pub(super) fn room(key: &str) -> Option<Arc<Room>> {
    let mut rooms = lock(rooms());
    rooms.retain(|_, room| room.strong_count() > 0);
    rooms.get(key).and_then(Weak::upgrade)
}
pub(super) fn publish_room(
    key: &str,
    requests: tokio::sync::mpsc::UnboundedSender<crate::transport::signal::RoomRequest>,
) -> Arc<Room> {
    let room = Arc::new(Room {
        key: key.to_owned(),
        requests,
    });
    lock(rooms()).insert(key.to_owned(), Arc::downgrade(&room));
    room
}
fn jobs() -> &'static Mutex<Vec<(Weak<Session>, CancellationToken)>> {
    static JOBS: OnceLock<Mutex<Vec<(Weak<Session>, CancellationToken)>>> = OnceLock::new();
    JOBS.get_or_init(Mutex::default)
}
pub(super) async fn shutdown_all() {
    let jobs = std::mem::take(&mut *lock(jobs()));
    for (session, _) in &jobs {
        if let Some(session) = session.upgrade() {
            session.request_close();
        }
    }
    for (_, done) in jobs {
        done.cancelled().await;
    }
}
impl Session {
    pub(super) fn activity(&self) -> super::LocalConnectionActivity {
        super::LocalConnectionActivity {
            viewing: self.viewing.load(Ordering::Acquire),
            controlling: self.peer.stream_control_handle().mouse().mode()
                != crate::features::remote_input::MouseMode::View,
        }
    }
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        account: String,
        peer: Arc<NativePeer>,
        forwarder: RtpForwarder,
        shutdown: Option<oneshot::Sender<()>>,
        task: Option<tokio::task::JoinHandle<Result<()>>>,
        side: tokio::sync::mpsc::UnboundedSender<crate::transport::signal::RoomRequest>,
    ) -> Arc<Self> {
        let (tx, ended) = watch::channel(None);
        let ended_sender = Mutex::new(Some(tx.clone()));
        let done = CancellationToken::new();
        match task {
            Some(task) => {
                let finish = done.clone();
                tokio::spawn(async move {
                    let _finish = finish.drop_guard();
                    let result = flatten_signal_task(task.await);
                    tx.send_replace(Some(result.err().map(|e| End {
                        signal: e.downcast_ref::<SignalFailure>().cloned(),
                        message: format!("{e:#}"),
                    })));
                });
            }
            // 挂在本机另一次连接上的会话不拥有 socket，结束由房间那边广播。
            None => drop(tx),
        }
        let session = Arc::new(Self {
            account,
            peer,
            forwarder: Mutex::new(Some(forwarder)),
            shutdown: Mutex::new(shutdown),
            ended,
            ended_sender,
            viewing: AtomicBool::new(false),
            owner: Mutex::new(None),
            side,
        });
        {
            let mut jobs = lock(jobs());
            jobs.retain(|(_, done)| !done.is_cancelled());
            jobs.push((Arc::downgrade(&session), done));
        }
        session
    }
    pub fn lease(self: &Arc<Self>, viewing: bool) -> Result<ForwarderLease> {
        let forwarder = if viewing {
            anyhow::ensure!(
                self.viewing
                    .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok(),
                "该设备的观看窗口已打开"
            );
            lock(&self.forwarder).take()
        } else {
            None
        };
        Ok(ForwarderLease {
            session: Arc::clone(self),
            forwarder,
            viewing,
        })
    }
    /// 在这条会话的信令 socket 上再开一路会话，返回它的 peer。
    pub(super) async fn open_side(
        &self,
        controller_device_id: String,
        profile: ConnectionMediaProfile,
        transport: crate::media::TransportChoice,
        purpose: crate::session::negotiation::ControlPurpose,
        cancel: &CancellationToken,
    ) -> Result<crate::transport::signal::Attached> {
        let (reply, receiver) = oneshot::channel();
        self.side
            .send(crate::transport::signal::RoomRequest::Attach(
                crate::transport::signal::SideChannelRequest {
                    reply,
                    controller_device_id,
                    profile,
                    transport,
                    purpose,
                    entitlements: None,
                },
            ))
            .map_err(|_| anyhow!("设备会话已结束，无法再开一路会话"))?;
        cancellable(cancel, async {
            receiver.await.context("再开一路会话时连接已结束")?
        })
        .await
    }
    pub async fn ended(&self) -> Result<()> {
        self.end_waiter().await
    }
    pub fn end_waiter(
        &self,
    ) -> impl std::future::Future<Output = Result<()>> + Send + 'static + use<> {
        let mut rx = self.ended.clone();
        async move {
            loop {
                if let Some(end) = rx.borrow_and_update().clone() {
                    return match end {
                        None => Ok(()),
                        Some(e) => Err(e
                            .signal
                            .map(anyhow::Error::new)
                            .unwrap_or_else(|| anyhow!(e.message))),
                    };
                }
                if rx.changed().await.is_err() {
                    bail!("设备连接已结束");
                }
            }
        }
    }
    pub fn request_close(&self) {
        tracing::debug!("shared session close requested");
        if let Some(tx) = lock(&self.shutdown).take() {
            let _ = tx.send(());
        }
        // 房间可能还要给别的会话留着，socket 不会跟着结束；这一路已经交出去
        // 了，等它的人马上就能走。
        if let Some(tx) = lock(&self.ended_sender).take() {
            tx.send_replace(Some(None));
        }
    }
}

pub(super) fn viewing_count(account: &str) -> usize {
    lock(jobs())
        .iter()
        .filter_map(|(s, _)| s.upgrade())
        .filter(|s| {
            s.account == account && s.viewing.load(Ordering::Acquire) && s.ended.borrow().is_none()
        })
        .count()
}
impl Drop for Session {
    fn drop(&mut self) {
        self.request_close();
    }
}
pub(super) struct ForwarderLease {
    pub session: Arc<Session>,
    forwarder: Option<RtpForwarder>,
    pub viewing: bool,
}
impl std::ops::Deref for ForwarderLease {
    type Target = RtpForwarder;
    fn deref(&self) -> &Self::Target {
        self.forwarder
            .as_ref()
            .expect("viewing lease owns media receiver")
    }
}
impl std::ops::DerefMut for ForwarderLease {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.forwarder
            .as_mut()
            .expect("viewing lease owns media receiver")
    }
}
impl Drop for ForwarderLease {
    fn drop(&mut self) {
        if self.viewing {
            *lock(&self.session.forwarder) = self.forwarder.take();
            self.session.viewing.store(false, Ordering::Release);
        }
    }
}
