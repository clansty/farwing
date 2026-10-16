//! Linux clipboard adapter for X11 and Wayland.
//!
//! Differences from the Windows/OLE adapter this replaces:
//! * X11 and Wayland have no delayed rendering across processes, so an offer
//!   from the remote is fetched immediately instead of when the user pastes.
//! * There is no clipboard-change notification, so local changes are polled.
//! * Files copied here are offered to the remote: it pulls the descriptor list
//!   and then the contents by offset, so nothing has to be promised in advance.
//!   Files the remote offers are mounted as a FUSE filesystem, so a paste or a
//!   drop gets real paths whose contents are fetched as they are read.
use super::formats::{self, Format};
use super::*;
use anyhow::Context as _;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::time::Instant;

pub(super) use super::formats::safe_name;

/// Anything larger is a transfer, not a clipboard paste.
const MAX_CLIP: usize = 32 * 1024 * 1024;
const POLL: Duration = Duration::from_millis(400);

pub(super) enum Command {
    Activate(Weak<Inner>),
    DropFiles(
        Arc<super::drag::Submission>,
        PreparedFiles,
        ClipboardFormatListRequestKind,
    ),
    Remove(u64),
    Offer(
        Weak<Inner>,
        u64,
        Vec<ClipboardFormat>,
        Option<ClipboardFormatListRequestKind>,
    ),
    Request(Weak<Inner>, u64, i64, ClipboardRequestKind),
    Text(Weak<Inner>, u64, i64, String),
    PublishFiles(
        Weak<Inner>,
        u64,
        PreparedFiles,
        tokio::sync::oneshot::Sender<Result<(), String>>,
    ),
}

/// Local files picked for a drag or a drop, walked before they are offered.
pub(super) struct PreparedFiles {
    files: LocalFiles,
    summary: FileSummary,
}
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct FileSummary {
    pub count: u32,
    pub bytes: u64,
    pub first_name: String,
}
impl PreparedFiles {
    pub fn summary(&self) -> FileSummary {
        self.summary.clone()
    }
}
pub(super) fn prepare_files(
    paths: Vec<PathBuf>,
    allowed: impl Fn() -> bool,
) -> Result<PreparedFiles> {
    ensure!(
        !paths.is_empty() && paths.len() <= MAX_FILES,
        "拖放文件数量无效"
    );
    let mut items = Vec::new();
    for path in paths {
        ensure!(allowed(), "文件准备已取消");
        ensure!(
            !std::fs::symlink_metadata(&path)?.is_symlink(),
            "不传输符号链接"
        );
        let root = std::fs::canonicalize(&path)?;
        let name = path
            .file_name()
            .context("不能拖放根目录")?
            .to_string_lossy()
            .into_owned();
        collect_file(&root, &root, &name, &mut items)?;
    }
    ensure!(allowed(), "文件准备已取消");
    let summary = file_summary(items.iter().map(|item| &item.desc))?;
    Ok(PreparedFiles {
        files: LocalFiles { items },
        summary,
    })
}
fn file_summary<'a>(
    items: impl Iterator<Item = &'a ClipboardFileDescriptor>,
) -> Result<FileSummary> {
    let mut summary = FileSummary {
        count: 0,
        bytes: 0,
        first_name: String::new(),
    };
    for item in items {
        if summary.first_name.is_empty() {
            summary.first_name = item.file_name.clone();
        }
        ensure!(safe_name(&item.file_name), "文件名称不安全");
        summary.count += 1;
        summary.bytes = summary
            .bytes
            .checked_add(item.file_size)
            .context("文件总大小溢出")?;
    }
    ensure!(
        summary.count > 0 && summary.count as usize <= MAX_FILES,
        "文件清单为空或过大"
    );
    Ok(summary)
}

/// How long a drop target may go without reading before the transfer is
/// called off. A file manager asking whether to replace a file stops reading
/// until the user answers.
const DROP_IDLE: Duration = Duration::from_secs(300);
/// How long a mounted drop outlives its offer, for a target still creating
/// the empty files and folders it has already listed.
const MOUNT_GRACE: Duration = Duration::from_secs(30);

/// What a drop target has read of each offered file.
struct Progress {
    sizes: Vec<u64>,
    reached: Mutex<Vec<u64>>,
    bytes: AtomicU64,
    last: Mutex<Instant>,
}
impl Progress {
    fn record(&self, index: u32, offset: u64, length: usize) {
        self.bytes.fetch_add(length as u64, Ordering::AcqRel);
        *lock(&self.last) = Instant::now();
        if let Some(end) = lock(&self.reached).get_mut(index as usize) {
            *end = (*end).max(offset + length as u64);
        }
    }
    fn complete(&self) -> bool {
        lock(&self.reached)
            .iter()
            .zip(&self.sizes)
            .all(|(reached, size)| reached >= size)
    }
}

/// Files a peer offers for a drop or a drag, read on demand through a mount.
pub(super) struct RemoteOffer {
    pub(super) session: Arc<Inner>,
    pub(super) epoch: u64,
    alive: AtomicBool,
    task: AtomicU32,
    descriptors: Mutex<Option<Vec<ClipboardFileDescriptor>>>,
    reading: Mutex<()>,
    dragging: AtomicBool,
    mount: Mutex<Option<(Arc<super::fuse::Mount>, Arc<Progress>)>>,
}
#[derive(Clone)]
pub(crate) struct FileOffer(pub(super) Arc<RemoteOffer>);
pub(crate) struct FileOfferStatus {
    pub bytes_read: u64,
    pub completed: Option<Result<(), String>>,
}
impl FileOffer {
    pub fn prepare(&self) -> Result<FileSummary> {
        file_summary(self.0.list()?.iter())
    }
    /// The offered files as local paths for a drag source, mounted on first use.
    pub fn object(&self) -> Result<crate::platform::drag_drop::Files> {
        self.0.valid()?;
        let mut slot = lock(&self.0.mount);
        if slot.is_none() {
            let descriptors = self.0.list()?;
            let progress = Arc::new(Progress {
                sizes: descriptors
                    .iter()
                    .map(|d| {
                        if d.file_attributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
                            0
                        } else {
                            d.file_size
                        }
                    })
                    .collect(),
                reached: Mutex::new(vec![0; descriptors.len()]),
                bytes: AtomicU64::new(0),
                last: Mutex::new(Instant::now()),
            });
            let tree = super::fuse::Tree::build(&descriptors)?;
            let reader = {
                let offer = Arc::downgrade(&self.0);
                let progress = progress.clone();
                Arc::new(move |index: u32, offset: u64, length: usize| {
                    let offer = offer.upgrade().context("拖放文件已失效")?;
                    offer.valid()?;
                    let data = offer.session.read_file(
                        offer.epoch,
                        offer.task(),
                        index,
                        offset,
                        length.min(FILE_BLOCK),
                        2,
                    )?;
                    progress.record(index, offset, data.len());
                    Ok(data)
                })
            };
            let mount = super::fuse::Mount::new(
                &super::fuse::mount_parent()?,
                super::fuse::generation(),
                tree,
                reader,
            )?;
            *slot = Some((Arc::new(mount), progress));
        }
        let (mount, _) = slot.as_ref().unwrap();
        Ok(crate::platform::drag_drop::Files {
            paths: mount.paths(),
            _hold: mount.clone(),
        })
    }
    pub fn dragging(&self, value: bool) {
        self.0.dragging.store(value, Ordering::Release);
        if !value && let Some((_, progress)) = lock(&self.0.mount).as_ref() {
            // The idle clock starts at the drop, not at the first hover.
            *lock(&progress.last) = Instant::now();
        }
    }
    pub fn cancel(&self) {
        self.0.cancel();
        self.0.session.cancel_pending();
    }
    pub(super) fn valid(&self) -> bool {
        self.0.valid().is_ok()
    }
    pub(super) fn manifest(&self) -> Result<Vec<ClipboardFileDescriptor>> {
        self.0.list()
    }
    pub(super) fn read(&self, index: usize, offset: u64, length: usize) -> Result<Vec<u8>> {
        self.0.valid()?;
        let _busy = self.0.enter_read()?;
        self.0.session.read_file(
            self.0.epoch,
            self.0.task(),
            index.try_into()?,
            offset,
            length,
            2,
        )
    }
    /// A drop is complete once the target has read every file to its end.
    pub fn status(&self) -> FileOfferStatus {
        let mount = lock(&self.0.mount);
        let Some((_, progress)) = mount.as_ref() else {
            return FileOfferStatus {
                bytes_read: 0,
                completed: None,
            };
        };
        let completed = if self.0.dragging.load(Ordering::Acquire) {
            None
        } else if progress.complete() {
            Some(Ok(()))
        } else if lock(&progress.last).elapsed() > DROP_IDLE {
            Some(Err("目标长时间未读取文件，拖放已停止".to_owned()))
        } else {
            None
        };
        FileOfferStatus {
            bytes_read: progress.bytes.load(Ordering::Acquire),
            completed,
        }
    }
}
impl RemoteOffer {
    fn new(session: Arc<Inner>, epoch: u64) -> Self {
        Self {
            session,
            epoch,
            alive: AtomicBool::new(true),
            task: AtomicU32::new(0),
            descriptors: Mutex::new(None),
            reading: Mutex::new(()),
            dragging: AtomicBool::new(false),
            mount: Mutex::new(None),
        }
    }
    fn cancel(&self) {
        self.alive.store(false, Ordering::Release);
        let task = self.task.swap(0, Ordering::AcqRel);
        if task != 0 {
            let _ = self.session.enqueue(Outbound::Cleanup(request(
                self.session.next(),
                ClipboardRequestKind::CancelRequest(ClipboardFileCancelRequest { task_id: task }),
            )));
        }
    }
    fn valid(&self) -> Result<()> {
        ensure!(
            self.alive.load(Ordering::Acquire) && self.session.valid(self.epoch),
            "剪贴板来源已失效"
        );
        Ok(())
    }
    fn task(&self) -> u32 {
        let existing = self.task.load(Ordering::Acquire);
        if existing != 0 {
            return existing;
        }
        let task = self.session.next() as u32;
        let _ = self
            .task
            .compare_exchange(0, task, Ordering::AcqRel, Ordering::Acquire);
        self.task.load(Ordering::Acquire)
    }
    fn list(&self) -> Result<Vec<ClipboardFileDescriptor>> {
        self.valid()?;
        let existing = lock(&self.descriptors).clone();
        if let Some(list) = existing {
            return Ok(list);
        }
        let _busy = self.enter_read()?;
        let list = self.session.descriptors(self.epoch, self.task())?;
        self.valid()?;
        *lock(&self.descriptors) = Some(list.clone());
        Ok(list)
    }
    fn enter_read(&self) -> Result<std::sync::MutexGuard<'_, ()>> {
        let guard = lock(&self.reading);
        self.valid()?;
        Ok(guard)
    }
}
impl Drop for RemoteOffer {
    fn drop(&mut self) {
        self.cancel();
        if let Some((mount, _)) = lock(&self.mount).take() {
            std::thread::spawn(move || {
                std::thread::sleep(MOUNT_GRACE);
                drop(mount);
            });
        }
    }
}

struct Worker {
    sender: SyncSender<Command>,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}

static WORKER: OnceLock<std::result::Result<Worker, String>> = OnceLock::new();

/// One file the local clipboard offers, described the way a Windows peer reads
/// it: a backslash path relative to the copied root, Windows attribute bits and
/// a FILETIME.
struct LocalFile {
    desc: ClipboardFileDescriptor,
    /// `None` for a directory, which has no contents to serve.
    path: Option<PathBuf>,
    /// The copied item this entry was reached through; contents are refused if
    /// the path stops resolving inside it.
    root: PathBuf,
}

/// A flattened snapshot of everything one local copy put on the clipboard.
struct LocalFiles {
    items: Vec<LocalFile>,
}

/// What this client currently owns locally, as the remote would see it.
#[derive(Default, Clone, PartialEq, Eq)]
pub(super) struct LocalSnapshot {
    pub(super) text: Option<String>,
    pub(super) image: Option<Vec<u8>>,
    /// The paths a local copy put on the clipboard. They are only walked when
    /// the remote asks for the list, so copying a large tree locally costs
    /// nothing until someone pastes it there.
    pub(super) sources: Vec<PathBuf>,
}

impl LocalSnapshot {
    fn is_empty(&self) -> bool {
        self.text.is_none() && self.image.is_none() && self.sources.is_empty()
    }

    /// CF_UNICODETEXT and CF_DIB are what a Windows peer understands; files
    /// travel as the descriptor and contents pair, as they do in OLE.
    fn format_ids(&self) -> Vec<(u32, String)> {
        let mut ids = Vec::new();
        if !self.sources.is_empty() {
            ids.push((
                formats::register("FileGroupDescriptorW"),
                "FileGroupDescriptorW".into(),
            ));
            ids.push((formats::register("FileContents"), "FileContents".into()));
            // `Preferred DropEffect` 不广告：Windows 生产端从来不把它放上线（见
            // `formats::supported`），只有 Linux 广告它就成了没有依据的平台差异，
            // 而「不给它官方客户端就停在试探阶段」这个说法并没有日志支撑。真被问到时
            // 照样按 COPY 答，见 `LocalSnapshot::data`。
            return ids;
        }
        if self.text.is_some() {
            ids.push((13, String::new()));
            ids.push((1, String::new()));
        }
        if self.image.is_some() {
            ids.push((8, String::new()));
        }
        ids
    }

    fn data(&self, format: &Format) -> Result<Vec<u8>> {
        match format.local {
            13 => self
                .text
                .as_deref()
                .map(formats::unicode)
                .context("本地剪贴板没有文本"),
            1 => self
                .text
                .as_deref()
                .map(|text| {
                    let mut bytes = text.replace('\n', "\r\n").into_bytes();
                    bytes.push(0);
                    bytes
                })
                .context("本地剪贴板没有文本"),
            8 => self.image.clone().context("本地剪贴板没有图片"),
            // 这块按 Windows 标准语义放 FILETIME（ticks）。曾经以为官方主控粘我们的
            // 文件时按名字来问的就是这一槽、而且按 Unix 秒读，于是这里也换成过秒——
            // 2026-10-16 实测把它否掉了：官方取我们的文件走的是 `FileDescListRequest`
            // （日志「剪贴板收到文件列表请求」连问四次），这条没被问过；当时那个「日期
            // 空白」是**线上描述符**里发 ticks 造成的（见 `protocol::wire_seconds`）。
            _ if format.wire.name == "FileGroupDescriptorW" => {
                ensure!(!self.sources.is_empty(), "本地剪贴板没有文件");
                let files = collect_files(&self.sources)?;
                let descriptors: Vec<_> = files.items.iter().map(|f| f.desc.clone()).collect();
                let blob = formats::descriptor_bytes(&descriptors, "不安全的文件名称")?;
                tracing::info!(
                    files = descriptors.len(),
                    ticks = descriptors.first().map_or(0, |f| f.last_write_time),
                    bytes = blob.len(),
                    first = %descriptors.first().map_or(String::new(), |f| f.file_name.clone()),
                    "剪贴板文件描述块已答出"
                );
                Ok(blob)
            }
            // 只做复制（DROPEFFECT_COPY = 1）。答成移动会让文件管理器把被控端
            // 的原文件搬走——这条不是可选的礼貌，是安全线。
            _ if format
                .wire
                .name
                .eq_ignore_ascii_case("Preferred DropEffect") =>
            {
                Ok(1u32.to_le_bytes().to_vec())
            }
            _ => bail!("不支持的本地剪贴板格式 {}", format.wire.name),
        }
    }
}

/// Windows file attributes this client synthesises from a Unix mode.
const FILE_ATTRIBUTE_READONLY: u32 = 0x1;
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;
/// A Unix mtime as the FILETIME a Windows peer stamps the pasted file with.
fn filetime(meta: &std::fs::Metadata) -> u64 {
    let seconds = meta.mtime();
    if seconds < -(FILETIME_EPOCH_OFFSET as i64) {
        return 0;
    }
    let ticks = (seconds + FILETIME_EPOCH_OFFSET as i64) as u64;
    ticks
        .saturating_mul(TICKS_PER_SECOND)
        .saturating_add(meta.mtime_nsec().max(0) as u64 / 100)
}

fn attributes(meta: &std::fs::Metadata) -> u32 {
    let mut bits = if meta.is_dir() {
        FILE_ATTRIBUTE_DIRECTORY
    } else {
        FILE_ATTRIBUTE_NORMAL
    };
    // Owner write is the closest thing this filesystem has to the read-only bit.
    if meta.mode() & 0o200 == 0 {
        bits |= FILE_ATTRIBUTE_READONLY;
    }
    bits
}

/// Walk one copied path into the flat descriptor list the protocol carries.
/// Names are relative to the copied item and use the separator Windows expects.
fn collect_file(path: &Path, root: &Path, name: &str, items: &mut Vec<LocalFile>) -> Result<()> {
    ensure!(
        items.len() < MAX_FILES && safe_name(name),
        "文件数量或名称不支持"
    );
    let meta = std::fs::symlink_metadata(path)?;
    // A symlink is not copied: the peer would receive its target under a name
    // that promises otherwise, and the target may sit outside the copy.
    ensure!(!meta.is_symlink(), "不传输符号链接");
    ensure!(meta.is_dir() || meta.is_file(), "只支持普通文件与目录");
    let canonical = std::fs::canonicalize(path)?;
    ensure!(canonical.starts_with(root), "文件超出复制范围");
    items.push(LocalFile {
        desc: ClipboardFileDescriptor {
            file_name: name.replace('/', "\\"),
            file_attributes: attributes(&meta),
            last_write_time: filetime(&meta),
            file_size: if meta.is_dir() { 0 } else { meta.len() },
        },
        path: (!meta.is_dir()).then(|| canonical.clone()),
        root: root.to_path_buf(),
    });
    if meta.is_dir() {
        let mut entries: Vec<_> = std::fs::read_dir(&canonical)?
            .collect::<std::result::Result<Vec<_>, _>>()?
            .into_iter()
            .map(|entry| entry.file_name())
            .collect();
        entries.sort();
        for entry in entries {
            collect_file(
                &canonical.join(&entry),
                root,
                &format!("{name}\\{}", entry.to_string_lossy()),
                items,
            )?;
        }
    }
    Ok(())
}

/// Flatten the paths the local clipboard holds. One unreadable entry fails the
/// whole offer rather than handing the peer a list it cannot complete.
fn collect_files(paths: &[PathBuf]) -> Result<LocalFiles> {
    let mut items = Vec::new();
    for path in paths {
        let root = std::fs::canonicalize(path)?;
        let name = root
            .file_name()
            .context("无法确定文件名")?
            .to_string_lossy()
            .into_owned();
        collect_file(&root, &root, &name, &mut items)?;
    }
    ensure!(!items.is_empty(), "文件列表为空");
    Ok(LocalFiles { items })
}

impl LocalFiles {
    /// Serve one `FileContentsRequest`. `flags` is 1 for the size and 2 for a
    /// range of the contents, as the official client sends them.
    fn read(&self, ask: &ClipboardFileContentsRequest) -> Result<Vec<u8>> {
        let file = self
            .items
            .get(ask.list_index as usize)
            .context("无效的文件索引")?;
        ensure!(ask.requested_len as usize <= FILE_BLOCK, "文件读取请求过大");
        if ask.flags == 1 {
            return Ok(file.desc.file_size.to_le_bytes().to_vec());
        }
        ensure!(
            ask.flags == 2 && file.desc.file_attributes & FILE_ATTRIBUTE_DIRECTORY == 0,
            "无效的文件读取类型"
        );
        let path = file.path.as_ref().context("该条目没有内容")?;
        // O_NOFOLLOW closes the window where the final component is swapped for
        // a link between the walk and this read.
        let mut handle = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)?;
        ensure!(
            std::fs::canonicalize(path)?.starts_with(&file.root),
            "文件超出原始复制范围"
        );
        let length =
            (u64::from(ask.requested_len)).min(file.desc.file_size.saturating_sub(ask.pos_offset));
        if length == 0 {
            return Ok(Vec::new());
        }
        use std::io::{Read as _, Seek as _};
        handle.seek(std::io::SeekFrom::Start(ask.pos_offset))?;
        let mut data = vec![0u8; length as usize];
        let mut filled = 0;
        while filled < data.len() {
            match handle.read(&mut data[filled..])? {
                0 => break,
                n => filled += n,
            }
        }
        data.truncate(filled);
        Ok(data)
    }
}

struct State {
    receiver: Receiver<Command>,
    clipboard: Option<arboard::Clipboard>,
    sessions: HashMap<u64, Weak<Inner>>,
    published: HashMap<u64, Vec<Format>>,
    local: LocalSnapshot,
    /// 上一次在 Wayland 那一侧看到的、以及上一次在 X11 那一侧看到的。两座孤岛各记
    /// 各的——共用一个的话，刚发布完 X11 那侧的内容，下一轮就会被 Wayland 那侧的
    /// 旧内容顶掉。
    wayland: LocalSnapshot,
    x11: LocalSnapshot,
    /// File lists handed out per session and task, kept until the clipboard
    /// changes so a slow remote can keep reading the copy it started on.
    tasks: HashMap<(u64, u32), Arc<LocalFiles>>,
    /// 本地那一批路径展开出来的清单，和 `local` 配套。
    ///
    /// 展开一次要整树遍历，而对方是按格式一个个来问的（同一份清单还可能被问好几
    /// 遍），每次都重走一遍就把剪贴板线程按住了。本地剪贴板一换它就作废，见
    /// `retire`——包括「同一份又复制了一遍」那种：那时内容没变，可文件本身可能已经
    /// 改过，重新走一遍才能拿到新的大小与改动时间。
    local_files: Option<Arc<LocalFiles>>,
    /// The remote copy currently published locally, mounted for as long as the
    /// clipboard points at it.
    mounted: Option<super::fuse::Mount>,
    /// 我们替这次复制持有的 X11 选择区（远端发来的文件、或者远端发来的文本）。
    offer: Option<super::x11_offer::Offer>,
    /// Files offered to one session for a drag or a drop rather than through
    /// the clipboard: a drop's list is handed out once, to the descriptor
    /// request that answers it; a drag's stays for every read of its session.
    drag_files: HashMap<u64, (Arc<LocalFiles>, Option<Arc<super::drag::Submission>>)>,
    /// Lists handed out from `drag_files`, which local copies do not retire.
    drag_tasks: HashMap<(u64, u32), Arc<LocalFiles>>,
    /// Set while this adapter writes, so the poll does not report its own write.
    writing: bool,
    /// 本地剪贴板上现在这一份是远端写进来的（`write_text`/`write_image`/
    /// `write_files` 置位，本地复制广播出去时清掉）。
    ///
    /// 剪贴板管理器（`wl-clip-persist` 一类）会在我们每次写入之后再以自己的名义认领
    /// 一次，于是每写一次就会多收一个写事件。多出来的那个会被算成「用户又复制了一
    /// 遍」，把刚收到的远端内容原样发回去——两台都装这类工具的机器之间会来回互推。
    /// 有这一位就不会。代价是这一位要到下一次本地复制广播出去才清：远端发来 R 之后，
    /// 用户在本地把一模一样的 R 再复制一遍不会重发。
    remote: bool,
    /// 本地剪贴板的「写次数」通道：内容没变但有人重新复制过时，只有它看得出来。
    /// `0` 表示没接上（合成器没这个协议），那时退回按内容判断。
    watch: Option<super::watch::Watch>,
    /// 我们已知的这份剪贴板内容对应的写次数。
    own_generation: u64,
}

impl State {
    /// 本地剪贴板这一份到此为止：发出去的文件清单和展开出来的树都跟着重来。
    fn retire(&mut self) {
        self.tasks.clear();
        self.local_files = None;
    }

    /// 本地这份清单。展开过一次就留着，见 `local_files`。
    fn local_files(&mut self) -> Result<Arc<LocalFiles>> {
        if let Some(files) = &self.local_files {
            return Ok(files.clone());
        }
        let files = Arc::new(collect_files(&self.local.sources)?);
        self.local_files = Some(files.clone());
        Ok(files)
    }
}

pub(super) fn start() -> Result<()> {
    WORKER
        .get_or_init(|| {
            let (sender, receiver) = sync_channel(64);
            let thread = std::thread::Builder::new()
                .name("UU clipboard".into())
                .spawn(move || run(receiver))
                .map_err(|error| format!("启动剪贴板线程失败：{error}"))?;
            Ok(Worker {
                sender,
                thread: Mutex::new(Some(thread)),
            })
        })
        .as_ref()
        .map(|_| ())
        .map_err(|error| anyhow!(error.clone()))
}

pub(super) fn post(command: Command) -> Result<()> {
    start()?;
    let worker = WORKER
        .get()
        .and_then(|worker| worker.as_ref().ok())
        .context("剪贴板线程不可用")?;
    worker
        .sender
        .try_send(command)
        .map_err(|_| anyhow!("剪贴板队列已满或已关闭"))
}

/// The Windows adapter pumps its STA here. This worker is a plain thread, so
/// callers waiting on a response simply keep waiting on their condvar.
pub(super) fn pump() {}

pub(super) fn shutdown() {
    let Some(Ok(worker)) = WORKER.get() else {
        return;
    };
    // Dropping every sender ends the receive loop; the clone here is the last one.
    let thread = lock(&worker.thread).take();
    if let Some(thread) = thread {
        let _ = worker.sender.try_send(Command::Remove(u64::MAX));
        drop(thread);
    }
}

fn run(receiver: Receiver<Command>) {
    let clipboard = match arboard::Clipboard::new() {
        Ok(clipboard) => Some(clipboard),
        Err(error) => {
            tracing::warn!(%error, "系统剪贴板不可用，剪贴板同步将保持关闭");
            None
        }
    };
    let mut state = State {
        receiver,
        clipboard,
        sessions: HashMap::new(),
        published: HashMap::new(),
        wayland: LocalSnapshot::default(),
        x11: LocalSnapshot::default(),
        local: LocalSnapshot::default(),
        tasks: HashMap::new(),
        local_files: None,
        mounted: None,
        offer: None,
        drag_files: HashMap::new(),
        drag_tasks: HashMap::new(),
        writing: false,
        remote: false,
        watch: Some(super::watch::Watch::start()),
        own_generation: 0,
    };
    loop {
        match state.receiver.recv_timeout(POLL) {
            Ok(command) => {
                if matches!(command, Command::Remove(u64::MAX)) {
                    break;
                }
                process(&mut state, command);
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if let Err(error) = poll_local(&mut state) {
                    tracing::debug!(%error, "读取本地剪贴板失败");
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
}

fn sessions(state: &State) -> Vec<Arc<Inner>> {
    state
        .sessions
        .values()
        .filter_map(std::sync::Weak::upgrade)
        // A drag session exchanges only the files it was given.
        .filter(|session| session.file_offers.is_none())
        .collect()
}

fn process(state: &mut State, command: Command) {
    for (files, ticket) in state.drag_files.values_mut() {
        if let Some(ticket) = ticket
            && !ticket.valid()
            && !files.items.is_empty()
        {
            ticket.fail("拖放等待超时或权限已撤销".into());
            // Keep the rejected publication until its descriptor request
            // arrives; never substitute ordinary copied files.
            *files = Arc::new(LocalFiles { items: Vec::new() });
        }
    }
    match command {
        Command::Activate(weak) => {
            if let Some(session) = weak.upgrade() {
                state.sessions.insert(session.id, Arc::downgrade(&session));
                tracing::debug!(
                    id = session.id,
                    sessions = state.sessions.len(),
                    files = session.file_allowed(),
                    "剪贴板会话已注册"
                );
            }
        }
        Command::Remove(id) => {
            state.sessions.remove(&id);
            state.published.remove(&id);
            state.drag_files.remove(&id);
            state.drag_tasks.retain(|(session, _), _| *session != id);
        }
        Command::Offer(weak, epoch, formats, drop) => {
            if let Some(session) = weak.upgrade().filter(|session| session.valid(epoch))
                && let Err(error) = if drop.is_some() || session.file_offers.is_some() {
                    // A dropped, auto-saved or dragged file list is not a
                    // copy; leave the local clipboard alone.
                    offer_files(&session, epoch, formats, drop)
                } else {
                    accept_offer(state, &session, epoch, formats)
                }
            {
                // The notice shows the outermost line; the cause is only in
                // the chain, and it is the part worth reading.
                tracing::warn!(error = format!("{error:#}"), "接受远端剪贴板失败");
                session.fail(error.to_string());
            }
        }
        Command::Text(weak, epoch, id, text) => {
            if let Some(session) = weak.upgrade().filter(|session| session.valid(epoch)) {
                let result = if text.is_empty() {
                    Err(anyhow!("空文本"))
                } else {
                    write_text(state, &text)
                };
                if let Err(error) = &result {
                    tracing::debug!(%error, "写入本地剪贴板文本失败");
                }
                let _ = session.emit(
                    epoch,
                    Envelope {
                        request: None,
                        response: Some(Response {
                            header: Some(Header { id }),
                            clip: None,
                            text: Some(ClipboardTextChangeResponse {
                                err: if result.is_ok() { 1 } else { 2 },
                            }),
                        }),
                    },
                );
            }
        }
        Command::DropFiles(ticket, files, action) => {
            let result = (|| -> Result<()> {
                ensure!(ticket.valid(), "文件拖放已过期");
                let session = ticket.session.upgrade().context("连接已结束")?;
                let links = formats::outgoing(
                    &file_format_ids(),
                    session.platform.load(Ordering::Acquire),
                    true,
                );
                state
                    .drag_files
                    .insert(session.id, (Arc::new(files.files), Some(ticket.clone())));
                session.emit(
                    ticket.epoch,
                    request(
                        session.next(),
                        ClipboardRequestKind::FormatList(ClipboardFormatListRequest {
                            formats: links.into_iter().map(|format| format.wire).collect(),
                            has_action: 1,
                            drag_drop_action: Some(action),
                        }),
                    ),
                )?;
                ticket.published.store(true, Ordering::Release);
                Ok(())
            })();
            if let Err(error) = result {
                ticket.fail(error.to_string());
            }
        }
        Command::PublishFiles(weak, epoch, files, reply) => {
            let result = (|| -> Result<()> {
                let session = weak
                    .upgrade()
                    .filter(|session| session.valid(epoch) && session.file_allowed())
                    .context("文件发布已取消")?;
                let links = formats::outgoing(
                    &file_format_ids(),
                    session.platform.load(Ordering::Acquire),
                    true,
                );
                state
                    .drag_files
                    .insert(session.id, (Arc::new(files.files), None));
                session.emit(
                    epoch,
                    request(
                        session.next(),
                        ClipboardRequestKind::FormatList(ClipboardFormatListRequest {
                            formats: links.into_iter().map(|format| format.wire).collect(),
                            has_action: 0,
                            drag_drop_action: None,
                        }),
                    ),
                )
            })();
            let _ = reply.send(result.map_err(|error| error.to_string()));
        }
        Command::Request(weak, epoch, id, kind) => {
            if let Some(session) = weak.upgrade().filter(|session| session.valid(epoch))
                && let Err(error) = serve(state, &session, epoch, id, kind)
            {
                session.fail(error.to_string());
            }
        }
    }
}

/// Fetch the best offered format now, because the local clipboard cannot
/// promise data it does not yet hold.
fn accept_offer(
    state: &mut State,
    session: &Arc<Inner>,
    epoch: u64,
    offered: Vec<ClipboardFormat>,
) -> Result<()> {
    if offered.is_empty() {
        return Ok(());
    }
    let platform = session.platform.load(Ordering::Acquire);
    let files = session.file_allowed();
    let links: Vec<Format> = offered
        .into_iter()
        .filter_map(|format| formats::incoming(format, platform, files))
        .collect();
    // Files win when offered: a peer that copied files usually also offers
    // their names as text, and pasting the names is not what was asked for.
    if files && links.iter().any(|link| link.local == descriptor_format()) {
        return accept_files(state, session, epoch);
    }
    let Some(format) = [13u32, 1, 8, 17]
        .into_iter()
        .find_map(|id| links.iter().find(|format| format.local == id))
        .cloned()
    else {
        return Ok(());
    };
    let data = session.data(epoch, &format.wire)?;
    ensure!(data.len() <= MAX_CLIP, "剪贴板内容过大");
    let data = formats::convert(data, &format, platform, false)?;
    match format.local {
        13 => {
            let text = utf16_text(&data)?;
            write_text(state, &text)?;
        }
        1 => {
            let text = String::from_utf8_lossy(&data)
                .trim_end_matches('\0')
                .replace("\r\n", "\n");
            write_text(state, &text)?;
        }
        8 | 17 => write_image(state, &data)?,
        _ => return Ok(()),
    }
    *lock(&session.error) = None;
    Ok(())
}

fn descriptor_format() -> u32 {
    formats::register("FileGroupDescriptorW")
}

/// The descriptor and contents pair a file list travels as.
fn file_format_ids() -> Vec<(u32, String)> {
    vec![
        (descriptor_format(), "FileGroupDescriptorW".into()),
        (formats::register("FileContents"), "FileContents".into()),
    ]
}

/// A file list offered for a drop or a drag rather than as a copy: the files
/// stay remote until the receiving side reads them.
fn offer_files(
    session: &Arc<Inner>,
    epoch: u64,
    offered: Vec<ClipboardFormat>,
    action: Option<ClipboardFormatListRequestKind>,
) -> Result<()> {
    ensure!(!offered.is_empty(), "独立文件提议为空");
    let platform = session.platform.load(Ordering::Acquire);
    let links: Vec<Format> = offered
        .into_iter()
        .filter_map(|format| formats::incoming(format, platform, session.file_allowed()))
        .collect();
    let contents = formats::register("FileContents");
    ensure!(
        session.file_allowed()
            && links.iter().any(|link| link.local == descriptor_format())
            && links
                .iter()
                .all(|link| link.local == descriptor_format() || link.local == contents),
        "独立会话只接受文件格式"
    );
    let offer = FileOffer(Arc::new(RemoteOffer::new(session.clone(), epoch)));
    if let Some(action) = action {
        return super::drag::receive(offer, action);
    }
    session
        .file_offers
        .as_ref()
        .context("文件提议没有接收方")?
        .try_send(offer)
        .map_err(|_| anyhow!("文件提议接收方已关闭或繁忙"))
}

/// Mount the remote's copy and point the local clipboard at it.
///
/// Only the list is fetched here. The contents follow one read at a time,
/// through the filesystem, if and when something actually opens them.
fn accept_files(state: &mut State, session: &Arc<Inner>, epoch: u64) -> Result<()> {
    // The task id the official Windows client uses for an inbound offer.
    const TASK: u32 = 0;
    let descriptors = session.descriptors(epoch, TASK)?;
    ensure!(!descriptors.is_empty(), "远端剪贴板文件列表为空");
    let tree = super::fuse::Tree::build(&descriptors)?;
    let parent = super::fuse::mount_parent()?;
    let reader = {
        let session = Arc::downgrade(session);
        Arc::new(move |index: u32, offset: u64, length: usize| {
            let session = session.upgrade().context("剪贴板会话已结束")?;
            session.read_file(epoch, TASK, index, offset, length.min(FILE_BLOCK), 2)
        })
    };
    let generation = super::fuse::generation();
    let mount = super::fuse::Mount::new(&parent, generation, tree, reader)?;
    let paths = mount.paths();
    write_files(state, &paths)?;
    // 每次远端复制都走到这里，而且只此一次。这条把两个关键事实分开记：「清单里有多
    // 少项、多大、什么时候改的」说明远端给的是不是新那一份，「挂到了哪一代」说明本地
    // 剪贴板有没有跟着换过去。两件事坏掉的样子完全不同（内容还是旧的 / 粘贴根本不动），
    // 光看现象分不出来。
    //
    // 名字不记：文件名是远端用户的内容，而发布出去的路径就是挂载点加上这些名字。
    tracing::info!(
        mount = generation,
        roots = paths.len(),
        files = descriptors.len(),
        size = descriptors[0].file_size,
        // 改过的文件往往名字和大小都不变（改两个字长度一样），上面两项就分不出
        // 「远端给的是新那一份」还是「还是旧的那份」。改动时间能分出来。
        modified = descriptors[0].last_write_time,
        "已挂载远端剪贴板文件"
    );
    // Held until the next copy replaces it: the clipboard still points here,
    // and the paste may be minutes away.
    state.mounted = Some(mount);
    *lock(&session.error) = None;
    Ok(())
}

fn utf16_text(bytes: &[u8]) -> Result<String> {
    ensure!(bytes.len().is_multiple_of(2), "无效的Unicode剪贴板");
    let mut words: Vec<u16> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_le_bytes(*pair))
        .collect();
    while words.last() == Some(&0) {
        words.pop();
    }
    if words.first() == Some(&0xfeff) {
        words.remove(0);
    }
    Ok(String::from_utf16(&words)?.replace("\r\n", "\n"))
}

fn write_text(state: &mut State, text: &str) -> Result<()> {
    let clipboard = state.clipboard.as_mut().context("系统剪贴板不可用")?;
    state.writing = true;
    let result = clipboard.set_text(text.to_owned());
    state.writing = false;
    result.context("写入系统剪贴板失败")?;
    // XWayland 的程序只看得到 X11 那一侧的选择区，而 arboard 在 Wayland 会话里写的是
    // 另一座孤岛（niri 这类合成器不桥接），光靠它远端发来的文本在 XWayland 程序里粘不
    // 出来。所以文本也像文件那样自己挂一份到 X11 上——`x11_read::wanted` 说的正是「这
    // 个会话里 arboard 落在 Wayland 那一侧、而且另有一座 X11 孤岛」，也只有那时才需
    // 要（它落回 X11 时，上面那次 `set_text` 写的就是 X11）。
    state.offer = None;
    state.local = LocalSnapshot {
        text: Some(text.to_owned()),
        ..Default::default()
    };
    // 写成的那一侧「上次看到的」当场就记成这一份，不能等下一轮把它读回来再记：读回来
    // 之前用户可能已经复制了别的。那时 X11 那一侧读到的是我们自己挂的这份，和旧记录一
    // 比像是「变了」，会把它当成本地复制发回去，顶掉对端随后收到的那份；Wayland 那一
    // 侧读到的是用户复制的东西，而它要是恰好和旧记录一样（同一份又复制了一遍），就会
    // 被当成「没变」吞掉。
    state.wayland = state.local.clone();
    if super::x11_read::wanted() {
        match super::x11_offer::Offer::publish_text(text.to_owned()) {
            Ok(offer) => {
                state.offer = Some(offer);
                state.x11 = state.local.clone();
            }
            Err(error) => tracing::debug!(error = format!("{error:#}"), "接管 X11 剪贴板失败"),
        }
    }
    state.remote = true;
    state.retire();
    state.mounted = None;
    Ok(())
}

/// Publish paths as a file copy on the local clipboard, and record them as the
/// copy this adapter put there so the poll does not announce them back.
///
/// Which clipboard that is takes some care: see `publish_files`.
fn write_files(state: &mut State, paths: &[PathBuf]) -> Result<()> {
    state.writing = true;
    let result = publish_files(state, paths);
    state.writing = false;
    let published = result.context("写入系统剪贴板失败")?;
    state.local = LocalSnapshot {
        sources: paths.to_vec(),
        ..Default::default()
    };
    // 和 `write_text` 一样，写成的那一侧当场记成这一份。没写成的那一侧不动：它上面还
    // 是原来那份，记成这一份的话，下一轮读到原来那份就会被当成一次新复制发出去。
    if published.arboard {
        state.wayland = state.local.clone();
    }
    if published.x11 {
        state.x11 = state.local.clone();
    }
    state.remote = true;
    state.retire();
    Ok(())
}

/// 把这一批路径挂到本地剪贴板上。
///
/// X11 与 Wayland 的选择区是两个互不相通的剪贴板：这条路在 niri 上实测双向都
/// 不过桥——xclip 写进去的东西 `wl-paste` 看不见，反过来也一样。X11 的 offer
/// 只有 XWayland 里的程序看得见，而 Wayland 原生的程序（GNOME 那套文件管理器
/// 就是）只读 Wayland 那一侧。只写一边，本地总有一半程序粘不出来。
///
/// 所以两边都写。arboard 的后端跟着 `WAYLAND_DISPLAY` 走（有这个变量就走
/// Wayland data-control），Wayland 会话里由它写 Wayland 那侧；X11 会话里它自己
/// 就落在 X11 上，这时不能让它抢先——它只会挂 `text/uri-list` 一个名字，而
/// GNOME 的文件管理器只认 `x-special/gnome-copied-files`，多个名字只有 X11
/// 的 offer 给得出。
fn publish_files(state: &mut State, paths: &[PathBuf]) -> Result<Published> {
    let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
    let mut published = Published::default();
    if wayland {
        arboard_files(state, paths).context("写入 Wayland 剪贴板失败")?;
        published.arboard = true;
    }
    match super::x11_offer::Offer::publish_files(paths.to_vec()) {
        Ok(offer) => {
            state.offer = Some(offer);
            published.x11 = true;
            // arboard 没落在 Wayland 上（没有 `WAYLAND_DISPLAY`，写的就是 X11）时，
            // X11 这一侧现在也是这一份。
            published.arboard |= !wayland;
            Ok(published)
        }
        Err(error) => {
            state.offer = None;
            tracing::debug!(error = format!("{error:#}"), "接管 X11 剪贴板失败");
            // Wayland 那一侧上一步已经写好了，这里失败只是少了 XWayland 那半个
            // 桌面；X11 会话里没有别处可写，只能退回 arboard 的单格式。
            if wayland {
                return Ok(published);
            }
            arboard_files(state, paths)?;
            published.arboard = true;
            Ok(published)
        }
    }
}

/// `publish_files` 写成了哪几侧。
#[derive(Clone, Copy, Default)]
struct Published {
    /// arboard 那一侧（`read_wayland` 读的那一侧）挂上了这一份。
    arboard: bool,
    /// X11 那一侧挂的是我们的 offer。
    x11: bool,
}

/// 让 arboard 挂上这一批文件：Wayland 会话里它写 Wayland 那一侧，X11 会话里它就在
/// X11 上——而那时它一次只挂 `text/uri-list` 一个名字，GNOME 的文件管理器只认
/// `x-special/gnome-copied-files`，所以它排在 X11 那条 offer 后面。
fn arboard_files(state: &mut State, paths: &[PathBuf]) -> Result<()> {
    let clipboard = state.clipboard.as_mut().context("系统剪贴板不可用")?;
    clipboard
        .set()
        .file_list(paths)
        .map_err(anyhow::Error::from)
        .context("写入系统剪贴板失败")
}

fn write_image(state: &mut State, dib: &[u8]) -> Result<()> {
    let image = dib_to_rgba(dib)?;
    let clipboard = state.clipboard.as_mut().context("系统剪贴板不可用")?;
    state.writing = true;
    let result = clipboard.set_image(arboard::ImageData {
        width: image.0 as usize,
        height: image.1 as usize,
        bytes: std::borrow::Cow::Owned(image.2),
    });
    state.writing = false;
    result.context("写入系统剪贴板失败")?;
    state.local = LocalSnapshot {
        image: Some(dib.to_vec()),
        ..Default::default()
    };
    // 同上：图片也没有记号，作废会把这张图原样发回去。
    state.remote = true;
    state.retire();
    state.mounted = None;
    state.offer = None;
    Ok(())
}

/// Strip the line ending a `text/uri-list` entry carries into the path.
///
/// RFC 2483 delimits that format with CRLF, which is what GNOME and every other
/// file manager here writes, but the parser these paths come from splits on the
/// line feed alone and leaves the carriage return on the end of the name. The
/// path then resolves to nothing and the copy looks empty.
fn trim_uri_path(path: PathBuf) -> PathBuf {
    use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
    let bytes = path.as_os_str().as_bytes();
    let trimmed = bytes
        .iter()
        .rposition(|byte| !matches!(byte, b'\r' | b'\n'))
        .map_or(0, |last| last + 1);
    if trimmed == bytes.len() {
        return path;
    }
    PathBuf::from(std::ffi::OsString::from_vec(bytes[..trimmed].to_vec()))
}

/// Read the local clipboard and, when it changed, announce the new formats.
///
/// 两个剪贴板都要读。X11 与 Wayland 在这里互不相通（见 `publish_files`）：XWayland
/// 里的程序复制时 Wayland 那一侧什么都不会变，光看 Wayland 就等于看不见它们。
fn poll_local(state: &mut State) -> Result<()> {
    if state.writing || state.sessions.is_empty() {
        return Ok(());
    }
    // 「变了没有」不能只看内容：同一个文件复制第二遍，路径、文本、像素全都一样，
    // 但那是一次新的复制，对端可能已经把上一份用掉了（官方 Windows 客户端粘完就不
    // 再拿那个 offer），不重新广播它就一直粘不上。Windows 那边有
    // `GetClipboardSequenceNumber()`，Linux 这边就是合成器给的这个写次数。
    let generation = state
        .watch
        .as_ref()
        .map_or(0, super::watch::Watch::generation);
    // 写事件通道还活着、而且自我们上次记下写次数以来没人认领过剪贴板：Wayland 那一侧
    // 的内容不可能变过（认领一次一定会在通道上留下一次前进），这时不必再走 arboard。
    // 剪贴板上挂着一张截图时，arboard 每一轮都要把整份 PNG 解出来，而这里 400ms 就
    // 来一轮，实测能一直烧掉半个核；没变的东西没有理由重解一遍。
    // X11 那一侧照旧每轮都读：两座孤岛不通（见 `publish_files`，本机实测双向都不过
    // 桥），那边有人复制不会在 Wayland 通道上留下事件，跟着一起跳过就再也看不见了。
    let settled = state.watch.as_ref().is_some_and(|watch| {
        watch.alive() && generation != 0 && generation == state.own_generation
    });
    let wayland = if settled {
        state.wayland.clone()
    } else {
        read_wayland(state)?
    };
    let x11 = super::x11_read::read();
    // 自己挂上去的远端副本不算「本地复制」：发回去等于向远端要它刚给我们的东西。
    let (own_wayland, own_x11) = (own_copy(&wayland.sources), own_copy(&x11.sources));
    let written = if own_wayland || own_x11 || state.remote {
        // 这一轮看到的内容还是我们自己挂上去的那份（远端复制过来的副本、或者远端刚
        // 发来的文本/图片）：写次数前进不算理由——那次前进很可能就是我们自己写它引
        // 起的（剪贴板管理器还会在我们每次写入后再认领一次，多出一个写次数），而合
        // 成器的事件到得比这一轮轮询晚，拿它当真就会把远端刚给我们的东西又发回去。
        false
    } else {
        generation != 0 && generation != state.own_generation
    };
    let (wayland_changed, x11_changed) = (
        written || (!own_wayland && wayland != state.wayland),
        written || (!own_x11 && x11 != state.x11),
    );
    if !own_wayland {
        state.wayland = wayland.clone();
    }
    if !own_x11 {
        state.x11 = x11.clone();
    }
    // 两边同一轮都变了时让给 X11：XWayland 里的程序只在那一侧看得见，而 Wayland
    // 那一侧的变化更可能是我们自己的东西在动。
    let snapshot = if x11_changed && !x11.is_empty() {
        x11
    } else if wayland_changed {
        wayland
    } else {
        // 两侧都没报告变化：这一轮前进的写次数是我们自己写剪贴板引起的（读回来
        // 的还是我们挂上去的那份远端副本，见 `own_copy`），把它记下来，否则下一轮
        // 还会当成「刚发生、没处理过的变化」，一直重读整份剪贴板——上面那个跳过
        // 也就永远轮不到。
        state.own_generation = generation;
        return Ok(());
    };
    if snapshot == state.local && !written {
        return Ok(());
    }
    if !snapshot.sources.is_empty() || !state.local.sources.is_empty() {
        tracing::debug!(
            files = snapshot.sources.len(),
            text = snapshot.text.is_some(),
            image = snapshot.image.is_some(),
            "本地剪贴板已变化"
        );
    }
    // 从这一刻起本地剪贴板上这一份是我们自己广播出去的本地复制，不再是远端写进来
    // 的那份。
    state.remote = false;
    state.local = snapshot;
    // 这份内容对应的写次数：下一次没前进就说明没人动过剪贴板。
    state.own_generation = generation;
    // The previous copy is gone; a read still in flight against it now fails.
    state.retire();
    if state.local.is_empty() {
        state.published.clear();
        return Ok(());
    }
    let ids = state.local.format_ids();
    for session in sessions(state) {
        let links = formats::outgoing(
            &ids,
            session.platform.load(Ordering::Acquire),
            session.file_allowed(),
        );
        // 本地复制的文件有没有真的广告出去：`advertised=0` 就是没发（文件权限被关或
        // 格式全被过滤），对面那头自然什么都不动。
        if !state.local.sources.is_empty() {
            tracing::info!(
                files = state.local.sources.len(),
                files_allowed = session.file_allowed(),
                advertised = links.len(),
                formats = %links.iter().map(|f| f.wire.name.clone()).collect::<Vec<_>>().join(","),
                generation,
                "本地文件剪贴板已发布给对端"
            );
        }
        if links.is_empty() {
            state.published.remove(&session.id);
            continue;
        }
        state.published.insert(session.id, links.clone());
        session.emit(
            session.epoch.load(Ordering::Acquire),
            request(
                session.next(),
                ClipboardRequestKind::FormatList(ClipboardFormatListRequest {
                    formats: links.into_iter().map(|format| format.wire).collect(),
                    has_action: 0,
                    drag_drop_action: None,
                }),
            ),
        )?;
        *lock(&session.error) = None;
    }
    Ok(())
}

/// Wayland 那一侧：进程里那个 arboard 实例。
fn read_wayland(state: &mut State) -> Result<LocalSnapshot> {
    let Some(clipboard) = state.clipboard.as_mut() else {
        return Ok(LocalSnapshot::default());
    };
    // The whole outbound path starts here, so when files never reach the remote
    // this says whether they were on the local clipboard at all.
    if tracing::enabled!(target: "openuuyc::clipboard", tracing::Level::DEBUG) {
        let probe = clipboard.get().file_list();
        tracing::debug!(target: "openuuyc::clipboard",
            sessions = state.sessions.len(),
            files = ?probe.as_ref().map(Vec::len).map_err(ToString::to_string),
            first = ?probe.as_ref().ok().and_then(|paths| paths.first().cloned()),
            held = state.local.sources.len(),
            "剪贴板轮询");
    }
    // A file manager puts the paths on the clipboard as `text/uri-list` and a
    // plain-text copy of the same names, so files are looked for first.
    let files = clipboard
        .get()
        .file_list()
        .unwrap_or_default()
        .into_iter()
        .map(trim_uri_path)
        .filter(|path| path.is_absolute())
        .collect::<Vec<_>>();
    if !files.is_empty() {
        return Ok(LocalSnapshot {
            sources: files,
            ..Default::default()
        });
    }
    // A copy the remote sent is published as paths into this process's own
    // filesystem, and under plain-text targets too. Offering their text would
    // replace the remote's copy with a path that only exists here.
    if let Some(text) = clipboard.get_text().ok().filter(|text| !text.is_empty()) {
        return Ok(LocalSnapshot {
            text: Some(text),
            ..Default::default()
        });
    }
    match clipboard.get_image() {
        Ok(image) => Ok(LocalSnapshot {
            image: Some(rgba_to_dib(
                image.width as u32,
                image.height as u32,
                &image.bytes,
            )?),
            ..Default::default()
        }),
        Err(_) => Ok(LocalSnapshot::default()),
    }
}

fn serve(
    state: &mut State,
    session: &Arc<Inner>,
    epoch: u64,
    id: i64,
    kind: ClipboardRequestKind,
) -> Result<()> {
    match kind {
        ClipboardRequestKind::FormatDataAsk(ask) => {
            let result = (|| -> Result<Vec<u8>> {
                let formats = state.published.get(&session.id).context("原剪贴板已失效")?;
                let format = formats
                    .iter()
                    .find(|format| {
                        format.wire.id == ask.format_id
                            && (ask.format_name.is_empty() || ask.format_name == format.wire.name)
                    })
                    .context("未发布该剪贴板格式")?;
                let data = state.local.data(format).inspect_err(|error| {
                    // 官方客户端是逐个格式来问的，答不上哪一条只在这里看得出来；
                    // 之前这里一声不吭，「粘贴不动」在日志里什么都没有。
                    tracing::info!(
                        format = %format.wire.name,
                        format_id = format.wire.id,
                        %error,
                        "剪贴板格式答不上"
                    );
                })?;
                formats::convert(data, format, session.platform.load(Ordering::Acquire), true)
            })();
            match result {
                Ok(data) if !data.is_empty() => {
                    session.enqueue(Outbound::Blocks(epoch, id, ask.block_key, data))
                }
                _ => session.emit(
                    epoch,
                    response(
                        id,
                        ClipboardResponseKind::FormatDataConfirm(ClipboardFormatDataConfirm {
                            err: 2,
                            block_key: ask.block_key,
                            block_count: 0,
                        }),
                    ),
                ),
            }
        }
        ClipboardRequestKind::FileDescListRequest(ask) => {
            // 「官方把我们的文件粘到 Windows 上」走哪条路是个岔口：走这条答出去的是
            // **线上描述符**（时间是 Unix 秒），另一条是按名字要 FileGroupDescriptorW
            // 那个二进制块（槽里现在是 ticks）。两条各留一条日志才分得清。
            tracing::debug!(task = ask.task_id, "剪贴板收到文件列表请求");
            let explicit = drag_list(state, session, ask.task_id);
            let dragged = explicit.is_some() || session.file_offers.is_some();
            let result = (|| -> Result<Arc<LocalFiles>> {
                ensure!(session.file_allowed(), "文件剪贴板已关闭");
                if let Some(files) = explicit {
                    let files = files?;
                    ensure!(!files.items.is_empty(), "文件列表为空");
                    return Ok(files);
                }
                ensure!(session.file_offers.is_none(), "拖放文件提议已失效");
                ensure!(
                    state.published.contains_key(&session.id),
                    "原文件剪贴板已失效"
                );
                ensure!(!state.local.sources.is_empty(), "本地剪贴板没有文件");
                ensure!(state.tasks.len() < 32, "文件任务过多");
                // 展开一次要整树遍历，而对面可能把同一条请求问好几遍（实测问过四
                // 次），展开结果留着复用，见 `local_files`。
                state.local_files()
            })();
            let Ok(files) = result else {
                return session.emit(
                    epoch,
                    response(
                        id,
                        ClipboardResponseKind::FileDescListResponse(
                            ClipboardFileDescriptorListResponse {
                                task_id: ask.task_id,
                                segment_count: 0,
                                err: 2,
                            },
                        ),
                    ),
                );
            };
            if dragged {
                ensure!(state.drag_tasks.len() < 32, "文件任务过多");
                state
                    .drag_tasks
                    .insert((session.id, ask.task_id), files.clone());
            } else {
                state.tasks.insert((session.id, ask.task_id), files.clone());
            }
            // Stay below the SDK's 512 KiB message ceiling even with long names.
            let mut segments = Vec::<Vec<ClipboardFileDescriptor>>::new();
            let mut current = Vec::new();
            let mut bytes = 0;
            for item in &files.items {
                let size = item.desc.encoded_len() + 8;
                if current.len() == 1500 || bytes + size > 450_000 {
                    segments.push(std::mem::take(&mut current));
                    bytes = 0;
                }
                // 线上发 Unix 秒（见 `wire_seconds`）：官方客户端按秒读，
                // 而我们内部、以及给 Windows 的 FILEDESCRIPTORW 里都是 ticks。
                let mut wire = item.desc.clone();
                wire.last_write_time = wire_seconds(wire.last_write_time);
                current.push(wire);
                bytes += size;
            }
            if !current.is_empty() {
                segments.push(current);
            }
            let mut messages = std::collections::VecDeque::new();
            messages.push_back(response(
                id,
                ClipboardResponseKind::FileDescListResponse(ClipboardFileDescriptorListResponse {
                    task_id: ask.task_id,
                    segment_count: segments.len() as u32,
                    err: 1,
                }),
            ));
            for (index, items) in segments.into_iter().enumerate() {
                messages.push_back(request(
                    session.next(),
                    ClipboardRequestKind::DescSegment(ClipboardFileDescriptorSegment {
                        task_id: ask.task_id,
                        segment_id: index as u32 + 1,
                        file_descs: items,
                    }),
                ));
            }
            session.enqueue(Outbound::Packets(epoch, messages))
        }
        ClipboardRequestKind::FileContentsRequest(ask) => {
            let result = if session.file_allowed() {
                state
                    .tasks
                    .get(&(session.id, ask.task_id))
                    .or_else(|| state.drag_tasks.get(&(session.id, ask.task_id)))
                    .context("文件任务已失效")
                    .and_then(|files| files.read(&ask))
            } else {
                Err(anyhow!("文件剪贴板已关闭"))
            };
            let (err, data) = match result {
                Ok(data) => (1, data),
                Err(error) => {
                    tracing::debug!(%error, index = ask.list_index, "读取本地文件失败");
                    (2, Vec::new())
                }
            };
            session.emit(
                epoch,
                response(
                    id,
                    ClipboardResponseKind::FileContentsResponse(ClipboardFileContentsResponse {
                        task_id: ask.task_id,
                        data,
                        err,
                        pos_offset: ask.pos_offset,
                        list_index: ask.list_index,
                    }),
                ),
            )
        }
        ClipboardRequestKind::CancelRequest(ask) => {
            state.tasks.remove(&(session.id, ask.task_id));
            state.drag_tasks.remove(&(session.id, ask.task_id));
            session.emit(
                epoch,
                response(
                    id,
                    ClipboardResponseKind::CancelResponse(ClipboardFileCancelResponse {
                        task_id: ask.task_id,
                        err: 1,
                    }),
                ),
            )
        }
        _ => Ok(()),
    }
}

/// Whether every path is one this process mounted for a remote copy.
fn own_copy(paths: &[PathBuf]) -> bool {
    let Some(parent) = super::fuse::mount_root() else {
        return false;
    };
    own_paths(paths, &parent)
}
fn own_paths(paths: &[PathBuf], parent: &Path) -> bool {
    !paths.is_empty() && paths.iter().all(|path| path.starts_with(parent))
}

/// The file list a drag or drop handed this session, if it has one. A drop's
/// list answers one descriptor request, which hands the submission off.
fn drag_list(
    state: &mut State,
    session: &Arc<Inner>,
    task: u32,
) -> Option<Result<Arc<LocalFiles>>> {
    if let Some(files) = state.drag_tasks.get(&(session.id, task)) {
        return Some(Ok(files.clone()));
    }
    let (files, ticket) = state.drag_files.get(&session.id)?.clone();
    let Some(ticket) = ticket else {
        return Some(Ok(files));
    };
    state.drag_files.remove(&session.id);
    if !ticket.valid() {
        return Some(Err(anyhow!("拖放文件提议已失效")));
    }
    ticket.handed_off();
    Some(Ok(files))
}

/// A packed device-independent bitmap, as CF_DIB carries it.
fn dib_to_rgba(dib: &[u8]) -> Result<(u32, u32, Vec<u8>)> {
    ensure!(dib.len() >= 40, "位图数据过短");
    let header = u32::from_le_bytes(dib[0..4].try_into()?) as usize;
    ensure!(
        (40..=124).contains(&header) && dib.len() > header,
        "不支持的位图头"
    );
    let width = i32::from_le_bytes(dib[4..8].try_into()?);
    let height = i32::from_le_bytes(dib[8..12].try_into()?);
    let depth = u16::from_le_bytes(dib[14..16].try_into()?);
    let compression = u32::from_le_bytes(dib[16..20].try_into()?);
    ensure!(depth == 32 || depth == 24, "仅支持 24/32 位位图");
    // BI_RGB and BI_BITFIELDS with the usual masks share this pixel layout.
    ensure!(compression == 0 || compression == 3, "不支持压缩位图");
    let bottom_up = height > 0;
    let width = u32::try_from(width.abs()).context("位图宽度无效")?;
    let height = u32::try_from(height.abs()).context("位图高度无效")?;
    ensure!(
        width > 0 && height > 0 && width <= 32768 && height <= 32768,
        "位图尺寸无效"
    );
    let bytes = usize::from(depth / 8);
    let stride = ((width as usize * bytes) + 3) & !3;
    let masks = if compression == 3 { 12 } else { 0 };
    let start = header + masks;
    ensure!(
        dib.len() >= start + stride * height as usize,
        "位图数据不完整"
    );
    let mut rgba = vec![0u8; width as usize * height as usize * 4];
    for row in 0..height as usize {
        let source = if bottom_up {
            height as usize - 1 - row
        } else {
            row
        };
        let line = &dib[start + source * stride..][..stride];
        for column in 0..width as usize {
            let pixel = &line[column * bytes..][..bytes];
            let target = (row * width as usize + column) * 4;
            rgba[target] = pixel[2];
            rgba[target + 1] = pixel[1];
            rgba[target + 2] = pixel[0];
            rgba[target + 3] = if bytes == 4 { pixel[3] } else { 255 };
        }
    }
    // A 32-bit DIB with an all-zero alpha channel is opaque in practice.
    if bytes == 4 && rgba.iter().skip(3).step_by(4).all(|alpha| *alpha == 0) {
        for alpha in rgba.iter_mut().skip(3).step_by(4) {
            *alpha = 255;
        }
    }
    Ok((width, height, rgba))
}

pub(super) fn rgba_to_dib(width: u32, height: u32, rgba: &[u8]) -> Result<Vec<u8>> {
    ensure!(width > 0 && height > 0, "图片尺寸无效");
    let pixels = width as usize * height as usize;
    ensure!(rgba.len() >= pixels * 4, "图片数据不完整");
    ensure!(pixels * 4 <= MAX_CLIP, "图片过大");
    let mut dib = Vec::with_capacity(40 + pixels * 4);
    dib.extend_from_slice(&40u32.to_le_bytes());
    dib.extend_from_slice(&(width as i32).to_le_bytes());
    // Positive height keeps the bottom-up order Windows applications expect.
    dib.extend_from_slice(&(height as i32).to_le_bytes());
    dib.extend_from_slice(&1u16.to_le_bytes());
    dib.extend_from_slice(&32u16.to_le_bytes());
    dib.extend_from_slice(&0u32.to_le_bytes());
    dib.extend_from_slice(&((pixels * 4) as u32).to_le_bytes());
    dib.extend_from_slice(&0i32.to_le_bytes());
    dib.extend_from_slice(&0i32.to_le_bytes());
    dib.extend_from_slice(&0u32.to_le_bytes());
    dib.extend_from_slice(&0u32.to_le_bytes());
    for row in (0..height as usize).rev() {
        for column in 0..width as usize {
            let pixel = &rgba[(row * width as usize + column) * 4..][..4];
            dib.extend_from_slice(&[pixel[2], pixel[1], pixel[0], pixel[3]]);
        }
    }
    Ok(dib)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 本机这台 Wayland 桌面上，X11 与 Wayland 的剪贴板互不相通（实测 `xclip`
    /// 写的 `wl-paste` 看不见，反过来也一样），所以文件复制只写 X11 那条 offer
    /// 时，Wayland 原生的文件管理器根本读不到 `text/uri-list`——伸手粘贴什么也
    /// 没有。这条验证 arboard 的 Wayland 后端确实能把文件挂上去，也就是
    /// `publish_files` 那一步的前提。
    #[test]
    #[ignore = "会占用真实剪贴板，需要 Wayland 会话"]
    fn file_copies_reach_the_wayland_selection() {
        let path = std::env::temp_dir().join("openuuyc-clipboard-probe.txt");
        std::fs::write(&path, b"probe").unwrap();
        let mut clipboard = arboard::Clipboard::new().unwrap();
        clipboard
            .set()
            .file_list(std::slice::from_ref(&path))
            .unwrap();
        std::thread::sleep(Duration::from_millis(500));
        let read = clipboard.get().file_list().unwrap_or_default();
        println!("从剪贴板读回 {read:?}");
        assert_eq!(read, vec![path]);
    }

    fn read_tree(path: &Path, out: &mut Vec<(String, Vec<u8>)>, base: &Path) {
        if path.is_dir() {
            let mut entries: Vec<_> = std::fs::read_dir(path)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .collect();
            entries.sort();
            for entry in entries {
                read_tree(&entry, out, base);
            }
        } else {
            let name = path
                .strip_prefix(base)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            out.push((name, std::fs::read(path).unwrap()));
        }
    }

    /// A drag between two isolated sessions: one publishes a folder, the
    /// other mounts the offer and reads it back through the filesystem.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "mounts FUSE under XDG_RUNTIME_DIR"]
    async fn drag_offer_reads_through_mount() {
        let (source, _) = Clipboard::isolated_files(Arc::new(|| true));
        let (target, mut offers) = Clipboard::isolated_files(Arc::new(|| true));
        for clipboard in [&source, &target] {
            clipboard.set_enabled(true).unwrap();
            clipboard.policy(true, true);
        }
        let (to_target, to_source) = (target.clone(), source.clone());
        tokio::spawn(source.sender(move |_, data| {
            let target = to_target.clone();
            async move { target.receive(&data).map(|_| ()) }
        }));
        tokio::spawn(target.sender(move |_, data| {
            let source = to_source.clone();
            async move { source.receive(&data).map(|_| ()) }
        }));
        let root = std::env::temp_dir().join(format!("openuuyc-drag-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("folder/sub")).unwrap();
        std::fs::write(root.join("folder/a.txt"), b"alpha").unwrap();
        let big: Vec<u8> = (0..1_300_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(root.join("folder/sub/big.bin"), &big).unwrap();
        std::fs::write(root.join("folder/empty"), b"").unwrap();
        let summary = source
            .publish_files(vec![root.join("folder")])
            .await
            .unwrap();
        assert_eq!(summary.bytes, 5 + big.len() as u64);
        let offer = tokio::time::timeout(Duration::from_secs(5), offers.recv())
            .await
            .unwrap()
            .unwrap();
        let expected = {
            let mut out = Vec::new();
            read_tree(&root.join("folder"), &mut out, &root);
            out
        };
        tokio::task::spawn_blocking(move || {
            let summary = offer.prepare().unwrap();
            assert_eq!(summary.count, 5);
            offer.dragging(true);
            let files = offer.object().unwrap();
            assert_eq!(files.paths.len(), 1);
            offer.dragging(false);
            assert!(offer.status().completed.is_none());
            let base = files.paths[0].parent().unwrap().to_path_buf();
            let mut read = Vec::new();
            read_tree(&files.paths[0], &mut read, &base);
            assert_eq!(read, expected);
            let status = offer.status();
            assert_eq!(status.bytes_read, 5 + 1_300_000);
            assert_eq!(status.completed, Some(Ok(())));
        })
        .await
        .unwrap();
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The official send to the controller's receive folder: the host
    /// session offers files with AutoSave, the controller saves them under
    /// Downloads and reports back.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "writes into $HOME/Downloads"]
    async fn auto_save_lands_in_downloads() {
        let host = Clipboard::guarded(None);
        host.host_role();
        let controller = Clipboard::guarded(None);
        for clipboard in [&host, &controller] {
            clipboard.set_enabled(true).unwrap();
            clipboard.set_files(true);
            clipboard.policy(true, true);
        }
        let (to_controller, to_host) = (controller.clone(), host.clone());
        tokio::spawn(host.sender(move |_, data| {
            let controller = to_controller.clone();
            async move { controller.receive(&data).map(|_| ()) }
        }));
        tokio::spawn(controller.sender(move |_, data| {
            let host = to_host.clone();
            async move { host.receive(&data).map(|_| ()) }
        }));
        let root = std::env::temp_dir().join(format!("openuuyc-save-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let name = format!("openuuyc-autosave-{}.txt", std::process::id());
        std::fs::write(root.join(&name), b"saved").unwrap();
        let ticket = host.send_files(vec![root.join(&name)]).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match ticket.state() {
                super::super::drag::SubmissionState::Complete { total, saved } => {
                    assert_eq!((total, saved), (1, 1));
                    break;
                }
                super::super::drag::SubmissionState::Failed(error) => panic!("{error}"),
                _ => {}
            }
            assert!(Instant::now() < deadline, "auto-save did not complete");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let saved = crate::platform::file_locations::user_dir("XDG_DOWNLOAD_DIR", "Downloads")
            .unwrap()
            .join(&name);
        assert_eq!(std::fs::read(&saved).unwrap(), b"saved");
        let _ = std::fs::remove_file(saved);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The official drop: the controller offers files with a drop point and
    /// the host drags them, through the mount, into whatever lies there.
    /// OPENUUYC_DND_DROP is the normalized point (`x,y`) on the first screen.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs an X display with a drop target"]
    async fn official_drop_reaches_target() {
        let at = std::env::var("OPENUUYC_DND_DROP").unwrap();
        let (x, y) = at.split_once(',').unwrap();
        let screen = crate::platform::capture::screens().unwrap()[0].id;
        let controller = Clipboard::guarded(None);
        let host = Clipboard::guarded(None);
        host.host_role();
        for clipboard in [&host, &controller] {
            clipboard.set_enabled(true).unwrap();
            clipboard.set_files(true);
            clipboard.policy(true, true);
        }
        let (to_controller, to_host) = (controller.clone(), host.clone());
        tokio::spawn(host.sender(move |_, data| {
            let controller = to_controller.clone();
            async move { controller.receive(&data).map(|_| ()) }
        }));
        tokio::spawn(controller.sender(move |_, data| {
            let host = to_host.clone();
            async move { host.receive(&data).map(|_| ()) }
        }));
        let path = std::env::temp_dir().join("openuuyc-official-drop.txt");
        std::fs::write(&path, b"dropped by the controller").unwrap();
        let ticket = controller
            .drop_files(
                vec![path],
                crate::protocol::drag_drop::Point {
                    screen,
                    x: x.parse().unwrap(),
                    y: y.parse().unwrap(),
                },
            )
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !matches!(
            ticket.state(),
            super::super::drag::SubmissionState::HandedOff
        ) {
            if let super::super::drag::SubmissionState::Failed(error) = ticket.state() {
                panic!("{error}");
            }
            assert!(Instant::now() < deadline, "drop was not taken");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        // Give the host's drag and the target's read time to finish.
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert!(
            host.snapshot().error.is_none(),
            "{:?}",
            host.snapshot().error
        );
    }

    #[test]
    fn remote_copies_are_not_offered_back() {
        let parent = Path::new("/run/user/1000/openuuyc/clipboard");
        assert!(own_paths(&[parent.join("4/a.png")], parent));
        assert!(own_paths(
            &[parent.join("4/a.png"), parent.join("4/b")],
            parent
        ));
        assert!(!own_paths(
            &[parent.join("4/a.png"), PathBuf::from("/home/a/b")],
            parent
        ));
        assert!(!own_paths(&[], parent));
        assert!(!own_paths(
            &[PathBuf::from("/run/user/1000/openuuyc/clipboard-other/x")],
            parent
        ));
    }

    #[test]
    fn remote_copies_under_a_symlinked_runtime_dir_are_still_ours() {
        let base = std::env::temp_dir().join(format!("openuuyc-runtime-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let real = base.join("real");
        let link = base.join("link");
        std::fs::create_dir_all(&real).unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let root = super::super::fuse::root_under(&link);
        let file = root.join("7/a.txt");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, b"a").unwrap();
        // What the clipboard hands back after arboard wrote our mount's path.
        let published = std::fs::canonicalize(&file).unwrap();
        assert!(own_paths(std::slice::from_ref(&published), &root));
        // The unresolved root is what used to be compared, and it misses.
        let raw = link.join("openuuyc").join("clipboard");
        assert!(!own_paths(&[published], &raw));
        // A runtime directory that cannot be resolved falls back to the raw path.
        let missing = base.join("missing");
        assert_eq!(
            super::super::fuse::root_under(&missing),
            missing.join("openuuyc").join("clipboard")
        );
        std::fs::remove_dir_all(&base).unwrap();
    }
}
