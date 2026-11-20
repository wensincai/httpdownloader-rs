//! Proxy support — HTTP CONNECT tunneling + SOCKS5 (RFC 1928/1929).
//!
//! Mirrors the original's proxy settings (registry `proxy_ip/proxy_port/...`,
//! connection.cpp builds the CONNECT request before any TLS layer).

use std::fmt;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::auth::basic_authorization;
use crate::error::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ProxyKind {
    Http,
    Socks5,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ProxyConfig {
    pub kind: ProxyKind,
    pub host: String,
    pub port: u16,
    pub username: Option<String>,
    pub password: Option<String>,
}

impl ProxyConfig {
    /// Parse `http://[user:pass@]host[:port]` or `socks5://[user:pass@]host[:port]`.
    /// Default ports: 8080 (http), 1080 (socks5).
    pub fn parse(s: &str) -> Result<Self> {
        let (scheme, rest) = s
            .split_once("://")
            .ok_or_else(|| Error::InvalidUrl(format!("proxy url missing scheme: {s}")))?;
        let kind = match scheme.to_ascii_lowercase().as_str() {
            "http" => ProxyKind::Http,
            "socks5" | "socks" => ProxyKind::Socks5,
            other => {
                return Err(Error::InvalidUrl(format!(
                    "unsupported proxy scheme '{other}' (want http:// or socks5://)"
                )))
            }
        };
        let default_port = match kind {
            ProxyKind::Http => 8080,
            ProxyKind::Socks5 => 1080,
        };

        // userinfo
        let (username, password, hostport) = match rest.find('@') {
            Some(i) => {
                let ui = &rest[..i];
                let (u, p) = match ui.split_once(':') {
                    Some((u, p)) => (Some(u.to_string()), Some(p.to_string())),
                    None => (Some(ui.to_string()), None),
                };
                (u, p, &rest[i + 1..])
            }
            None => (None, None, rest),
        };

        let (host, port) = if let Some(stripped) = hostport.strip_prefix('[') {
            // IPv6 literal [::1]:1080
            match stripped.split_once(']') {
                Some((h, after)) => {
                    let port = after
                        .strip_prefix(':')
                        .and_then(|s| s.parse::<u16>().ok())
                        .unwrap_or(default_port);
                    (format!("[{h}]"), port)
                }
                None => return Err(Error::InvalidUrl("bad proxy IPv6 literal".into())),
            }
        } else {
            match hostport.rsplit_once(':') {
                Some((h, p)) => (
                    h.to_string(),
                    p.parse::<u16>()
                        .map_err(|_| Error::InvalidUrl(format!("bad proxy port '{p}'")))?,
                ),
                None => (hostport.to_string(), default_port),
            }
        };
        if host.is_empty() {
            return Err(Error::InvalidUrl("empty proxy host".into()));
        }
        Ok(ProxyConfig { kind, host, port, username, password })
    }

    fn credential(&self) -> Option<String> {
        match (&self.username, &self.password) {
            (Some(u), Some(p)) => Some(format!("{u}:{p}")),
            (Some(u), None) => Some(u.clone()),
            _ => None,
        }
    }
}

impl fmt::Display for ProxyConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let scheme = match self.kind {
            ProxyKind::Http => "http",
            ProxyKind::Socks5 => "socks5",
        };
        write!(f, "{scheme}://")?;
        if let Some(u) = &self.username {
            write!(f, "{u}")?;
            if let Some(p) = &self.password {
                write!(f, ":{p}")?;
            }
            write!(f, "@")?;
        }
        write!(f, "{}:{}", self.host, self.port)
    }
}

/// Open a raw TCP connection to `host:port` through the proxy, ready for
/// optional TLS layering by the caller.
pub async fn connect(cfg: &ProxyConfig, host: &str, port: u16) -> Result<TcpStream> {
    let mut tcp = TcpStream::connect((cfg.host.as_str(), cfg.port)).await?;
    match cfg.kind {
        ProxyKind::Http => http_connect(&mut tcp, cfg, host, port).await?,
        ProxyKind::Socks5 => socks5_connect(&mut tcp, cfg, host, port).await?,
    }
    Ok(tcp)
}

/// HTTP CONNECT tunnel: send request, require "HTTP/1.x 200".
async fn http_connect(
    tcp: &mut TcpStream,
    cfg: &ProxyConfig,
    host: &str,
    port: u16,
) -> Result<()> {
    let mut req = format!("CONNECT {host}:{port} HTTP/1.1\r\nHost: {host}:{port}\r\n");
    if let (Some(u), Some(p)) = (&cfg.username, &cfg.password) {
        req.push_str(&format!("Proxy-Authorization: {}\r\n", basic_authorization(u, p)));
    }
    req.push_str("Proxy-Connection: keep-alive\r\n\r\n");
    tcp.write_all(req.as_bytes()).await?;
    tcp.flush().await?;

    let mut buf = Vec::with_capacity(1024);
    let mut byte = [0u8; 1];
    loop {
        let n = tcp.read(&mut byte).await?;
        if n == 0 {
            return Err(Error::MalformedResponse("proxy closed during CONNECT reply".into()));
        }
        buf.push(byte[0]);
        if buf.ends_with(b"\r\n\r\n") || buf.ends_with(b"\n\n") {
            break;
        }
        if buf.len() > 32 * 1024 {
            return Err(Error::MalformedResponse("proxy CONNECT reply too large".into()));
        }
    }
    let text = String::from_utf8_lossy(&buf);
    let status_line = text.lines().next().unwrap_or("");
    // e.g. "HTTP/1.1 200 Connection established"
    let ok = status_line
        .split_ascii_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .map(|c| (200..300).contains(&c))
        .unwrap_or(false);
    if !ok {
        return Err(Error::Other(format!("proxy CONNECT failed: {status_line}")));
    }
    Ok(())
}

/// SOCKS5 handshake (RFC 1928) with optional user/pass subnegotiation (RFC 1929).
async fn socks5_connect(
    tcp: &mut TcpStream,
    cfg: &ProxyConfig,
    host: &str,
    port: u16,
) -> Result<()> {
    let want_auth = cfg.credential().is_some();
    // greeting: VER=5, NMETHODS, methods (0x00 no-auth, 0x02 user/pass)
    let methods: &[u8] = if want_auth { &[0x00, 0x02] } else { &[0x00] };
    let mut greeting = vec![0x05u8, methods.len() as u8];
    greeting.extend_from_slice(methods);
    tcp.write_all(&greeting).await?;
    tcp.flush().await?;

    let mut reply = [0u8; 2];
    tcp.read_exact(&mut reply).await?;
    if reply[0] != 0x05 {
        return Err(Error::Other(format!("bad socks version byte {:#x}", reply[0])));
    }
    match reply[1] {
        0x00 => {} // no-auth accepted
        0x02 => {
            // user/pass subnegotiation: VER=1, ULEN, UNAME, PLEN, PASSWD
            let cred = cfg.credential().unwrap_or_default();
            let (u, p) = cred.split_once(':').unwrap_or((cred.as_str(), ""));
            let mut req = vec![0x01u8, u.len() as u8];
            req.extend_from_slice(u.as_bytes());
            req.push(p.len() as u8);
            req.extend_from_slice(p.as_bytes());
            tcp.write_all(&req).await?;
            let mut auth_reply = [0u8; 2];
            tcp.read_exact(&mut auth_reply).await?;
            if auth_reply[1] != 0x00 {
                return Err(Error::Auth("socks5 username/password rejected".into()));
            }
        }
        0xff => return Err(Error::Auth("socks5: no acceptable auth method".into())),
        m => return Err(Error::Other(format!("socks5: unexpected method {m:#x}"))),
    }

    // CONNECT request: VER=5 CMD=1 RSV=0 ATYP ADDR PORT
    let mut req = vec![0x05u8, 0x01, 0x00];
    req.extend_from_slice(&encode_addr(host)?);
    req.extend_from_slice(&port.to_be_bytes());
    tcp.write_all(&req).await?;
    tcp.flush().await?;

    // reply: VER REP RSV ATYP ADDR(port-dependent len)
    let mut head = [0u8; 4];
    tcp.read_exact(&mut head).await?;
    if head[1] != 0x00 {
        return Err(Error::Other(format!("socks5 connect failed, rep={:#x}", head[1])));
    }
    // skip bound address
    let skip = match head[3] {
        0x01 => 4 + 2,
        0x03 => {
            let mut l = [0u8; 1];
            tcp.read_exact(&mut l).await?;
            l[0] as usize + 2
        }
        0x04 => 16 + 2,
        a => return Err(Error::Other(format!("socks5: bad atyp {a:#x}"))),
    };
    let mut junk = vec![0u8; skip];
    tcp.read_exact(&mut junk).await?;
    Ok(())
}

/// Encode address with ATYP prefix (IPv4 / FQDN / IPv6).
fn encode_addr(host: &str) -> Result<Vec<u8>> {
    // try IPv4
    if let Ok(ip) = host.parse::<std::net::Ipv4Addr>() {
        let mut v = vec![0x01u8];
        v.extend_from_slice(&ip.octets());
        return Ok(v);
    }
    // try IPv6 (strip brackets if present)
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = bare.parse::<std::net::Ipv6Addr>() {
        let mut v = vec![0x04u8];
        v.extend_from_slice(&ip.octets());
        return Ok(v);
    }
    // FQDN
    let b = host.as_bytes();
    if b.len() > 255 {
        return Err(Error::InvalidUrl("socks5 hostname too long".into()));
    }
    let mut v = vec![0x03u8, b.len() as u8];
    v.extend_from_slice(b);
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_http_proxy() {
        let p = ProxyConfig::parse("http://proxy.local").unwrap();
        assert_eq!(p.kind, ProxyKind::Http);
        assert_eq!(p.port, 8080);
        assert!(p.username.is_none());
        let p2 = ProxyConfig::parse("http://u:p@1.2.3.4:3128").unwrap();
        assert_eq!(p2.username.as_deref(), Some("u"));
        assert_eq!(p2.password.as_deref(), Some("p"));
        assert_eq!(p2.port, 3128);
        assert_eq!(p2.to_string(), "http://u:p@1.2.3.4:3128");
    }

    #[test]
    fn parse_socks5_proxy() {
        let p = ProxyConfig::parse("socks5://u:p@[::1]:1080").unwrap();
        assert_eq!(p.kind, ProxyKind::Socks5);
        assert_eq!(p.host, "[::1]");
        assert_eq!(p.port, 1080);
        let p2 = ProxyConfig::parse("socks://gw").unwrap();
        assert_eq!(p2.kind, ProxyKind::Socks5);
        assert_eq!(p2.port, 1080);
    }

    #[test]
    fn parse_rejects() {
        assert!(ProxyConfig::parse("proxy.local").is_err());
        assert!(ProxyConfig::parse("ftp://x").is_err());
    }

    #[test]
    fn addr_encoding() {
        assert_eq!(encode_addr("1.2.3.4").unwrap(), vec![0x01, 1, 2, 3, 4]);
        let mut domain = vec![0x03u8, 11];
        domain.extend_from_slice(b"example.com");
        assert_eq!(encode_addr("example.com").unwrap(), domain);
        let v6 = encode_addr("[::1]").unwrap();
        assert_eq!(v6[0], 0x04);
        assert_eq!(v6.len(), 17);
    }

    #[test]
    fn greeting_bytes() {
        // documented shape of the greeting we build
        let methods: &[u8] = &[0x00, 0x02];
        let mut greeting = vec![0x05u8, methods.len() as u8];
        greeting.extend_from_slice(methods);
        assert_eq!(greeting, vec![0x05, 0x02, 0x00, 0x02]);
    }
}
