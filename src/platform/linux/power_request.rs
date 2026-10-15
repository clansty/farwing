//! 采集和查看期间别让机器睡过去，顺便在被控端熄屏后把屏点回来。
//!
//! Windows 用 `PowerCreateRequest` 拿进程级的电源请求；Linux 走 systemd-logind 的
//! 阻止锁：`idle` 让会话不算空闲（合成器/锁屏就不会关屏），`sleep` 挡住待机。
//! 锁挂在返回的那个 fd 上，fd 一关（进程退出也算）logind 自己就放掉，所以这里只要
//! 把它握着。拿不到锁不是致命的——调用方只在日志里说一句。
//!
//! 采集开始时还要把已经熄掉的屏点回来，否则主控看到的是一块黑屏。点屏不合成输入，
//! 几种途径挨个试：GNOME 的省电模式只能走 Mutter 设回 on（重置 X11 屏保没用）；
//! KDE、Xfce、xscreensaver 等认会话总线上的 `SimulateUserActivity`（GNOME 会回
//! NotSupported）；X11 下再重置内核屏保并把被 DPMS 关掉的显示器强行打开。
use anyhow::{Context, Result};
use zbus::blocking::{Connection, Proxy};
use zbus::zvariant::{OwnedFd, OwnedValue, Value};

/// 一把阻止锁。它活着的时候，机器不会因为我们没在动它就睡过去。
pub(crate) struct Request {
    /// logind 的约定：fd 关掉，锁就没了。
    _lock: OwnedFd,
}

impl Request {
    /// 被控端在采集：显示器要亮着，机器也别待机；顺带把已经熄掉的屏点回来。
    pub(crate) fn capture() -> Result<Self> {
        let request = Self::inhibit("idle:sleep", "OpenUUYC 正在采集远程桌面");
        wake_display();
        request
    }

    /// 主控端在看：只要别关屏，机器该睡还是睡。
    pub(crate) fn viewing() -> Result<Self> {
        Self::inhibit("idle", "OpenUUYC 正在查看远程桌面")
    }

    fn inhibit(locks: &str, what: &str) -> Result<Self> {
        let connection = Connection::system().context("连接系统总线失败")?;
        let manager = Proxy::new(
            &connection,
            "org.freedesktop.login1",
            "/org/freedesktop/login1",
            "org.freedesktop.login1.Manager",
        )
        .context("systemd-logind 不可用")?;
        let lock: OwnedFd = manager
            .call("Inhibit", &(locks, "OpenUUYC", what, "block"))
            .with_context(|| format!("申请阻止锁失败（{locks}）"))?;
        Ok(Self { _lock: lock })
    }
}

/// 会话总线上的 `org.freedesktop.ScreenSaver`，唤醒走它的 `SimulateUserActivity`。
fn screensaver(connection: &Connection) -> Result<Proxy<'static>> {
    Proxy::new(
        connection,
        "org.freedesktop.ScreenSaver",
        "/org/freedesktop/ScreenSaver",
        "org.freedesktop.ScreenSaver",
    )
    .context("桌面未提供 org.freedesktop.ScreenSaver 接口")
}

/// 把熄掉的屏点回来。每种途径挨个试，桌面没提供哪一种就跳过哪一种；都不成也不算
/// 失败——没有 X 服务器的 Hyprland 之类根本没有可用的唤醒途径。
fn wake_display() {
    let mut woken = Vec::new();
    if let Ok(connection) = Connection::session() {
        match wake_mutter(&connection) {
            Ok(true) => woken.push("Mutter"),
            Ok(false) => {}
            Err(error) => {
                tracing::debug!(error = %format!("{error:#}"), "Mutter display wake failed")
            }
        }
        if screensaver(&connection)
            .and_then(|proxy| Ok(proxy.call::<_, _, ()>("SimulateUserActivity", &())?))
            .is_ok()
        {
            woken.push("SimulateUserActivity");
        }
    }
    if !super::capture::wayland_session() {
        match wake_x11() {
            Ok(true) => woken.push("X11 DPMS"),
            Ok(false) => {}
            Err(error) => tracing::debug!(error = %format!("{error:#}"), "X11 display wake failed"),
        }
    }
    tracing::debug!(?woken, "capture display wake requested");
}

/// GNOME 熄屏走 Mutter 的省电模式，只有 Mutter 自己能点回来。
fn wake_mutter(connection: &Connection) -> Result<bool> {
    const INTERFACE: &str = "org.gnome.Mutter.DisplayConfig";
    let properties = Proxy::new(
        connection,
        INTERFACE,
        "/org/gnome/Mutter/DisplayConfig",
        "org.freedesktop.DBus.Properties",
    )?;
    let Ok(mode) = properties.call::<_, _, OwnedValue>("Get", &(INTERFACE, "PowerSaveMode")) else {
        return Ok(false);
    };
    if i32::try_from(mode).is_ok_and(|mode| mode == 0) {
        return Ok(false);
    }
    properties
        .call::<_, _, ()>("Set", &(INTERFACE, "PowerSaveMode", Value::from(0i32)))
        .context("设置 Mutter 显示电源模式失败")?;
    Ok(true)
}

fn wake_x11() -> Result<bool> {
    use x11rb::connection::{Connection as _, RequestConnection as _};
    use x11rb::protocol::dpms::{self, ConnectionExt as _, DPMSMode};
    use x11rb::protocol::xproto::{ConnectionExt as _, ScreenSaver};
    let (connection, _) = x11rb::connect(None).context("连接 X11 显示失败")?;
    // 重置内核屏保并重新计时；它不会把已经断电的显示器点回来。
    connection.force_screen_saver(ScreenSaver::RESET)?.check()?;
    let mut woken = false;
    if connection
        .extension_information(dpms::X11_EXTENSION_NAME)?
        .is_some()
    {
        let info = connection.dpms_info()?.reply()?;
        if info.state && info.power_level != DPMSMode::ON {
            connection.dpms_force_level(DPMSMode::ON)?.check()?;
            woken = true;
        }
    }
    connection.flush()?;
    Ok(woken)
}

#[cfg(test)]
mod tests {
    /// 把本机显示器熄一下再唤醒。需要桌面会话总线与 `DISPLAY`：GNOME 上走 Mutter，
    /// X11 上还会用 `xset` 走一遍 DPMS。
    #[test]
    #[ignore]
    fn capture_wakes_a_blanked_display() {
        use zbus::zvariant::{OwnedValue, Value};
        let connection = zbus::blocking::Connection::session().expect("session bus");
        let properties = zbus::blocking::Proxy::new(
            &connection,
            "org.gnome.Mutter.DisplayConfig",
            "/org/gnome/Mutter/DisplayConfig",
            "org.freedesktop.DBus.Properties",
        )
        .unwrap();
        const INTERFACE: &str = "org.gnome.Mutter.DisplayConfig";
        let mutter = || -> Option<i32> {
            let mode: OwnedValue = properties.call("Get", &(INTERFACE, "PowerSaveMode")).ok()?;
            i32::try_from(mode).ok()
        };
        let pause = || std::thread::sleep(std::time::Duration::from_millis(500));
        if mutter().is_some() {
            properties
                .call::<_, _, ()>("Set", &(INTERFACE, "PowerSaveMode", Value::from(3i32)))
                .unwrap();
            pause();
            assert_eq!(mutter(), Some(3));
            let request = super::Request::capture().expect("inhibit");
            pause();
            let woken = mutter();
            drop(request);
            println!("Mutter power-save mode after capture: {woken:?}");
            assert_eq!(woken, Some(0));
        }
        let xset = |args: &[&str]| {
            std::process::Command::new("xset")
                .args(args)
                .output()
                .ok()
                .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
        };
        if !crate::platform::linux::capture::wayland_session() && xset(&["q"]).is_some() {
            xset(&["dpms", "force", "off"]);
            pause();
            assert!(xset(&["q"]).unwrap().contains("Monitor is Off"));
            let request = super::Request::capture().expect("inhibit");
            pause();
            let state = xset(&["q"]).unwrap();
            drop(request);
            println!(
                "X11 DPMS after capture: on = {}",
                state.contains("Monitor is On")
            );
            assert!(state.contains("Monitor is On"));
        }
    }
}
