//! Pure HTTP header parsing functions.
//!
//! Mirrors http_parsing.cpp: GetHTTPStatus / GetContentLength / GetContentRange /
//! GetLocation / GetContentEncoding / GetTransferEncoding / GetConnection /
//! GetContentDisposition / ParseHTTPHeader. All functions are total (no I/O),
//! operating on the raw header block.

/// Split a raw header block into (status_line, headers) — headers keep original casing.
pub fn split_status(raw: &[u8]) -> Option<(String, Vec<(String, String)>)> {
    let text = std::str::from_utf8(raw).ok()?;
    let mut lines = text.split("\r\n");
    let status = lines.next()?.to_string();
    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    Some((status, headers))
}

/// "HTTP/1.1 200 OK" -> 200. Mirrors GetHTTPStatus.
pub fn status_code(status_line: &str) -> Option<u16> {
    let mut it = status_line.split_ascii_whitespace();
    let _ver = it.next()?;
    it.next()?.parse().ok()
}

pub fn get_header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

/// Content-Length. Mirrors GetContentLength: absent or invalid -> None.
pub fn content_length(headers: &[(String, String)]) -> Option<u64> {
    get_header(headers, "Content-Length")?.parse().ok()
}

/// Accept-Ranges + presence of Content-Range on a 206.
/// Mirrors the original's use of Accept-Ranges: bytes before sending Range.
pub fn accept_ranges(headers: &[(String, String)]) -> bool {
    get_header(headers, "Accept-Ranges")
        .map(|v| v.eq_ignore_ascii_case("bytes"))
        .unwrap_or(false)
}

/// Parse "Content-Range: bytes 100-199/500" -> (start, end, total Option).
/// Mirrors GetContentRange.
pub fn content_range(headers: &[(String, String)]) -> Option<(u64, u64, Option<u64>)> {
    let v = get_header(headers, "Content-Range")?;
    let v = v.strip_prefix("bytes").unwrap_or(v).trim();
    let (range, total) = v.split_once('/')?;
    let (s, e) = range.trim().split_once('-')?;
    let start = s.trim().parse().ok()?;
    let end = e.trim().parse().ok()?;
    let total = match total.trim() {
        "*" => None,
        t => t.parse().ok(),
    };
    Some((start, end, total))
}

/// Location header for redirects. Mirrors GetLocation (original resolves
/// relative paths; callers do the same via `resolve_redirect`).
pub fn location(headers: &[(String, String)]) -> Option<&str> {
    get_header(headers, "Location")
}

/// Resolve a possibly-relative Location against the request URL (string level,
/// mirrors the URL surgery in HandleRedirect).
pub fn resolve_redirect(base: &crate::url::Url, loc: &str) -> Option<crate::url::Url> {
    let loc = loc.trim();
    if loc.contains("://") {
        return crate::url::Url::parse(loc).ok();
    }
    if let Some(rest) = loc.strip_prefix("//") {
        // scheme-relative
        let scheme = match base.scheme {
            crate::url::Scheme::Http => "http",
            crate::url::Scheme::Https => "https",
            crate::url::Scheme::Ftp => "ftp",
            crate::url::Scheme::Ftps => "ftps",
            crate::url::Scheme::Sftp => "sftp",
        };
        return crate::url::Url::parse(&format!("{scheme}://{rest}")).ok();
    }
    if loc.starts_with('/') {
        return crate::url::Url::parse(&format!("{}://{}:{}{}", scheme_str(base), base.host, base.port, loc)).ok();
    }
    // relative to current directory
    let dir = match base.resource.rfind('/') {
        Some(i) => &base.resource[..=i],
        None => "/",
    };
    crate::url::Url::parse(&format!(
        "{}://{}:{}{}{}",
        scheme_str(base),
        base.host,
        base.port,
        dir,
        loc
    ))
    .ok()
}

fn scheme_str(u: &crate::url::Url) -> &'static str {
    match u.scheme {
        crate::url::Scheme::Http => "http",
        crate::url::Scheme::Https => "https",
        crate::url::Scheme::Ftp => "ftp",
        crate::url::Scheme::Ftps => "ftps",
        crate::url::Scheme::Sftp => "sftp",
    }
}

/// Content-Encoding. Mirrors GetContentEncoding.
pub fn content_encoding(headers: &[(String, String)]) -> Option<String> {
    get_header(headers, "Content-Encoding").map(|s| s.to_ascii_lowercase())
}

/// Transfer-Encoding. Mirrors GetTransferEncoding.
pub fn transfer_encoding(headers: &[(String, String)]) -> Option<String> {
    get_header(headers, "Transfer-Encoding").map(|s| s.to_ascii_lowercase())
}

/// "close" per GetConnection.
pub fn connection_close(headers: &[(String, String)]) -> bool {
    get_header(headers, "Connection")
        .map(|v| v.eq_ignore_ascii_case("close"))
        .unwrap_or(false)
}

/// filename from Content-Disposition (attachment; filename="...").
/// Mirrors GetContentDisposition — returns raw token; RFC 5987 filename* handled simply.
pub fn content_disposition(headers: &[(String, String)]) -> Option<String> {
    let v = get_header(headers, "Content-Disposition")?;
    // prefer filename*= (UTF-8) then filename=
    if let Some(f) = extract_param(v, "filename*") {
        // naive RFC 5987: charset'lang'value
        let val = f.splitn(3, '\'').nth(2).unwrap_or(&f);
        return Some(percent_decode(val));
    }
    extract_param(v, "filename").map(|f| f.trim_matches('"').to_string())
}

fn extract_param(v: &str, name: &str) -> Option<String> {
    for part in v.split(';') {
        let part = part.trim();
        if let Some(rest) = part.strip_prefix(name) {
            if let Some(val) = rest.strip_prefix('=') {
                return Some(val.trim().to_string());
            }
        }
    }
    None
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(h), Some(l)) = (hex_val(b[i + 1]), hex_val(b[i + 2])) {
                out.push(h * 16 + l);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers() -> Vec<(String, String)> {
        vec![
            ("Content-Length".to_string(), "1234".to_string()),
            ("Accept-Ranges".to_string(), "bytes".to_string()),
            ("Content-Encoding".to_string(), "gzip".to_string()),
            ("Location".to_string(), "/new/place".to_string()),
        ]
    }

    #[test]
    fn status() {
        assert_eq!(status_code("HTTP/1.1 200 OK"), Some(200));
        assert_eq!(status_code("HTTP/1.0 404 Not Found"), Some(404));
        assert_eq!(status_code("garbage"), None);
    }

    #[test]
    fn headers_case_insensitive() {
        let h = headers();
        assert_eq!(get_header(&h, "content-length"), Some("1234"));
        assert_eq!(content_length(&h), Some(1234));
        assert!(accept_ranges(&h));
        assert_eq!(content_encoding(&h).as_deref(), Some("gzip"));
    }

    #[test]
    fn content_range_parse() {
        let hs = vec![("Content-Range".into(), "bytes 100-199/500".into())];
        assert_eq!(content_range(&hs), Some((100, 199, Some(500))));
        let hs2 = vec![("Content-Range".into(), "bytes 0-9/*".into())];
        assert_eq!(content_range(&hs2), Some((0, 9, None)));
    }

    #[test]
    fn redirect_resolution() {
        let base = crate::url::Url::parse("http://a.com/dir/file.zip").unwrap();
        let r = resolve_redirect(&base, "other.zip").unwrap();
        assert_eq!(r.resource, "/dir/other.zip");
        let r2 = resolve_redirect(&base, "/root.bin").unwrap();
        assert_eq!(r2.resource, "/root.bin");
        let r3 = resolve_redirect(&base, "https://b.com/x").unwrap();
        assert_eq!(r3.host, "b.com");
    }

    #[test]
    fn disposition() {
        let hs = vec![(
            "Content-Disposition".into(),
            "attachment; filename=\"my file.zip\"".into(),
        )];
        assert_eq!(content_disposition(&hs).as_deref(), Some("my file.zip"));
        let hs2 = vec![(
            "Content-Disposition".into(),
            "attachment; filename*=UTF-8''%E6%B5%8B%E8%AF%95.zip".into(),
        )];
        assert_eq!(content_disposition(&hs2).as_deref(), Some("测试.zip"));
    }
}
