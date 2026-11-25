//! Optional SOCKS5 egress for TURN-over-TCP/TLS relay connections.
//!
//! The application installs a process-wide configuration once at startup.
//! With [`ProxyMode::Prefer`] the allocator only gathers LAN host candidates
//! and TURN relays dialled through the proxy, so a slow direct uplink is not
//! selected while LAN peers still connect directly.

use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{lookup_host, TcpSocket, TcpStream};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ProxyMode {
    /// Never use the proxy.
    #[default]
    Off,
    /// Keep LAN host candidates, drop direct WAN (srflx / global host / direct
    /// relay) candidates and relay through the proxy instead.
    Prefer,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProxyConfig {
    /// `host:port` of the SOCKS5 server.
    pub address: String,
    pub username: Option<String>,
    pub password: Option<String>,
}

/// An IP network such as `11.11.0.0/16`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cidr {
    network: IpAddr,
    prefix: u8,
}

impl Cidr {
    pub fn parse(value: &str) -> Result<Self, String> {
        let (ip, prefix) = value.split_once('/').unwrap_or((value, ""));
        let network: IpAddr = ip.trim().parse().map_err(|_| format!("invalid CIDR {value:?}"))?;
        let max = if network.is_ipv4() { 32 } else { 128 };
        let prefix = if prefix.is_empty() {
            max
        } else {
            prefix.trim().parse::<u8>().map_err(|_| format!("invalid CIDR {value:?}"))?
        };
        if prefix > max {
            return Err(format!("invalid CIDR prefix in {value:?}"));
        }
        Ok(Self { network, prefix })
    }

    pub fn contains(&self, ip: IpAddr) -> bool {
        match (self.network, ip) {
            (IpAddr::V4(n), IpAddr::V4(ip)) => {
                let mask = u32::MAX.checked_shl(32 - self.prefix as u32).unwrap_or(0);
                u32::from(n) & mask == u32::from(ip) & mask
            }
            (IpAddr::V6(n), IpAddr::V6(ip)) => {
                let mask = u128::MAX.checked_shl(128 - self.prefix as u32).unwrap_or(0);
                u128::from(n) & mask == u128::from(ip) & mask
            }
            _ => false,
        }
    }
}

impl ProxyConfig {
    /// Parses `socks5://[user:pass@]host:port` (`socks5h://` accepted too).
    pub fn parse(value: &str) -> Result<Self, String> {
        let rest = value
            .strip_prefix("socks5://")
            .or_else(|| value.strip_prefix("socks5h://"))
            .ok_or("proxy URL must start with socks5://")?;
        let rest = rest.trim_end_matches('/');
        let (auth, address) = match rest.rsplit_once('@') {
            Some((auth, address)) => (Some(auth), address),
            None => (None, rest),
        };
        if address.is_empty() || !address.contains(':') {
            return Err("proxy URL needs host:port".into());
        }
        let (username, password) = match auth {
            Some(auth) => {
                let (user, pass) = auth.split_once(':').unwrap_or((auth, ""));
                (Some(user.to_string()), Some(pass.to_string()))
            }
            None => (None, None),
        };
        Ok(Self {
            address: address.to_string(),
            username,
            password,
        })
    }
}

static ROLES: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(3);

/// Which side of a connection may use the proxy: bit 0 = host (controlled),
/// bit 1 = viewer (controlling). Default is both.
pub fn set_roles(host: bool, viewer: bool) {
    ROLES.store(
        u8::from(host) | (u8::from(viewer) << 1),
        std::sync::atomic::Ordering::Relaxed,
    );
}

pub fn role_enabled(host: bool) -> bool {
    let bit = if host { 1 } else { 2 };
    ROLES.load(std::sync::atomic::Ordering::Relaxed) & bit != 0
}

static LAN: RwLock<Vec<Cidr>> = RwLock::new(Vec::new());
static PROXIED_RELAYS: std::sync::Mutex<Vec<SocketAddr>> = std::sync::Mutex::new(Vec::new());

/// Extra networks treated as local (kept direct, labelled LAN) besides the
/// built-in private / link-local / CGNAT ranges, e.g. an intranet on public
/// address space. Independent of the proxy mode.
pub fn set_lan_networks(networks: Vec<Cidr>) {
    *LAN.write().unwrap_or_else(|e| e.into_inner()) = networks;
}

pub(crate) fn note_proxied(address: SocketAddr) {
    let mut relays = PROXIED_RELAYS.lock().unwrap_or_else(|e| e.into_inner());
    if relays.len() >= 64 {
        relays.remove(0);
    }
    relays.push(address);
}

/// True when `address` is a candidate (relay or proxy-reflexive) that goes
/// through the SOCKS5 proxy.
pub fn is_proxied(address: SocketAddr) -> bool {
    PROXIED_RELAYS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains(&address)
}

static PROXY: RwLock<Option<Arc<(ProxyConfig, ProxyMode)>>> = RwLock::new(None);

/// Installs (or clears, with `None`) the process-wide proxy.
pub fn set_proxy(config: Option<(ProxyConfig, ProxyMode)>) {
    *PROXY.write().unwrap_or_else(|e| e.into_inner()) = config.map(Arc::new);
}

pub(crate) fn active() -> Option<Arc<(ProxyConfig, ProxyMode)>> {
    PROXY
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .filter(|value| value.1 != ProxyMode::Off)
}

pub(crate) fn prefer_proxy() -> bool {
    active().is_some_and(|value| value.1 == ProxyMode::Prefer)
}

/// Addresses that stay on a local / overlay network. Candidates on these are
/// kept even when the proxy is preferred.
pub fn is_configured_lan(ip: IpAddr) -> bool {
    LAN.read()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .any(|net| net.contains(ip))
}

pub fn is_lan_ip(ip: IpAddr) -> bool {
    if is_configured_lan(ip) {
        return true;
    }
    match ip {
        IpAddr::V4(ip) => {
            let o = ip.octets();
            ip.is_private()
                || ip.is_loopback()
                || ip.is_link_local()
                || (o[0] == 100 && (64..128).contains(&o[1]))
        }
        IpAddr::V6(ip) => {
            let s = ip.segments();
            ip.is_loopback() || (s[0] & 0xfe00) == 0xfc00 || (s[0] & 0xffc0) == 0xfe80
        }
    }
}

/// Opens a TCP stream to `target` through the SOCKS5 server (CONNECT).
pub(crate) async fn connect(
    config: &ProxyConfig,
    target: SocketAddr,
    local_ip: Option<IpAddr>,
) -> io::Result<TcpStream> {
    tokio::time::timeout(HANDSHAKE_TIMEOUT, connect_inner(config, target, local_ip))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "SOCKS5 handshake timed out"))?
}

async fn connect_inner(
    config: &ProxyConfig,
    target: SocketAddr,
    local_ip: Option<IpAddr>,
) -> io::Result<TcpStream> {
    let (mut stream, _) = open_authenticated(config, local_ip).await?;
    let (_, _) = request(&mut stream, 1, target).await?;
    Ok(stream)
}

/// TCP connection to the proxy with the greeting / authentication done.
async fn open_authenticated(
    config: &ProxyConfig,
    local_ip: Option<IpAddr>,
) -> io::Result<(TcpStream, SocketAddr)> {
    let proxy_addr = lookup_host(config.address.as_str())
        .await?
        .find(|addr| local_ip.is_none_or(|ip| ip.is_ipv4() == addr.is_ipv4()))
        .ok_or_else(|| io::Error::other("SOCKS5 server has no address of this family"))?;
    let socket = if proxy_addr.is_ipv4() {
        TcpSocket::new_v4()?
    } else {
        TcpSocket::new_v6()?
    };
    if let Some(ip) = local_ip {
        socket.bind(SocketAddr::new(ip, 0))?;
    }
    let mut stream = socket.connect(proxy_addr).await?;
    stream.set_nodelay(true)?;

    let with_auth = config.username.is_some();
    if with_auth {
        stream.write_all(&[5, 2, 0, 2]).await?;
    } else {
        stream.write_all(&[5, 1, 0]).await?;
    }
    let mut choice = [0u8; 2];
    stream.read_exact(&mut choice).await?;
    if choice[0] != 5 {
        return Err(io::Error::other("not a SOCKS5 server"));
    }
    match choice[1] {
        0 => {}
        2 if with_auth => {
            let user = config.username.as_deref().unwrap_or("").as_bytes();
            let pass = config.password.as_deref().unwrap_or("").as_bytes();
            if user.len() > 255 || pass.len() > 255 {
                return Err(io::Error::other("SOCKS5 credentials too long"));
            }
            let mut request = vec![1, user.len() as u8];
            request.extend_from_slice(user);
            request.push(pass.len() as u8);
            request.extend_from_slice(pass);
            stream.write_all(&request).await?;
            let mut status = [0u8; 2];
            stream.read_exact(&mut status).await?;
            if status[1] != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "SOCKS5 authentication rejected",
                ));
            }
        }
        0xff => return Err(io::Error::other("SOCKS5 server rejected all auth methods")),
        other => return Err(io::Error::other(format!("unsupported SOCKS5 method {other}"))),
    }
    Ok((stream, proxy_addr))
}

fn write_addr(out: &mut Vec<u8>, address: SocketAddr) {
    match address.ip() {
        IpAddr::V4(ip) => {
            out.push(1);
            out.extend_from_slice(&ip.octets());
        }
        IpAddr::V6(ip) => {
            out.push(4);
            out.extend_from_slice(&ip.octets());
        }
    }
    out.extend_from_slice(&address.port().to_be_bytes());
}

/// Sends a SOCKS5 request (1 = CONNECT, 3 = UDP ASSOCIATE) and returns the
/// bound address from the reply (`None` for a domain-name reply).
async fn request(
    stream: &mut TcpStream,
    command: u8,
    target: SocketAddr,
) -> io::Result<(u8, Option<SocketAddr>)> {
    let mut request = vec![5, command, 0];
    write_addr(&mut request, target);
    stream.write_all(&request).await?;

    let mut head = [0u8; 4];
    stream.read_exact(&mut head).await?;
    if head[0] != 5 {
        return Err(io::Error::other("invalid SOCKS5 reply"));
    }
    if head[1] != 0 {
        return Err(io::Error::other(format!(
            "SOCKS5 request failed (reply code {})",
            head[1]
        )));
    }
    let bound = match head[3] {
        1 => {
            let mut raw = [0u8; 6];
            stream.read_exact(&mut raw).await?;
            Some(SocketAddr::new(
                IpAddr::from([raw[0], raw[1], raw[2], raw[3]]),
                u16::from_be_bytes([raw[4], raw[5]]),
            ))
        }
        4 => {
            let mut raw = [0u8; 18];
            stream.read_exact(&mut raw).await?;
            let mut ip = [0u8; 16];
            ip.copy_from_slice(&raw[..16]);
            Some(SocketAddr::new(
                IpAddr::from(ip),
                u16::from_be_bytes([raw[16], raw[17]]),
            ))
        }
        3 => {
            let mut len = [0u8; 1];
            stream.read_exact(&mut len).await?;
            let mut discard = vec![0u8; len[0] as usize + 2];
            stream.read_exact(&mut discard).await?;
            None
        }
        _ => return Err(io::Error::other("invalid SOCKS5 address type")),
    };
    Ok((head[1], bound))
}

/// A UDP socket whose datagrams are relayed by the SOCKS5 server (UDP
/// ASSOCIATE). Peers see the proxy's address, so the server-reflexive address
/// learnt through it is the proxy's public endpoint.
pub(crate) struct Socks5Udp {
    socket: tokio::net::UdpSocket,
    relay: SocketAddr,
    local: SocketAddr,
    closed: tokio_util::sync::CancellationToken,
}

pub(crate) async fn udp_associate(
    config: &ProxyConfig,
    local_ip: IpAddr,
) -> io::Result<Arc<Socks5Udp>> {
    tokio::time::timeout(HANDSHAKE_TIMEOUT, async {
        let (mut control, proxy_addr) = open_authenticated(config, Some(local_ip)).await?;
        let unspecified: SocketAddr = if local_ip.is_ipv4() {
            "0.0.0.0:0".parse().unwrap()
        } else {
            "[::]:0".parse().unwrap()
        };
        let socket = tokio::net::UdpSocket::bind(SocketAddr::new(local_ip, 0)).await?;
        let (_, bound) = request(&mut control, 3, unspecified).await?;
        let mut relay = bound.ok_or_else(|| io::Error::other("SOCKS5 UDP relay is a domain"))?;
        if relay.ip().is_unspecified() {
            relay.set_ip(proxy_addr.ip());
        }
        let closed = tokio_util::sync::CancellationToken::new();
        // The association lives exactly as long as this TCP connection.
        let watch = closed.clone();
        tokio::spawn(async move {
            let mut sink = [0u8; 64];
            loop {
                tokio::select! {
                    _ = watch.cancelled() => break,
                    read = control.read(&mut sink) => {
                        if matches!(read, Ok(0) | Err(_)) {
                            watch.cancel();
                            break;
                        }
                    }
                }
            }
        });
        let local = socket.local_addr()?;
        log::info!("SOCKS5 UDP association via {proxy_addr}, relay endpoint {relay}");
        Ok(Arc::new(Socks5Udp {
            socket,
            relay,
            local,
            closed,
        }))
    })
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "SOCKS5 UDP associate timed out"))?
}

fn udp_error(error: io::Error) -> util::Error {
    util::Error::Io(error.into())
}

#[async_trait::async_trait]
impl util::Conn for Socks5Udp {
    async fn connect(&self, _addr: SocketAddr) -> Result<(), util::Error> {
        Err(util::Error::Other("SOCKS5 UDP is connectionless".into()))
    }

    async fn recv(&self, buf: &mut [u8]) -> Result<usize, util::Error> {
        Ok(self.recv_from(buf).await?.0)
    }

    async fn recv_from(&self, buf: &mut [u8]) -> Result<(usize, SocketAddr), util::Error> {
        let mut packet = vec![0u8; buf.len() + 22];
        loop {
            let (length, from) = tokio::select! {
                _ = self.closed.cancelled() => return Err(util::Error::ErrClosedListener),
                result = self.socket.recv_from(&mut packet) => result.map_err(udp_error)?,
            };
            if from != self.relay || length < 4 || packet[2] != 0 {
                continue;
            }
            let (source, offset) = match packet[3] {
                1 if length >= 10 => (
                    SocketAddr::new(
                        IpAddr::from([packet[4], packet[5], packet[6], packet[7]]),
                        u16::from_be_bytes([packet[8], packet[9]]),
                    ),
                    10,
                ),
                4 if length >= 22 => {
                    let mut ip = [0u8; 16];
                    ip.copy_from_slice(&packet[4..20]);
                    (
                        SocketAddr::new(
                            IpAddr::from(ip),
                            u16::from_be_bytes([packet[20], packet[21]]),
                        ),
                        22,
                    )
                }
                _ => continue,
            };
            let payload = length - offset;
            let copied = payload.min(buf.len());
            buf[..copied].copy_from_slice(&packet[offset..offset + copied]);
            return Ok((copied, source));
        }
    }

    async fn send(&self, _buf: &[u8]) -> Result<usize, util::Error> {
        Err(util::Error::Other("SOCKS5 UDP needs a destination".into()))
    }

    async fn send_to(&self, buf: &[u8], target: SocketAddr) -> Result<usize, util::Error> {
        let mut packet = Vec::with_capacity(buf.len() + 22);
        packet.extend_from_slice(&[0, 0, 0]);
        write_addr(&mut packet, target);
        packet.extend_from_slice(buf);
        self.socket
            .send_to(&packet, self.relay)
            .await
            .map_err(udp_error)?;
        Ok(buf.len())
    }

    fn local_addr(&self) -> Result<SocketAddr, util::Error> {
        Ok(self.local)
    }

    fn remote_addr(&self) -> Option<SocketAddr> {
        None
    }

    async fn close(&self) -> Result<(), util::Error> {
        self.closed.cancel();
        Ok(())
    }

    fn as_any(&self) -> &(dyn std::any::Any + Send + Sync) {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_urls() {
        let c = ProxyConfig::parse("socks5://u:p@1.2.3.4:1080").unwrap();
        assert_eq!(c.address, "1.2.3.4:1080");
        assert_eq!(c.username.as_deref(), Some("u"));
        assert_eq!(c.password.as_deref(), Some("p"));
        let c = ProxyConfig::parse("socks5://host:1080/").unwrap();
        assert!(c.username.is_none());
        assert!(ProxyConfig::parse("http://x:1").is_err());
        assert!(ProxyConfig::parse("socks5://nohost").is_err());
    }

    #[test]
    fn cidr() {
        let c = Cidr::parse("11.11.0.0/16").unwrap();
        assert!(c.contains("11.11.5.6".parse().unwrap()));
        assert!(!c.contains("11.12.0.1".parse().unwrap()));
        assert!(Cidr::parse("0.0.0.0/0").unwrap().contains("8.8.8.8".parse().unwrap()));
        assert!(Cidr::parse("10.0.0.1").unwrap().contains("10.0.0.1".parse().unwrap()));
        assert!(Cidr::parse("1.2.3.4/33").is_err());
    }

    #[test]
    fn lan_detection() {
        assert!(is_lan_ip("192.168.1.2".parse().unwrap()));
        assert!(is_lan_ip("100.100.1.1".parse().unwrap()));
        assert!(is_lan_ip("fd00::1".parse().unwrap()));
        assert!(!is_lan_ip("8.8.8.8".parse().unwrap()));
        assert!(!is_lan_ip("2408::1".parse().unwrap()));
    }

    #[tokio::test]
    async fn connects_through_mock_server() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut hello = [0u8; 4];
            s.read_exact(&mut hello).await.unwrap();
            assert_eq!(hello, [5, 2, 0, 2]);
            s.write_all(&[5, 2]).await.unwrap();
            let mut auth = [0u8; 5];
            s.read_exact(&mut auth).await.unwrap();
            assert_eq!(auth, [1, 1, b'u', 1, b'p']);
            s.write_all(&[1, 0]).await.unwrap();
            let mut req = [0u8; 10];
            s.read_exact(&mut req).await.unwrap();
            assert_eq!(req, [5, 1, 0, 1, 9, 9, 9, 9, 0x01, 0xbb]);
            s.write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0]).await.unwrap();
            s.write_all(b"hi").await.unwrap();
        });
        let config = ProxyConfig::parse(&format!("socks5://u:p@{address}")).unwrap();
        let mut stream = connect(&config, "9.9.9.9:443".parse().unwrap(), None)
            .await
            .unwrap();
        let mut reply = [0u8; 2];
        stream.read_exact(&mut reply).await.unwrap();
        assert_eq!(&reply, b"hi");
        server.await.unwrap();
    }
}
