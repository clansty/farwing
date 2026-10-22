//! 设备标签：本机一份，同步目录里每个安装身份一份。
//!
//! 同一台设备只认最后写的那一份，所以删掉一个标签就是少了它，不需要墓碑。
//! 文件名按安装身份分开，同步工具（网盘、Syncthing、git）面对不同文件不会
//! 产生冲突副本，也就没有「保哪一份」要问用户。
//!
//! 不收账号作用域：设备 ID 本身就是全局唯一的设备身份，而别的账号的设备根本
//! 不会出现在本账号的设备清单里，回收站里多几条也用不上。用昵称做主键反而会
//! 在改名后把已有标签挡在外面。
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const SCHEMA: u8 = 1;
/// 一台设备的标签上限，和单个标签的字数上限：够分组用，又不至于把设备行撑爆。
pub(super) const MAX_TAGS: usize = 12;
pub(super) const MAX_TAG_CHARS: usize = 24;
const MAX_FILES: usize = 256;
const MAX_FILE_BYTES: u64 = 1024 * 1024;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Entry {
    ts: u64,
    #[serde(default)]
    tags: Vec<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct WriterFile {
    #[serde(default)]
    schema: u8,
    #[serde(default)]
    writer: String,
    /// 设备 ID -> 那一次写入。
    #[serde(default)]
    devices: BTreeMap<String, Entry>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct Settings {
    writer: String,
    sync_dir: Option<PathBuf>,
}

impl Settings {
    fn path(root: &Path) -> PathBuf {
        root.join("settings.json")
    }
    /// 安装身份只存在本机，不跟着同步目录走：两台机器共用一个身份会互相覆盖。
    fn load_or_create(root: &Path) -> Result<Self> {
        let path = Self::path(root);
        let mut settings = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice::<Self>(&bytes).context("标签设置文件损坏")?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(error) => return Err(error).context("读取标签设置失败"),
        };
        if settings.writer.is_empty() {
            settings.writer = uuid::Uuid::new_v4().simple().to_string();
            settings.save(root)?;
        }
        Ok(settings)
    }
    fn save(&self, root: &Path) -> Result<()> {
        write_atomic(&Self::path(root), &serde_json::to_vec_pretty(self)?)
    }
}

fn root() -> Result<PathBuf> {
    Ok(crate::platform::paths::require_local_app_data()?.join("OpenUUYC/device-tags"))
}

/// 一次同步的结果，供界面说明「读到了什么、跳过了什么」。
#[derive(Clone, Debug, Default)]
pub(super) struct Report {
    pub files: usize,
    pub skipped: usize,
    pub at: Option<SystemTime>,
    pub error: Option<String>,
}

pub(super) struct Store {
    root: PathBuf,
    writer: String,
    sync_dir: Option<PathBuf>,
    devices: BTreeMap<String, Entry>,
    report: Report,
}

impl Store {
    pub fn open() -> Result<Self> {
        let root = root()?;
        let settings = Settings::load_or_create(&root)?;
        Self::with(root, settings)
    }

    fn with(root: PathBuf, settings: Settings) -> Result<Self> {
        let mut store = Self {
            root,
            writer: settings.writer,
            sync_dir: settings.sync_dir,
            devices: BTreeMap::new(),
            report: Report::default(),
        };
        store.reload();
        Ok(store)
    }

    pub fn tags(&self, device_id: &str) -> &[String] {
        self.devices
            .get(device_id)
            .map_or(&[], |entry| entry.tags.as_slice())
    }

    pub fn report(&self) -> &Report {
        &self.report
    }

    /// 本机在同步目录里写的那份文件名，界面用它告诉用户哪一份是自己的。
    pub fn writer(&self) -> &str {
        &self.writer
    }

    pub fn sync_dir(&self) -> Option<&Path> {
        self.sync_dir.as_deref()
    }

    /// 清空同步目录只影响以后往哪写，已经合并进来的标签还在本机。
    pub fn set_sync_dir(&mut self, dir: Option<PathBuf>) -> Result<()> {
        let dir = dir.filter(|path| !path.as_os_str().is_empty());
        if let Some(dir) = &dir {
            // 必须是已经存在的目录：否则打错一个字就会在别处悄悄建出一个空目录。
            ensure!(dir.is_dir(), "同步目录不存在：{}", dir.display());
        }
        Settings {
            writer: self.writer.clone(),
            sync_dir: dir.clone(),
        }
        .save(&self.root)?;
        self.sync_dir = dir;
        self.reload();
        Ok(())
    }

    /// 写入一台设备的标签集合；`tags` 已经去过重、限长。
    pub fn set(&mut self, device_id: &str, tags: Vec<String>) -> Result<()> {
        crate::account::api::validate_device_id(device_id)?;
        let entry = Entry {
            ts: self.next_ts(),
            tags,
        };
        // 空集合也要留一条记录：不然别的机器上的旧标签下次同步就复活了。
        self.devices.insert(device_id.to_owned(), entry);
        self.flush()
    }

    /// 读本机那份和同步目录里的所有份，合并后回写自己的那一份。
    pub fn reload(&mut self) {
        let mut report = Report::default();
        let mut paths = Vec::new();
        let merged = (|| -> Result<BTreeMap<String, Entry>> {
            let mut devices = BTreeMap::new();
            paths.push(self.own_path());
            if let Some(dir) = &self.sync_dir {
                paths.extend(mirror_files(dir)?);
            }
            for path in &paths {
                match read_file(path) {
                    Some(file) => merge(&mut devices, file.devices),
                    None if path.exists() => report.skipped += 1,
                    None => {}
                }
            }
            Ok(devices)
        })();
        report.files = paths.iter().filter(|path| path.exists()).count();
        report.at = Some(SystemTime::now());
        match merged {
            Ok(devices) => {
                self.devices = devices;
                // 写不进去（只读目录、网盘正在同步）不影响这次读到的结果。
                if let Err(error) = self.flush() {
                    report.error = Some(format!("写入标签文件失败：{error:#}"));
                }
            }
            // 读失败就整个不动，界面继续用上一次的内存状态。
            Err(error) => report.error = Some(format!("读取标签文件失败：{error:#}")),
        }
        self.report = report;
    }

    fn next_ts(&self) -> u64 {
        // 混合逻辑时钟：新值必须大于任何见过的值，否则本机时钟慢的时候改动永远输。
        let seen = self
            .devices
            .values()
            .map(|entry| entry.ts)
            .max()
            .unwrap_or(0);
        now_ms().max(seen + 1)
    }

    fn own_path(&self) -> PathBuf {
        self.root.join(format!("{}.json", self.writer))
    }

    fn flush(&mut self) -> Result<()> {
        let file = WriterFile {
            schema: SCHEMA,
            writer: self.writer.clone(),
            devices: self.devices.clone(),
        };
        let bytes = serde_json::to_vec_pretty(&file)?;
        write_atomic(&self.own_path(), &bytes)?;
        if let Some(dir) = &self.sync_dir {
            std::fs::create_dir_all(dir).with_context(|| format!("无法创建同步目录 {dir:?}"))?;
            write_atomic(&dir.join(format!("{}.json", self.writer)), &bytes)?;
        }
        Ok(())
    }
}

/// 同步目录里我们只读本程序的文件名，别的 JSON 一概不碰。
fn mirror_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(paths),
        Err(error) => return Err(error).with_context(|| format!("无法读取同步目录 {dir:?}")),
    };
    for entry in entries {
        if paths.len() >= MAX_FILES {
            break;
        }
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        // 同步工具留下的冲突副本（`x.sync-conflict-….json`）也一起读：
        // 内容是一样的，按设备取最新就够了，不需要人工删。
        if !name.ends_with(".json") {
            continue;
        }
        let path = entry.path();
        if path.is_file() {
            paths.push(path);
        }
    }
    paths.sort();
    Ok(paths)
}

fn read_file(path: &Path) -> Option<WriterFile> {
    let file = std::fs::File::open(path).ok()?;
    let mut bytes = Vec::new();
    file.take(MAX_FILE_BYTES + 1).read_to_end(&mut bytes).ok()?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return None;
    }
    serde_json::from_slice::<WriterFile>(&bytes).ok()
}

fn merge(into: &mut BTreeMap<String, Entry>, from: BTreeMap<String, Entry>) {
    for (id, entry) in from {
        match into.get_mut(&id) {
            Some(current) if current.ts > entry.ts => {}
            Some(current) if current.ts == entry.ts => {
                // 罕见的同毫秒并发：并集即可，结果与读取顺序无关。
                for tag in entry.tags {
                    if !current.tags.contains(&tag) {
                        current.tags.push(tag);
                    }
                }
            }
            _ => {
                into.insert(id, entry);
            }
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

/// 单行输入按空白和逗号切分，顺手去掉 `#`、去重、限长。
pub(super) fn parse(input: &str) -> Vec<String> {
    let mut tags: Vec<String> = Vec::new();
    for raw in input.split([' ', '\t', ',', '，', ';', '；', '#', '、']) {
        let tag = raw.trim();
        if tag.is_empty() || tag.chars().any(char::is_control) {
            continue;
        }
        let tag: String = tag.chars().take(MAX_TAG_CHARS).collect();
        if !tags.iter().any(|existing| existing.eq_ignore_ascii_case(&tag)) {
            tags.push(tag);
        }
        if tags.len() >= MAX_TAGS {
            break;
        }
    }
    tags
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("标签文件路径无效")?;
    std::fs::create_dir_all(parent)?;
    let temp = parent.join(format!(".tags-{}.tmp", uuid::Uuid::new_v4().simple()));
    let result = (|| -> Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temp, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result.with_context(|| format!("无法写入标签文件 {path:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(ts: u64, tags: &[&str]) -> Entry {
        Entry {
            ts,
            tags: tags.iter().map(|tag| (*tag).to_owned()).collect(),
        }
    }

    fn device_id(n: u8) -> String {
        format!("{:016x}", n)
    }

    #[test]
    fn later_write_wins_per_device() {
        let mut into = BTreeMap::new();
        merge(&mut into, BTreeMap::from([(device_id(1), entry(9, &["旧"]))]));
        merge(&mut into, BTreeMap::from([(device_id(1), entry(4, &["更旧"]))]));
        merge(&mut into, BTreeMap::from([(device_id(1), entry(12, &[]))]));
        let stored = &into[&device_id(1)];
        assert!(stored.tags.is_empty(), "清空的写入必须留下记录");
        assert_eq!(stored.ts, 12);
    }

    #[test]
    fn merge_is_order_independent() {
        let one = BTreeMap::from([
            (device_id(1), entry(5, &["家里"])),
            (device_id(2), entry(7, &["公司"])),
        ]);
        let two = BTreeMap::from([
            (device_id(1), entry(6, &["主力机"])),
            (device_id(2), entry(3, &["备用"])),
        ]);
        let mut forward = BTreeMap::new();
        merge(&mut forward, one.clone());
        merge(&mut forward, two.clone());
        let mut backward = BTreeMap::new();
        merge(&mut backward, two);
        merge(&mut backward, one);
        assert_eq!(forward, backward);
        assert_eq!(forward[&device_id(1)].tags, vec!["主力机".to_owned()]);
        assert_eq!(forward[&device_id(2)].tags, vec!["公司".to_owned()]);
    }

    #[test]
    fn equal_timestamp_unions_tags() {
        let mut into = BTreeMap::new();
        merge(&mut into, BTreeMap::from([(device_id(1), entry(5, &["家里"]))]));
        merge(&mut into, BTreeMap::from([(device_id(1), entry(5, &["公司"]))]));
        assert_eq!(
            into[&device_id(1)].tags,
            vec!["家里".to_owned(), "公司".to_owned()]
        );
    }

    #[test]
    fn parse_splits_and_dedupes() {
        assert_eq!(
            parse(" 家里,公司 #主力机，家里"),
            vec!["家里".to_owned(), "公司".to_owned(), "主力机".to_owned()]
        );
        assert_eq!(parse("   "), Vec::<String>::new());
    }

    #[test]
    fn parse_limits_count_and_length() {
        let long = "字".repeat(MAX_TAG_CHARS + 5);
        assert_eq!(parse(&long)[0].chars().count(), MAX_TAG_CHARS);
        let many = (0..MAX_TAGS + 4)
            .map(|n| format!("t{n}"))
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(parse(&many).len(), MAX_TAGS);
    }

    fn scratch(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "openuuyc-tags-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    /// 两台机器共用一个同步目录：各写各的文件，读到的是最后写入的那台。
    #[test]
    fn two_writers_converge_through_one_directory() {
        let base = scratch("converge");
        let shared = base.join("share");
        std::fs::create_dir_all(&shared).unwrap();
        let machine = |name: &str| {
            Store::with(
                base.join(name),
                Settings {
                    writer: name.to_owned(),
                    sync_dir: Some(shared.clone()),
                },
            )
            .unwrap()
        };
        let first = device_id(1);
        let second = device_id(2);

        let mut left = machine("left");
        left.set(&first, vec!["家里".to_owned()]).unwrap();
        left.set(&second, vec!["公司".to_owned()]).unwrap();

        let mut right = machine("right");
        assert_eq!(right.tags(&first), ["家里".to_owned()]);
        right.set(&first, vec!["主力机".to_owned()]).unwrap();
        assert_eq!(right.tags(&second), ["公司".to_owned()]);

        left.reload();
        assert_eq!(left.tags(&first), ["主力机".to_owned()], "后写的赢");

        // 清空也是一次写入，不能被另一台的旧文件复活。
        right.set(&first, Vec::new()).unwrap();
        left.reload();
        assert!(left.tags(&first).is_empty(), "删除必须保留记录");

        // 各写各的文件，同步目录里不会有第二份自己。
        left.set(&second, vec!["备用".to_owned()]).unwrap();
        assert!(shared.join("left.json").is_file());
        assert!(shared.join("right.json").is_file());
        assert_eq!(std::fs::read_dir(&shared).unwrap().count(), 2);

        std::fs::remove_dir_all(&base).unwrap();
    }

    /// 只读不了别人的文件不该连累自己：坏文件跳过去，其余照常合并。
    #[test]
    fn broken_file_is_skipped() {
        let base = scratch("broken");
        let shared = base.join("share");
        std::fs::create_dir_all(&shared).unwrap();
        std::fs::write(shared.join("broken.json"), b"{ not json").unwrap();
        let mut store = Store::with(
            base.join("local"),
            Settings {
                writer: "local".to_owned(),
                sync_dir: Some(shared.clone()),
            },
        )
        .unwrap();
        store.set(&device_id(1), vec!["家里".to_owned()]).unwrap();
        assert_eq!(store.report().skipped, 1);
        assert!(store.report().error.is_none());
        assert_eq!(store.tags(&device_id(1)), ["家里".to_owned()]);
        std::fs::remove_dir_all(&base).unwrap();
    }
}
