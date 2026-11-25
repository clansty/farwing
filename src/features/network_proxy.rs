//! Optional SOCKS5 egress for relayed media / file traffic.
//!
//! Configuration order (first hit wins): `--socks5` / `--direct` on the command
//! line, `OPENUUYC_SOCKS5` (+ `OPENUUYC_SOCKS5_MODE`) in the environment, then
//! `network.json` in the per-user data directory:
//!
//! ```json
//! { "socks5": "socks5://user:pass@host:1080", "mode": "prefer", "lan": ["11.11.0.0/16"] }
//! ```
//!
//! `mode = "prefer"` keeps LAN / overlay (RFC 1918, ULA, link-local, CGNAT)
//! paths direct, drops direct WAN candidates and relays through the proxy.
//! `lan` lists extra networks (CIDR) to keep direct, e.g. an intranet on public
//! address space; env `OPENUUYC_LAN=11.11.0.0/16,...` does the same.
//! `scope` picks which side uses the proxy: `"host"` (only the controlled machine,
//! i.e. incoming connections), `"viewer"` or `"both"` (default); env
//! `OPENUUYC_SOCKS5_SCOPE`.
//! `mode = "off"` (default) never touches the proxy.
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};
use webrtc::ice::socks5::{Cidr, ProxyConfig, ProxyMode, role_enabled, set_lan_networks, set_proxy, set_roles};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub socks5: Option<String>,
    /// "off" | "prefer"; a configured proxy without a mode means "prefer".
    pub mode: Option<String>,
    /// Extra local networks kept direct, e.g. `["11.11.0.0/16"]`.
    pub lan: Vec<String>,
    /// Which side routes through the proxy: "both" (default), "host" (only the
    /// controlled machine, i.e. incoming connections) or "viewer".
    pub scope: Option<String>,
}

/// Whether a connection of this role (`host` = controlled machine) may use the
/// proxy.
pub fn egress_for(host: bool) -> bool {
    role_enabled(host)
}

impl Settings {
    fn path() -> Result<std::path::PathBuf> {
        Ok(crate::platform::paths::require_local_app_data()?
            .join("OpenUUYC")
            .join("network.json"))
    }

    pub fn load() -> Result<Self> {
        match std::fs::read(Self::path()?) {
            Ok(bytes) => serde_json::from_slice(&bytes).context("读取 network.json 失败"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error).context("读取 network.json 失败"),
        }
    }

    fn lan_networks(&self) -> Result<Vec<Cidr>> {
        self.lan
            .iter()
            .map(|net| Cidr::parse(net).map_err(anyhow::Error::msg))
            .collect()
    }

    fn resolve(&self) -> Result<Option<(ProxyConfig, ProxyMode)>> {
        let Some(url) = self.socks5.as_deref().filter(|url| !url.is_empty()) else {
            return Ok(None);
        };
        let config = ProxyConfig::parse(url).map_err(anyhow::Error::msg)?;
        let mode = match self.mode.as_deref().unwrap_or("prefer") {
            "prefer" => ProxyMode::Prefer,
            "off" => ProxyMode::Off,
            other => bail!("未知的代理模式 {other:?}（可用：off、prefer）"),
        };
        Ok(Some((config, mode)))
    }
}

fn env_scope() -> Option<String> {
    std::env::var("OPENUUYC_SOCKS5_SCOPE").ok()
}

fn env_lan() -> Vec<String> {
    std::env::var("OPENUUYC_LAN")
        .map(|value| value.split(',').map(|part| part.trim().to_owned()).filter(|part| !part.is_empty()).collect())
        .unwrap_or_default()
}

/// Installs the process-wide proxy. `cli_url` overrides everything; `direct`
/// disables the proxy for this run.
pub fn configure(cli_url: Option<&str>, direct: bool) -> Result<()> {
    let file = Settings::load()?;
    // LAN networks apply regardless of the proxy mode (they also fix the
    // LAN / P2P label for intranets on public address space).
    let mut lan = file.lan.clone();
    lan.extend(env_lan());
    set_lan_networks(Settings { lan, ..Settings::default() }.lan_networks()?);
    match file.scope.as_deref().or(env_scope().as_deref()).unwrap_or("both") {
        "both" => set_roles(true, true),
        "host" => set_roles(true, false),
        "viewer" => set_roles(false, true),
        other => bail!("未知的代理范围 {other:?}（可用：both、host、viewer）"),
    }
    if direct {
        set_proxy(None);
        return Ok(());
    }
    let settings = if let Some(url) = cli_url {
        Settings { socks5: Some(url.to_owned()), ..Settings::default() }
    } else if let Ok(url) = std::env::var("OPENUUYC_SOCKS5") {
        Settings {
            socks5: Some(url),
            mode: std::env::var("OPENUUYC_SOCKS5_MODE").ok(),
            ..Settings::default()
        }
    } else {
        file
    };
    match settings.resolve()? {
        Some((config, mode)) => {
            tracing::info!(address = %config.address, ?mode, "SOCKS5 egress configured");
            set_proxy(Some((config, mode)));
        }
        None => set_proxy(None),
    }
    Ok(())
}

/// Upload-speed comparison: the same POST body sent directly and through the
/// SOCKS5 server. This measures the raw uplinks that relayed traffic will use.
pub async fn speed_test(url: &str, proxy: Option<&str>, megabytes: usize, rounds: usize) -> Result<()> {
    let proxy_url = match proxy {
        Some(url) => url.to_owned(),
        None => Settings::load()?
            .socks5
            .or_else(|| std::env::var("OPENUUYC_SOCKS5").ok())
            .context("没有配置 SOCKS5：使用 --socks5 或写入 network.json")?,
    };
    ProxyConfig::parse(&proxy_url).map_err(anyhow::Error::msg)?;
    let _ = rustls::crypto::ring::default_provider().install_default();
    let body = vec![0x5au8; megabytes.max(1) * 1024 * 1024];
    println!("目标 {url}，每轮上传 {} MiB，共 {rounds} 轮", megabytes.max(1));
    for (label, proxied) in [("直连", false), ("SOCKS5", true)] {
        let mut builder = reqwest::Client::builder()
            .timeout(Duration::from_secs(120))
            .no_proxy();
        if proxied {
            builder = builder.proxy(reqwest::Proxy::all(&proxy_url)?);
        }
        let client = builder.build()?;
        let mut best = 0f64;
        for round in 1..=rounds.max(1) {
            let started = Instant::now();
            let result = client.post(url).body(body.clone()).send().await;
            let elapsed = started.elapsed().as_secs_f64();
            match result {
                Ok(response) if response.status().is_success() => {
                    let mbps = body.len() as f64 * 8.0 / 1e6 / elapsed;
                    best = best.max(mbps);
                    println!("[{label}] 第 {round} 轮：{mbps:.1} Mbps（{elapsed:.1}s）");
                }
                Ok(response) => println!("[{label}] 第 {round} 轮：HTTP {}", response.status()),
                Err(error) => println!("[{label}] 第 {round} 轮失败：{error}"),
            }
        }
        println!("[{label}] 最佳 {best:.1} Mbps");
    }
    Ok(())
}
