//! FTP / FTPS client — mirrors the FTP branch of the original IOCP engine
//! (connection.cpp handles ftp:// with PASV/EPSV + REST resume).
//!
//! Flow per transfer: control channel login → TYPE I → PASV/EPSV → (REST) →
//! RETR → stream data channel → wait 226.
//! FTPS is explicit TLS (AUTH TLS + PBSZ 0 + PROT P); both control and data
//! channels are upgraded.

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::error::{Error, Result};
use crate::http::client::{tls_connect, Conn};
use crate::url::Url;

/// One server reply: numeric code + collected text lines.
#[derive(Debug, Clone)]
pub struct FtpReply {
    pub code: u16,
    pub text: String,
}

impl FtpReply {
    pub fn kind(&self) -> u16 {
        self.code / 100
    }
}

pub struct FtpClient {
    ctrl: Option<Conn>,
    /// explicit TLS active (control + data upgrades)
    pub tls: bool,
}

impl FtpClient {
    fn ctrl(&mut self) -> &mut Conn {
        self.ctrl.as_mut().expect("ftp control channel present")
    }

    /// Connect the control channel (plain; FTPS upgrades via `upgrade_tls`).
    /// Reads the 220 banner.
    pub async fn connect(url: &Url) -> Result<Self> {
        let tcp = TcpStream::connect((url.host.as_str(), url.port)).await?;
        let mut c = FtpClient { ctrl: Some(Conn::Plain(tcp)), tls: false };
        c.read_reply().await?; // 220 banner
        Ok(c)
    }

    /// Send a command and read the (possibly multiline) reply.
    pub async fn send(&mut self, cmd: &str) -> Result<FtpReply> {
        self.ctrl().write_all(format!("{cmd}\r\n").as_bytes()).await?;
        self.ctrl().flush().await?;
        self.read_reply().await
    }

    /// Read one complete reply (multiline-aware). Public: callers wait for
    /// the post-transfer 226/426 reply outside `send`.
    pub async fn wait_reply(&mut self) -> Result<FtpReply> {
        self.read_reply().await
    }

    async fn read_reply(&mut self) -> Result<FtpReply> {
        let mut buf = Vec::with_capacity(256);
        let mut byte = [0u8; 1];
        let mut lines: Vec<String> = Vec::new();
        loop {
            let n = self.ctrl().read(&mut byte).await?;
            if n == 0 {
                return Err(Error::MalformedResponse("ftp control channel closed".into()));
            }
            if byte[0] == b'\n' {
                // complete line at buf (strip trailing \r)
                let mut line = String::from_utf8_lossy(&buf).into_owned();
                if line.ends_with('\r') {
                    line.pop();
                }
                buf.clear();
                lines.push(line);
                // termination: last line starts with the first line's code + space
                if let Some(done) = Self::reply_complete(&lines) {
                    return Ok(done);
                }
            } else {
                buf.push(byte[0]);
                if buf.len() > 8 * 1024 {
                    return Err(Error::MalformedResponse("ftp reply line too long".into()));
                }
            }
        }
    }

    fn reply_complete(lines: &[String]) -> Option<FtpReply> {
        let first = lines.first()?;
        if first.len() < 3 {
            return None;
        }
        let code = first[..3].parse::<u16>().ok()?;
        let cont = first.as_bytes().get(3) == Some(&b'-');
        if !cont {
            return Some(FtpReply { code, text: first[4..].to_string() });
        }
        let last = lines.last()?;
        if last.starts_with(&format!("{code} ")) || last.starts_with(&format!("{code}\u{a0}")) {
            Some(FtpReply {
                code,
                text: lines[1..].join("\n"),
            })
        } else {
            None // still inside multiline reply
        }
    }

    /// USER/PASS login (mirrors original anonymous default).
    pub async fn login(&mut self, user: &str, pass: &str) -> Result<()> {
        let r = self.send(&format!("USER {user}")).await?;
        match r.code {
            230 => Ok(()), // logged in without password
            331 => {
                let p = self.send(&format!("PASS {pass}")).await?;
                if p.kind() == 2 {
                    Ok(())
                } else {
                    Err(Error::Auth(format!("ftp login failed: {} {}", p.code, p.text)))
                }
            }
            _ => Err(Error::Auth(format!("ftp login failed: {} {}", r.code, r.text))),
        }
    }

    /// Explicit TLS upgrade of an already-plain control channel (AUTH TLS +
    /// PBSZ 0 + PROT P). Only valid when constructed with tls=false.
    pub async fn upgrade_tls(&mut self, host: &str) -> Result<()> {
        let r = self.send("AUTH TLS").await?;
        if r.code != 234 {
            return Err(Error::Other(format!("AUTH TLS rejected: {} {}", r.code, r.text)));
        }
        // swap out the plain channel and wrap it in TLS
        let old = self.ctrl.take().expect("control channel");
        let Conn::Plain(tcp) = old else {
            return Err(Error::Other("control channel already TLS".into()));
        };
        self.ctrl = Some(Conn::Tls(tls_connect(tcp, host).await?));
        self.tls = true;
        let r2 = self.read_reply().await?; // server may re-send banner after handshake
        if r2.kind() != 2 {
            return Err(Error::Other(format!("post-TLS greeting: {} {}", r2.code, r2.text)));
        }
        let r3 = self.send("PBSZ 0").await?;
        if r3.kind() != 2 {
            return Err(Error::Other(format!("PBSZ rejected: {} {}", r3.code, r3.text)));
        }
        let r4 = self.send("PROT P").await?;
        if r4.kind() != 2 {
            return Err(Error::Other(format!("PROT P rejected: {} {}", r4.code, r4.text)));
        }
        Ok(())
    }

    pub async fn binary_mode(&mut self) -> Result<()> {
        let r = self.send("TYPE I").await?;
        if r.kind() != 2 {
            return Err(Error::Other(format!("TYPE I rejected: {} {}", r.code, r.text)));
        }
        Ok(())
    }

    /// SIZE path → Some(len); server without SIZE (or dir path) → None.
    pub async fn size(&mut self, path: &str) -> Result<Option<u64>> {
        let r = self.send(&format!("SIZE {path}")).await?;
        if r.code == 213 {
            Ok(r.text.trim().parse().ok())
        } else {
            Ok(None)
        }
    }

    /// REST 0 probe — true when the server honors resume offsets (350).
    pub async fn rest_supported(&mut self) -> Result<bool> {
        let r = self.send("REST 0").await?;
        Ok(r.code == 350)
    }

    /// Enter passive mode; returns the (host, port) of the data channel.
    pub async fn passive(&mut self, fallback_host: &str) -> Result<(String, u16)> {
        // prefer EPSV (NAT-friendlier, IPv6 safe)
        if let Ok(r) = self.send("EPSV").await {
            if r.code == 229 {
                // "(|||port|)"
                if let Some(port) = extract_epsv_port(&r.text) {
                    return Ok((fallback_host.to_string(), port));
                }
            }
        }
        let r = self.send("PASV").await?;
        if r.code != 227 {
            return Err(Error::Other(format!("PASV rejected: {} {}", r.code, r.text)));
        }
        let (ip, port) = parse_pasv(&r.text)
            .ok_or_else(|| Error::MalformedResponse(format!("bad PASV reply: {}", r.text)))?;
        // Some servers advertise 0.0.0.0 — fall back to the control host
        let host = if ip == "0.0.0.0" { fallback_host.to_string() } else { ip };
        Ok((host, port))
    }

    /// Open the data channel, position at `start` via REST, start RETR.
    /// Returns the data stream; caller reads it to EOF then `finish_retr`.
    pub async fn retr_at(&mut self, path: &str, start: u64, ftp_host: &str) -> Result<Conn> {
        let (host, port) = self.passive(ftp_host).await?;
        let tcp = TcpStream::connect((host.as_str(), port)).await?;
        let mut data = if self.tls { Conn::Tls(tls_connect(tcp, &host).await?) } else { Conn::Plain(tcp) };

        if start > 0 {
            let r = self.send(&format!("REST {start}")).await?;
            if r.code != 350 {
                return Err(Error::Other(format!("REST {start} rejected: {} {}", r.code, r.text)));
            }
        }
        let r = self.send(&format!("RETR {path}")).await?;
        if r.kind() != 1 {
            // 4xx/5xx: release the data connection
            let _ = data.shutdown().await;
            return Err(Error::Other(format!("RETR rejected: {} {}", r.code, r.text)));
        }
        Ok(data)
    }

    /// After the data stream reaches EOF, expect 226.
    pub async fn finish_retr(&mut self) -> Result<()> {
        let r = self.read_reply().await?;
        if r.kind() == 2 {
            Ok(())
        } else {
            Err(Error::Other(format!("transfer not confirmed: {} {}", r.code, r.text)))
        }
    }

    pub async fn quit(&mut self) {
        let _ = self.send("QUIT").await;
        if let Some(mut c) = self.ctrl.take() {
            let _ = c.shutdown().await;
        }
    }
}

/// Extract the port from an EPSV reply text "(|||2121|)".
pub fn extract_epsv_port(text: &str) -> Option<u16> {
    let inner = text.split('(').nth(1)?.split(')').next()?;
    let parts: Vec<&str> = inner.split('|').filter(|s| !s.is_empty()).collect();
    parts.last()?.parse().ok()
}

/// Parse a PASV reply "227 Entering Passive Mode (h1,h2,h3,h4,p1,p2)".
pub fn parse_pasv(text: &str) -> Option<(String, u16)> {
    let inner = text.split('(').nth(1)?.split(')').next()?;
    let nums: Vec<&str> = inner.split(',').map(str::trim).collect();
    if nums.len() != 6 {
        return None;
    }
    let n: Vec<u16> = nums.iter().map(|s| s.parse().ok()).collect::<Option<_>>()?;
    let ip = format!("{}.{}.{}.{}", n[0], n[1], n[2], n[3]);
    let port = (n[4] << 8) | n[5];
    Some((ip, port))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epsv_port() {
        assert_eq!(extract_epsv_port("Entering Extended Passive Mode (|||2121|)"), Some(2121));
        assert_eq!(extract_epsv_port("no parens"), None);
    }

    #[test]
    fn pasv_parse() {
        let (ip, port) = parse_pasv("Entering Passive Mode (192,168,1,2,200,50)").unwrap();
        assert_eq!(ip, "192.168.1.2");
        assert_eq!(port, (200 << 8) | 50);
        assert!(parse_pasv("bad").is_none());
    }

    #[test]
    fn reply_complete_single() {
        let lines = vec!["220 ready".to_string()];
        let r = FtpClient::reply_complete(&lines).unwrap();
        assert_eq!(r.code, 220);
    }

    #[test]
    fn reply_complete_multiline() {
        let lines = vec![
            "220-multi".to_string(),
            "line2".to_string(),
            "220 done".to_string(),
        ];
        let r = FtpClient::reply_complete(&lines).unwrap();
        assert_eq!(r.code, 220);
        assert!(r.text.contains("line2"));
    }

    #[test]
    fn reply_incomplete() {
        let lines = vec!["150-open".to_string()];
        assert!(FtpClient::reply_complete(&lines).is_none());
    }
}
