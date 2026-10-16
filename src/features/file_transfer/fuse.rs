//! 把被控端的文件系统挂成一个本地只读卷。
//!
//! 传统传输窗口要先选文件、再挑一个落地目录，本质是“下载到某处”。FUSE 把远端目录树
//! 直接摊在文件管理器里：复制就是普通复制，不需要先知道文件要放哪、也不需要多一份临时副本。
//!
//! 协议只提供两件事：`ReadDir` 列一个目录，和一次基于清单的下载（`SendRequest` →
//! `FileAsk`/`FileBlock` → `Complete`）。没有“从某偏移读 N 字节”的随机读接口，所以这里
//! 用目录列举喂 inode 与元数据，读某个文件时把它收进本地副本，后续读取都从副本里取。
//!
//! 收的时候按需来：打开文件只是记账，真的读了才起拉取；读到哪儿就等哪儿，不预取整份；
//! 最后一个句柄关掉、文件还没收完就停掉（文件管理器为缩略图看一眼就走的最常见）。副本
//! 落在 `$XDG_CACHE_HOME`，超过上限淘汰最久没用的一份——远端才是真源。

use anyhow::{Context as _, Result, bail, ensure};
use std::{
    collections::HashMap,
    ffi::OsStr,
    os::unix::fs::FileExt,
    path::{Path, PathBuf},
    sync::{
        Condvar, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use super::{
    BLOCK, Payload, contract, lock, protocol::*, storage, success,
    tasks::{self, Rpc},
};
use crate::account::{api::DeviceInfo, client::AuthenticatedClient};
use crate::media::ConnectionMediaOptions;
use crate::session::controller::ControllerConnection;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// 官方协议用 `:/` 表示“列根/卷”，与主界面远端窗格的起点一致。
const REMOTE_ROOT: &str = ":/";
const ROOT_INO: u64 = 1;
/// 远端目录会变，属性缓存只留很短一瞬，让文件管理器看到较新的结果。
const TTL: Duration = Duration::from_secs(1);
/// 内核单次 read 可能很大；协议单块更小，由下载任务拆分，这里只做上限。
const MAX_READ: u32 = 1 << 20;
/// 本地留的副本总量上限。超了就把最久没读、又没人在用的那些整份删掉；远端是
/// 真源，删掉的读回来就有。文件管理器翻一遍目录、或者往里拷大文件，都不该把
/// 用户的盘塞满。
const CACHE_LIMIT: u64 = 2 << 30;
/// 上传失败重试几次。本地那份数据一直在，重试沿用同一个任务标识与被控端的临时
/// 文件，所以是从断的地方接着传，不是从头再来。
const UPLOAD_ATTEMPTS: u32 = 3;
/// 重试之间的等待，按次数递增。
const UPLOAD_RETRY_DELAY: Duration = Duration::from_secs(5);
/// 上传前的落定窗口：GLib 覆盖粘贴是「写临时文件 → rename 到目标」，rename 紧跟
/// 在关文件之后。等过窗口再按当时的名字上传，远端就不会冒出临时名，rename 也不必
/// 等上传跑完。
const UPLOAD_SETTLE: Duration = Duration::from_millis(800);
/// 续传时先要回这么多已经落盘的字节，跟本地比对，确认被控端真的按 `resume_point`
/// 从中间发的。对不上就退回从 0 收（只多花一次请求）。
const RESUME_PROBE: u64 = 512 * 1024;
const NAME_MAX: u32 = 255;

/// 远端给的 `modified_time` 是 **Unix 秒**，但它是**有符号**换算出来的：
/// 文件时间戳落在 1970 以前时（Windows 上「未知/1601 纪元」那一类很常见）会算出
/// 负数，写进协议里的 `u64` 字段就回绕成一个接近 `u64::MAX` 的巨值——实测见过
/// `18446744062065078195`，按有符号读回来正是 `-11644473421`（1970 前约 369 年，
/// 即 1601 纪元附近加了几分钟时钟偏差）。所以这里按有符号解释：
/// 0、负数（回绕出来的）、以及离谱的未来值都当「未知」。
fn mtime(seconds: u64) -> SystemTime {
    /// `SystemTime` 装得下的上限，约 9999-12-31。
    const MAX: i64 = 253_402_300_799;
    let signed = seconds as i64;
    if !(1..=MAX).contains(&signed) {
        return UNIX_EPOCH;
    }
    UNIX_EPOCH + Duration::from_secs(signed as u64)
}

/// 让具体的 errno 穿过 `anyhow` 走到 FUSE 的回复里：文件管理器靠它区分「已存在」
/// 「不是目录」这类情况，一律回 EIO 会让它当成链路故障。
#[derive(Debug)]
struct Errno(i32);
impl std::fmt::Display for Errno {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(std::io::Error::from_raw_os_error(self.0).to_string().as_str())
    }
}
impl std::error::Error for Errno {}
fn errno(error: &anyhow::Error) -> i32 {
    error.downcast_ref::<Errno>().map_or(libc::EIO, |code| code.0)
}

/// 官方 `FileEntry.entry_type` 的目录判定：0 普通目录、2 目录符号链接、3 磁盘/卷根。
fn is_directory(entry_type: i32) -> bool {
    matches!(entry_type, 0 | 2 | 3)
}

struct Node {
    /// 远端绝对路径，直接来自 `FileEntry.full_path`。
    path: String,
    /// 自己在父目录里的名字。上传时要把它当清单里的相对路径发给被控端。
    name: String,
    parent: u64,
    dir: bool,
    size: u64,
    mtime: SystemTime,
    /// 已列过的目录项 (名字, inode)；`None` 表示还没列。文件永远不列。
    children: Option<Vec<(String, u64)>>,
    /// 本地新建、还没传上去：远端列不出来，靠这个标记记住它，列目录时补进去。
    pending: bool,
    /// 已经删掉、但可能还有句柄开着：从目录里摘掉，读还读得动，上传要撤回。
    gone: bool,
}

#[derive(Default)]
struct State {
    nodes: HashMap<u64, Node>,
    by_path: HashMap<String, u64>,
    next: u64,
}

/// 一个已打开文件的下载缓存：远端文件在本地落一份，读的时候从这里取。
///
/// 远端协议只有「整份发过来」，没有「从任意偏移读 N 字节」。所以读命中 `[0, valid)`
/// 就从本地文件取；读到还没落盘的区间，才起一次拉取把后面的字节接着写进来。
/// 本地已知的字节区间：升序、不重叠、相接即合并。
///
/// 远端文件是稀疏收下来的——冷读可以直接从某个偏移开始收，前面那段本地就没有。那种
/// 洞在文件里是零，但零不是远端的内容，不能当数据给出去，所以「已知到哪」不能再当成
/// 一个前缀长度。
#[derive(Clone, Default)]
struct Ranges(Vec<(u64, u64)>);

impl Ranges {
    /// 整段都是本地真源：写过的文件就是这样（写出来的洞里是零，那本来就是对的内容）。
    fn whole(length: u64) -> Self {
        if length == 0 {
            Self::default()
        } else {
            Self(vec![(0, length)])
        }
    }

    fn end(&self) -> u64 {
        self.0.last().map_or(0, |(_, end)| *end)
    }

    fn insert(&mut self, start: u64, end: u64) {
        if start >= end {
            return;
        }
        // 按起点插入，保持升序——顺序错了会把中间真实的洞当邻居合并掉。
        let position = self.0.partition_point(|(existing, _)| *existing < start);
        self.0.insert(position, (start, end));
        let mut merged: Vec<(u64, u64)> = Vec::with_capacity(self.0.len());
        for (start, end) in self.0.drain(..) {
            match merged.last_mut() {
                Some(last) if start <= last.1 => {
                    last.0 = last.0.min(start);
                    last.1 = last.1.max(end);
                }
                _ => merged.push((start, end)),
            }
        }
        self.0 = merged;
    }

    fn covers(&self, start: u64, end: u64) -> bool {
        start >= end
            || self
                .0
                .iter()
                .any(|(from, to)| *from <= start && end <= *to)
    }

    /// 包含 `offset` 的那一段的结尾；`offset` 落在洞或末尾之外就是 `None`。
    fn covering_end(&self, offset: u64) -> Option<u64> {
        self.0
            .iter()
            .find(|(from, to)| *from <= offset && offset < *to)
            .map(|(_, to)| *to)
    }

    /// 紧挨在 `offset` 下面那一段的长度：续传校验要拿它当重叠区。
    fn probe_below(&self, offset: u64) -> u64 {
        self.0
            .iter()
            .find(|(_, end)| *end == offset)
            .map_or(0, |(start, end)| end - start)
    }

    /// `offset` 所在（或其后最近）的那个洞的起点。取洞的头而不是 `offset` 自己，
    /// 是为了让一次拉取填出尽量长的一整段连续已知区。
    fn gap_start(&self, offset: u64) -> u64 {
        if let Some(end) = self.covering_end(offset) {
            return end;
        }
        self.0
            .iter()
            .take_while(|(from, _)| *from <= offset)
            .last()
            .map_or(0, |(_, end)| *end)
    }
}

struct CacheEntry {
    path: PathBuf,
    state: Mutex<CacheState>,
    ready: Condvar,
}

struct CacheState {
    /// 本地已经当真收下来的字节区间。
    known: Ranges,
    /// 对端已经把这文件发到头了。之后超出已知区间的读就是 EOF，不再去要。
    complete: bool,
    /// 正在跑的拉取：`None` 表示没人在拉。里面是它自己的「停」令牌——中止不能用
    /// `Rpc` 的 stop（那会让收尾的 Complete 也发不出去，被控端就会一直挂着一个
    /// 等确认的发送任务），所以拉取循环自己 `select` 这个令牌。
    fetch: Option<CancellationToken>,
    /// 打开中的句柄数。降到 0 就把没拉完的拉取停掉：文件管理器为了缩略图打开又
    /// 关掉的文件，不该整份搬下来。
    readers: usize,
    /// 拉取失败的原因。留着，之后每次读都据此报错，不自己重试。
    error: Option<String>,
    /// 最近一次被读的时间（进程内的相对毫秒），淘汰缓存时挑最旧的。
    used: u64,
    /// 本地改过、要整体传回远端的长度（`None` 表示没有待传的改动）。本地这份文件
    /// 就是真源，长度即它的长度。
    dirty: Option<u64>,
    /// 正在传，别再起一个。
    uploading: bool,
    /// 上一次传失败的原因（留给界面/日志；本地数据还在）。
    upload_error: Option<String>,
}

struct Cache {
    dir: PathBuf,
    entries: Mutex<HashMap<u64, std::sync::Arc<CacheEntry>>>,
    /// 给 `used` 用的单调时钟。
    clock: AtomicU64,
}

struct RemoteFs {
    wire: std::sync::Arc<super::Transport>,
    runtime: tokio::runtime::Handle,
    stop: CancellationToken,
    /// 目录树与 inode。上传任务也要看（它得知道文件现在叫什么、在哪个目录下），
    /// 所以共享出去。
    state: std::sync::Arc<Mutex<State>>,
    cache: std::sync::Arc<Cache>,
    /// 待上传的 inode。写侧只管把 inode 丢进来，传的事交给后台任务——本地写是
    /// 磁盘速度，传是网络速度，让文件管理器等网络就等于整个卷卡住。
    uploads: mpsc::UnboundedSender<u64>,
    /// 本次会话是否已经确认过对端遵守 `resume_point`。确认过之后，冷读（本地一个
    /// 字节都没有的那种）才敢直接要目标偏移——不然只能从 0 把前面整段搬一遍。
    resume_trusted: std::sync::Arc<AtomicBool>,
    uid: u32,
    gid: u32,
}

impl RemoteFs {
    fn new(
        wire: std::sync::Arc<super::Transport>,
        runtime: tokio::runtime::Handle,
        stop: CancellationToken,
        state: std::sync::Arc<Mutex<State>>,
        cache: std::sync::Arc<Cache>,
        uploads: mpsc::UnboundedSender<u64>,
        resume_trusted: std::sync::Arc<AtomicBool>,
    ) -> Self {
        // 树与本地副本都建在外面：上传任务也要看同一份（它得知道文件现在叫什么名字）。
        {
            let mut state = lock(&state);
            state.nodes.insert(
                ROOT_INO,
                Node {
                    path: REMOTE_ROOT.into(),
                    name: String::new(),
                    parent: ROOT_INO,
                    dir: true,
                    size: 0,
                    mtime: UNIX_EPOCH,
                    children: None,
                    pending: false,
                    gone: false,
                },
            );
            state.by_path.insert(REMOTE_ROOT.to_owned(), ROOT_INO);
            state.next = ROOT_INO + 1;
        }
        Self {
            wire,
            runtime,
            stop,
            state,
            cache,
            uploads,
            resume_trusted,
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
        }
    }

    fn attr(&self, ino: u64) -> Option<fuser::FileAttr> {
        let state = lock(&self.state);
        let node = state.nodes.get(&ino)?;
        Some(fuser::FileAttr {
            ino,
            size: if node.dir { 0 } else { node.size },
            blocks: if node.dir {
                0
            } else {
                node.size.div_ceil(512)
            },
            atime: node.mtime,
            mtime: node.mtime,
            ctime: node.mtime,
            crtime: node.mtime,
            kind: if node.dir {
                fuser::FileType::Directory
            } else {
                fuser::FileType::RegularFile
            },
            perm: if node.dir { 0o755 } else { 0o644 },
            nlink: if node.dir { 2 } else { 1 },
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            blksize: 512,
            flags: 0,
        })
    }

    /// 确保某个目录的子项已列过，返回 (名字, inode) 列表。
    fn children(&self, ino: u64) -> Result<Vec<(String, u64)>> {
        {
            let state = lock(&self.state);
            let node = state.nodes.get(&ino).context("未知目录")?;
            ensure!(node.dir, "不是目录");
            if let Some(children) = &node.children {
                return Ok(children.clone());
            }
        }
        let path = {
            let state = lock(&self.state);
            state.nodes[&ino].path.clone()
        };
        let entries = self.runtime.block_on(tasks::directory(
            self.wire.clone(),
            path.clone(),
            self.stop.clone(),
        ))?;
        let mut state = lock(&self.state);
        let mut children = Vec::with_capacity(entries.len());
        for entry in entries {
            // `full_path` 是远端给的绝对路径；缺失时按本地习惯拼一个，保证 by_path 稳定。
            let child_path = if entry.full_path.is_empty() {
                format!("{}/{}", path.trim_end_matches(['/', '\\']), entry.name)
            } else {
                entry.full_path.clone()
            };
            let child = match state.by_path.get(&child_path) {
                Some(&existing) => existing,
                None => {
                    let child_ino = state.next;
                    state.next += 1;
                    state.nodes.insert(
                        child_ino,
                        Node {
                            path: child_path.clone(),
                            name: entry.name.clone(),
                            parent: ino,
                            dir: is_directory(entry.entry_type),
                            size: entry.size,
                            mtime: mtime(entry.modified_time),
                            children: None,
                            pending: false,
                            gone: false,
                        },
                    );
                    state.by_path.insert(child_path, child_ino);
                    child_ino
                }
            };
            // 同一个路径只应属于一个父目录；同名重复时以本次列举到的父目录为准。
            if let Some(node) = state.nodes.get_mut(&child) {
                node.parent = ino;
            }
            children.push((entry.name, child));
        }
        // 本地新建、还没传上去的文件远端列不出来（远端根本还没有），补进去。
        for (&child_ino, node) in state.nodes.iter() {
            if node.pending
                && !node.gone
                && node.parent == ino
                && !children.iter().any(|(name, _)| name == &node.name)
            {
                children.push((node.name.clone(), child_ino));
            }
        }
        if let Some(node) = state.nodes.get_mut(&ino) {
            node.children = Some(children.clone());
        }
        Ok(children)
    }

    /// `Ok(None)` 是「这个目录里没有这个名字」，`Err` 才是真的出错（网络、握手）。
    /// 两者必须分开：`lookup` 把它们分别映射成 ENOENT 与 EIO，否则链路抖一下就会
    /// 让递归遍历悄悄跳过真实存在的文件。
    fn lookup_child(&self, parent: u64, name: &str) -> Result<Option<u64>> {
        if name == "." {
            return Ok(Some(parent));
        }
        if name == ".." {
            let state = lock(&self.state);
            return Ok(Some(state.nodes.get(&parent).context("未知目录")?.parent));
        }
        let children = self.children(parent)?;
        Ok(children
            .iter()
            .find(|(candidate, _)| candidate == name)
            .map(|(_, ino)| *ino))
    }

    /// 取某个文件节点的 (远端路径, 声明大小)。
    fn node_file(&self, ino: u64) -> Result<(String, u64)> {
        let state = lock(&self.state);
        let node = state.nodes.get(&ino).context("未知文件")?;
        ensure!(!node.dir, "这是目录");
        Ok((node.path.clone(), node.size))
    }

    /// 等到本地副本覆盖 `[offset, offset + length)`（或对端说到头了）。
    ///
    /// 本地有改动（`dirty`）时**不联网**：本地那份才是真源，远端那份已经旧了。
    fn wait_until(
        &self,
        entry: &std::sync::Arc<CacheEntry>,
        remote: String,
        offset: u64,
        length: u64,
    ) -> Result<()> {
        let mut state = lock(&entry.state);
        state.used = self.cache.clock.fetch_add(1, Ordering::Relaxed);
        loop {
            if let Some(error) = &state.error {
                bail!("远端读取失败：{error}");
            }
            if state.dirty.is_some() || state.known.covers(offset, offset + length) {
                return Ok(());
            }
            // 对端说到头了：只有「要的范围整个在已知末尾之外」才算 EOF；落在冷读留下
            // 的洞里还是得去要（那里本地是零，不是远端的内容）。
            if state.complete && offset >= state.known.end() {
                return Ok(());
            }
            if state.fetch.is_none() {
                let from = Self::fetch_from(
                    &state.known,
                    offset,
                    self.resume_trusted.load(Ordering::Relaxed),
                );
                self.start_fetch(entry, remote.clone(), from, &mut state);
            }
            // 用带超时的等待：拉取可能因为连接结束被中断，那时不会再有人 notify，
            // 无限等待会让卸载卡在 join 上。
            if self.stop.is_cancelled() {
                bail!("远端连接已结束");
            }
            state = entry
                .ready
                .wait_timeout(state, Duration::from_millis(500))
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .0;
        }
    }

    /// 这次拉取该从哪儿（`resume_point`）要。
    ///
    /// 对端已经确认过遵守续传位置（`trusted`）时，直接要读者要的那个偏移——冷读大文件
    /// 的尾部才不用把前半段也搬一遍，这也是「能随机读」的关键。没确认过就只能从缺口
    /// 起点往后续：只有紧挨在起点**下面**的字节能拿来校验对端到底有没有照做，而缺口
    /// 起点正好是已知段的结尾，能拿它当重叠区。
    fn fetch_from(known: &Ranges, offset: u64, trusted: bool) -> u64 {
        if trusted {
            return offset;
        }
        // 从缺口起点往后续：那里紧挨着的就是已知段，拿它当重叠区能校验对端有没有照做。
        known.gap_start(offset)
    }

    /// 写已有文件之前把本地副本补全：远端协议只能整份上传，本地不完整就上传等于把
    /// 文件截断。大文件会在这里等整份下载完——这是「本地是写缓存」的代价。
    fn ensure_local_complete(&self, ino: u64) -> Result<()> {
        let (path, remote_size) = self.node_file(ino)?;
        let entry = self.cache_entry(ino)?;
        self.wait_until(&entry, path, 0, remote_size)?;
        let state = lock(&entry.state);
        if state.dirty.is_none() && !state.known.covers(0, remote_size) {
            let have = state.known.end();
            bail!("本地副本不完整（{have}/{remote_size}），不能改写远端文件");
        }
        Ok(())
    }

    /// 把本地副本打开用于写：不截断，副本文件没了就重建。
    fn local_write(entry: &CacheEntry) -> Result<std::fs::File> {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(&entry.path)
            .with_context(|| format!("打开本地副本 {} 失败", entry.path.display()))
    }

    /// 截到 0 并标脏（`O_TRUNC`）：新的内容全部由后面的写决定，不必先去拿旧内容。
    fn truncate_local(&self, ino: u64) -> Result<()> {
        let entry = self.cache_entry(ino)?;
        Self::local_write(&entry)?.set_len(0)?;
        let mut state = lock(&entry.state);
        // 截断后整份文件就是 [0, 0)（空的），写下来多少本地就有多少。
        state.known = Ranges::default();
        state.complete = false;
        state.dirty = Some(0);
        drop(state);
        if let Some(node) = lock(&self.state).nodes.get_mut(&ino) {
            node.size = 0;
        }
        Ok(())
    }

    /// 把一个 inode 排进上传队列。写侧只标脏，传的事交给后台任务。
    fn schedule_upload(&self, ino: u64) {
        let _ = self.uploads.send(ino);
    }

    /// 父目录已经列过的话，把新子项挂进去；没列过就不用管，下次列会带上。
    fn attach_child(state: &mut State, parent: u64, name: &str, child: u64) {
        if let Some(children) = state
            .nodes
            .get_mut(&parent)
            .and_then(|node| node.children.as_mut())
            && !children.iter().any(|(existing, _)| existing == name)
        {
            children.push((name.to_owned(), child));
        }
    }

    fn detach_child(state: &mut State, parent: u64, name: &str) {
        if let Some(children) = state
            .nodes
            .get_mut(&parent)
            .and_then(|node| node.children.as_mut())
        {
            children.retain(|(existing, _)| existing != name);
        }
    }

    /// 新建（或按标志打开）一个文件。返回属性与句柄（我们不用 fh，一律 0）。
    fn create_entry(&self, parent: u64, name: &str, flags: i32) -> Result<(fuser::FileAttr, u64)> {
        Self::valid_name(name)?;
        let parent_path = {
            let state = lock(&self.state);
            let node = state.nodes.get(&parent).context("未知目录")?;
            ensure!(node.dir, "父项不是目录");
            node.path.clone()
        };
        if let Some(ino) = self.lookup_child(parent, name)? {
            // 同名项已经在：O_EXCL 要报「已存在」，否则就是「打开已有文件」。
            if flags & libc::O_EXCL != 0 {
                return Err(Errno(libc::EEXIST).into());
            }
            let pending = lock(&self.state)
                .nodes
                .get(&ino)
                .is_some_and(|node| node.pending);
            if pending {
                // 本地新建的同名项：沿用，后面的写接着往这份里写。
            } else if flags & libc::O_TRUNC != 0 {
                self.truncate_local(ino)?;
            } else {
                self.ensure_local_complete(ino)?;
            }
            if let Ok(entry) = self.cache_entry(ino) {
                lock(&entry.state).readers += 1;
            }
            return Ok((self.attr(ino).context("未知文件")?, 0));
        }
        // 远端可能有我们还没列到的同名文件；上传的同名策略是「覆盖」，不会撞名。
        let path = super::service::join_remote(&parent_path, name);
        let ino = {
            let mut state = lock(&self.state);
            let ino = state.next;
            state.next = state.next.saturating_add(1);
            state.nodes.insert(
                ino,
                Node {
                    path: path.clone(),
                    name: name.to_owned(),
                    parent,
                    dir: false,
                    size: 0,
                    mtime: SystemTime::now(),
                    children: None,
                    pending: true,
                    gone: false,
                },
            );
            state.by_path.insert(path, ino);
            Self::attach_child(&mut state, parent, name, ino);
            ino
        };
        let entry = self.cache_entry(ino)?;
        lock(&entry.state).readers += 1;
        Ok((self.attr(ino).context("刚建好的节点不见了")?, 0))
    }

    /// 把内核写下来的数据落到本地副本，并标成待传。
    fn write_at(&self, ino: u64, offset: u64, data: &[u8]) -> Result<usize> {
        let entry = self.cache_entry(ino)?;
        Self::local_write(&entry)?.write_all_at(data, offset)?;
        let end = offset
            .checked_add(data.len() as u64)
            .context("文件偏移溢出")?;
        let length = {
            let mut state = lock(&entry.state);
            // 写过的文件整份都是本地的真源（写出来的洞里是零，那本来就是对的内容），
            // 长度按「已有的文件长度」与「写到的位置」取大。
            let length = state.known.end().max(end);
            state.known = Ranges::whole(length);
            state.dirty = Some(state.dirty.unwrap_or(0).max(end));
            length
        };
        if let Some(node) = lock(&self.state).nodes.get_mut(&ino) {
            node.size = node.size.max(length);
        }
        Ok(data.len())
    }

    /// `ftruncate`：把本地副本改到指定长度并标脏。
    fn truncate_to(&self, ino: u64, size: u64) -> Result<()> {
        let pending = lock(&self.state)
            .nodes
            .get(&ino)
            .is_some_and(|node| node.pending);
        if !pending {
            // 改短或改长都得以完整内容为前提：要么截掉的是远端已有的部分，要么
            // 补出来的是零，本地这份都得先是完整的。
            self.ensure_local_complete(ino)?;
        }
        let entry = self.cache_entry(ino)?;
        Self::local_write(&entry)?.set_len(size)?;
        {
            let mut state = lock(&entry.state);
            state.known = Ranges::whole(size);
            state.complete = false;
            state.dirty = Some(size);
        }
        if let Some(node) = lock(&self.state).nodes.get_mut(&ino) {
            node.size = size;
        }
        self.schedule_upload(ino);
        Ok(())
    }

    /// 删一个远端条目。本地新建、还没传上去的只删本地。
    fn remove_entry(&self, parent: u64, name: &OsStr, dir: bool, reply: fuser::ReplyEmpty) {
        let Some(name) = name.to_str() else {
            reply.error(libc::EINVAL);
            return;
        };
        match self.remove_child(parent, name, dir) {
            Ok(()) => reply.ok(),
            Err(error) => {
                tracing::debug!(%error, name, "删除远端文件失败");
                reply.error(errno(&error));
            }
        }
    }

    fn remove_child(&self, parent: u64, name: &str, dir: bool) -> Result<()> {
        let Some(ino) = self.lookup_child(parent, name)? else {
            return Err(Errno(libc::ENOENT).into());
        };
        let (path, pending, is_dir) = {
            let state = lock(&self.state);
            let node = state.nodes.get(&ino).context("未知文件")?;
            (node.path.clone(), node.pending, node.dir)
        };
        ensure!(
            is_dir == dir,
            if dir { "不是目录" } else { "是目录" }
        );
        if dir {
            // 官方被控端的 `RemoveDir` 不看 `recursive`，非空目录也整个删掉（实测：
            // `rmdir` 一个非空目录，远端那棵树就没了）。POSIX 的 `rmdir` 必须拒绝
            // 非空目录，所以这里自己先查一遍——不然用户一个 rmdir 就能删掉一棵树。
            let entries = self.children(ino)?;
            if !entries.is_empty() {
                return Err(Errno(libc::ENOTEMPTY).into());
            }
        }
        if !pending {
            let request = if dir {
                Req::RemoveDir(FileRemoveDir {
                    id: None,
                    path: path.clone(),
                    recursive: false,
                })
            } else {
                Req::RemoveFile(FileRemoveFile { path: path.clone() })
            };
            self.runtime.block_on(tasks::operation(
                self.wire.clone(),
                request,
                self.stop.clone(),
            ))?;
        }
        {
            let mut state = lock(&self.state);
            Self::detach_child(&mut state, parent, name);
            state.by_path.remove(&path);
            if let Some(node) = state.nodes.get_mut(&ino) {
                node.gone = true;
                node.pending = false;
            }
        }
        // 上传排过队、或者正传着：让它别传（传完了后台会撤回远端那份）。
        if let Ok(entry) = self.cache_entry(ino) {
            let mut state = lock(&entry.state);
            state.dirty = None;
        }
        Ok(())
    }

    /// `rename`：POSIX 的语义是**替换**目标，而被控端（Windows）的 rename 不覆盖
    /// 已存在的目标，所以目标在我们这边看得见时先删掉它。
    fn rename_entry(&self, parent: u64, name: &str, newparent: u64, newname: &str) -> Result<()> {
        Self::valid_name(newname)?;
        let Some(ino) = self.lookup_child(parent, name)? else {
            return Err(Errno(libc::ENOENT).into());
        };
        let (old_path, pending, is_dir) = {
            let state = lock(&self.state);
            let node = state.nodes.get(&ino).context("未知文件")?;
            (node.path.clone(), node.pending, node.dir)
        };
        if let Some(target) = self.lookup_child(newparent, newname)? {
            if target == ino {
                // 同一个文件换个写法，什么都不用做。
                return Ok(());
            }
            let target_dir = lock(&self.state)
                .nodes
                .get(&target)
                .is_some_and(|node| node.dir);
            self.remove_child(newparent, newname, target_dir)?;
        }
        let new_path = {
            let state = lock(&self.state);
            let dest = state.nodes.get(&newparent).context("未知目录")?;
            ensure!(dest.dir, "目标父项不是目录");
            super::service::join_remote(&dest.path, newname)
        };
        if !pending {
            self.runtime.block_on(tasks::operation(
                self.wire.clone(),
                Req::Rename(FileRename {
                    path: old_path.clone(),
                    new_name: new_path.clone(),
                }),
                self.stop.clone(),
            ))?;
        }
        {
            let mut state = lock(&self.state);
            Self::detach_child(&mut state, parent, name);
            state.by_path.remove(&old_path);
            if let Some(node) = state.nodes.get_mut(&ino) {
                node.path = new_path.clone();
                node.name = newname.to_owned();
                node.parent = newparent;
            }
            state.by_path.insert(new_path, ino);
            Self::attach_child(&mut state, newparent, newname, ino);
        }
        Ok(())
    }

    /// 新建目录。被控端只认「在父目录下新建」，名字由它定（可能给「新建文件夹」），
    /// 所以 `tasks::create_directory` 建完会再改名一次。
    fn make_directory(&self, parent: u64, name: &str) -> Result<fuser::FileAttr> {
        Self::valid_name(name)?;
        let parent_path = {
            let state = lock(&self.state);
            let node = state.nodes.get(&parent).context("未知目录")?;
            ensure!(node.dir, "父项不是目录");
            node.path.clone()
        };
        if self.lookup_child(parent, name)?.is_some() {
            return Err(Errno(libc::EEXIST).into());
        }
        let path = self.runtime.block_on(tasks::create_directory(
            self.wire.clone(),
            parent_path,
            name.to_owned(),
            self.stop.clone(),
        ))?;
        let ino = {
            let mut state = lock(&self.state);
            let ino = state.next;
            state.next = state.next.saturating_add(1);
            state.nodes.insert(
                ino,
                Node {
                    path: path.clone(),
                    name: name.to_owned(),
                    parent,
                    dir: true,
                    size: 0,
                    mtime: SystemTime::now(),
                    children: None,
                    pending: false,
                    gone: false,
                },
            );
            state.by_path.insert(path, ino);
            Self::attach_child(&mut state, parent, name, ino);
            ino
        };
        self.attr(ino).context("刚建好的目录不见了")
    }

    /// 名字合法性：内核给的名字必须是单段。
    fn valid_name(name: &str) -> Result<()> {
        ensure!(
            !name.is_empty() && name.len() <= NAME_MAX as usize,
            "文件名为空或过长"
        );
        ensure!(name != "." && name != "..", "非法文件名");
        ensure!(!name.contains(['/', '\\', '\0']), "文件名含路径分隔符");
        Ok(())
    }

    /// 读本地副本：需要的区间还没落盘就起一次拉取，等它就绪后从本地取。
    fn read_file(&self, ino: u64, offset: u64, size: u32) -> Result<Vec<u8>> {
        match self.read_file_inner(ino, offset, size)? {
            Some(data) => Ok(data),
            // 本地副本文件被外面删了：清掉内存里记的有效长度，重新拉一遍（拉取会重建）。
            None => {
                tracing::debug!(ino, "本地副本不见了，重新拉");
                self.cache_entry_repair(ino);
                self.read_file_inner(ino, offset, size)?
                    .context("本地副本反复丢失")
            }
        }
    }

    fn read_file_inner(&self, ino: u64, offset: u64, size: u32) -> Result<Option<Vec<u8>>> {
        let (path, remote_size) = self.node_file(ino)?;
        if offset >= remote_size {
            return Ok(Some(Vec::new()));
        }
        let wanted = u64::from(size.min(MAX_READ)).min(remote_size - offset);
        let entry = self.cache_entry(ino)?;
        self.wait_until(&entry, path, offset, wanted)?;
        if self.stop.is_cancelled() {
            bail!("远端连接已结束");
        }
        // 已知区间可能比要的短（读到文件末尾、或对端说发到头了）：给短的那个，内核
        // 会把它当 EOF。远端声明的大小与实际内容也可能不一致，用 saturating 兜底。
        let available = lock(&entry.state).known.covering_end(offset).unwrap_or(0);
        let take = usize::try_from(available.saturating_sub(offset).min(wanted)).unwrap_or(0);
        if take == 0 {
            return Ok(Some(Vec::new()));
        }
        // `~/.cache` 下的东西会被系统清理工具、tmpfiles 之流删掉，而我们记着的
        // 已知区间还在——那种情况交给调用方清掉记忆再拉一遍。
        let file = match std::fs::File::open(&entry.path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let mut buffer = vec![0u8; take];
        file.read_exact_at(&mut buffer, offset)?;
        Ok(Some(buffer))
    }

    /// 本地副本文件不见了：把内存里记的有效长度清掉，下次读会重新拉（拉取会重建文件）。
    fn cache_entry_repair(&self, ino: u64) {
        let entry = lock(&self.cache.entries).get(&ino).cloned();
        if let Some(entry) = entry {
            let mut state = lock(&entry.state);
            state.known = Ranges::default();
            state.complete = false;
            state.error = None;
        }
    }

    /// 取（必要时创建）某个 inode 的本地副本条目。**不**在这里起拉取：打开文件管理器
    /// 里一个目录就会打开一堆文件（生成缩略图、嗅探类型），只打开不读的不该产生流量。
    fn cache_entry(&self, ino: u64) -> Result<std::sync::Arc<CacheEntry>> {
        let mut entries = lock(&self.cache.entries);
        if let Some(entry) = entries.get(&ino) {
            return Ok(entry.clone());
        }
        let cache_path = self.cache.dir.join(ino.to_string());
        std::fs::File::create(&cache_path)
            .with_context(|| format!("创建缓存文件 {} 失败", cache_path.display()))?;
        let entry = std::sync::Arc::new(CacheEntry {
            path: cache_path,
            state: Mutex::new(CacheState {
                known: Ranges::default(),
                complete: false,
                fetch: None,
                readers: 0,
                error: None,
                used: 0,
                dirty: None,
                uploading: false,
                upload_error: None,
            }),
            ready: Condvar::new(),
        });
        entries.insert(ino, entry.clone());
        Ok(entry)
    }

    /// 起一次拉取。调用时要握着 `entry` 的锁。
    ///
    /// 本地已经有前缀时用 `resume_point` 从中间续：不然为了读文件尾巴得把前面整段
    /// 再搬一遍（实测读 18 MB 文件的最后 1 MB 要下 15 MB）。续传位置由
    /// [`stream_to_cache`] 拿回来的字节自己校验，不假设被控端会照做。
    fn start_fetch(
        &self,
        entry: &std::sync::Arc<CacheEntry>,
        remote: String,
        from: u64,
        state: &mut CacheState,
    ) {
        let abort = CancellationToken::new();
        state.fetch = Some(abort.clone());
        // 校验重叠区：紧挨在 `from` 下面那段已知字节，请对端**重发**一遍，跟本地比。
        // 对端说它从 `request` 开始发——比中了才说明这个偏移是真的。
        let probe = state.known.probe_below(from).min(RESUME_PROBE);
        let request = from - probe;
        let trusted = self.resume_trusted.clone();
        let wire = self.wire.clone();
        let stop = self.stop.clone();
        let cache = self.cache.clone();
        let cache_path = entry.path.clone();
        let task_entry = std::sync::Arc::clone(entry);
        self.runtime.spawn(async move {
            let result = stream_to_cache(
                wire,
                remote,
                stop,
                abort,
                request,
                probe,
                cache_path,
                task_entry.clone(),
                trusted,
            )
            .await;
            let mut state = lock(&task_entry.state);
            match result {
                Ok((end, finished)) => {
                    state.known.insert(request, end);
                    // 对端说发到头了：再往后的读就是 EOF，不必一遍遍去要。
                    if finished {
                        state.complete = true;
                    }
                }
                Err(error) => state.error = Some(format!("{error:#}")),
            }
            state.fetch = None;
            task_entry.ready.notify_all();
            drop(state);
            cache.evict_if_needed();
        });
    }
}

impl Cache {
    /// 本地副本超过上限时，删掉最久没读、又没人在用的那些。远端是真源，删掉的下次
    /// 读回来就有；正在读的、正在拉的都不动。
    fn evict_if_needed(&self) {
        loop {
            let mut total = 0u64;
            let mut victim: Option<(u64, u64, PathBuf)> = None;
            {
                let entries = lock(&self.entries);
                for (&ino, entry) in entries.iter() {
                    let state = lock(&entry.state);
                    total += state.known.end();
                    if state.readers > 0 || state.fetch.is_some() {
                        continue;
                    }
                    if victim
                        .as_ref()
                        .is_none_or(|(used, ..)| state.used < *used)
                    {
                        victim = Some((state.used, ino, entry.path.clone()));
                    }
                }
            }
            if total <= CACHE_LIMIT {
                return;
            }
            let Some((_, ino, path)) = victim else {
                // 全都在用；下一次再来。
                return;
            };
            lock(&self.entries).remove(&ino);
            let _ = std::fs::remove_file(&path);
            tracing::debug!(ino, "本地副本超出上限，淘汰最久没用的一份");
        }
    }
}

/// 把 `remote` 指向的远端文件收进本地副本。
///
/// 协议只给「整份」，而且块里不带偏移，流里字节的位置只能靠「从哪儿开始发」推。
/// `FileConfirm.resume_point` 就是说这件事的：本地已经有 `known` 个字节时，我们
/// 请它从 `known` 之前一点点发（见 [`RESUME_PROBE`]），拿回来先跟本地那段比一比
/// ——对得上，说明它真的按偏移发了，那就只落盘 `known` 之后的部分；对不上（被控端
/// 忽略了续传位置、从 0 头发），就把流当成从 0 开始，跳过本地已有的前缀。两条路都
/// 不会把字节写到错的位置上。每落一段就把有效长度推给读侧。
async fn stream_to_cache(
    wire: std::sync::Arc<super::Transport>,
    remote: String,
    stop: CancellationToken,
    abort: CancellationToken,
    // 请对端从哪个偏移开始发（`FileConfirm.resume_point`）。
    request: u64,
    // `[request, request + probe)` 这段本地已经有了，请它重发一遍来校验「对端到底
    // 有没有按这个偏移发」；0 表示没法校验（从 0 起，或已确认过对端时的直接跳偏移）。
    probe: u64,
    cache_path: PathBuf,
    entry: std::sync::Arc<CacheEntry>,
    // 会话内是否已经确认过对端遵守续传位置：这次校验成功就置上，失败就撤掉。
    trusted: std::sync::Arc<AtomicBool>,
) -> Result<(u64, bool)> {
    let mut rpc = Rpc::new(wire, stop, contract::Owner::Transfer)?;
    rpc.req(
        Req::SendRequest(FileTransferSendRequest {
            id: rpc.task(-1),
            path: remote,
            list_data: Vec::new(),
        }),
        false,
    )
    .await?;
    let response = loop {
        match rpc.next().await?.payload {
            Payload::Response(Res::SendResponse(value)) => break value,
            // 远端会在清单之前先报一次目录大小，这里不需要，确认即可。
            Payload::Request(Req::DirSizeRead(value)) => {
                rpc.check(&value.id, None)?;
            }
            _ => bail!("远端文件下载响应类型不匹配"),
        }
    };
    rpc.check(&response.id, None)?;
    success(response.err)?;
    let files = if response.list_data.is_empty() {
        response.files
    } else {
        storage::decompress::<FileList>(&response.list_data)?.files
    };
    // 带上 create：本地副本可能被外面清掉了，重建一个（读权限是给续传校验用的）。
    let file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(&cache_path)
        .with_context(|| format!("打开缓存文件 {} 失败", cache_path.display()))?;
    // 流从文件里的哪个偏移开始：第一块到手才能定（拿本地已有的那段比对）。
    let mut stream_start: Option<u64> = None;
    let mut received = 0u64;
    let mut size: Option<u64> = files.first().map(|info| info.size);
    loop {
        let incoming = tokio::select! {
            biased;
            _ = abort.cancelled() => {
                // 没人读了。得告诉被控端一声：它按窗口发、等到窗口满了就停在原地
                // 等确认，不取消的话那边会一直挂着一个发送任务。
                let _ = rpc
                    .req(
                        Req::Complete(FileTransferComplete {
                            id: rpc.task(-1),
                            error: 7,
                        }),
                        false,
                    )
                    .await;
                return Ok((stream_start.unwrap_or(request) + received, false));
            }
            incoming = rpc.next() => incoming?,
        };
        match incoming.payload {
            Payload::Request(Req::FileAsk(value)) => {
                let index = rpc.check(&value.id, None)?;
                ensure!(index == 1, "远端文件序号不符");
                let info = files.first().context("远端未返回文件清单")?;
                contract::metadata(info, &value)?;
                size = Some(info.size);
                rpc.res(Res::FileConfirm(FileTransferConfirm {
                    id: rpc.task(index),
                    skip: false,
                    err: 1,
                    resume_point: request,
                }))
                .await?;
            }
            Payload::Request(Req::FileBlock(value)) => {
                rpc.check(&value.id, Some(1))?;
                let size = size.context("远端未声明文件大小")?;
                let start = match stream_start {
                    Some(start) => start,
                    None => {
                        let start = if probe == 0 {
                            // 没有可校验的本地字节：要么本来就从 0 要（不需要校验），
                            // 要么是已经确认过对端遵守续传位置之后的一次直接跳偏移。
                            request
                        } else {
                            let compare = usize::try_from(probe.min(value.data.len() as u64))?;
                            let mut local = vec![0u8; compare];
                            file.read_exact_at(&mut local, request)?;
                            if local == value.data[..compare] {
                                // 对端确实按 resume_point 发的：这一会话里记下来，
                                // 之后冷读（没有任何本地字节可校验）才敢直接跳偏移，
                                // 而不是从 0 把前面整段搬一遍。
                                trusted.store(true, Ordering::Relaxed);
                                request
                            } else {
                                // 它没按续传位置发（或者发的是别的东西）：按从流首
                                // 对齐，同一会话里也不再信任它。
                                trusted.store(false, Ordering::Relaxed);
                                tracing::warn!(
                                    remote_size = size,
                                    resume_point = request,
                                    "被控端未按续传位置发送，改按流首对齐"
                                );
                                0
                            }
                        };
                        stream_start = Some(start);
                        start
                    }
                };
                let position = start + received;
                let end = contract::received_bytes(position, value.data.len(), size)?;
                file.write_all_at(&value.data, position)?;
                {
                    let mut state = lock(&entry.state);
                    state.known.insert(position, end);
                }
                entry.ready.notify_all();
                received += value.data.len() as u64;
                rpc.res(Res::BlockConfirm(FileTransferBlockConfirm {
                    id: rpc.task(1),
                    block_id: value.block_id,
                    err: 1,
                    block_len: value.data.len() as i32,
                }))
                .await?;
            }
            Payload::Request(Req::Complete(value)) => {
                let index = rpc.check(&value.id, None)?;
                if value.error != 1 {
                    success(value.error)?;
                }
                if index == -1 {
                    rpc.res(Res::Result(FileTransferResult {
                        id: rpc.task(-1),
                        file_error: 1,
                        err_msg: String::new(),
                    }))
                    .await?;
                    return Ok((stream_start.unwrap_or(request) + received, true));
                }
                ensure!(index == 1, "远端完成了未预期的文件");
                rpc.res(Res::Result(FileTransferResult {
                    id: rpc.task(1),
                    file_error: 1,
                    err_msg: String::new(),
                }))
                .await?;
            }
            Payload::Request(Req::DirSizeRead(value)) => {
                rpc.check(&value.id, None)?;
            }
            _ => bail!("下载过程收到意外消息"),
        }
    }
}

/// 一个正在上传的文件的进度。主界面顶部那条 FUSE 状态栏显示它。
#[derive(Clone)]
pub(crate) struct UploadProgress {
    pub(crate) path: String,
    pub(crate) sent: u64,
    pub(crate) total: u64,
    /// 第几次尝试（第一次是 1）。重试中它会涨上去。
    pub(crate) attempt: u32,
}

#[derive(Default)]
struct Uploads {
    active: Vec<UploadProgress>,
    /// 最后一次失败的原因。本地数据还在，下次写同一个文件会再传。
    error: Option<String>,
}

/// 上传进度放进程级静态：挂载跑在本进程的另一个线程/运行时里，界面直接读它就行。
fn uploads() -> &'static Mutex<Uploads> {
    static UPLOADS: OnceLock<Mutex<Uploads>> = OnceLock::new();
    UPLOADS.get_or_init(Mutex::default)
}

/// 给界面用：正在上传的快照与最后一次失败。
pub(crate) fn upload_progress() -> (Vec<UploadProgress>, Option<String>) {
    let state = lock(uploads());
    (state.active.clone(), state.error.clone())
}

fn progress_start(path: &str, total: u64, attempt: u32) {
    let mut state = lock(uploads());
    state.error = None;
    let entry = UploadProgress {
        path: path.to_owned(),
        sent: 0,
        total,
        attempt,
    };
    match state.active.iter_mut().find(|item| item.path == path) {
        Some(current) => *current = entry,
        None => state.active.push(entry),
    }
}

fn progress_advance(path: &str, sent: u64, total: u64) {
    let mut state = lock(uploads());
    if let Some(item) = state.active.iter_mut().find(|item| item.path == path) {
        item.sent = sent;
        item.total = total;
    }
}

fn progress_finish(path: &str, error: Option<String>) {
    let mut state = lock(uploads());
    state.active.retain(|item| item.path != path);
    if let Some(error) = error {
        state.error = Some(error);
    }
}

/// 一次上传要的东西：本地文件、目标目录、名字、长度、源修改时间。
struct Upload {
    entry: std::sync::Arc<CacheEntry>,
    local: PathBuf,
    destination: String,
    name: String,
    remote: String,
    modified: u64,
}

/// 同一个文件多次尝试之间要保持的东西。
///
/// 重试**必须**沿用同一个任务标识、并改用「继续原任务」的策略：被控端那边标识
/// 已经有记录时会直接拒绝新建（"任务标识已有记录"），而沿用标识还能让它从自己的
/// 临时文件接着收——重试是从断的地方续，不是从头再来。
struct Sending {
    key: String,
    started: bool,
}

/// 还有没有要传的东西。
fn needs_upload(state: &Mutex<State>, cache: &Cache, ino: u64) -> bool {
    if lock(state).nodes.get(&ino).is_none_or(|node| node.gone) {
        return false;
    }
    lock(&cache.entries)
        .get(&ino)
        .is_some_and(|entry| lock(&entry.state).dirty.is_some())
}

/// 准备一次上传：把目标目录、名字、长度快照下来。返回 `None` 表示没得传、已经有人
/// 在传、或者文件已经被删了。
fn upload_job(state: &Mutex<State>, cache: &Cache, ino: u64) -> Option<Upload> {
    let entry = lock(&cache.entries).get(&ino).cloned()?;
    let (destination, name, remote, modified) = {
        let entry_state = lock(&entry.state);
        entry_state.dirty?;
        if entry_state.uploading {
            return None;
        }
        let state = lock(state);
        let node = state.nodes.get(&ino)?;
        if node.gone {
            return None;
        }
        let parent = state.nodes.get(&node.parent)?;
        (
            parent.path.clone(),
            node.name.clone(),
            node.path.clone(),
            node.mtime
                .duration_since(UNIX_EPOCH)
                .map(|elapsed| elapsed.as_secs())
                .unwrap_or(0),
        )
    };
    lock(&entry.state).uploading = true;
    Some(Upload {
        local: entry.path.clone(),
        entry,
        destination,
        name,
        remote,
        modified,
    })
}

/// 上传队列的消费循环。一次只传一个文件；传完发现又变脏了就接着传。
async fn upload_worker(
    state: std::sync::Arc<Mutex<State>>,
    cache: std::sync::Arc<Cache>,
    wire: std::sync::Arc<super::Transport>,
    stop: CancellationToken,
    mut jobs: mpsc::UnboundedReceiver<u64>,
) {
    loop {
        let ino = tokio::select! {
            biased;
            _ = stop.cancelled() => return,
            job = jobs.recv() => match job { Some(ino) => ino, None => return },
        };
        // 落定窗口：让紧跟其后的 rename 先改完名字，顺便把连续的小写合成一次传输。
        tokio::select! {
            biased;
            _ = stop.cancelled() => return,
            _ = tokio::time::sleep(UPLOAD_SETTLE) => {}
        }
        while let Some(job) = upload_job(&state, &cache, ino) {
            let mut sending = Sending {
                key: uuid::Uuid::new_v4().to_string(),
                started: false,
            };
            let mut failure = None;
            for attempt in 1..=UPLOAD_ATTEMPTS {
                match push_file(&wire, &stop, &job, &mut sending, attempt).await {
                    Ok(()) => {
                        failure = None;
                        break;
                    }
                    Err(error) => {
                        progress_finish(&job.remote, Some(format!("{error:#}")));
                        tracing::warn!(%error, path=%job.remote, attempt, "上传到被控端失败");
                        failure = Some(error);
                        if stop.is_cancelled() {
                            break;
                        }
                        tokio::select! {
                            biased;
                            _ = stop.cancelled() => break,
                            _ = tokio::time::sleep(UPLOAD_RETRY_DELAY * attempt) => {}
                        }
                    }
                }
            }
            let failed = failure.is_some();
            if failed {
                // 本地数据留着（`dirty` 不清）：下次写同一个文件会再排一次上传。
                let message = failure.map(|error| format!("{error:#}")).unwrap_or_default();
                let mut entry = lock(&job.entry.state);
                entry.uploading = false;
                entry.upload_error = Some(message);
            } else {
                // 传上去了：本地这份与远端一致，不再算脏。
                tracing::info!(path=%job.remote, "远端文件上传完成");
                {
                    let mut entry = lock(&job.entry.state);
                    entry.dirty = None;
                    entry.uploading = false;
                    entry.upload_error = None;
                }
                let gone = {
                    let mut state = lock(&state);
                    match state.nodes.get_mut(&ino) {
                        Some(node) => {
                            node.pending = false;
                            node.gone
                        }
                        None => false,
                    }
                };
                if gone {
                    // 传的过程中文件被删了：远端那份也要撤掉，不然它会留在被控端上。
                    let removed = tasks::operation(
                        wire.clone(),
                        Req::RemoveFile(FileRemoveFile {
                            path: job.remote.clone(),
                        }),
                        stop.clone(),
                    )
                    .await;
                    tracing::info!(path=%job.remote, ok=removed.is_ok(), "文件在上传期间被删，已撤回远端那份");
                }
            }
            // 失败就别再排了（等下一条指令），成功则看等的时候有没有又写脏。
            if failed || !needs_upload(&state, &cache, ino) {
                break;
            }
        }
    }
}

/// 把一个本地文件整份传给被控端（`ReceiveRequest` → `FileAsk`/`FileBlock` → `Complete`）。
///
/// 跟传输窗口里的上传是同一套协议序列（`tasks::upload`），这里只做单文件、不管暂停
/// 续传：FUSE 这边一个句柄关掉就是一次传完，失败了下一次写再传。重复的是协议序列
/// 本身，不是可以简单共用的逻辑——那边围着任务记录与进度转，这边围着 inode 转。
async fn push_file(
    wire: &std::sync::Arc<super::Transport>,
    stop: &CancellationToken,
    job: &Upload,
    sending: &mut Sending,
    attempt: u32,
) -> Result<()> {
    use tokio::io::{AsyncReadExt as _, AsyncSeekExt as _};
    let mut file = tokio::fs::File::open(&job.local)
        .await
        .with_context(|| format!("打开本地副本 {} 失败", job.local.display()))?;
    let size = file.metadata().await?.len();
    progress_start(&job.remote, size, attempt);
    // 清单里的时间是 FILETIME ticks：被控端直接拿它当目标文件的时间戳，给 Unix 秒会
    // 落到 1601 年（见 `storage::wire_time` 的注释）。
    let modified = storage::wire_time(job.modified);
    let mut rpc = Rpc::new(wire.clone(), stop.clone(), contract::Owner::Transfer)?;
    let list = storage::compress(&FileList {
        files: vec![FileInfo {
            rel_path: job.name.clone(),
            size,
            modified_time: modified,
        }],
    })?;
    rpc.req(
        Req::ReceiveRequest(FileTransferReceiveRequest {
            id: rpc.task(-1),
            path: job.destination.clone(),
            files: Vec::new(),
            // 第一次是「同名时覆盖」（1）：同名怎么办文件管理器已经决定过了（替换是
            // 写临时名再 rename），被控端再自作主张改成「xx (1)」会把用户看到的
            // 文件名弄乱。重试改成「继续原任务」（4），因为同一个任务标识只认第一次
            // 新建——而且它那边还留着没传完的临时文件，续着传比从头再传快。
            file_op_strategy: if sending.started { 4 } else { 1 },
            folder_name: String::new(),
            list_data: list,
            task_unique_id: sending.key.clone(),
        }),
        false,
    )
    .await?;
    match rpc.next().await.context("等待上传初始化响应")?.payload {
        Payload::Response(Res::ReceiveResponse(value)) => {
            rpc.check(&value.id, None)?;
            // 这个错误码意思是「远端还没真的接上」，那下次重试还得按新建来。
            if !matches!(value.err, 4 | 8 | 12) {
                sending.started = true;
            }
            success(value.err)?;
        }
        _ => bail!("上传初始化响应类型不匹配"),
    }
    rpc.req(
        Req::FileAsk(FileTransferAsk {
            id: rpc.task(1),
            last_modified: modified,
            file_size: size,
        }),
        false,
    )
    .await?;
    let confirm = match rpc.next().await.context("等待文件准备响应")?.payload {
        Payload::Response(Res::FileConfirm(value)) => value,
        _ => bail!("文件准备响应类型不匹配"),
    };
    rpc.check(&confirm.id, Some(1))?;
    if confirm.skip {
        progress_finish(&job.remote, None);
        return Ok(());
    }
    success(confirm.err)?;
    ensure!(confirm.resume_point <= size, "远端续传位置超过源文件");
    let mut accepted = confirm.resume_point;
    file.seek(std::io::SeekFrom::Start(confirm.resume_point)).await?;
    let mut position = confirm.resume_point;
    let mut block = 0i32;
    let mut pending = contract::PendingBlocks::default();
    while position < size || !pending.is_empty() {
        if position < size && pending.len() < 16 {
            let length = BLOCK.min((size - position) as usize);
            let mut data = vec![0u8; length];
            let reading = file.read_exact(&mut data);
            tokio::select! {
                biased;
                _ = stop.cancelled() => bail!("连接已结束，上传中断"),
                result = reading => {
                    result.context("读取本地副本失败")?;
                }
            }
            block = block.checked_add(1).context("文件块编号溢出")?;
            rpc.req(
                Req::FileBlock(FileTransferBlock {
                    id: rpc.task(1),
                    block_id: block,
                    data,
                }),
                true,
            )
            .await?;
            pending.insert(block, length)?;
            position += length as u64;
            if pending.len() < 16 && position < size {
                continue;
            }
        }
        match rpc.next().await.context("等待文件块确认")?.payload {
            Payload::Response(Res::BlockConfirm(value)) => {
                rpc.check(&value.id, Some(1))?;
                accepted += pending.confirm(&value)? as u64;
                progress_advance(&job.remote, accepted, size);
            }
            _ => bail!("上传文件块响应类型不匹配"),
        }
    }
    rpc.req(
        Req::Complete(FileTransferComplete {
            id: rpc.task(1),
            error: 1,
        }),
        false,
    )
    .await?;
    match rpc.next().await.context("等待单文件完成确认")?.payload {
        Payload::Response(Res::Result(value)) => {
            rpc.check(&value.id, Some(1))?;
            success(value.file_error)?;
        }
        _ => bail!("单文件完成响应类型不匹配"),
    }
    // 整个任务的收尾：被控端收到就不再回（见 tasks::upload 的同一处）。
    rpc.req(
        Req::Complete(FileTransferComplete {
            id: rpc.task(-1),
            error: 1,
        }),
        false,
    )
    .await?;
    progress_finish(&job.remote, None);
    Ok(())
}

impl fuser::Filesystem for RemoteFs {
    fn lookup(
        &mut self,
        _req: &fuser::Request<'_>,
        parent: u64,
        name: &OsStr,
        reply: fuser::ReplyEntry,
    ) {
        let Some(name) = name.to_str() else {
            reply.error(libc::ENOENT);
            return;
        };
        match self.lookup_child(parent, name) {
            Ok(Some(ino)) => match self.attr(ino) {
                Some(attr) => reply.entry(&TTL, &attr, 0),
                None => reply.error(libc::ENOENT),
            },
            Ok(None) => reply.error(libc::ENOENT),
            Err(error) => {
                tracing::debug!(%error, "lookup 失败，按 I/O 错误上报");
                reply.error(libc::EIO);
            }
        }
    }

    fn getattr(
        &mut self,
        _req: &fuser::Request<'_>,
        ino: u64,
        _fh: Option<u64>,
        reply: fuser::ReplyAttr,
    ) {
        match self.attr(ino) {
            Some(attr) => reply.attr(&TTL, &attr),
            None => reply.error(libc::ENOENT),
        }
    }

    fn readdir(
        &mut self,
        _req: &fuser::Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: fuser::ReplyDirectory,
    ) {
        let (parent, is_dir) = {
            let state = lock(&self.state);
            match state.nodes.get(&ino) {
                Some(node) => (node.parent, node.dir),
                None => {
                    reply.error(libc::ENOENT);
                    return;
                }
            }
        };
        if !is_dir {
            reply.error(libc::ENOTDIR);
            return;
        }
        let children = match self.children(ino) {
            Ok(children) => children,
            Err(error) => {
                tracing::debug!(%error, ino, "列出远端目录失败");
                reply.error(libc::EIO);
                return;
            }
        };
        let mut entries = vec![
            (ino, fuser::FileType::Directory, ".".to_owned()),
            (parent, fuser::FileType::Directory, "..".to_owned()),
        ];
        for (name, child) in children {
            let kind = {
                let state = lock(&self.state);
                if state.nodes[&child].dir {
                    fuser::FileType::Directory
                } else {
                    fuser::FileType::RegularFile
                }
            };
            entries.push((child, kind, name));
        }
        for (position, (entry_ino, kind, name)) in
            entries.into_iter().enumerate().skip(offset.max(0) as usize)
        {
            // 缓冲区满不是错误：内核会带着新 offset 再来。
            if reply.add(entry_ino, position as i64 + 1, kind, name) {
                break;
            }
        }
        reply.ok();
    }

    fn open(&mut self, _req: &fuser::Request<'_>, ino: u64, flags: i32, reply: fuser::ReplyOpen) {
        let dir = {
            let state = lock(&self.state);
            match state.nodes.get(&ino) {
                Some(node) => node.dir,
                None => {
                    reply.error(libc::ENOENT);
                    return;
                }
            }
        };
        if dir {
            reply.error(libc::EISDIR);
            return;
        }
        // 写打开：先让本地这份能当完整的内容用（远端协议只能整份上传）。
        if flags & (libc::O_WRONLY | libc::O_RDWR) != 0 {
            let pending = lock(&self.state)
                .nodes
                .get(&ino)
                .is_some_and(|node| node.pending);
            let prepared = if pending {
                // 本地新建的，本来就只有我们写的那些字节。
                Ok(())
            } else if flags & libc::O_TRUNC != 0 {
                self.truncate_local(ino)
            } else {
                // 改写已有文件：得先把远端那份整个拿下来，不然传上去就是残缺的。
                self.ensure_local_complete(ino)
            };
            if let Err(error) = prepared {
                tracing::debug!(%error, ino, "打开远端文件用于写失败");
                reply.error(errno(&error));
                return;
            }
        }
        // 只是记账，不起拉取：文件管理器打开文件多半是为了看一眼（缩略图、类型
        // 嗅探），只打开不读的不该产生任何流量；真的读了再拉。
        if let Ok(entry) = self.cache_entry(ino) {
            lock(&entry.state).readers += 1;
        }
        reply.opened(0, 0);
    }

    /// 最后一个句柄关掉、文件又没收完，就把拉取停掉。缩略图那类「打开、读一点、
    /// 关掉」的用法，省下来的就是这里。
    fn release(
        &mut self,
        _req: &fuser::Request<'_>,
        ino: u64,
        _fh: u64,
        _flags: i32,
        _lock_owner: Option<u64>,
        _flush: bool,
        reply: fuser::ReplyEmpty,
    ) {
        let entry = lock(&self.cache.entries).get(&ino).cloned();
        if let Some(entry) = entry {
            let dirty = {
                let mut state = lock(&entry.state);
                state.readers = state.readers.saturating_sub(1);
                if state.readers == 0
                    && let Some(abort) = &state.fetch
                {
                    abort.cancel();
                }
                state.dirty.is_some()
            };
            // 本地已经落盘了，传回远端是后台的事：在这里等网络就等于整个卷卡住。
            if dirty {
                self.schedule_upload(ino);
            }
        }
        reply.ok();
    }

    fn read(
        &mut self,
        _req: &fuser::Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: fuser::ReplyData,
    ) {
        match self.read_file(ino, offset.max(0) as u64, size) {
            Ok(data) => reply.data(&data),
            Err(error) => {
                tracing::debug!(%error, ino, offset, "读取远端文件失败");
                reply.error(libc::EIO);
            }
        }
    }

    /// 关文件前的收尾。数据已经在本地下盘，这里只把改动排进上传队列，不等它传完。
    fn flush(
        &mut self,
        _req: &fuser::Request<'_>,
        ino: u64,
        _fh: u64,
        _lock_owner: u64,
        reply: fuser::ReplyEmpty,
    ) {
        let dirty = lock(&self.cache.entries)
            .get(&ino)
            .is_some_and(|entry| lock(&entry.state).dirty.is_some());
        if dirty {
            self.schedule_upload(ino);
        }
        reply.ok();
    }

    fn create(
        &mut self,
        _req: &fuser::Request<'_>,
        parent: u64,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        flags: i32,
        reply: fuser::ReplyCreate,
    ) {
        let Some(name) = name.to_str() else {
            reply.error(libc::EINVAL);
            return;
        };
        match self.create_entry(parent, name, flags) {
            Ok((attr, fh)) => reply.created(&TTL, &attr, 0, fh, 0),
            Err(error) => {
                tracing::debug!(%error, name, "新建远端文件失败");
                reply.error(errno(&error));
            }
        }
    }

    fn write(
        &mut self,
        _req: &fuser::Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        data: &[u8],
        _write_flags: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: fuser::ReplyWrite,
    ) {
        match self.write_at(ino, offset.max(0) as u64, data) {
            Ok(written) => reply.written(written as u32),
            Err(error) => {
                tracing::debug!(%error, ino, "写远端文件失败");
                reply.error(errno(&error));
            }
        }
    }

    fn mkdir(
        &mut self,
        _req: &fuser::Request<'_>,
        parent: u64,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        reply: fuser::ReplyEntry,
    ) {
        let Some(name) = name.to_str() else {
            reply.error(libc::EINVAL);
            return;
        };
        match self.make_directory(parent, name) {
            Ok(attr) => reply.entry(&TTL, &attr, 0),
            Err(error) => {
                tracing::debug!(%error, name, "新建远端目录失败");
                reply.error(errno(&error));
            }
        }
    }

    fn unlink(&mut self, _req: &fuser::Request<'_>, parent: u64, name: &OsStr, reply: fuser::ReplyEmpty) {
        self.remove_entry(parent, name, false, reply);
    }

    fn rmdir(&mut self, _req: &fuser::Request<'_>, parent: u64, name: &OsStr, reply: fuser::ReplyEmpty) {
        self.remove_entry(parent, name, true, reply);
    }

    fn rename(
        &mut self,
        _req: &fuser::Request<'_>,
        parent: u64,
        name: &OsStr,
        newparent: u64,
        newname: &OsStr,
        _flags: u32,
        reply: fuser::ReplyEmpty,
    ) {
        let (Some(name), Some(newname)) = (name.to_str(), newname.to_str()) else {
            reply.error(libc::EINVAL);
            return;
        };
        match self.rename_entry(parent, name, newparent, newname) {
            Ok(()) => reply.ok(),
            Err(error) => {
                tracing::debug!(%error, name, newname, "重命名远端文件失败");
                reply.error(errno(&error));
            }
        }
    }

    fn setattr(
        &mut self,
        _req: &fuser::Request<'_>,
        ino: u64,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<fuser::TimeOrNow>,
        mtime: Option<fuser::TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<u64>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<u32>,
        reply: fuser::ReplyAttr,
    ) {
        // 协议里没有 chmod/utimens：权限位接受但不落地，时间留在本地——上传时它
        // 就是文件在被控端上的修改时间，所以这里记下来是有用的。
        if let Some(mtime) = mtime {
            let stamp = match mtime {
                fuser::TimeOrNow::SpecificTime(time) => time,
                fuser::TimeOrNow::Now => SystemTime::now(),
            };
            if let Some(node) = lock(&self.state).nodes.get_mut(&ino) {
                node.mtime = stamp;
            }
        }
        if let Some(size) = size
            && let Err(error) = self.truncate_to(ino, size)
        {
            tracing::debug!(%error, ino, size, "截断远端文件失败");
            reply.error(errno(&error));
            return;
        }
        match self.attr(ino) {
            Some(attr) => reply.attr(&TTL, &attr),
            None => reply.error(libc::ENOENT),
        }
    }

    fn statfs(&mut self, _req: &fuser::Request<'_>, _ino: u64, reply: fuser::ReplyStatfs) {
        // 文件管理器拷贝前会问剩余空间，报 0 它就直接拒绝拷贝。远端真正的容量协议
        // 里没有，给一个够大的壳，让 df/gvfs 与拷贝前检查都能过。
        let block = 4096u64;
        let total = 256 * 1024 * 1024 * 1024 / block;
        reply.statfs(
            total,
            total / 2,
            total / 2,
            1 << 20,
            1 << 20,
            block as u32,
            NAME_MAX,
            block as u32,
        );
    }
}

fn mount_options() -> Vec<fuser::MountOption> {
    vec![
        fuser::MountOption::FSName("openuuyc-remote".to_owned()),
        fuser::MountOption::Subtype(crate::platform::linux::REMOTE_FS_SUBTYPE.to_owned()),
        fuser::MountOption::NoSuid,
        fuser::MountOption::NoDev,
        fuser::MountOption::NoAtime,
        // 让 gvfs 在文件管理器侧栏给这个卷留一个条目。gvfs 读挂载选项，所以要以
        // 挂载参数的形式送进去，不能只在进程里记着。
        fuser::MountOption::CUSTOM("x-gvfs-show".to_owned()),
    ]
}

/// 挂载点放运行目录（它本来就该是个短命路径），本地副本放缓存目录。
///
/// 副本**不能**跟挂载点一起放 `$XDG_RUNTIME_DIR`：那是 tmpfs，等于把远端文件的每个
/// 字节都塞进内存。缓存目录也不能退到 `/tmp`（同样常是内存盘），所以拿 `$HOME` 兜底。
fn mount_paths() -> (PathBuf, PathBuf) {
    let mountpoint = match xdg_runtime_dir() {
        Some(base) => base.join("openuuyc/remote-fs"),
        None => PathBuf::from("/tmp/openuuyc-remote-fs"),
    };
    (mountpoint, cache_root().join("remote-fs"))
}

/// 本地副本的根目录：`$XDG_CACHE_HOME/openuuyc`，没有就用 `~/.cache/openuuyc`。
fn cache_root() -> PathBuf {
    std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
        .unwrap_or_else(|| PathBuf::from("/var/tmp"))
        .join("openuuyc")
}

/// 本进程是不是跑在图形会话里。`xdg-open` 只在这种情况下才叫得动文件管理器。
fn graphical_session() -> bool {
    let session = std::env::var("XDG_SESSION_TYPE").unwrap_or_default();
    matches!(session.as_str(), "wayland" | "x11")
        || std::env::var_os("WAYLAND_DISPLAY").is_some()
        || std::env::var_os("DISPLAY").is_some()
}

/// `XDG_RUNTIME_DIR` 解析到规范路径：进程里可能带着符号链接形式，缓存与挂载点落在同一棵树上更省心。
fn xdg_runtime_dir() -> Option<PathBuf> {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .map(|path| std::fs::canonicalize(&path).unwrap_or(path))
}

/// 进程上次异常退出留下的挂载点：先尽力卸载，再清掉整个目录重建，避免挂到一个死挂载点上。
fn retire_stale(mountpoint: &Path) {
    unmount_now(mountpoint);
    let _ = std::fs::remove_dir_all(mountpoint);
}

/// 摘掉挂载点。**先用普通卸载**：它成功时内核会中止这个 FUSE 连接，会话线程读到
/// ENODEV 就退出；懒卸载只是把挂载点从命名空间里摘掉、连接还活着，会话线程会一直
/// 阻塞在 read 上——实测 `session.join()` 就此卡死，界面上表现为「结束」按钮点了没
/// 反应。普通卸载真被占用（有程序开着里面的文件）时才退回懒卸载。
fn unmount_now(mountpoint: &Path) {
    if unmount_with(&["-u", "-q"], mountpoint) {
        return;
    }
    let _ = unmount_with(&["-u", "-q", "-z"], mountpoint);
}

/// 跑一次 fusermount3，返回它是不是成功（成功就是卸载干净了）。
fn unmount_with(args: &[&str], mountpoint: &Path) -> bool {
    let mut command = std::process::Command::new("fusermount3");
    command.args(args).arg(mountpoint);
    command
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

struct Active {
    generation: u64,
    mountpoint: PathBuf,
    shutdown: std::sync::mpsc::Sender<()>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Active {
    fn shutdown(mut self) -> Result<()> {
        let _ = self.shutdown.send(());
        if let Some(thread) = self.thread.take() {
            thread
                .join()
                .map_err(|_| anyhow::anyhow!("远端文件系统线程异常退出"))?;
        }
        Ok(())
    }
}

fn active() -> &'static Mutex<Option<Active>> {
    static ACTIVE: OnceLock<Mutex<Option<Active>>> = OnceLock::new();
    ACTIVE.get_or_init(Mutex::default)
}

fn next_generation() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// 把 `device` 的远端文件系统挂到临时目录。已经有挂载时先把旧的卸掉。
///
/// 挂载由全局状态与 [`unmount`] 掌握：这里**不返回**任何需要调用方一直拿着的东西
/// ——调用方多半在后台线程里调它，一个「drop 就卸载」的守卫会在返回的瞬间把刚建好
/// 的挂载拆掉（踩过）。
pub(crate) fn mount(
    client: std::sync::Arc<AuthenticatedClient>,
    device: DeviceInfo,
    options: ConnectionMediaOptions,
) -> Result<()> {
    unmount()?;
    let (mountpoint, cache_dir) = mount_paths();
    if let Some(parent) = mountpoint.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("创建 {} 失败", parent.display()))?;
    }
    retire_stale(&mountpoint);
    std::fs::create_dir_all(&mountpoint)
        .with_context(|| format!("创建挂载点 {} 失败", mountpoint.display()))?;
    let _ = std::fs::remove_dir_all(&cache_dir);
    std::fs::create_dir_all(&cache_dir)
        .with_context(|| format!("创建缓存目录 {} 失败", cache_dir.display()))?;

    let generation = next_generation();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<()>>();
    let (shutdown_tx, shutdown_rx) = std::sync::mpsc::channel::<()>();
    let worker = {
        let mountpoint = mountpoint.clone();
        std::thread::Builder::new()
            .name("openuuyc-remote-fs".into())
            .spawn(move || {
                session_thread(
                    client, device, options, mountpoint, cache_dir, ready_tx, shutdown_rx,
                )
            })
            .context("启动远端文件系统线程失败")?
    };

    match ready_rx.recv() {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            let _ = shutdown_tx.send(());
            let _ = worker.join();
            return Err(error.context("挂载远端文件系统失败"));
        }
        Err(_) => {
            let _ = worker.join();
            bail!("远端文件系统线程在挂载完成前退出");
        }
    }

    *lock(active()) = Some(Active {
        generation,
        mountpoint: mountpoint.clone(),
        shutdown: shutdown_tx,
        thread: Some(worker),
    });
    // 挂上就把文件管理器叫出来。失败不影响挂载本身（比如没有 xdg-open、没有桌面会话）。
    if graphical_session() {
        match std::process::Command::new("xdg-open").arg(&mountpoint).spawn() {
            Ok(_) => {}
            Err(error) => tracing::debug!(%error, "xdg-open 打不开挂载点，不影响挂载"),
        }
    } else {
        // 没有图形会话时**别**叫它：`xdg-open` 会经 D-Bus 唤起一个继承本进程环境的
        // 文件管理器，那个环境里 XDG_SESSION_TYPE=tty、没有 WAYLAND_DISPLAY，它初始化
        // 不了显示服务，弹出来是个空窗口（从脚本/测试里挂载时实测）。
        tracing::debug!("没有图形会话，不自动打开文件管理器");
    }
    Ok(())
}

/// 现在挂着的话，挂载点在哪。主界面靠它决定要不要显示「结束」按钮。
pub(crate) fn mounted_path() -> Option<PathBuf> {
    lock(active())
        .as_ref()
        .map(|active| active.mountpoint.clone())
}

/// 卸载并结束这次会话。没挂着就是 no-op。
pub(crate) fn unmount() -> Result<()> {
    // 先把状态摘出来再收尾：收尾会 join 后台线程，不能握着全局锁等它。
    let active = lock(active()).take();
    match active {
        Some(active) => active.shutdown(),
        None => Ok(()),
    }
}

/// 后台会话线程：自己起一个运行时跑连接与 FUSE 会话，不依赖调用方是否身处异步上下文。
fn session_thread(
    client: std::sync::Arc<AuthenticatedClient>,
    device: DeviceInfo,
    options: ConnectionMediaOptions,
    mountpoint: PathBuf,
    cache_dir: PathBuf,
    ready: std::sync::mpsc::Sender<Result<()>>,
    shutdown: std::sync::mpsc::Receiver<()>,
) {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("openuuyc-remote-fs-rt")
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            let _ = ready.send(Err(error.into()));
            return;
        }
    };
    let handle = runtime.handle().clone();
    let stop = CancellationToken::new();
    let setup = handle.block_on(async {
        let connection = connect(&client, &device, options, &stop).await?;
        let wire = connection.stream_control_handle().file_transfer().clone();
        let (end_tx, end_rx) = tokio::sync::oneshot::channel();
        let alive = tokio::spawn(connection.keep_alive(end_rx));
        // 上传队列的消费者：写侧只把 inode 丢进来，真正的传输在后台按本地的节奏跑。
        let cache = std::sync::Arc::new(Cache {
            dir: cache_dir.clone(),
            entries: Mutex::new(HashMap::new()),
            clock: AtomicU64::new(0),
        });
        let state = std::sync::Arc::new(Mutex::new(State::default()));
        let (uploads, jobs) = mpsc::unbounded_channel();
        let resume_trusted = std::sync::Arc::new(AtomicBool::new(false));
        tokio::spawn(upload_worker(
            state.clone(),
            cache.clone(),
            wire.clone(),
            stop.clone(),
            jobs,
        ));
        let filesystem = RemoteFs::new(
            wire,
            handle.clone(),
            stop.clone(),
            state,
            cache,
            uploads,
            resume_trusted,
        );
        let session = fuser::spawn_mount2(filesystem, &mountpoint, &mount_options())
            .with_context(|| format!("挂载 {} 失败", mountpoint.display()))?;
        Ok::<_, anyhow::Error>((session, end_tx, alive))
    });
    let (session, end_tx, alive) = match setup {
        Ok(value) => value,
        Err(error) => {
            let _ = ready.send(Err(error));
            drop(runtime);
            return;
        }
    };
    if ready.send(Ok(())).is_err() {
        return;
    }
    // 等主线程喊停；在此之前保持运行时活着，下载任务与会话都靠它跑。
    // 等停止。**同时盯着会话自己有没有结束**：文件管理器（或用户）从外面
    // `fusermount3 -u` 卸载时没人会给我们发信号，FUSE 会话线程读到 ENODEV 就退出了，
    // 而我们还在这里等——那样挂载已经没了、连接却一直活着，界面也以为还挂着。
    let mut externally_unmounted = false;
    loop {
        match shutdown.recv_timeout(Duration::from_millis(250)) {
            Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
        }
        if session.guard.is_finished() {
            externally_unmounted = true;
            break;
        }
    }
    stop.cancel();
    drop(end_tx);
    let _ = handle.block_on(alive);
    // 先摘挂载点：正常时内核会中止这个 FUSE 连接，会话线程读到 ENODEV 就退出。
    unmount_now(&mountpoint);
    // 但**不能在这里无限等它**：实测见过会话线程不退（挂载点已经从 /proc/self/mounts
    // 摘掉、也没有任何进程占着它，线程却一直阻塞在 read /dev/fuse 上）——界面上就是
    // 「结束」按钮点了没反应。所以有界等待，等不到就把线程与它的运行时留在后台：
    // 挂载点已经摘掉、不再有请求进来，它只是阻塞在那里，泄漏量有界。
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while !session.guard.is_finished() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    if session.guard.is_finished() {
        // 用底层的 join：会话线程结束时若带着错误（或 panic）不该把收尾也一起炸掉。
        let fuser::BackgroundSession { guard, .. } = session;
        let _ = guard.join();
        drop(runtime);
        let _ = std::fs::remove_dir_all(&cache_dir);
        let _ = std::fs::remove_dir(&mountpoint);
        if externally_unmounted {
            tracing::info!("远端文件系统已被从外部卸载，收尾");
            // 界面靠全局状态判断「还挂着没有」。我们**就是**那个线程，不能 join 自己，
            // 所以只把槽位摘掉（丢掉手柄与发送端）。
            drop(lock(active()).take());
        }
    } else {
        tracing::warn!("FUSE 会话线程没有在卸载后退出，把它留在后台（挂载点已经摘掉）");
        drop(session);
        // 运行时不能销毁：那个线程如果被唤醒（理论上不会）还会用它。
        std::mem::forget(runtime);
    }
}

/// 建立文件传输连接。与 `service::start` 走同一套 `connect_files`，但不挂窗口。
async fn connect(
    client: &AuthenticatedClient,
    device: &DeviceInfo,
    options: ConnectionMediaOptions,
    stop: &CancellationToken,
) -> Result<ControllerConnection> {
    let list = client.list_devices().await?;
    let target = list
        .my_binded_devices
        .iter()
        .find(|d| d.device_id == device.device_id)
        .context("设备已不在当前账号中")?;
    ensure!(
        matches!(target.platform, 1 | 4)
            && target.device_id != client.device_id()
            && target.is_connected()
            && target.controllable
            && target.controlled_support,
        "设备当前不允许文件连接"
    );
    let policy = client.feature_catalog().policy(target.platform, &target.version_name);
    let connection =
        ControllerConnection::connect_files(client, target, policy, options, stop, None).await?;
    if let Err(error) = connection.wait_port_mapping_ready().await {
        let _ = connection.close().await;
        return Err(error);
    }
    let supported = connection.stream_control_handle().file_transfer().supported();
    if !supported {
        let _ = connection.close().await;
        bail!("被控端不支持当前文件传输协议");
    }
    Ok(connection)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read as _;

    /// 远端的时间可能有三种形态：正常的 Unix 秒、0（未知）、以及有符号换算回绕
    /// 出来的巨值（实测值就在下面）。三种都不能让 `SystemTime` 加法溢出。
    #[test]
    fn remote_times_that_would_overflow_are_unknown() {
        assert_eq!(mtime(0), UNIX_EPOCH);
        assert_eq!(mtime(1_791_980_189), UNIX_EPOCH + Duration::from_secs(1_791_980_189));
        // 官方被控送来的 -11644473421，按 u64 就是这么大。
        assert_eq!(mtime(18_446_744_062_065_078_195), UNIX_EPOCH);
        // 离谱的未来值也不能溢出。
        assert_eq!(mtime(u64::MAX), UNIX_EPOCH);
        assert_eq!(mtime(253_402_300_800), UNIX_EPOCH);
    }

    /// 已知区间的合并/覆盖判断：冷读会留下洞，读洞必须重新去要，不能拿零当数据。
    #[test]
    fn ranges_track_holes() {
        let mut ranges = Ranges::default();
        assert!(!ranges.covers(0, 1));
        ranges.insert(100, 200);
        assert!(ranges.covers(100, 200));
        assert!(!ranges.covers(0, 100));
        assert!(!ranges.covers(199, 201));
        // 相接/重叠的段合并
        ranges.insert(200, 300);
        assert_eq!(ranges.0, vec![(100, 300)]);
        ranges.insert(50, 150);
        assert_eq!(ranges.0, vec![(50, 300)]);
        // 中间挖一段：洞两侧都还在
        let mut split = Ranges::default();
        split.insert(0, 10);
        split.insert(20, 30);
        assert!(!split.covers(5, 25));
        assert_eq!(split.covering_end(5), Some(10));
        assert_eq!(split.covering_end(15), None);
        assert_eq!(split.end(), 30);
        // 洞的起点（不是 offset 自己）：填出来才是一整段连续的
        assert_eq!(split.gap_start(15), 10);
        // 在已知段里：往后最近的洞从这段结尾开始
        assert_eq!(split.gap_start(0), 10);
        assert_eq!(split.gap_start(25), 30);
        // 20 正下方是洞，没有可校验的字节；10 正下方是已知段，能校验
        assert_eq!(split.probe_below(20), 0);
        assert_eq!(split.probe_below(10), 10);
        assert_eq!(split.probe_below(0), 0);
    }

    /// 拉取起点：没确认过对端就只敢从缺口起点往后续（那里下面有字节能校验），
    /// 确认过才敢直接跳到读者要的偏移。
    #[test]
    fn fetch_origin_depends_on_trust() {
        let mut known = Ranges::default();
        known.insert(0, 1024);
        known.insert(4096, 8192);
        // 落在已知段里、只是要得更长 → 从这段结尾接着要
        assert_eq!(RemoteFs::fetch_from(&known, 512, false), 1024);
        // 落在洞里 → 从洞的起点开始（下面就是已知段，能校验）
        assert_eq!(RemoteFs::fetch_from(&known, 2048, false), 1024);
        // 起点就是 0 的情况（缺口在最前面）：从 0 要，本来也不需要校验
        let mut head = Ranges::default();
        head.insert(1024, 2048);
        assert_eq!(RemoteFs::fetch_from(&head, 0, false), 0);
        // 确认过对端 → 直接跳到读者要的位置
        assert_eq!(RemoteFs::fetch_from(&known, 6000, true), 6000);
    }

    /// 名字校验：内核传来的名字必须是单段，否则我们拼出来的远端路径会写到别的目录去。
    #[test]
    fn names_must_be_a_single_component() {
        for good in ["a.txt", "带空格 的名字", "..a", "a..b"] {
            assert!(RemoteFs::valid_name(good).is_ok(), "{good} 应当合法");
        }
        for bad in ["", ".", "..", "a/b", "a\\b", "a\0b"] {
            assert!(RemoteFs::valid_name(bad).is_err(), "{bad:?} 应当被拒");
        }
        assert!(RemoteFs::valid_name(&"x".repeat(NAME_MAX as usize + 1)).is_err());
    }

    /// 上传进度是界面唯一能看出「传到哪儿了」的地方，别让它悄悄坏掉。
    #[test]
    fn upload_progress_is_visible_then_cleared() {
        progress_start("D:\\a\\b.bin", 1000, 1);
        progress_advance("D:\\a\\b.bin", 400, 1000);
        let (active, error) = upload_progress();
        assert_eq!(active.len(), 1);
        assert_eq!(
            (active[0].sent, active[0].total, active[0].attempt),
            (400, 1000, 1)
        );
        assert!(error.is_none());
        progress_finish("D:\\a\\b.bin", None);
        assert!(upload_progress().0.is_empty());
        progress_start("D:\\a\\c.bin", 10, 2);
        progress_finish("D:\\a\\c.bin", Some("断了".into()));
        let (active, error) = upload_progress();
        assert!(active.is_empty());
        assert_eq!(error.as_deref(), Some("断了"));
    }

    /// 挂上之后不卸，留给人在旁边看：验证 gvfs / 文件管理器认不认这个卷（`x-gvfs-show`
    /// 之类的用户选项内核不会留在挂载表里，得问 gvfs 自己）。
    ///
    /// OPENUUYC_TEST_DEVICE="<别名>" cargo test --lib features::file_transfer::fuse \
    ///   -- --ignored --nocapture mount_and_hold
    #[test]
    #[ignore = "需要真实设备与登录态，手动跑"]
    fn mount_and_hold_for_inspection() {
        let Ok(target) = std::env::var("OPENUUYC_TEST_DEVICE") else {
            eprintln!("跳过：未设置 OPENUUYC_TEST_DEVICE");
            return;
        };
        // 手动跑的时候要看得见 FUSE 自己的日志（续传校验、上传完成/失败），测试进程
        // 不走 diagnostics::logging 那套文件日志，所以这里起一个订阅器：别的模块只放
        // 警告，本模块放到 info。
        let _ = tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::new(
                    "warn,openuuyc::features::file_transfer::fuse=info",
                ),
            )
            .with_target(true)
            .try_init();
        let runtime = tokio::runtime::Runtime::new().expect("创建运行时失败");
        let client = std::sync::Arc::new(
            runtime
                .block_on(async { AuthenticatedClient::from_saved_session() })
                .expect("读取登录态失败"),
        );
        let device = runtime
            .block_on(async {
                client
                    .list_devices()
                    .await
                    .expect("拉取设备列表失败")
                    .my_binded_devices
                    .into_iter()
                    .find(|d| d.alias == target || d.device_id == target)
            })
            .expect("没有找到设备");
        mount(client, device, ConnectionMediaOptions::default()).expect("挂载失败");
        let root = mounted_path().expect("取挂载点失败");
        let hold = std::env::var("OPENUUYC_TEST_HOLD_SECS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(300);
        eprintln!("已挂载 {}，等待外部卸载（最多 {hold} 秒）", root.display());
        eprintln!("请在另一个终端跑：fusermount3 -u {}", root.display());
        let deadline = std::time::Instant::now() + Duration::from_secs(hold);
        while std::time::Instant::now() < deadline {
            if mounted_path().is_none() {
                eprintln!("检测到外部卸载 ✓（会话与连接都收尾了）");
                std::process::exit(0);
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        eprintln!("90 秒内没有检测到外部卸载 ✗");
        let _ = unmount();
        std::process::exit(0);
    }

    /// 真机集成测试：需要本机已登录，并设置 `OPENUUYC_TEST_DEVICE` 为设备别名或 device_id。
    ///
    /// OPENUUYC_TEST_DEVICE="<别名或 id>" \
    ///   cargo test --lib features::file_transfer::fuse -- --ignored --nocapture
    #[test]
    #[ignore = "需要真实设备与登录态，手动跑"]
    fn mounts_and_reads_real_device() {
        let Ok(target) = std::env::var("OPENUUYC_TEST_DEVICE") else {
            eprintln!("跳过：未设置 OPENUUYC_TEST_DEVICE（设备别名或 device_id）");
            return;
        };
        // 登录态与设备列表都要求在 Tokio 运行时里调（客户端自己会 spawn）。
        let runtime = tokio::runtime::Runtime::new().expect("创建测试运行时失败");
        let client = std::sync::Arc::new(
            runtime
                .block_on(async { AuthenticatedClient::from_saved_session() })
                .expect("读取已保存的登录态失败"),
        );
        let device = runtime
            .block_on(async {
                let list = client.list_devices().await.expect("拉取设备列表失败");
                list.my_binded_devices
                    .into_iter()
                    .find(|d| d.alias == target || d.device_id == target)
            })
            .expect("没有找到匹配的设备（按别名或 device_id）");
        eprintln!("连接设备：{} ({})", device.alias, device.device_id);

        mount(client.clone(), device, ConnectionMediaOptions::default()).expect("挂载失败");
        let root = mounted_path().expect("挂载后应能取到挂载点");
        eprintln!("挂载点：{}", root.display());
        // 挂载选项要能在 /proc/self/mounts 里看到 —— gvfs 就是读它决定要不要在
        // 文件管理器侧栏里显示这个卷的，所以这里直接打出来（含 x-gvfs-show）。
        let target = root.to_string_lossy().to_string();
        let entry = std::fs::read_to_string("/proc/self/mounts")
            .ok()
            .and_then(|mounts| {
                mounts
                    .lines()
                    .find(|line| line.split_whitespace().nth(1) == Some(target.as_str()))
                    .map(str::to_owned)
            });
        eprintln!("挂载选项(mounts)：{}", entry.unwrap_or_else(|| "（没有）".into()));
        // libmount（gvfs 走它）读的是 mountinfo，未知选项在老格式的 mounts 里会被省掉。
        let entry = std::fs::read_to_string("/proc/self/mountinfo")
            .ok()
            .and_then(|mounts| {
                mounts
                    .lines()
                    .find(|line| line.split_whitespace().nth(4) == Some(target.as_str()))
                    .map(str::to_owned)
            });
        eprintln!("挂载选项(mountinfo)：{}", entry.unwrap_or_else(|| "（没有）".into()));

        let entries: Vec<_> = std::fs::read_dir(&root)
            .expect("读取挂载点失败")
            .filter_map(|entry| entry.ok())
            .collect();
        assert!(!entries.is_empty(), "远端根目录不应为空");
        for entry in entries.iter().take(8) {
            let dir = entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false);
            eprintln!("  {}{}", entry.file_name().to_string_lossy(), if dir { "/" } else { "" });
        }

        let candidate = smallest_file(&root, 2).expect("远端没有找到可读的文件用于读取测试");
        eprintln!("读取文件头部：{}", candidate.display());
        let mut head = [0u8; 64];
        let mut file = std::fs::File::open(&candidate).expect("打开远端文件失败");
        let read = file.read(&mut head).expect("读取远端文件失败");
        eprintln!("读到 {read} 字节：{:02x?}", &head[..read]);
        assert!(read > 0, "读头部应至少拿到 1 字节");

        unmount().expect("卸载失败");
        eprintln!("FUSE 挂载、列目录、读文件、卸载都成功");
        // 客户端的后台运行时线程不会自己退出，测试函数返回后进程还挂着——手动跑这个
        // 测试时就表现为 cargo test 一直不结束、白等到超时。都验完了就明确退出。
        std::process::exit(0);
        assert!(mounted_path().is_none(), "卸载后不应再有挂载点");
        drop(mount);
        runtime.block_on(client.close());
    }

    /// 在 `depth` 层内找出最小的普通文件，避免测试去下载一个大文件。
    fn smallest_file(dir: &Path, depth: u32) -> Option<PathBuf> {
        let mut best: Option<(u64, PathBuf)> = None;
        let mut directories = Vec::new();
        for entry in std::fs::read_dir(dir).ok()?.flatten() {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_file() {
                let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
                // 空文件读不出内容，作为候选没有意义，跳过。
                if size > 0 && best.as_ref().is_none_or(|(current, _)| size < *current) {
                    best = Some((size, entry.path()));
                }
            } else if kind.is_dir() && depth > 0 {
                directories.push(entry.path());
            }
        }
        for directory in directories {
            if let Some((size, path)) = smallest_file(&directory, depth - 1).map(|path| {
                let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(u64::MAX);
                (size, path)
            }) && best.as_ref().is_none_or(|(current, _)| size < *current)
            {
                best = Some((size, path));
            }
        }
        best.map(|(_, path)| path)
    }
}
