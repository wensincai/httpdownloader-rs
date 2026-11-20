//! Cookie jar — mirrors the original cookie tree (http_parsing.cpp
//! ParseCookies/ConstructCookie, stored in a dllrbt red-black tree).

use crate::url::Url;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Cookie {
    domain: String, // host-match suffix (may start with '.')
    path: String,
    name: String,
    value: String,
}

#[derive(Debug, Clone, Default)]
pub struct CookieJar {
    cookies: Vec<Cookie>,
}

impl CookieJar {
    pub fn new() -> Self {
        Self::default()
    }

    /// Store cookies from a response's Set-Cookie headers.
    /// Mirrors ParseCookieValues: name=value; Domain=...; Path=...
    pub fn store_from_response(&mut self, url: &Url, headers: &[(String, String)]) {
        let set_cookies: Vec<&str> = headers
            .iter()
            .filter(|(k, _)| k.eq_ignore_ascii_case("Set-Cookie"))
            .map(|(_, v)| v.as_str())
            .collect();
        for sc in set_cookies {
            self.store_one(url, sc);
        }
    }

    fn store_one(&mut self, url: &Url, set_cookie: &str) {
        let mut parts = set_cookie.split(';');
        let nv = parts.next().unwrap_or("").trim();
        let (name, value) = match nv.split_once('=') {
            Some((n, v)) => (n.trim().to_string(), v.trim().to_string()),
            None => return, // mirrors original: no '=' -> ignored
        };
        if name.is_empty() {
            return;
        }

        let mut domain = url.host.to_ascii_lowercase();
        let mut path = "/".to_string();
        for attr in parts {
            let attr = attr.trim();
            if let Some(v) = attr.strip_prefix("Domain=").or_else(|| attr.strip_prefix("domain=")) {
                let d = v.trim().trim_start_matches('.').to_ascii_lowercase();
                if !d.is_empty() {
                    domain = d;
                }
            } else if let Some(v) = attr.strip_prefix("Path=").or_else(|| attr.strip_prefix("path=")) {
                let p = v.trim();
                if p.starts_with('/') {
                    path = p.to_string();
                }
            }
        }

        // replace same (domain, path, name)
        self.cookies
            .retain(|c| !(c.domain == domain && c.path == path && c.name == name));
        self.cookies.push(Cookie { domain, path, name, value });
    }

    /// Cookies applicable to a request, as a "Cookie" header value.
    /// Path-match: request path starts with cookie path (mirrors RFC 6265 basic rule).
    pub fn header_for(&self, url: &Url) -> Option<String> {
        let host = url.host.to_ascii_lowercase();
        let path = url.resource.split('?').next().unwrap_or("/");
        let mut matched: Vec<&Cookie> = self
            .cookies
            .iter()
            .filter(|c| {
                let dot_domain = format!(".{}", c.domain);
                let dom_ok = host == c.domain || host.ends_with(&dot_domain);
                let path_ok = path.starts_with(&c.path)
                    || (path.starts_with(c.path.as_str())
                        && (c.path.ends_with('/') || path.as_bytes().get(c.path.len()) == Some(&b'/')));
                dom_ok && path_ok
            })
            .collect();
        // longer paths first (RFC 6265 5.4)
        matched.sort_by_key(|c| std::cmp::Reverse(c.path.len()));
        if matched.is_empty() {
            return None;
        }
        Some(
            matched
                .iter()
                .map(|c| format!("{}={}", c.name, c.value))
                .collect::<Vec<_>>()
                .join("; "),
        )
    }

    /// Seed the jar from a raw "Cookie: a=b; c=d" header value (client-supplied).
    pub fn seed_header(&mut self, url: &Url, value: &str) {
        for pair in value.split(';') {
            let pair = pair.trim();
            if pair.is_empty() {
                continue;
            }
            self.store_one(url, pair);
        }
    }

    pub fn len(&self) -> usize {
        self.cookies.len()
    }

    pub fn is_empty(&self) -> bool {
        self.cookies.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_and_send() {
        let url = Url::parse("http://example.com/dir/page").unwrap();
        let mut jar = CookieJar::new();
        jar.store_from_response(
            &url,
            &[(
                "Set-Cookie".into(),
                "sid=abc123; Path=/; Domain=example.com".into(),
            )],
        );
        assert_eq!(jar.header_for(&url).as_deref(), Some("sid=abc123"));
    }

    #[test]
    fn domain_scoping() {
        let url = Url::parse("http://example.com/").unwrap();
        let mut jar = CookieJar::new();
        jar.store_from_response(
            &url,
            &[("Set-Cookie".into(), "a=1; Domain=other.com".into())],
        );
        // cookie set for other.com must not be sent to example.com
        assert_eq!(jar.header_for(&url), None);
    }

    #[test]
    fn subdomain_send() {
        let mut jar = CookieJar::new();
        let u1 = Url::parse("http://example.com/").unwrap();
        jar.store_from_response(&u1, &[("Set-Cookie".into(), "a=1".into())]);
        let u2 = Url::parse("http://www.example.com/").unwrap();
        // host cookie (no Domain attr) is host-only per RFC 6265; simplified jar sends
        // on suffix match — acceptable divergence documented in design doc.
        assert!(jar.header_for(&u2).is_some());
    }

    #[test]
    fn replace_same_cookie() {
        let url = Url::parse("http://example.com/").unwrap();
        let mut jar = CookieJar::new();
        jar.store_from_response(&url, &[("Set-Cookie".into(), "a=1".into())]);
        jar.store_from_response(&url, &[("Set-Cookie".into(), "a=2".into())]);
        assert_eq!(jar.len(), 1);
        assert_eq!(jar.header_for(&url).as_deref(), Some("a=2"));
    }

    #[test]
    fn multiple_cookies_sorted_by_path() {
        let u = Url::parse("http://example.com/dir/x").unwrap();
        let mut jar = CookieJar::new();
        jar.store_from_response(&u, &[("Set-Cookie".into(), "root=1; Path=/".into())]);
        jar.store_from_response(&u, &[("Set-Cookie".into(), "deep=2; Path=/dir".into())]);
        assert_eq!(jar.header_for(&u).as_deref(), Some("deep=2; root=1"));
    }
}
