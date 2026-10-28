//! Port of security.py: the hardening layer around every request. Host allowlist (DNS rebinding, 421), empty / dot /
//! backslash / NUL path segments (400), same-origin `Origin` check and the custom CSRF header on non-GET requests from
//! non-bearer callers (403), the Content-Length body cap (413; the streaming cap is applied while reading), strict
//! response headers (replacing whatever the handler set), no CORS, metrics and a metadata-only audit log. It never
//! trusts the peer address.

use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use hub_core::PyValue;

use crate::config::Config;
use crate::pyjson::{self, Style, obj};

pub const SAFE: [&str; 3] = ["GET", "HEAD", "OPTIONS"];
pub const MUTATING: [&str; 4] = ["POST", "PUT", "PATCH", "DELETE"];
pub const CSRF_HEADER: &str = "x-agent-knowledge";
pub const CSP: &str = "default-src 'none'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";
pub const SECURITY_HEADERS: [(&str, &str); 6] = [
    ("content-security-policy", CSP),
    ("x-content-type-options", "nosniff"),
    ("referrer-policy", "no-referrer"),
    ("x-frame-options", "DENY"),
    ("cache-control", "no-store"),
    ("cross-origin-resource-policy", "same-origin"),
];

/// The allowed `Host` values (lowercased): loopback names and the configured host on the configured port, plus
/// `allowed_hosts`.
pub fn allowed_hosts(cfg: &Config) -> Vec<String> {
    let p = cfg.port;
    let mut hosts: Vec<String> = [
        format!("127.0.0.1:{p}"),
        format!("localhost:{p}"),
        format!("[::1]:{p}"),
        format!("{}:{p}", cfg.host),
    ]
    .iter()
    .map(|h| h.to_lowercase())
    .collect();
    hosts.extend(cfg.allowed_hosts.iter().map(|h| h.to_lowercase()));
    hosts
}

/// An exception raised inside the middleware (Python answers it with ServerErrorMiddleware's bare 500).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MiddlewareError;

/// `urllib.parse.urlsplit(url).netloc` (Python 3.13): C0 controls and spaces stripped in front, tab/CR/LF removed,
/// an optional `scheme:`, then the `//netloc` part up to `/`, `?` or `#`. `Err` = Python's ValueError for unbalanced
/// IPv6 brackets (an exception in the middleware: a 500).
pub fn netloc(url: &str) -> Result<String, MiddlewareError> {
    let url: String = url
        .trim_start_matches(|c: char| c <= ' ')
        .chars()
        .filter(|c| !matches!(c, '\t' | '\r' | '\n'))
        .collect();
    let rest = match url.find(':') {
        Some(i)
            if i > 0
                && url[..i].chars().next().is_some_and(|c| c.is_ascii_alphabetic())
                && url[..i].chars().all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c)) =>
        {
            &url[i + 1..]
        }
        _ => url.as_str(),
    };
    let Some(r) = rest.strip_prefix("//") else {
        return Ok(String::new());
    };
    let end = r.find(['/', '?', '#']).unwrap_or(r.len());
    let n = &r[..end];
    if n.contains('[') != n.contains(']') {
        return Err(MiddlewareError);
    }
    if n.contains('[') {
        check_bracketed_netloc(n)?;
    }
    Ok(n.to_string())
}

/// `urllib.parse._check_bracketed_netloc`: nothing before `[`, only `:port` after `]`, and the bracketed host is an
/// IPv6 address or a valid IPvFuture literal.
fn check_bracketed_netloc(netloc: &str) -> Result<(), MiddlewareError> {
    let host_port = netloc.rsplit_once('@').map_or(netloc, |(_, h)| h);
    let Some((before, bracketed)) = host_port.split_once('[') else {
        return Ok(());
    };
    if !before.is_empty() {
        return Err(MiddlewareError);
    }
    let (hostname, port) = bracketed.split_once(']').unwrap_or((bracketed, ""));
    if !port.is_empty() && !port.starts_with(':') {
        return Err(MiddlewareError);
    }
    if let Some(rest) = hostname.strip_prefix('v') {
        let (hex, tail) = rest.split_once('.').unwrap_or((rest, ""));
        if hex.is_empty() || !hex.chars().all(|c| c.is_ascii_hexdigit()) || tail.is_empty() {
            return Err(MiddlewareError);
        }
        return Ok(());
    }
    // ipaddress.ip_address(): IPv6 only (an IPv4 address cannot be in brackets); a %scope suffix is allowed
    let addr = hostname.split_once('%').map_or(hostname, |(a, _)| a);
    addr.parse::<std::net::Ipv6Addr>()
        .map(|_| ())
        .map_err(|_| MiddlewareError)
}

/// `_precheck(scope, headers)`: `Some((status, code, detail))` to refuse; `Err` = an exception (500).
#[allow(clippy::type_complexity)]
pub fn precheck(
    cfg: &Config,
    hosts: &[String],
    method: &str,
    path: &str,
    headers: &[(String, String)],
) -> Result<Option<(u16, &'static str, &'static str)>, MiddlewareError> {
    // the middleware's header dict: lowercase names, the LAST value wins
    let get = |name: &str| headers.iter().rev().find(|(k, _)| k == name).map(|(_, v)| v.as_str());
    let host = get("host").unwrap_or("").to_lowercase();
    if !hosts.contains(&host) {
        return Ok(Some((421, "misdirected_request", "unrecognised Host header")));
    }
    let segs: Vec<&str> = path.split('/').skip(1).collect();
    let empty_inside = segs.len() > 1 && segs[..segs.len() - 1].contains(&"");
    if empty_inside || segs.contains(&".") || segs.contains(&"..") || path.contains('\\') || path.contains('\0') {
        return Ok(Some((
            400,
            "invalid_path",
            "empty, dot or backslash path segments are not allowed",
        )));
    }
    if !SAFE.contains(&method) {
        if let Some(origin) = get("origin")
            && (origin == "null" || netloc(origin)?.to_lowercase() != host)
        {
            return Ok(Some((403, "cross_origin", "cross-origin requests are not allowed")));
        }
        // Bearer callers hold no ambient credential a browser could replay; everyone else needs the custom header,
        // which a cross-site page cannot send without a CORS preflight (never granted).
        if get("authorization").is_none() && get(CSRF_HEADER) != Some("1") {
            return Ok(Some((
                403,
                "csrf_header_required",
                "send the X-Agent-Knowledge: 1 header on non-GET requests",
            )));
        }
        if let Some(cl) = get("content-length")
            && !cl.is_empty()
            && cl.chars().all(|c| c.is_ascii_digit())
            && cl
                .parse::<u128>()
                .map_or(true, |n| n > u128::try_from(cfg.body_cap_bytes.max(0)).unwrap_or(0))
        {
            return Ok(Some((413, "payload_too_large", "request body too large")));
        }
    }
    Ok(None)
}

/// `_audit(method, path, status, who, size)`: metadata only, never the body (documents and queries are private).
pub fn audit(data_dir: &Path, method: &str, path: &str, status: u16, who: &str, size: u64) {
    let ts = hub_core::common::now();
    let ts = format!("{ts:.3}").parse::<f64>().unwrap_or(ts); // round(time.time(), 3)
    let rec = obj([
        ("ts", PyValue::Float(ts)),
        ("who", PyValue::str(who)),
        ("method", PyValue::str(method)),
        ("path", PyValue::Str(path.chars().take(200).collect())),
        ("status", PyValue::Int(i64::from(status))),
        ("bytes", PyValue::Int(i64::try_from(size).unwrap_or(i64::MAX))),
    ]);
    let line = format!("{}\n", pyjson::dumps(&rec, Style::Std));
    let r = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .open(data_dir.join("audit.jsonl"))
        .and_then(|mut f| f.write_all(line.as_bytes()));
    if r.is_err() {
        tracing::error!("audit write failed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn netloc_like_urlsplit() {
        assert_eq!(netloc("http://Evil.example:80/x"), Ok("Evil.example:80".into()));
        assert_eq!(netloc("null"), Ok(String::new()));
        assert_eq!(netloc("  http://127.0.0.1:8795"), Ok("127.0.0.1:8795".into()));
        assert_eq!(netloc("//h:1"), Ok("h:1".into()));
        assert_eq!(netloc("http://[::1"), Err(MiddlewareError));
        assert_eq!(netloc("http://a\tb/"), Ok("ab".into()));
        assert_eq!(netloc("http://[::1]:8795"), Ok("[::1]:8795".into()));
        assert_eq!(netloc("http://[127.0.0.1]"), Err(MiddlewareError));
        assert_eq!(netloc("http://x[::1]"), Err(MiddlewareError));
    }
}
