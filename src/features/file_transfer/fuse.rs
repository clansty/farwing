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
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use super::{
    Payload, contract, lock, protocol::*, storage, success,
    tasks::{self, Rpc},
};
use crate::account::{api::DeviceInfo, client::AuthenticatedClient};
use crate::media::ConnectionMediaOptions;
use crate::session::controller::ControllerConnection;
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

/// 官方 `FileEntry.entry_type` 的目录判定：0 普通目录、2 目录符号链接、3 磁盘/卷根。
fn is_directory(entry_type: i32) -> bool {
    matches!(entry_type, 0 | 2 | 3)
}

struct Node {
    /// 远端绝对路径，直接来自 `FileEntry.full_path`。
    path: String,
    parent: u64,
    dir: bool,
    size: u64,
    mtime: SystemTime,
    /// 已列过的目录项 (名字, inode)；`None` 表示还没列。文件永远不列。
    children: Option<Vec<(String, u64)>>,
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
struct CacheEntry {
    path: PathBuf,
    state: Mutex<CacheState>,
    ready: Condvar,
}

struct CacheState {
    /// `[0, valid)` 已经落盘、可以当真；以外是稀疏文件里的洞，读出来是零，不能给。
    valid: u64,
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
    state: Mutex<State>,
    cache: std::sync::Arc<Cache>,
    uid: u32,
    gid: u32,
}

impl RemoteFs {
    fn new(
        wire: std::sync::Arc<super::Transport>,
        runtime: tokio::runtime::Handle,
        stop: CancellationToken,
        cache_dir: PathBuf,
    ) -> Self {
        let mut nodes = HashMap::new();
        nodes.insert(
            ROOT_INO,
            Node {
                path: REMOTE_ROOT.into(),
                parent: ROOT_INO,
                dir: true,
                size: 0,
                mtime: UNIX_EPOCH,
                children: None,
            },
        );
        let mut by_path = HashMap::new();
        by_path.insert(REMOTE_ROOT.to_owned(), ROOT_INO);
        Self {
            wire,
            runtime,
            stop,
            state: Mutex::new(State {
                nodes,
                by_path,
                next: ROOT_INO + 1,
            }),
            cache: std::sync::Arc::new(Cache {
                dir: cache_dir,
                entries: Mutex::new(HashMap::new()),
                clock: AtomicU64::new(0),
            }),
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
            // 卷本身以只读挂载，也没有实现 write，这些位只是给文件管理器看的。
            perm: if node.dir { 0o755 } else { 0o444 },
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
                            parent: ino,
                            dir: is_directory(entry.entry_type),
                            size: entry.size,
                            mtime: mtime(entry.modified_time),
                            children: None,
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
        let (path, remote_size) = {
            let state = lock(&self.state);
            let node = state.nodes.get(&ino).context("未知文件")?;
            ensure!(!node.dir, "目录不可读");
            (node.path.clone(), node.size)
        };
        if offset >= remote_size {
            return Ok(Some(Vec::new()));
        }
        let wanted = u64::from(size.min(MAX_READ)).min(remote_size - offset);
        let entry = self.cache_entry(ino)?;
        let mut state = lock(&entry.state);
        state.used = self.cache.clock.fetch_add(1, Ordering::Relaxed);
        loop {
            if let Some(error) = &state.error {
                bail!("远端读取失败：{error}");
            }
            // 要的字节已经在本地；或者远端声明的大小已经整份落盘，那之后就是 EOF。
            if offset + wanted <= state.valid || state.valid >= remote_size {
                break;
            }
            if state.fetch.is_none() {
                self.start_fetch(&entry, path.clone(), &mut state);
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
        if self.stop.is_cancelled() {
            bail!("远端连接已结束");
        }
        let available = state.valid;
        drop(state);
        // 远端声明的大小与实际内容可能不一致，用 saturating 保证越界读退化成空读。
        let take = usize::try_from(available.saturating_sub(offset).min(wanted)).unwrap_or(0);
        if take == 0 {
            return Ok(Some(Vec::new()));
        }
        // `~/.cache` 下的东西会被系统清理工具、tmpfiles 之流删掉，而我们记着的
        // `valid` 还在——那种情况交给调用方清掉记忆再拉一遍。
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
            state.valid = 0;
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
                valid: 0,
                fetch: None,
                readers: 0,
                error: None,
                used: 0,
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
        state: &mut CacheState,
    ) {
        let abort = CancellationToken::new();
        state.fetch = Some(abort.clone());
        let known = state.valid;
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
                known,
                cache_path,
                task_entry.clone(),
            )
            .await;
            let mut state = lock(&task_entry.state);
            match result {
                Ok(received) => state.valid = state.valid.max(received),
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
                    total += state.valid;
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
    known: u64,
    cache_path: PathBuf,
    entry: std::sync::Arc<CacheEntry>,
) -> Result<u64> {
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
    // 已经有这么多字节是可信的；请求它从 `request_from` 开始发。
    let probe = known.min(RESUME_PROBE);
    let request_from = known - probe;
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
                return Ok(stream_start.unwrap_or(request_from) + received);
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
                    resume_point: request_from,
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
                            request_from
                        } else {
                            let compare = usize::try_from(probe.min(value.data.len() as u64))?;
                            let mut local = vec![0u8; compare];
                            file.read_exact_at(&mut local, request_from)?;
                            if local == value.data[..compare] {
                                request_from
                            } else {
                                // 它没按续传位置发（或者发的是别的东西）：按从 0 头
                                // 处理，错位的字节会被下面跳过。
                                tracing::warn!(
                                    remote_size = size,
                                    resume_point = request_from,
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
                if end > known {
                    let skip = usize::try_from(known.saturating_sub(position))
                        .unwrap_or(usize::MAX)
                        .min(value.data.len());
                    file.write_all_at(&value.data[skip..], position + skip as u64)?;
                    let mut state = lock(&entry.state);
                    state.valid = state.valid.max(end);
                    let valid = state.valid;
                    drop(state);
                    if valid > 0 {
                        entry.ready.notify_all();
                    }
                }
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
                    return Ok(stream_start.unwrap_or(request_from) + received);
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
        if flags & (libc::O_WRONLY | libc::O_RDWR) != 0 {
            reply.error(libc::EROFS);
            return;
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
            let mut state = lock(&entry.state);
            state.readers = state.readers.saturating_sub(1);
            if state.readers == 0
                && let Some(abort) = &state.fetch
            {
                abort.cancel();
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

    fn statfs(&mut self, _req: &fuser::Request<'_>, _ino: u64, reply: fuser::ReplyStatfs) {
        // 容量对只读卷没有意义，给一个自洽的壳，让 df/gvfs 不至于报零。
        reply.statfs(0, 0, 0, 0, 0, 4096, NAME_MAX, 4096);
    }
}

fn mount_options() -> Vec<fuser::MountOption> {
    vec![
        fuser::MountOption::FSName("openuuyc-remote".to_owned()),
        fuser::MountOption::Subtype(crate::platform::linux::REMOTE_FS_SUBTYPE.to_owned()),
        fuser::MountOption::RO,
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
    match std::process::Command::new("xdg-open").arg(&mountpoint).spawn() {
        Ok(_) => {}
        Err(error) => tracing::debug!(%error, "xdg-open 打不开挂载点，不影响挂载"),
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
        let filesystem = RemoteFs::new(wire, handle.clone(), stop.clone(), cache_dir.clone());
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
        // 手动跑的时候要看得见 FUSE 自己的警告（比如续传位置没被遵守），测试进程不
        // 走 diagnostics::logging 那套文件日志，所以这里起一个只放警告的订阅器。
        let _ = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::WARN)
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
