//! HTTP/1.1 client — mirrors the HTTP branch of `IOCPConnection` (connection.cpp)
//! plus `ParseHTTPHeader`/`GetHTTPHeader`/`HandleRedirect` (http_parsing.cpp).
//!
//! M1 scope: single connection GET/POST, redirects (max 10, mirrors original),
//! gzip/deflate + chunked decode, range requests, connection close semantics.

use std::time::Duration;

use flate2::read::{DeflateDecoder, GzDecoder};
use std::io::Read;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::error::{Error, Result};
use crate::http::header as hdr;
use crate::url::{Scheme, Url};

const MAX_REDIRECTS: usize = 10; // mirrors original redirect cap
const MAX_HEADER: usize = 64 * 1024; // generous; original caps its header buffers too
const READ_CHUNK: usize = 16 * 1024;

pub struct ResponseMeta {
    pub status: u16,
    pub headers: Vec<(String, String)>,
}

/// Progress sink: (absolute_offset, data).
pub type OnData<'a> = &'a mut (dyn FnMut(u64, &[u8]) + Send);

/// Unified connection so TLS and plain TCP share one code path.
#[allow(clippy::large_enum_variant)]
pub enum Conn {
    Plain(TcpStream),
    /// rustls TLS stream over a plain TCP socket (direct or proxied).
    Tls(tokio_rustls::client::TlsStream<TcpStream>),
}

impl Conn {
    pub fn is_tls(&self) -> bool {
        matches!(self, Conn::Tls(_))
    }
}

impl AsyncRead for Conn {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match &mut *self {
            Conn::Plain(s) => std::pin::Pin::new(s).poll_read(cx, buf),
            Conn::Tls(s) => std::pin::Pin::new(s).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for Conn {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        data: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        match &mut *self {
            Conn::Plain(s) => std::pin::Pin::new(s).poll_write(cx, data),
            Conn::Tls(s) => std::pin::Pin::new(s).poll_write(cx, data),
        }
    }
    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match &mut *self {
            Conn::Plain(s) => std::pin::Pin::new(s).poll_flush(cx),
            Conn::Tls(s) => std::pin::Pin::new(s).poll_flush(cx),
        }
    }
    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match &mut *self {
            Conn::Plain(s) => std::pin::Pin::new(s).poll_shutdown(cx),
            Conn::Tls(s) => std::pin::Pin::new(s).poll_shutdown(cx),
        }
    }
}

/// Perform a GET (or POST if `body` given) and stream the response body into `sink`
/// (called with absolute file offset). Follows redirects; returns final meta.
pub async fn request_stream(
    url: &Url,
    method: &str,
    body: Option<&[u8]>,
    extra_headers: &[(String, String)],
    range: Option<(u64, u64)>, // inclusive start, exclusive end
    proxy: Option<&crate::proxy::ProxyConfig>,
    sink: OnData<'_>,
) -> Result<ResponseMeta> {
    let mut current = url.clone();
    let mut redirects = 0usize;

    loop {
        let (meta, mut stream) =
            connect_and_send(&current, method, body, extra_headers, range, proxy).await?;

        match meta.status {
            301 | 302 | 303 | 307 | 308 => {
                redirects += 1;
                if redirects > MAX_REDIRECTS {
                    return Err(Error::TooManyRedirects { url: current.to_string() });
                }
                if let Some(loc) = hdr::location(&meta.headers) {
                    let next = hdr::resolve_redirect(&current, loc)
                        .ok_or_else(|| Error::MalformedResponse(format!("bad Location: {loc}")))?;
                    tracing::info!(from = %current, to = %next, "redirect");
                    current = next;
                    continue;
                }
                return Ok(meta);
            }
            s if (500..=599).contains(&s) => {
                // mirror original: 5xx is retryable — caller (engine) decides; surface it
                return Err(Error::HttpStatus { status: s, url: current.to_string() });
            }
            _ => {}
        }

        // stream body (handles content-encoding + chunked)
        let encoding = hdr::content_encoding(&meta.headers);
        let chunked = hdr::transfer_encoding(&meta.headers)
            .map(|t| t.contains("chunked"))
            .unwrap_or(false);

        let start_offset = range.map(|(s, _)| s).unwrap_or(0);
        let mut offset = start_offset;

        let mut raw = Vec::with_capacity(READ_CHUNK);
        let mut buf = vec![0u8; READ_CHUNK];
        loop {
            let n = stream.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            raw.extend_from_slice(&buf[..n]);
            if chunked {
                let (decoded, consumed, done) = decode_chunked(&raw);
                if consumed > 0 {
                    raw.drain(..consumed);
                    if !decoded.is_empty() {
                        let out = maybe_decompress(&decoded, encoding.as_deref())?;
                        let off = offset;
                        offset += out.len() as u64;
                        sink(off, &out);
                    }
                }
                if done {
                    break;
                }
            } else {
                let out = maybe_decompress(&raw, encoding.as_deref())?;
                raw.clear();
                if !out.is_empty() {
                    let off = offset;
                    offset += out.len() as u64;
                    sink(off, &out);
                }
            }
        }
        if !chunked && !raw.is_empty() {
            let out = maybe_decompress(&raw, encoding.as_deref())?;
            if !out.is_empty() {
                sink(offset, &out);
            }
        }
        return Ok(meta);
    }
}

/// Decompress a body chunk if Content-Encoding present.
fn maybe_decompress(data: &[u8], encoding: Option<&str>) -> Result<Vec<u8>> {
    match encoding {
        Some("gzip") => {
            let mut d = GzDecoder::new(data);
            let mut out = Vec::new();
            // tolerate partial member (streaming chunk); errors keep what we got
            let _ = d.read_to_end(&mut out);
            Ok(out)
        }
        Some("deflate") => {
            let mut d = DeflateDecoder::new(data);
            let mut out = Vec::new();
            let _ = d.read_to_end(&mut out);
            Ok(out)
        }
        _ => Ok(data.to_vec()),
    }
}

/// Minimal chunked transfer decoding: returns (decoded_bytes, consumed_input, is_final).
fn decode_chunked(raw: &[u8]) -> (Vec<u8>, usize, bool) {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while let Some(i) = find_crlf(&raw[pos..]) {
        let line_end = pos + i;
        let line = String::from_utf8_lossy(&raw[pos..line_end]);
        let size_str = line.split(';').next().unwrap_or("").trim();
        let size = match usize::from_str_radix(size_str, 16) {
            Ok(s) => s,
            Err(_) => break,
        };
        pos = line_end + 2;
        if size == 0 {
            return (out, pos.min(raw.len()), true);
        }
        if raw.len() < pos + size + 2 {
            break; // incomplete chunk
        }
        out.extend_from_slice(&raw[pos..pos + size]);
        pos += size + 2; // chunk data + CRLF
    }
    (out, pos, false)
}

fn find_crlf(b: &[u8]) -> Option<usize> {
    b.windows(2).position(|w| w == b"\r\n")
}

/// Open TCP (optionally via proxy, optionally TLS) connection, send request,
/// read full header block.
async fn connect_and_send(
    url: &Url,
    method: &str,
    body: Option<&[u8]>,
    extra: &[(String, String)],
    range: Option<(u64, u64)>,
    proxy: Option<&crate::proxy::ProxyConfig>,
) -> Result<(ResponseMeta, Conn)> {
    let tcp = match proxy {
        Some(cfg) => crate::proxy::connect(cfg, &url.host, url.port).await?,
        None => TcpStream::connect((url.host.as_str(), url.port)).await?,
    };
    let mut stream = match url.scheme {
        Scheme::Http => Conn::Plain(tcp),
        Scheme::Https => Conn::Tls(tls_connect(tcp, &url.host).await?),
        _ => return Err(Error::InvalidUrl(format!("non-http scheme in http client: {url}"))),
    };

    let mut req = format!(
        "{} {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: httpdownloader-rs/{}\r\nAccept: */*\r\nConnection: close\r\n",
        method,
        url.resource,
        url.host_header(),
        env!("CARGO_PKG_VERSION")
    );
    if let Some((s, e)) = range {
        if e != u64::MAX {
            req.push_str(&format!("Range: bytes={}-{}\r\n", s, e - 1));
        } else {
            req.push_str(&format!("Range: bytes={}-\r\n", s));
        }
    }
    for (k, v) in extra {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    stream.write_all(req.as_bytes()).await?;
    if let Some(b) = body {
        stream.write_all(b).await?;
    }
    stream.flush().await?;

    // read until blank line
    let mut header_buf = Vec::with_capacity(4096);
    let mut byte = [0u8; 1];
    loop {
        let n = stream.read(&mut byte).await?;
        if n == 0 {
            return Err(Error::MalformedResponse("connection closed during header".into()));
        }
        header_buf.push(byte[0]);
        if header_buf.len() > MAX_HEADER {
            return Err(Error::MalformedResponse("header too large".into()));
        }
        if header_buf.ends_with(b"\r\n\r\n") || header_buf.ends_with(b"\n\n") {
            break;
        }
    }
    let (status_line, headers) =
        hdr::split_status(&header_buf).ok_or_else(|| Error::MalformedResponse("bad header".into()))?;
    let status = hdr::status_code(&status_line)
        .ok_or_else(|| Error::MalformedResponse(format!("bad status line: {status_line}")))?;

    Ok((ResponseMeta { status, headers }, stream))
}

/// TLS handshake over a raw TCP socket (direct or tunneled through a proxy).
/// Shared with the FTP(S) client for control/data channel upgrades.
pub(crate) async fn tls_connect(
    tcp: TcpStream,
    host: &str,
) -> Result<tokio_rustls::client::TlsStream<TcpStream>> {    let mut roots = rustls::RootCertStore::empty();
    let loaded = rustls_native_certs::load_native_certs();
    for cert in loaded.certs {
        let _ = roots.add(cert);
    }
    let config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let server = rustls::pki_types::ServerName::try_from(host.to_string())
        .map_err(|_| Error::InvalidUrl(format!("bad tls name: {host}")))?;
    let conn = tokio_rustls::TlsConnector::from(std::sync::Arc::new(config))
        .connect(server, tcp)
        .await?;
    Ok(conn)
}

/// HEAD request returning headers (used to probe size/accept-ranges).
pub async fn probe(
    url: &Url,
    extra: &[(String, String)],
    proxy: Option<&crate::proxy::ProxyConfig>,
) -> Result<ResponseMeta> {
    let (meta, _stream) = connect_and_send(url, "HEAD", None, extra, None, proxy).await?;
    Ok(meta)
}

/// Sleep helper used by engine retry backoff (mirrors original backoff growth).
pub async fn backoff(attempt: u8) {
    let d = Duration::from_millis(250u64 << attempt.min(5));
    tokio::time::sleep(d).await;
}
