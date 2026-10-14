//! Linux device facts and the signed-in user's static desktop wallpaper, in
//! the shape the Windows profile reports.
//!
//! Every fact comes from files an unprivileged desktop user can read. The
//! SMBIOS UUID (`/sys/class/dmi/id/product_uuid`) is readable only by root, so
//! the system UUID is derived from the installation's machine ID instead; it
//! then stays the same whoever runs the client, which the registration check
//! requires.
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{io::Read, path::PathBuf, process::Command, time::Duration};

#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Hardware {
    pub name: String,
    pub system_uuid: String,
    pub machine_guid: String,
    pub os: String,
    pub base_board: String,
    pub cpu: String,
    pub video: Vec<String>,
    pub mac: String,
    pub memory: i64,
    pub screen: String,
    pub errors: Vec<String>,
}

/// Namespace of the system UUID derived from the machine ID.
const SYSTEM_NAMESPACE: uuid::Uuid = uuid::Uuid::from_u128(0x3f0e6f7c_8a5b_4d2e_9c61_0b7e4a2d9f15);

fn read_trimmed(path: &str) -> Result<String> {
    Ok(std::fs::read_to_string(path)
        .with_context(|| format!("读取 {path}"))?
        .trim()
        .to_owned())
}

/// The installation identity, `/etc/machine-id` as a hyphenated UUID.
fn machine_id() -> Result<uuid::Uuid> {
    let text = read_trimmed("/etc/machine-id")
        .or_else(|_| read_trimmed("/var/lib/dbus/machine-id"))
        .context("系统未提供机器标识")?;
    let id = uuid::Uuid::try_parse(&text).context("机器标识格式无效")?;
    ensure!(!id.is_nil(), "机器标识为空");
    Ok(id)
}

pub(crate) fn computer_name() -> Result<String> {
    let name = read_trimmed("/proc/sys/kernel/hostname")?;
    ensure!(!name.is_empty(), "系统未提供主机名");
    Ok(name)
}

impl Hardware {
    pub fn read() -> Result<Self> {
        let machine = machine_id()?;
        let mut value = Self {
            name: computer_name()?,
            system_uuid: uuid::Uuid::new_v5(&SYSTEM_NAMESPACE, machine.as_bytes())
                .to_string()
                .to_uppercase(),
            machine_guid: machine.to_string(),
            ..Self::default()
        };
        let mut field = |label: &str, result: Result<String>| match result {
            Ok(value) if !value.is_empty() => value,
            Ok(_) => {
                value.errors.push(format!("{label}：系统未提供"));
                String::new()
            }
            Err(e) => {
                value.errors.push(format!("{label}：{e:#}"));
                String::new()
            }
        };
        value.cpu = field("处理器", proc_field("/proc/cpuinfo", "model name"));
        value.base_board = field(
            "主板",
            (|| {
                Ok(format!(
                    "Manufacturer: {}  Product: {}",
                    read_trimmed("/sys/class/dmi/id/board_vendor")?,
                    read_trimmed("/sys/class/dmi/id/board_name")?
                ))
            })(),
        );
        value.os = field("操作系统", os());
        value.mac = field("网卡地址", primary_mac());
        match graphics() {
            Ok(names) => value.video = names,
            Err(e) => value.errors.push(format!("显卡：{e:#}")),
        }
        match proc_field("/proc/meminfo", "MemTotal").and_then(|total| {
            let kib = total
                .split_whitespace()
                .next()
                .context("内存大小缺失")?
                .parse::<i64>()?;
            Ok(kib / 1024)
        }) {
            Ok(memory) => value.memory = memory,
            Err(e) => value.errors.push(format!("内存：{e:#}")),
        }
        value.screen = primary_screen().unwrap_or_else(|| "0x0".into());
        Ok(value)
    }
}

fn proc_field(path: &str, key: &str) -> Result<String> {
    let text = std::fs::read_to_string(path).with_context(|| format!("读取 {path}"))?;
    Ok(text
        .lines()
        .find_map(|line| line.split_once(':').filter(|(name, _)| name.trim() == key))
        .map(|(_, value)| value.trim().to_owned())
        .unwrap_or_default())
}

fn os() -> Result<String> {
    let release = std::fs::read_to_string("/etc/os-release")
        .or_else(|_| std::fs::read_to_string("/usr/lib/os-release"))
        .context("读取 os-release")?;
    let name = release
        .lines()
        .find_map(|line| line.strip_prefix("PRETTY_NAME="))
        .map(|value| value.trim().trim_matches('"').to_owned())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "Linux".into());
    let kernel = read_trimmed("/proc/sys/kernel/osrelease")?;
    let bits = if cfg!(target_pointer_width = "64") {
        "64-bit"
    } else {
        "32-bit"
    };
    Ok(format!("{name} {bits} · Linux {kernel}"))
}

/// Display controllers on the PCI bus (class 0x03), named from `pci.ids`
/// the way `lspci` names them; each model once, like the Windows list.
fn graphics() -> Result<Vec<String>> {
    let mut names = Vec::new();
    let database = ["/usr/share/misc/pci.ids", "/usr/share/hwdata/pci.ids"]
        .iter()
        .find_map(|path| std::fs::read_to_string(path).ok());
    for entry in std::fs::read_dir("/sys/bus/pci/devices").context("读取 PCI 设备")? {
        let path = entry?.path();
        let read = |name: &str| read_trimmed(&path.join(name).to_string_lossy());
        if !read("class").is_ok_and(|class| class.starts_with("0x03")) {
            continue;
        }
        let hex = |name: &str| -> Result<u16> {
            let text = read(name)?;
            Ok(u16::from_str_radix(text.trim_start_matches("0x"), 16)?)
        };
        let (vendor, device) = (hex("vendor")?, hex("device")?);
        let name = database
            .as_deref()
            .and_then(|database| pci_name(database, vendor, device))
            .unwrap_or_else(|| format!("PCI {vendor:04x}:{device:04x}"));
        if !names.contains(&name) {
            names.push(name);
        }
    }
    Ok(names)
}

/// The `pci.ids` name of the PCI device at a sysfs device directory.
pub(super) fn pci_device_name(device: &std::path::Path) -> Option<String> {
    let hex = |name: &str| -> Option<u16> {
        let text = read_trimmed(&device.join(name).to_string_lossy()).ok()?;
        u16::from_str_radix(text.trim_start_matches("0x"), 16).ok()
    };
    let (vendor, id) = (hex("vendor")?, hex("device")?);
    ["/usr/share/misc/pci.ids", "/usr/share/hwdata/pci.ids"]
        .iter()
        .find_map(|path| std::fs::read_to_string(path).ok())
        .and_then(|database| pci_name(&database, vendor, id))
}

/// "NVIDIA RTX A6000" from "NVIDIA Corporation" and "GA102GL [RTX A6000]":
/// the bracketed marketing name where `pci.ids` has one.
fn pci_name(database: &str, vendor: u16, device: u16) -> Option<String> {
    let short = |name: &str| -> String {
        match (name.find('['), name.rfind(']')) {
            (Some(start), Some(end)) if start < end => name[start + 1..end].to_owned(),
            _ => name.to_owned(),
        }
    };
    let vendor_prefix = format!("{vendor:04x}  ");
    let device_prefix = format!("\t{device:04x}  ");
    let mut lines = database.lines();
    let vendor_name = lines.find_map(|line| line.strip_prefix(&vendor_prefix))?;
    let vendor_name = if vendor_name.contains('[') {
        short(vendor_name)
    } else {
        vendor_name
            .split_whitespace()
            .next()
            .unwrap_or(vendor_name)
            .to_owned()
    };
    let device_name = lines
        .take_while(|line| line.starts_with('\t') || line.starts_with('#') || line.is_empty())
        .find_map(|line| line.strip_prefix(&device_prefix))?;
    Some(format!("{vendor_name} {}", short(device_name)))
}

/// The physical Ethernet or Wi-Fi adapter carrying the default route with the
/// lowest metric, as Windows picks the gateway adapter with the lowest metric.
fn primary_mac() -> Result<String> {
    let routes = std::fs::read_to_string("/proc/net/route").context("读取路由表")?;
    let mut candidates = Vec::new();
    for line in routes.lines().skip(1) {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let [interface, destination, _gateway, flags, _, _, metric, ..] = fields[..] else {
            continue;
        };
        let flags = u32::from_str_radix(flags, 16).unwrap_or(0);
        // A default route that is up and goes through a gateway.
        if destination != "00000000" || flags & 0x3 != 0x3 {
            continue;
        }
        let device = PathBuf::from("/sys/class/net").join(interface);
        let physical = device.join("device").exists()
            && read_trimmed(&device.join("type").to_string_lossy()).is_ok_and(|t| t == "1")
            && read_trimmed(&device.join("operstate").to_string_lossy()).is_ok_and(|s| s == "up");
        if !physical {
            continue;
        }
        let address = read_trimmed(&device.join("address").to_string_lossy())?;
        candidates.push((metric.parse::<u32>().unwrap_or(u32::MAX), address));
    }
    candidates.sort();
    Ok(candidates
        .into_iter()
        .next()
        .map(|(_, mac)| mac.to_uppercase())
        .unwrap_or_default())
}

fn primary_screen() -> Option<String> {
    let monitors = super::display::topology::Topology::query(true)
        .ok()?
        .monitors();
    let monitor = monitors
        .iter()
        .find(|monitor| monitor.primary)
        .or_else(|| monitors.first())?;
    Some(format!("{}x{}", monitor.width, monitor.height))
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct WallpaperSource {
    pub path: PathBuf,
    pub modified: std::time::SystemTime,
    pub length: u64,
}
impl WallpaperSource {
    /// Windows may read a cached copy of the configured picture; Linux always
    /// reads the configured file itself.
    pub(crate) fn cached(&self) -> bool {
        false
    }
}

/// 一个壁纸来源：给出本机壁纸文件，或说明这一路为什么不行。
type Backend = (&'static str, fn() -> Result<PathBuf>);

/// The static picture the desktop shows now.
///
/// 不能只看 `XDG_CURRENT_DESKTOP`：niri、Hyprland、sway 这些合成器自己不管壁纸，
/// 变量里只有合成器的名字，真正铺图的是 shell（noctalia、caelestia…）或守护进程
/// （swww、hyprpaper、swaybg）。反过来也不能拿 gsettings 当通用兜底——别的桌面
/// 留下的旧值会一直躺在 dconf 里，读出来是一张早就不显示的图。
///
/// 顺序：活着的进程/服务先问（它们报的就是屏幕上那张）→ 当前桌面环境自己的配置
/// → 其它 shell 的落盘状态。前两类都失败时宁可报错，也不猜一条路径出来。
pub(crate) fn wallpaper_source() -> Result<WallpaperSource> {
    let mut tried = Vec::new();
    for (name, backend) in wallpaper_backends() {
        match backend().and_then(accepted) {
            Ok(source) => {
                tracing::debug!(backend = name, path = %source.path.display(), "壁纸来源");
                return Ok(source);
            }
            Err(error) => tried.push(format!("{name}: {error:#}")),
        }
    }
    bail!("没有找到本机壁纸（已尝试 {}）", tried.join("；"))
}

/// 后端给的路径要真的是本机上一张静态图片，才交给上报流程。
fn accepted(path: PathBuf) -> Result<WallpaperSource> {
    ensure!(!path.as_os_str().is_empty(), "没有配置静态图片壁纸");
    ensure!(path.is_absolute(), "壁纸必须是本机文件");
    let metadata = std::fs::metadata(&path).context("读取当前桌面壁纸")?;
    ensure!(
        metadata.is_file() && metadata.len() <= crate::platform::wallpaper::MAXIMUM_FILE,
        "壁纸文件过大或不是图片文件"
    );
    Ok(WallpaperSource {
        path,
        modified: metadata.modified()?,
        length: metadata.len(),
    })
}

/// 候选来源，顺序即优先级。
fn wallpaper_backends() -> Vec<Backend> {
    let desktop = std::env::var("XDG_CURRENT_DESKTOP")
        .unwrap_or_default()
        .to_ascii_uppercase();
    let mut list: Vec<Backend> = Vec::new();
    // 活着的 shell：noctalia 的 IPC 只有它在跑才答得上话。
    if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        list.push(("noctalia IPC", noctalia_ipc_wallpaper));
    }
    // 活着的守护进程：没在跑就查不到，因此不会读到别处留下的旧值。
    list.push(("swww", swww_wallpaper));
    list.push(("hyprpaper", hyprpaper_wallpaper));
    list.push(("swaybg/wbg", daemon_wallpaper));
    // 桌面环境自己的配置。
    if desktop.contains("KDE") {
        list.push(("Plasma", plasma_wallpaper));
    }
    if desktop.contains("GNOME") || desktop.contains("UNITY") || desktop.contains("BUDGIE") {
        list.push(("GNOME", gnome_wallpaper));
    }
    if desktop.contains("CINNAMON") {
        list.push(("Cinnamon", cinnamon_wallpaper));
    }
    if desktop.contains("MATE") {
        list.push(("MATE", mate_wallpaper));
    }
    if desktop.contains("XFCE") {
        list.push(("XFCE", xfce_wallpaper));
    }
    if desktop.contains("LXQT") {
        list.push(("LXQt", lxqt_wallpaper));
    }
    if desktop.contains("COSMIC") {
        list.push(("COSMIC", cosmic_wallpaper));
    }
    // 其它 shell 的落盘状态：不依赖桌面环境的名字，放在最后。
    list.push(("noctalia 缓存", noctalia_cache_wallpaper));
    list.push(("noctalia v5", noctalia_v5_wallpaper));
    list.push(("caelestia", caelestia_wallpaper));
    list.push(("illogical-impulse", illogical_impulse_wallpaper));
    list
}

/// 一个 XDG 目录：环境变量给出绝对路径时用它，否则退回 `$HOME/<fallback>`。
fn xdg_dir(variable: &str, fallback: &str) -> Result<PathBuf> {
    std::env::var_os(variable)
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .map(|home| home.join(fallback))
        })
        .filter(|path| path.is_absolute())
        .context("找不到用户目录（需要 HOME）")
}
fn config_dir() -> Result<PathBuf> {
    xdg_dir("XDG_CONFIG_HOME", ".config")
}
fn cache_dir() -> Result<PathBuf> {
    xdg_dir("XDG_CACHE_HOME", ".cache")
}
fn state_dir() -> Result<PathBuf> {
    xdg_dir("XDG_STATE_HOME", ".local/state")
}

/// shell 报回来的路径：`file://` 按 URI 解，纯色之类带 scheme 的一律拒绝。
fn shell_path(value: &str) -> Result<PathBuf> {
    let value = value.trim();
    ensure!(!value.is_empty(), "没有配置静态图片壁纸");
    if value.starts_with("file://") {
        return file_uri(value);
    }
    ensure!(!value.contains("://"), "当前壁纸不是图片文件（{value}）");
    Ok(PathBuf::from(value))
}

/// noctalia（Quickshell 版）的 IPC：多屏给 JSON 映射，单屏给裸路径。
fn noctalia_ipc_wallpaper() -> Result<PathBuf> {
    let output = command_stdout(
        "qs",
        &[
            "-c",
            "noctalia-shell",
            "ipc",
            "call",
            "wallpaper",
            "get",
            "",
        ],
    )?;
    noctalia_ipc_path(&output)
}

/// IPC 的两种回答：多屏是 `{"DP-3": "…"}`，单屏是裸路径。
fn noctalia_ipc_path(output: &str) -> Result<PathBuf> {
    if output.trim_start().starts_with('{') {
        let screens: std::collections::BTreeMap<String, String> =
            serde_json::from_str(output).context("解析 noctalia 的多屏壁纸映射")?;
        // BTreeMap 让「取哪块屏」在多次调用之间是确定的，否则上报会来回跳。
        return screens
            .values()
            .find(|path| !path.trim().is_empty())
            .context("noctalia 没有报告任何屏幕的壁纸")
            .and_then(|path| shell_path(path));
    }
    shell_path(output)
}

/// noctalia 的落盘状态：IPC 不可用时读它（写入有防抖，可能比屏幕慢一拍）。
fn noctalia_cache_wallpaper() -> Result<PathBuf> {
    let file = cache_dir()?.join("noctalia/wallpapers.json");
    let text = std::fs::read_to_string(&file).context("读取 noctalia 壁纸缓存")?;
    noctalia_cache_path(&text, noctalia_dark_mode())
}

/// 缓存的形状：`wallpapers[screen]` 是 `{light,dark}`（旧版本是裸字符串），
/// 空字符串那个键是「所有屏幕」的默认值。
fn noctalia_cache_path(text: &str, dark: bool) -> Result<PathBuf> {
    let cache: serde_json::Value = serde_json::from_str(text).context("解析 noctalia 壁纸缓存")?;
    let wallpapers = cache
        .get("wallpapers")
        .and_then(serde_json::Value::as_object)
        .context("缓存里没有壁纸表")?;
    let pick = |entry: &serde_json::Value| -> Option<String> {
        match entry {
            // 条目是 {"light":…,"dark":…}；旧版本直接写路径字符串。
            serde_json::Value::Object(slots) => [dark, !dark]
                .into_iter()
                .find_map(|wanted| {
                    slots
                        .get(if wanted { "dark" } else { "light" })
                        .and_then(serde_json::Value::as_str)
                })
                .map(ToOwned::to_owned),
            serde_json::Value::String(path) => Some(path.clone()),
            _ => None,
        }
    };
    // 空字符串键是「所有屏幕」的默认值，比某一块屏更稳。
    for key in std::iter::once("").chain(wallpapers.keys().map(String::as_str)) {
        if let Some(path) = wallpapers.get(key).and_then(pick) {
            return shell_path(&path);
        }
    }
    bail!("缓存里没有可用的壁纸")
}

/// noctalia 用浅色还是深色壁纸；设置读不到时按浅色找，找不到再换另一张。
fn noctalia_dark_mode() -> bool {
    let Ok(file) = config_dir().map(|dir| dir.join("noctalia/settings.json")) else {
        return false;
    };
    std::fs::read_to_string(file)
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .and_then(|settings| settings.get("colorSchemes")?.get("darkMode")?.as_bool())
        .unwrap_or(false)
}

/// noctalia v5（C++ 版）的 CLI；它没在跑时退回落盘状态。
fn noctalia_v5_wallpaper() -> Result<PathBuf> {
    match command_stdout("noctalia", &["msg", "wallpaper-get"]) {
        Ok(output) => shell_path(&output),
        Err(error) => noctalia_v5_state_wallpaper().context(format!("{error:#}")),
    }
}

/// noctalia v5 的 state 目录：`[wallpaper.last]` 是最近一次设置的那张。
fn noctalia_v5_state_wallpaper() -> Result<PathBuf> {
    let text = std::fs::read_to_string(state_dir()?.join("noctalia/settings.toml"))
        .context("读取 noctalia v5 状态")?;
    let mut section = "";
    let mut monitors = None;
    for line in text.lines() {
        let line = line.trim();
        if let Some(name) = line
            .strip_prefix('[')
            .and_then(|line| line.strip_suffix(']'))
        {
            section = name;
            continue;
        }
        if !section.starts_with("wallpaper.") {
            continue;
        }
        let Some(value) = line
            .strip_prefix("path")
            .map(str::trim_start)
            .and_then(|line| line.strip_prefix('='))
        else {
            continue;
        };
        let value = value.trim().trim_matches('"');
        if value.is_empty() {
            continue;
        }
        if section == "wallpaper.last" {
            return Ok(PathBuf::from(value));
        }
        monitors.get_or_insert_with(|| PathBuf::from(value));
    }
    monitors.context("noctalia v5 状态里没有壁纸路径")
}

/// caelestia 把当前壁纸的路径写在自己的状态目录里。
fn caelestia_wallpaper() -> Result<PathBuf> {
    let value = std::fs::read_to_string(state_dir()?.join("caelestia/wallpaper/path.txt"))
        .context("读取 caelestia 壁纸状态")?;
    shell_path(&value)
}

/// end-4 的 dots（illogical-impulse）把壁纸写在自己的 Quickshell 配置里。
fn illogical_impulse_wallpaper() -> Result<PathBuf> {
    let text = std::fs::read_to_string(config_dir()?.join("illogical-impulse/config.json"))
        .context("读取 illogical-impulse 配置")?;
    let config: serde_json::Value =
        serde_json::from_str(&text).context("解析 illogical-impulse 配置")?;
    let path = config
        .get("background")
        .and_then(|value| value.get("wallpaperPath"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    shell_path(path)
}

/// swww 在 `swww query` 里报每块屏当前显示的东西。
fn swww_wallpaper() -> Result<PathBuf> {
    swww_query_path(&command_stdout("swww", &["query"])?)
}

/// 每屏一行：`DP-1: 3792x1600, scale: 1.35, currently displaying: image: /x.png`。
/// 纯色那行是 `color: 1A1A2E`，没有路径。
fn swww_query_path(output: &str) -> Result<PathBuf> {
    let mut solid = false;
    for line in output.lines() {
        let Some((_, displayed)) = line.rsplit_once(", currently displaying: ") else {
            continue;
        };
        if let Some(path) = displayed.strip_prefix("image: ") {
            return Ok(PathBuf::from(path));
        }
        solid |= displayed.starts_with("color: ");
    }
    ensure!(!solid, "当前壁纸是纯色");
    bail!("swww 没有报告图片壁纸")
}

/// hyprpaper 0.7 的 IPC 报「屏幕 = 图」；0.8 换成自定义协议，只能读配置。
fn hyprpaper_wallpaper() -> Result<PathBuf> {
    if let Ok(output) = command_stdout("hyprctl", &["hyprpaper", "listactive"]) {
        for line in output.lines() {
            if let Some((_, path)) = line.split_once(" = ")
                && path.contains('/')
            {
                return Ok(PathBuf::from(path.trim()));
            }
        }
    }
    let text = std::fs::read_to_string(config_dir()?.join("hypr/hyprpaper.conf"))
        .context("读取 hyprpaper 配置")?;
    for line in text.lines() {
        let line = line.trim();
        // 0.7 是一行 `wallpaper = <monitor>,<路径>；0.8 的块里写 `path = <路径>`。
        let value = if let Some(value) = line.strip_prefix("wallpaper") {
            value.trim_start().strip_prefix('=').map(|value| {
                // 逗号后面才是路径，monitor 可以为空。
                value.rsplit(',').next().unwrap_or(value).trim()
            })
        } else {
            line.strip_prefix("path")
                .map(str::trim_start)
                .and_then(|line| line.strip_prefix('='))
                .map(str::trim)
        };
        if let Some(value) = value.map(|value| value.trim_matches('"'))
            && !value.is_empty()
        {
            return Ok(PathBuf::from(value));
        }
    }
    bail!("hyprpaper 配置里没有壁纸路径")
}

/// swaybg 与 wbg 没有查询接口，只能看它们在命令行里带着哪张图。
fn daemon_wallpaper() -> Result<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    let entries = std::fs::read_dir("/proc").context("读取 /proc")?;
    for entry in entries.flatten() {
        let Ok(pid) = entry.file_name().into_string() else {
            continue;
        };
        if pid.parse::<u32>().is_err() {
            continue;
        }
        let Ok(cmdline) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
            continue;
        };
        let args: Vec<&std::ffi::OsStr> = cmdline
            .split(|byte| *byte == 0)
            .filter(|arg| !arg.is_empty())
            .map(std::ffi::OsStr::from_bytes)
            .collect();
        let Some(program) = args
            .first()
            .map(std::path::Path::new)
            .and_then(std::path::Path::file_name)
            .map(|name| name.as_bytes())
        else {
            continue;
        };
        let image = match program {
            b"swaybg" => args
                .iter()
                .position(|arg| *arg == "-i" || *arg == "--image")
                .and_then(|index| args.get(index + 1)),
            b"wbg" => args
                .iter()
                .skip(1)
                .find(|arg| !arg.to_string_lossy().starts_with('-')),
            _ => continue,
        };
        if let Some(image) = image
            && !image.is_empty()
        {
            return Ok(PathBuf::from(image));
        }
    }
    bail!("没有找到带图片的 swaybg/wbg 进程")
}

/// GNOME 与 Budgie 的图片壁纸在 GSettings 里，深浅两套配色各一个键。
fn gnome_wallpaper() -> Result<PathBuf> {
    let dark = gsettings("org.gnome.desktop.interface", "color-scheme")
        .is_ok_and(|scheme| scheme == "prefer-dark");
    let keys = if dark {
        ["picture-uri-dark", "picture-uri"]
    } else {
        ["picture-uri", "picture-uri-dark"]
    };
    for key in keys {
        // 另一套配色没单独设过时，GNOME 用的是同一张图。
        if let Ok(path) = gsettings_uri("org.gnome.desktop.background", key)
            && !path.as_os_str().is_empty()
        {
            return Ok(path);
        }
    }
    bail!("org.gnome.desktop.background 里没有静态图片壁纸")
}

/// Cinnamon 用 GNOME 那套 schema 的 Cinnamon 版。
fn cinnamon_wallpaper() -> Result<PathBuf> {
    for key in ["picture-uri", "picture-uri-dark"] {
        if let Ok(path) = gsettings_uri("org.cinnamon.desktop.background", key)
            && !path.as_os_str().is_empty()
        {
            return Ok(path);
        }
    }
    bail!("Cinnamon 没有设置静态图片壁纸")
}

/// MATE 存的是裸路径，不是 `file://` URI。
fn mate_wallpaper() -> Result<PathBuf> {
    let value = gsettings("org.mate.background", "picture-filename")?;
    ensure!(!value.is_empty(), "MATE 没有设置静态图片壁纸");
    Ok(PathBuf::from(value))
}

/// XFCE 的 xfconf 里每屏每工作区一个 `last-image`。
fn xfce_wallpaper() -> Result<PathBuf> {
    let listed = command_stdout("xfconf-query", &["-c", "xfce4-desktop", "-l"])?;
    let mut keys: Vec<&str> = listed
        .lines()
        .map(str::trim)
        .filter(|key| key.ends_with("/last-image"))
        .collect();
    keys.sort_unstable();
    for key in keys {
        if let Ok(value) = command_stdout("xfconf-query", &["-c", "xfce4-desktop", "-p", key]) {
            let value = value.trim();
            if !value.is_empty() {
                return file_uri(value);
            }
        }
    }
    bail!("xfce4-desktop 里没有设置壁纸")
}

/// LXQt 把壁纸交给 pcmanfm-qt，值在它自己的 ini 里。
fn lxqt_wallpaper() -> Result<PathBuf> {
    let text = std::fs::read_to_string(config_dir()?.join("pcmanfm-qt/lxqt/settings.conf"))
        .context("读取 pcmanfm-qt 配置")?;
    let mut in_desktop = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_desktop = line.eq_ignore_ascii_case("[Desktop]");
            continue;
        }
        if in_desktop && let Some(value) = line.strip_prefix("Wallpaper=") {
            return file_uri(value.trim());
        }
    }
    bail!("pcmanfm-qt 配置里没有壁纸")
}

/// COSMIC 的背景服务一 key 一个 RON 文件，`all` 是「所有屏幕」的默认值。
fn cosmic_wallpaper() -> Result<PathBuf> {
    let dir = config_dir()?.join("cosmic/com.system76.CosmicBackground/v1");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .context("读取 COSMIC 背景配置")?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_file())
        .collect();
    files.sort();
    files.sort_by_key(|path| path.file_name() != Some(std::ffi::OsStr::new("all")));
    for file in files {
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        if let Some(path) = cosmic_source(&text) {
            return Ok(path);
        }
    }
    bail!("COSMIC 背景配置里没有图片")
}

/// `source: Path("/x.png")` 才是图片；`Color(` / `Gradient(` 不是。
fn cosmic_source(text: &str) -> Option<PathBuf> {
    let rest = text.split_once("source:")?.1.trim_start();
    let rest = rest.strip_prefix("Path(")?.trim_start().strip_prefix('"')?;
    let end = rest.find('"')?;
    Some(PathBuf::from(&rest[..end]))
}

pub(crate) fn wallpaper_image(source: &WallpaperSource) -> Result<Vec<u8>> {
    let maximum = crate::platform::wallpaper::MAXIMUM_FILE;
    let mut bytes = Vec::new();
    std::fs::File::open(&source.path)?
        .take(maximum + 1)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() as u64 <= maximum, "壁纸文件过大");
    // A GNOME slideshow is an XML file, not a picture.
    let jpeg = crate::platform::wallpaper::card(bytes).context("壁纸不是静态图片")?;
    ensure!(wallpaper_source()? == *source, "壁纸在读取期间已改变");
    Ok(jpeg)
}

/// 读壁纸要问的东西散在 dconf 的二进制库、各家 shell 的状态文件与命令行接口里，
/// 没有统一的读法；这里给一个带超时的只读查询，谁都能用。
const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);

fn command_stdout(program: &str, args: &[&str]) -> Result<String> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .with_context(|| format!("无法运行 {program}"))?;
    let deadline = std::time::Instant::now() + COMMAND_TIMEOUT;
    loop {
        match child.try_wait()? {
            Some(status) => {
                ensure!(
                    status.success(),
                    "{program} {} 没有给出结果",
                    args.join(" ")
                );
                break;
            }
            None if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20))
            }
            None => {
                let _ = child.kill();
                let _ = child.wait();
                bail!("{program} 超时");
            }
        }
    }
    let mut output = String::new();
    child
        .stdout
        .take()
        .with_context(|| format!("{program} 没有输出"))?
        .read_to_string(&mut output)?;
    Ok(output.trim().to_owned())
}

/// One GSettings string value, read with the `gsettings` tool: the settings
/// live in dconf's binary database, which has no stable file format to parse.
fn gsettings(schema: &str, key: &str) -> Result<String> {
    Ok(command_stdout("gsettings", &["get", schema, key])?
        .trim_matches('\'')
        .to_owned())
}

fn gsettings_uri(schema: &str, key: &str) -> Result<PathBuf> {
    file_uri(&gsettings(schema, key)?)
}

/// The path of a `file://` URI; any other scheme is not a local file.
fn file_uri(uri: &str) -> Result<PathBuf> {
    if uri.is_empty() {
        return Ok(PathBuf::new());
    }
    let Some(encoded) = uri.strip_prefix("file://") else {
        if uri.starts_with('/') {
            return Ok(PathBuf::from(uri));
        }
        bail!("壁纸必须是本机文件");
    };
    let mut bytes = Vec::with_capacity(encoded.len());
    let mut rest = encoded.as_bytes();
    while let Some((&byte, tail)) = rest.split_first() {
        if byte == b'%'
            && let Some(hex) = tail.get(..2)
            && let Ok(value) = u8::from_str_radix(std::str::from_utf8(hex)?, 16)
        {
            bytes.push(value);
            rest = &tail[2..];
        } else {
            bytes.push(byte);
            rest = tail;
        }
    }
    use std::os::unix::ffi::OsStringExt;
    Ok(PathBuf::from(std::ffi::OsString::from_vec(bytes)))
}

/// KDE Plasma's image wallpaper: the first desktop containment's `Image`.
fn plasma_wallpaper() -> Result<PathBuf> {
    let config = config_dir()?.join("plasma-org.kde.plasma.desktop-appletsrc");
    let text = std::fs::read_to_string(&config).context("读取 Plasma 桌面配置")?;
    plasma_image_path(&text)
}

/// Plasma 的图片壁纸写在 `[Containments][N][Wallpaper][org.kde.image][General]`
/// 的 `Image` 里，值可以带 `#dark` / `#light` / `#day-night` 后缀。
fn plasma_image_path(text: &str) -> Result<PathBuf> {
    let mut in_image = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_image = line.ends_with("[Wallpaper][org.kde.image][General]");
            continue;
        }
        if in_image && let Some(value) = line.strip_prefix("Image=") {
            let value = value.trim();
            return file_uri(value.split('#').next().unwrap_or(value));
        }
    }
    bail!("Plasma 桌面当前没有静态图片壁纸")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_graphics_from_pci_ids() {
        let database = "10de  NVIDIA Corporation\n\t2230  GA102GL [RTX A6000]\n\t2231  Other\n1002  Advanced Micro Devices, Inc. [AMD/ATI]\n\t73bf  Navi 21 [Radeon RX 6800/6800 XT / 6900 XT]\n8086  Intel Corporation\n\t4680  AlderLake-S GT1\n";
        assert_eq!(
            pci_name(database, 0x10de, 0x2230).as_deref(),
            Some("NVIDIA RTX A6000")
        );
        assert_eq!(
            pci_name(database, 0x1002, 0x73bf).as_deref(),
            Some("AMD/ATI Radeon RX 6800/6800 XT / 6900 XT")
        );
        assert_eq!(
            pci_name(database, 0x8086, 0x4680).as_deref(),
            Some("Intel AlderLake-S GT1")
        );
        // A device listed under another vendor is not borrowed.
        assert_eq!(pci_name(database, 0x10de, 0x73bf), None);
    }

    #[test]
    fn decodes_file_uris() {
        assert_eq!(
            file_uri("file:///usr/share/backgrounds/a%20b.png").unwrap(),
            PathBuf::from("/usr/share/backgrounds/a b.png")
        );
        assert_eq!(file_uri("").unwrap(), PathBuf::new());
        assert!(file_uri("https://example.com/a.png").is_err());
    }

    /// Needs a desktop session: `cargo test -- --ignored reads_this_machine`.
    #[test]
    #[ignore]
    fn reads_this_machine() {
        let hardware = Hardware::read().unwrap();
        println!(
            "name {}\nsystem {}\nmachine {}\nos {}\nboard {}\ncpu {}\nvideo {:?}\nmac {}\nmemory {} MiB\nscreen {}\nerrors {:?}",
            hardware.name,
            hardware.system_uuid,
            hardware.machine_guid,
            hardware.os,
            hardware.base_board,
            hardware.cpu,
            hardware.video,
            hardware.mac,
            hardware.memory,
            hardware.screen,
            hardware.errors
        );
        let source = wallpaper_source().unwrap();
        println!("wallpaper {}", source.path.display());
        let card = wallpaper_image(&source).unwrap();
        println!("card {} bytes", card.len());
    }

    #[test]
    fn reads_noctalia_ipc_answers() {
        // 多屏是 JSON 映射，键序不保证；单屏是裸路径。
        let multi = r#"{"DP-3":"/x/late.png","DP-1":"/x/early.png"}"#;
        assert_eq!(
            noctalia_ipc_path(multi).unwrap(),
            PathBuf::from("/x/early.png")
        );
        assert_eq!(
            noctalia_ipc_path("/x/only.png\n").unwrap(),
            PathBuf::from("/x/only.png")
        );
        // 纯色壁纸不是文件；空回答也不行。
        assert!(noctalia_ipc_path("solid://#1a1a2e").is_err());
        assert!(noctalia_ipc_path("").is_err());
    }

    #[test]
    fn reads_noctalia_cache_entries() {
        let cache = r#"{
            "defaultWallpaper": "/x/default.png",
            "wallpapers": {
                "": {"light": "/x/all.png", "dark": "/x/all-dark.png"},
                "DP-1": {"light": "/x/light.png", "dark": "/x/dark.png"},
                "DP-2": "/x/legacy.png"
            }
        }"#;
        // 「所有屏幕」那个键优先。
        assert_eq!(
            noctalia_cache_path(cache, true).unwrap(),
            PathBuf::from("/x/all-dark.png")
        );
        assert_eq!(
            noctalia_cache_path(cache, false).unwrap(),
            PathBuf::from("/x/all.png")
        );
        // 只有具体屏幕时按键名取第一块，旧格式的裸字符串也认。
        let screens = r#"{"wallpapers":{"DP-2":"/x/legacy.png","DP-1":{"dark":"/x/d.png"}}}"#;
        assert_eq!(
            noctalia_cache_path(screens, false).unwrap(),
            PathBuf::from("/x/d.png")
        );
    }

    #[test]
    fn reads_swww_query_output() {
        let output = "DP-1: 3792x1600, scale: 1.35, currently displaying: image: /x/a.jpg\n\
                      DP-2: 1920x1080, scale: 1, currently displaying: color: 1A1A2E";
        assert_eq!(swww_query_path(output).unwrap(), PathBuf::from("/x/a.jpg"));
        assert!(
            swww_query_path("DP-1: 1x1, scale: 1, currently displaying: color: 000000").is_err()
        );
    }

    #[test]
    fn strips_plasma_image_fragment() {
        let config = "[Containments][1][Wallpaper][org.kde.image][General]\nImage=/usr/share/wallpapers/Flow#dark\n";
        assert_eq!(
            plasma_image_path(config).unwrap(),
            PathBuf::from("/usr/share/wallpapers/Flow")
        );
        assert!(
            plasma_image_path("[Containments][1][Wallpaper][org.kde.slideshow][General]\n")
                .is_err()
        );
    }

    #[test]
    fn reads_cosmic_ron_sources() {
        let file = r#"( output: "all", source: Path("/x/bg.jpg"), filter_by_theme: true, )"#;
        assert_eq!(cosmic_source(file).unwrap(), PathBuf::from("/x/bg.jpg"));
        // 纯色与渐变不是图片。
        assert!(cosmic_source(r#"( source: Color(Single([0.1, 0.2, 0.3])), )"#).is_none());
        assert!(cosmic_source(r#"( source: Gradient([]), )"#).is_none());
    }
}
