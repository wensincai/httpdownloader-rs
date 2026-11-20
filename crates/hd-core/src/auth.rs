//! HTTP authentication — mirrors http_parsing.cpp GetAuthenticate (challenge
//! parsing) and the Authorization construction for Basic + Digest.
//!
//! Digest implements RFC 7616 with qop=auth (MD5 / SHA-256), the variants the
//! original supports via its own crypto.

use std::fmt::Write as _;

use md5::{Digest as _, Md5};
use sha2::Sha256;

/// Parsed WWW-Authenticate challenge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Challenge {
    Basic,
    Digest(DigestParams),
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DigestParams {
    pub realm: String,
    pub nonce: String,
    pub opaque: Option<String>,
    pub algorithm: Algorithm,
    pub qop: Option<Vec<String>>, // None = legacy (no qop support from server)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Algorithm {
    #[default]
    Md5,
    Sha256,
    Md5Sess,
    Sha256Sess,
}

impl Algorithm {
    fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_uppercase().as_str() {
            "MD5" => Some(Algorithm::Md5),
            "SHA-256" => Some(Algorithm::Sha256),
            "MD5-SESS" => Some(Algorithm::Md5Sess),
            "SHA-256-SESS" => Some(Algorithm::Sha256Sess),
            _ => None,
        }
    }
    fn hash_hex(&self, a: &str) -> String {
        let bytes = match self {
            Algorithm::Sha256 | Algorithm::Sha256Sess => {
                let mut h = Sha256::new();
                h.update(a.as_bytes());
                h.finalize().to_vec()
            }
            _ => {
                let mut h = Md5::new();
                h.update(a.as_bytes());
                h.finalize().to_vec()
            }
        };
        to_hex(&bytes)
    }
}

fn to_hex(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 2);
    for x in b {
        let _ = write!(s, "{x:02x}");
    }
    s
}

/// Parse a WWW-Authenticate value. Mirrors GetAuthenticate's job (simplified:
/// one challenge, Basic or Digest).
pub fn parse_challenge(value: &str) -> Option<Challenge> {
    let value = value.trim();
    if let Some(rest) = value.strip_prefix("Basic").or_else(|| value.strip_prefix("basic")) {
        let _ = rest; // realm is not needed for Basic
        return Some(Challenge::Basic);
    }
    if let Some(rest) = value.strip_prefix("Digest").or_else(|| value.strip_prefix("digest")) {
        let rest = rest.trim();
        if !rest.is_empty() && !rest.contains('=') {
            return Some(Challenge::Digest(DigestParams::default()));
        }
        let mut p = DigestParams::default();
        for kv in split_params(rest) {
            if let Some((k, v)) = kv.split_once('=') {
                let k = k.trim().to_ascii_lowercase();
                let v = v.trim().trim_matches('"').to_string();
                match k.as_str() {
                    "realm" => p.realm = v,
                    "nonce" => p.nonce = v,
                    "opaque" => p.opaque = Some(v),
                    "algorithm" => {
                        if let Some(a) = Algorithm::parse(&v) {
                            p.algorithm = a;
                        }
                    }
                    "qop" => {
                        p.qop = Some(
                            v.split(',')
                                .map(|s| s.trim().to_string())
                                .filter(|s| !s.is_empty())
                                .collect(),
                        );
                    }
                    _ => {}
                }
            }
        }
        if p.nonce.is_empty() {
            return None;
        }
        return Some(Challenge::Digest(p));
    }
    None
}

/// Split "k=v, k2="v2"" respecting quotes.
fn split_params(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    for ch in s.chars() {
        match ch {
            '"' => {
                in_quotes = !in_quotes;
                cur.push(ch);
            }
            ',' if !in_quotes => {
                out.push(cur.clone());
                cur.clear();
            }
            _ => cur.push(ch),
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur);
    }
    out
}

/// Build a Digest Authorization header value (qop=auth, or legacy when server
/// did not advertise qop). `cnonce` is passed in so tests can reproduce RFC vectors.
pub fn digest_authorization(
    p: &DigestParams,
    user: &str,
    pass: &str,
    method: &str,
    uri: &str,
    nc: u32,
    cnonce: &str,
) -> String {
    let ha1 = format!("{user}:{realm}:{pass}", realm = p.realm);
    let ha1 = match p.algorithm {
        Algorithm::Md5Sess | Algorithm::Sha256Sess => {
            format!("{}:{cnonce}", p.algorithm.hash_hex(&ha1))
        }
        _ => p.algorithm.hash_hex(&ha1),
    };
    let ha2 = p.algorithm.hash_hex(&format!("{method}:{uri}"));

    let qop = p.qop.as_ref().and_then(|q| {
        q.iter().find(|s| s.eq_ignore_ascii_case("auth")).cloned()
    });

    let nonce = &p.nonce;
    let response = match &qop {
        Some(qop_name) => {
            let nc_str = format!("{nc:08x}");
            p.algorithm
                .hash_hex(&format!("{ha1}:{nonce}:{nc_str}:{cnonce}:{qop_name}:{ha2}"))
        }
        None => p.algorithm.hash_hex(&format!("{ha1}:{nonce}:{ha2}")),
    };

    let mut out = format!(
        r#"Digest username="{user}", realm="{}", nonce="{nonce}", uri="{uri}", response="{response}", algorithm={:?}"#,
        p.realm,
        match p.algorithm {
            Algorithm::Md5 => "MD5",
            Algorithm::Sha256 => "SHA-256",
            Algorithm::Md5Sess => "MD5-SESS",
            Algorithm::Sha256Sess => "SHA-256-SESS",
        }
    );
    if let Some(op) = &p.opaque {
        let _ = write!(out, r#", opaque="{op}""#);
    }
    if let Some(qop_name) = &qop {
        let _ = write!(
            out,
            r#", qop={qop_name}, nc={nc:08x}, cnonce="{cnonce}""#
        );
    }
    out
}

/// Deterministic per-request cnonce (original uses random; determinism is
/// friendlier for golden tests).
pub fn make_cnonce(nc: u32) -> String {
    let mut h = Md5::new();
    h.update(format!("httpdownloader-rs:{nc}").as_bytes());
    to_hex(&h.finalize())
}

/// Minimal base64 (std has none).
fn base64(raw: &[u8]) -> String {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in raw.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}

/// Basic authorization header value.
pub fn basic_authorization(user: &str, pass: &str) -> String {
    format!("Basic {}", base64(format!("{user}:{pass}").as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_basic_challenge() {
        assert_eq!(parse_challenge(r#"Basic realm="x""#), Some(Challenge::Basic));
        assert_eq!(parse_challenge("Basic"), Some(Challenge::Basic));
    }

    #[test]
    fn parse_digest_challenge() {
        let v = r#"Digest realm="test", nonce="abc123", qop="auth,auth-int", algorithm=MD5, opaque="zzz""#;
        match parse_challenge(v) {
            Some(Challenge::Digest(p)) => {
                assert_eq!(p.realm, "test");
                assert_eq!(p.nonce, "abc123");
                assert_eq!(p.algorithm, Algorithm::Md5);
                assert_eq!(p.opaque.as_deref(), Some("zzz"));
                assert_eq!(p.qop, Some(vec!["auth".into(), "auth-int".into()]));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn digest_rfc2617_qop() {
        // RFC 2617 §3.5 example (qop=auth, MD5, fixed cnonce)
        let p = DigestParams {
            realm: "testrealm@host.com".into(),
            nonce: "dcd98b7102dd2f0e8b11d0f600bfb0c093".into(),
            opaque: None,
            algorithm: Algorithm::Md5,
            qop: Some(vec!["auth".into()]),
        };
        let auth = digest_authorization(
            &p,
            "Mufasa",
            "Circle Of Life",
            "GET",
            "/dir/index.html",
            1,
            "0a4f113b",
        );
        assert!(
            auth.contains(r#"response="6629fae49393a05397450978507c4ef1""#),
            "{auth}"
        );
        assert!(auth.contains(r#"nc=00000001"#), "{auth}");
        assert!(auth.contains(r#"qop=auth"#), "{auth}");
        assert!(auth.contains(r#"cnonce="0a4f113b""#), "{auth}");
    }

    #[test]
    fn digest_legacy_no_qop() {
        // Same credentials without qop: response = MD5(HA1:nonce:HA2)
        let p = DigestParams {
            realm: "testrealm@host.com".into(),
            nonce: "dcd98b7102dd2f0e8b11d0f600bfb0c093".into(),
            opaque: None,
            algorithm: Algorithm::Md5,
            qop: None,
        };
        let auth = digest_authorization(
            &p, "Mufasa", "Circle Of Life", "GET", "/dir/index.html", 1, "",
        );
        assert!(
            auth.contains(r#"response="670fd8c2df070c60b045671b8b24ff02""#),
            "{auth}"
        );
        assert!(!auth.contains("qop"), "{auth}");
    }

    #[test]
    fn basic_known_vector() {
        assert_eq!(basic_authorization("Aladdin", "open sesame"), "Basic QWxhZGRpbjpvcGVuIHNlc2FtZQ==");
    }
}
