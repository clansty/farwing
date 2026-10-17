# OpenUUYC

![OpenUUYC](assets/banner.png)

OpenUUYC 是用 Rust 编写的 UU 远程第三方客户端，支持 Windows 和 Linux，支持主控、被控和远程协助。

后续计划支持 Linux 等其他平台，移植工作请先通过 [Issue](https://github.com/djkcyl/openuuyc/issues) 沟通。

## 下载与使用

从 [Releases](https://github.com/djkcyl/openuuyc/releases) 下载 Windows x64 客户端，扫码或短信登录后连接设备，支持 UU 官方客户端；无需登录也可在登录页开启“接受远程协助”。

最新稳定版为 [v1.0.1](https://github.com/djkcyl/farwing/releases/tag/v1.0.1)。

最新测试版为 [v1.0.2-beta.5](https://github.com/djkcyl/farwing/releases/tag/v1.0.2-beta.5)，包含协助邀请链接、被控托盘提示、观看窗口剪贴板开关，以及网络参数、单轨切屏和文件时间修正。

接收测试版更新提醒：在“关于”中开启“允许测试版”（默认关闭）。

**从 alpha.6 或更早版本升级需重新登录，设备 ID 会改变，旧设备条目需手动删除。**

## 能力表

功能取决于双方能力与权限；Linux x64 需自行构建，两个平台的逐项差异见[功能矩阵](FEATURES.md)。

| 功能 | 主控端 | 被控端 | 说明 |
| --- | --- | --- | --- |
| 同账号远程连接 | 支持同时连接多台设备 | 支持 | 本机在连接设置中开启“允许被控” |
| 设备 ID / 验证码协助 | 支持主动连接，也能直接点开邀请链接 | 支持账号及游客协助 | 随机 / 自定义密码、本机确认；主控支持最近连接和收藏，对方分享的 `uuremote://external/assistance?...` 链接会拉起本程序并填好设备 ID 与验证码 |
| 画面传输 | 接收与播放 | 采集与编码 | H.264 / H.265 / AV1、真彩及 HDR，可调画质、码率和帧率；AV1 与 SDR 自动 10 位需双方 OpenUUYC 支持 |
| 编解码 | DXVA11 硬解 / Rust 软解 | NVENC、AMF、QSV 硬编 / Rust H.264 软编 | 可选格式与显卡首选；软解支持 H.264 / AV1，整个客户端最多一个软解播放窗口 |
| 键鼠与触摸 | 键盘、鼠标控制 | 键鼠及移动端原生触摸接收 | 相对/绝对鼠标、组合键、光标同步；可选 125 / 500 / 1000 Hz 鼠标节流（默认关闭） |
| 多显示器与显示设置 | 切屏、多窗口、本地比例拉伸、分辨率及 DPI 调整 | 多屏采集与设置接收 | 屏幕标签可拖出独立窗口；本地比例拉伸不修改远端分辨率，物理设置按目标支持的配置应用 |
| 虚拟屏、超级屏与无屏接管 | 支持操作远端 | 支持 | 需安装显示驱动；自动兜底屏断线保留，其他可用屏幕接入后回收 |
| 桌面声音 | 播放与远程音质调整 | 采集与发送 | 默认或指定设备，设备变化后恢复；64 / 128 / 192 / 256 kbps，远程调音质需双方 alpha.6+ |
| 仅音频连接 | 支持 | 支持 | 默认 256 kbps，提供波形、电平、网络 RTT、精简模式，可恢复画面 |
| 麦克风与虚拟声卡 | 选择麦克风、调整音质并发送 | 虚拟扬声器及远程麦克风接收 | 被控端需选装虚拟音频驱动，可配置默认音频设备切换策略 |
| 剪贴板与文件拖放 | 支持 | 支持 | 文字、富文本、图片及文件复制；OpenUUYC 双端支持双向拖放，与官方端连接时使用其支持的发送方式 |
| 独立文件传输 | 支持 | 支持官方 Windows / macOS 收发 | 目录浏览、收发、文件管理及暂停续传；被控端需登录 Windows |
| 端口转发 | 支持 | 支持 | TCP 与官方互通；UDP 需两端 OpenUUYC，复用可靠通道；被控需开启许可 |
| 批注与指示工具 | 支持发送 | 支持接收 | 画笔、形状、撤销重做、白板、激光笔和鼠标指示；被控端需登录 Windows |
| 远程电源操作 | 开机、关机和重启 | 关机、重启、远程开机设置与局域网唤醒协助 | 提供有线网卡检查/配置；WoL 需 BIOS 开启及同网协助设备或已配置的 UU 路由器 |

被控通知可选程序浮窗或 Windows 通知，支持连接确认、断开及文件收发提醒。

向官方被控端拖入文件，需开启键鼠控制、剪贴板同步和文件复制。

另支持设备管理、自定义快捷键、本机诊断、性能监控、异常图标及[插件和节点图](plugins/README.md)。

全屏观看时，悬停顶部横条可展开操作栏并退出全屏，移开后自动收起；游戏鼠标锁定时需先释放键鼠控制。

支持本机导出及远端获取脱敏诊断包（ZIP / zstd，最多 32 MiB）；远端获取需双方 beta.3+，并开启被控端文件访问权限。

## 安装与服务

主页“安装服务”会将程序安装到 `Program Files\OpenUUYC`，安装后台服务、输入和显示驱动，创建快捷方式并配置托盘自启动。安装需管理员授权及驱动证书信任；关闭主窗口进入托盘，托盘“退出”结束运行。

“连接设置 → 启动 → 开机自启”控制登录 Windows 后是否在托盘启动：安装服务后指向已安装的程序，未安装时指向当前运行的程序。后续更新保留关闭状态；若 Windows 已禁用自启动，页面会提示并提供系统启动设置入口。关闭它不影响后台服务，已授权并允许被控的常驻服务仍可在开机后接入。

安装服务并开启被控后，支持锁屏、PIN 界面及重启后未登录 Windows 时接入。

“安装服务”的确认对话框里可勾选「用 OpenUUYC 打开 UU 远程协助邀请链接」：勾上后把本用户的 `uuremote://` 协议登记给本程序，别人分享的邀请链接就会直接拉起 OpenUUYC 并填入设备 ID 和验证码。登记在安装成功后指向已安装程序，也可在连接设置中更改。只写本用户注册表，不动官方客户端的 HKLM 登记；接管前备份已有的 HKCU 登记，取消或卸载时恢复。若其他程序后来接管，则保留其设置。链接里的 `controlledLogin` 只描述被协助端当时的登录状态，不影响连接方式。

更新时运行新版 EXE，点击“更新并打开”即可更新已有安装。虚拟声卡在“连接设置 → 服务管理”中选装；卸载可选择保留虚拟显示驱动、虚拟声卡及用户数据。

### 虚拟音频驱动的使用条件

**虚拟音频驱动使用测试签名，尚未取得 Microsoft 正式签名。** 使用虚拟扬声器或接收远程麦克风，需要手动准备测试启动环境；普通画面、键鼠和桌面声音不需要此设置。

- 临时测试：Shift + 重启 → 疑难解答 → 高级选项 → 启动设置 → 重启 → **7 / F7 禁用驱动程序强制签名**，仅本次启动有效。[Windows 说明](https://support.microsoft.com/en-us/windows/experience/startup-boot/windows-startup-settings)
- 持续测试：管理员执行 `bcdedit /set testsigning on` 后重启；恢复时执行 `bcdedit /set testsigning off` 并重启。Secure Boot、BitLocker 或组织策略可能限制操作。[微软说明](https://learn.microsoft.com/en-us/windows-hardware/drivers/install/the-testsigning-boot-configuration-option)

测试启动会降低驱动加载保护，程序不会自动修改这些设置。

## Linux 版差异

Linux 版可以登录、管理设备、观看和控制远端桌面，界面走 wgpu（Vulkan，缺失时回退 OpenGL），X11 与 Wayland 均可运行。本机被控在 Xorg 与 Wayland 会话里都能跑，两者的采集与注入方式不同。与 Windows 版相比：

- **解码**：H.264 与 H.265 通过 VA-API 硬件解码（H.264 为 Constrained Baseline、Main、High，H.265 为 Main，都是 8 位 4:2:0），其余 H.264 格式与 AV1 使用 Rust 软件解码；H.265 没有软件回退，驱动不提供该 profile 的 VLD 时不对对端声明这一编码。AV1 软解与 H.265 硬解都只声明 8 位 4:2:0（播放窗口按 NV12 绘制），10 位与 4:4:4 不向对端声明。双方都有硬件编解码时协商优先选硬件。硬件解码需要对应的 VA-API 驱动：NVIDIA 上是 `libva-nvidia-driver`（Ubuntu 为 `nvidia-vaapi-driver`），libva 2.15 以前认不出 NVIDIA 显卡对应的驱动，程序会自行指定，无需设置 `LIBVA_DRIVER_NAME`。编解码首选中的解码方式与编码方式（硬件优先、仅硬件、仅软件）可用，但不能指定显卡：VA-API 与 NVENC 都使用桌面所在的显卡，显卡一栏显示为“自动”。
- **剪贴板**：主控与被控两端都支持文字、图片与文件双向同步。粘贴远端复制的文件时，文件通过挂载在 `$XDG_RUNTIME_DIR` 下的只读 FUSE 文件系统按需读取，需要安装 `fuse3`。文件拖放在 Xorg 会话中可用，经 XDND 协议完成：作为主控，本地文件可拖进播放窗口（OpenUUYC 对端为实时拖放，官方被控为松手后投放），也可从 OpenUUYC 被控的桌面拖出到本地桌面，官方被控用队列浮窗发来的文件保存到下载目录；作为被控，主控拖入的文件在落点处投放给本机程序，向官方 Windows 主控发送文件时，在桌面上拖动文件会于屏幕顶部弹出待发送队列浮窗。远端文件以 FUSE 挂载的路径交给目标程序，读取时才按需传输；目标读完全部文件即视为完成，五分钟未读取则停止。Wayland 会话下：被控端把本机文件拖出到主控需要合成器提供 wlr-layer-shell（sway、Hyprland、niri 等 wlroots 系都有，被控端借此接住这次拖动；GNOME 的 Mutter 没有，在它上面拖出不了）；主控一侧的拖入与拖出经自建的 `wl_data_device` 也能用（只接 Wayland 原生程序的拖动，XWayland 侧不桥接）；被控接收主控拖入的文件与待发送队列浮窗在 Wayland 会话中仍不支持（提示“Wayland 桌面暂不支持拖放文件”）。拖出到本地或远端后不能再拖回原窗口接续原拖动（Windows 版经 OLE 支持），Linux 端不向对端声明这项能力，两端都把拖出视为结束。拖起来跟着指针的那张预览图由被控端自己拼：是图片就用它的缩略图，其余按文件类型从图标主题里取，选了好几个就错开叠两张；需要 gio（glib2）与 rsvg-convert（librsvg），两者取不到就不带图，主控那边显示它自己的图标。对端要能收这张图才会有预览，上游 OpenUUYC 的 Windows 版尚未支持这项，拖到它那儿时没有预览图；Linux 作为主控时不显示对端发来的预览图（X11 拖放没有拖动预览图）。
- **被控文件传输与电源**：独立文件传输在本机桌面用户身份下运行，路径为 Unix 路径，根目录列出 `/` 与 xdg 用户目录（文档、桌面、下载等），不允许经过符号链接。远程关机与重启经 systemd-logind 执行，受 polkit 策略约束（有其他用户登录时可能需要管理员授权）。远程开机的网络登记、局域网唤醒协助与网卡检查可用（检查只读：唤醒权限取自 sysfs，魔术包状态需安装 `ethtool`）；配置网卡需要 root，程序内不提供，请用 `sudo ethtool -s <网卡> wol g` 设置；开启远程开机时启用登录自启（见下）。
- **本机被控**：在已登录的桌面内运行。画面采集按 Sunshine 的优先级选择：Xorg 下先用 NVIDIA NvFBC（画面直接抓进显存交给 NVENC，仅在 NVENC 可用时选用），再到 X11 MIT-SHM，最后是 XDG 屏幕共享门户（PipeWire）；Wayland 下先用合成器自己的 wlroots screencopy（niri、Sway、Hyprland 等都提供，不需要任何授权；合成器只在实际有变化时交付画面），再到门户。门户第一次使用时需要在本机屏幕上点“共享”，授权会被记住（`~/.local/share/OpenUUYC/screencast-restore-token`，在系统隐私设置中可撤销）；还没授权、而这次又只有门户能采（合成器不提供 wlroots screencopy）时，登录与启动不探测本机编码能力、也不开采集，免得把授权窗弹在登录过程里——被控设置的编码列表在第一次被控连接后才按实际能力显示；可用环境变量 `OPENUUYC_CAPTURE`（如 `nvfbc`、`wlr`、`x11,portal`）指定顺序。编码优先用硬件：NVIDIA 上为 NVENC（H.264/H.265，4:2:0 与 4:4:4，8 位；AV1 为 4:2:0 8 位，需要带 AV1 编码单元的显卡，尚未在这类显卡上实测），AMD 与 Intel 上为 VA-API（H.264 / H.265 / AV1 按驱动能力逐项试编后申报，4:2:0 8 位），硬件不可用时回退 Rust H.264 软件编码（没有 HDR 采集，因此没有 10 位）；桌面声音参照 Sunshine 经 PulseAudio 客户端接口录制所选播放设备的监听源（PulseAudio 与 PipeWire 均可）；该设备静音或音量为 0 时可能录不到声音；键鼠经 uinput 创建虚拟设备注入（`/dev/uinput` 需要写权限，发行包会装好 `/usr/lib/udev/rules.d/70-openuuyc-uinput.rules`（`uaccess`，即当前登录会话的用户才有权限，不倚赖 input 组）；Wayland 会话与锁屏下都可操作，鼠标走绝对定位设备，移动端触摸：Wayland 会话下触点交给一台虚拟多点触摸屏（与键鼠同一套 `/dev/uinput` 权限，不需要额外配置），双指滚动、捏合与手势都是真的，不再是把一个触点翻成鼠标左键；合成器布局读不到、主控看的不止一块屏、或者屏幕被转过时退回单指指针模拟（模拟走绝对指针，落点总是对的；合成器怎么把触摸屏认到哪块屏上各家不一样，猜错的代价是点错屏，所以这几件事先不猜）。Xorg 会话仍按单指指针模拟；文字输入按本机键位映射，字符得是本地布局能打出来的，非 us 布局需自行确认），拿不到权限时回退 X11 XTest（无需任何权限，按物理键位映射，文字输入不依赖键盘布局，但只在 Xorg 会话内有效）；可用环境变量 `OPENUUYC_INPUT`（`uinput`、`xtest`、`auto`）指定，默认 `auto` 只在 Wayland 会话建 uinput，Xorg 会话直接走 XTest；显式写 `OPENUUYC_INPUT=uinput` 时仍会建 uinput；支持物理多屏与通过 RandR 切换分辨率，退出时恢复。暂无 10 位与 HDR、KMS 采集、超级屏、无显示器兜底屏、按显示器 DPI、远端麦克风与调整默认音频设备（OpenUUYC Audio 虚拟声卡是 Windows 驱动；Linux 上开启时会提示暂不支持），也没有 Windows 那样的机器级后台服务，登录自启只是一个随图形会话起停的用户服务，因此无法在登录界面被控（锁屏时的键鼠输入可以送达，但画面无法采集）。虚拟屏目前只在 Wayland + niri 下能加，要内核 ≥ 6.19 并启用 `openuuyc-vkms.socket`；采集到编码的零拷贝只对 AMD / Intel 的 VA-API 成立（NVENC 还要在显存里多拷一次，软件编码要过系统内存）。逐项见[功能矩阵](FEATURES.md)。
- **息屏与休眠**：本机画面被采集期间经 systemd-logind 同时挡住自动息屏与待机（`idle:sleep`）；观看远端时只挡自动息屏（`idle`），机器仍会按空闲策略待机。锁随会话结束释放；logind 不可用时只记录警告、照常连接，屏幕可能按空闲策略熄灭（Windows 版此时拒绝采集）。开始采集时会点亮已熄灭的显示器（GNOME 经 Mutter、KDE / Xfce 等经 `SimulateUserActivity`、X11 经 DPMS），不注入模拟输入；Hyprland 等只走 Wayland 且无上述接口的合成器无法点亮。
- **访问通知**：「系统通知」走桌面的 freedesktop 通知服务（GNOME、KDE 等），允许、拒绝、查看按钮直接回到正在运行的程序；「程序浮窗」在 X11 下放在主显示器右下角，Wayland 下由合成器决定位置。
- **设备资料**：上报本机真实硬件，取自主机名、`/etc/os-release` 与内核版本、`/proc`、DMI 主板信息、PCI 显卡（按 `pci.ids` 命名）和默认路由所在网卡。SMBIOS 系统 UUID 只有 root 能读，因此系统标识由 `/etc/machine-id` 派生，重装系统后视为新设备。壁纸按“正在运行的 shell 或壁纸守护进程 → 当前桌面环境自己的配置 → 其它 shell 的落盘状态”依次尝试：noctalia（IPC，退回落盘状态）、caelestia、illogical-impulse、swww、hyprpaper、swaybg/wbg、KDE Plasma、GNOME 与 Budgie、Cinnamon、MATE、XFCE、LXQt、COSMIC；一路都读不到时诊断页会列出试过哪些来源以及各自的原因，不会拿别的桌面留下的旧设置顶替。
- **安装与托盘**：`openuuyc.desktop` 登记了 `x-scheme-handler/uuremote`，因此协助邀请链接也能交给本程序打开，但默认打开方式由用户决定（`xdg-mime default openuuyc.desktop x-scheme-handler/uuremote`），安装包不会替用户抢走官方客户端或 wine 里的登记。没有 Windows 那样的“安装服务”，直接运行构建出的程序。关闭窗口隐藏到托盘（StatusNotifierItem；GNOME 需 AppIndicator 扩展，Ubuntu 默认启用），桌面没有托盘时关闭窗口即退出；面板比程序晚起来时每 2 秒重试，托盘一出现就改回关闭收进托盘。登录自启是一个 systemd 用户服务：发行包把 `openuuyc.service` 装到 `/usr/lib/systemd/user/`（`WantedBy=graphical-session.target`，所以它跟着图形会话起停，也没有“登录前被控”），用 `systemctl --user enable openuuyc.service` 开启；便携运行时程序会按当前可执行文件在 `~/.config/systemd/user/` 写一份同名单元。桌面到不了 `graphical-session.target` 时（XFCE、MATE、Cinnamon、普通 X 会话，以及没有 systemd 的发行版），开关改写 XDG 自启动条目 `~/.config/autostart/openuuyc.desktop`，由桌面的会话管理器启动，在桌面的“启动应用程序”里停用它开关也显示为关闭；两种只会留一种，关闭开关时两种都删。直接启动、既不接这个 target 也不处理 XDG 自启动的合成器（没有 uwsm 或 dex 的 sway、Hyprland 等）两种都不会被拉起，请在合成器配置里自行启动 `OpenUUYC gui --background`。它以 `gui --background` 运行，完全不建窗口也不建绘制表面，只在托盘里；再点一次图标会把已经在跑的实例叫到前台：启动器给出的启动通知令牌（Wayland 的 `XDG_ACTIVATION_TOKEN`、X11 的 `DESKTOP_STARTUP_ID`）随请求转交，用来把窗口提到前台并结束启动反馈，合成器或窗口管理器仍可能按自己的防抢焦点策略只做提示。被控设置里的「开机自启」开关直接读写这个单元的启用状态，与 `systemctl --user enable/disable` 完全等价。`systemctl --user stop`（以及 SIGTERM）走的是正常退出，本机在线状态会被正常撤销而不是被直接杀掉；服务模式下日志同时进 journal：`journalctl --user -u openuuyc`。
- **未接入**：批注与白板（主控发送和被控接收；被控收到批注请求会拒绝）、播放器侧的插件效果链（插件管理页与节点图编辑器能打开，但没有执行链）、多显示器独立窗口、HDR、全局快捷键（快捷键仅在播放窗口获得焦点时生效），以及诊断页中的解码检查。
- **凭据**：登录态保存在系统密钥环（Secret Service），需要运行 gnome-keyring、KWallet 等服务；没有明文回退。

Wayland 下窗口的拖动与缩放由合成器接管，因此不支持窗口吸附等 Windows 专有行为。

## 构建

### Windows

需要 Rust stable（MSVC）、Visual Studio C++ 构建工具、Windows SDK、CMake 和 NASM，命令行工具需加入 PATH。H.264 软件编解码和 AV1 软件解码使用项目 Rust 核心及 SIMD 汇编。

```powershell
git clone https://github.com/djkcyl/openuuyc.git
cd openuuyc
cargo dist
```

产物为 `target/dist/` 下的单文件 EXE。仅构建原生 EXE 可用 `cargo dist --native`；命令行参数见 `--help`。

源码已包含签名驱动包和公钥证书，构建主程序不需要签名私钥。修改驱动及发布前验证见 [驱动构建说明](drivers/README.md)。

### Linux

需要 Rust stable（edition 2024）与 C/C++ 工具链。Ubuntu 22.04 及以上：

```bash
sudo apt install build-essential cmake clang nasm pkg-config libva-dev \
    libasound2-dev libdbus-1-dev libxkbcommon-dev libxkbcommon-x11-dev \
    libwayland-dev libx11-dev libxcb1-dev libxrandr-dev libxi-dev libxcursor-dev \
    libgl1-mesa-dev libvulkan-dev libudev-dev libssl-dev fonts-noto-cjk fuse3
git clone https://github.com/djkcyl/openuuyc.git
cd openuuyc
cargo build --release
./target/release/OpenUUYC gui
```

`cargo dist` 只用于 Windows 打包，Linux 直接用 `cargo build`。

打包为 deb（GitHub Actions 的 “Linux deb” 工作流在 Ubuntu 22.04 上构建同样的包，产物见运行页面的 Artifacts，打 `v*` 标签时附到 Release）：

```bash
cargo install cargo-deb
cargo build --release --bin OpenUUYC
cargo deb --no-build
sudo apt install ./target/debian/openuuyc_*.deb
```

打包为 Arch 包（`packaging/arch/PKGBUILD`，用 makepkg 从当前工作树增量编译，不重新拉依赖）：

```bash
./packaging/arch/build.sh       # 产物在 target/arch/，附一个 .sha256
./packaging/arch/build.sh -i    # 顺手 sudo pacman -U 装上
```

包名是 `openuuyc-clansty`：它 provides `openuuyc`，并与上游
[DoyoDia/openuuyc-linux](https://github.com/DoyoDia/openuuyc-linux) 的 AUR 包
`openuuyc-bin` / `openuuyc-git` 互相 conflicts——三者装的是同一套文件，装重了会互相顶掉，
所以装本包前要先卸掉那两个（反过来也一样）。

需要 `base-devel`；选项与环境变量见 `./packaging/arch/build.sh --help`。

安装后从应用菜单启动，或运行 `OpenUUYC`。

## 命令行更新

从新下载的程序包运行 `OpenUUYC.exe update --silent`，静默更新已有安装。会中断当前连接，完成后不自动打开控制中心或重启 Windows；非管理员运行时仍需确认系统 UAC。

无人值守脚本使用 `update --silent --no-elevate`，由安装所属用户的管理员终端执行；权限不足返回 740，不弹 UAC。退出码：0 成功、3010 需重启、170 已有安装操作、1605 未安装、2404 驱动占用，其他失败为 1。不要同时保留其他更新窗口。

## 反馈与许可

问题和建议请提交到 [Issues](https://github.com/djkcyl/openuuyc/issues)。报告问题时附上双方版本、操作步骤、大致发生时间及导出的诊断包，分享前检查是否含私人信息。

OpenUUYC 非网易官方项目。源码公开，但项目整体未采用开源许可证，使用与分发条件见 [LICENSE](LICENSE)。第三方及派生代码保留各自的许可，详见 [THIRD_PARTY_NOTICES](THIRD_PARTY_NOTICES)。

## 鸣谢

感谢以下团队、项目和贡献者为 OpenUUYC 提供的参考、技术基础与开发支持：

- [网易 UU 远程](https://uuyc.163.com/)：本项目的协议兼容目标，也是远程连接功能与交互行为的参考。
- [SudoVDA](https://github.com/SudoMaker/SudoVDA) 与 [Microsoft Windows Driver Samples](https://github.com/microsoft/Windows-driver-samples)：虚拟显示驱动的源码基础，以及虚拟音频驱动的示例参考。
- [FFmpeg](https://ffmpeg.org/)：视频编解码基础算法及硬件解码适配的重要来源。
- [rav1d](https://github.com/memorysafety/rav1d) / dav1d：AV1 软件解码核心及 SIMD 实现。
- [Cisco OpenH264](licenses/openh264-algorithms.txt)：软件编码器的码率控制、屏幕变化检测与运动搜索等算法参考。
- [WebRTC](https://webrtc.googlesource.com/src/+/refs/branch-heads/5481/)、[WebRTC-rs](https://github.com/webrtc-rs/webrtc) 与 [goog_cc](https://github.com/kixelated/goog_cc)：实时传输、音频接收处理与拥塞控制基础。
- Opus / [SpeexDSP](src/media/audio/COPYING.SpeexDSP)：音频编解码与重采样算法。
- egui / [egui-directx11](https://github.com/NekomaruQwQ/egui-directx11)：图形界面与渲染基础。
- [OpenAI](https://openai.com/)：Codex 为本项目的开发提供 AI 编程辅助。
- [sisi0318](https://github.com/sisi0318)：提供两个 Codex x20 账号，支持本项目开发。
- ~~[Tibo（Thibault Sottiaux）](https://x.com/thsottiaux)：感谢他与 Codex 团队提供的额外额度重置。~~

也感谢[项目贡献者](https://github.com/djkcyl/openuuyc/graphs/contributors)及提供测试反馈和改进建议的用户。完整的第三方来源与许可说明见 [THIRD_PARTY_NOTICES](THIRD_PARTY_NOTICES)。
