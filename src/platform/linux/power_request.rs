//! 采集和查看期间别让机器睡过去。
//!
//! Windows 用 `PowerCreateRequest` 拿进程级的电源请求；Linux 走 systemd-logind 的
//! 阻止锁：`idle` 让会话不算空闲（合成器/锁屏就不会关屏），`sleep` 挡住待机。
//! 锁挂在返回的那个 fd 上，fd 一关（进程退出也算）logind 自己就放掉，所以这里只要
//! 把它握着。拿不到锁不是致命的——调用方只在日志里说一句。
use anyhow::{Context, Result};
use zbus::blocking::{Connection, Proxy};
use zbus::zvariant::OwnedFd;

/// 一把阻止锁。它活着的时候，机器不会因为我们没在动它就睡过去。
pub(crate) struct Request {
    /// logind 的约定：fd 关掉，锁就没了。
    _lock: OwnedFd,
}

impl Request {
    /// 被控端在采集：显示器要亮着，机器也别待机。
    pub(crate) fn capture() -> Result<Self> {
        Self::inhibit("idle:sleep", "OpenUUYC 正在采集远程桌面")
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
