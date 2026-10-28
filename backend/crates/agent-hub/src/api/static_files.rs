//! main.py `_mount_frontend`: static files at `/` with an SPA fallback to index.html. Serves ONLY index.html and
//! css/, js/, i18n/ (no `..`/symlink escape), a missing asset is a 404 (never the SPA), `/api/...` is a 404 problem.
//! Every file is served `Cache-Control: no-cache` with Starlette's FileResponse headers (content-length, last-modified,
//! etag = md5("<st_mtime>-<st_size>"), accept-ranges) and a matching `If-None-Match` answers 304. `Range` requests are
//! answered like Starlette's FileResponse: 206 for one range or several (`multipart/byteranges`), 400 for a malformed
//! header, 416 for a start past the end, the whole file for a stale `If-Range` or over 100 ranges.
//! Security control S-STATIC. Owner: CORE (LOCKED).

use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::Arc;

use axum::body::Body;
use http::{HeaderValue, Response, StatusCode, header};
use md5::Digest;

use super::errors::problem_parts;
use super::{AppState, Req};
use hub_core::py::{float, os::realpath};

pub const STATIC_DIRS: [&str; 3] = ["css", "js", "i18n"];
pub const STATIC_CACHE: &str = "no-cache";

fn not_found(detail: &str) -> Response<Body> {
    problem_parts(404, "not_found", "not found", detail)
}

/// `mimetypes.guess_type` for the files a frontend has (Starlette adds `; charset=utf-8` to text/*).
pub fn content_type(path: &Path) -> String {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let t = match ext.as_str() {
        "html" | "htm" => "text/html",
        "css" => "text/css",
        "js" | "mjs" => "text/javascript",
        "json" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "ico" => "image/vnd.microsoft.icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "md" => "text/markdown",
        _ => "text/plain",
    };
    if t.starts_with("text/") {
        format!("{t}; charset=utf-8")
    } else {
        t.to_string()
    }
}

/// Starlette `FileResponse.max_ranges`: a header asking for more ranges is answered with the whole file.
pub const MAX_RANGES: usize = 100;

/// The result of Starlette's `FileResponse._parse_range_header(http_range, file_size)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RangeOutcome {
    /// `[start, end)` byte ranges, sorted and merged when there are several; empty = serve the whole file (200).
    Ranges(Vec<(u64, u64)>),
    /// `MalformedRangeHeader(content)`: 400 with this text body.
    Malformed(&'static str),
    /// `RangeNotSatisfiable`: 416 with `Content-Range: bytes */<size>`.
    Unsatisfiable,
}

/// Starlette `FileResponse._parse_ranges`: parts that are empty, `-`, dash-less or not integers are skipped.
fn parse_ranges(range_: &str, size: i128) -> Vec<(i128, i128)> {
    use hub_core::py::text::{int_str, strip};
    let mut out = Vec::new();
    for part in range_.split(',') {
        let part = strip(part);
        if part.is_empty() || part == "-" {
            continue;
        }
        let Some((s, e)) = part.split_once('-') else { continue };
        let (s, e) = (strip(s), strip(e));
        let start = if s.is_empty() {
            match int_str(e) {
                Some(n) => size.saturating_sub(n).max(0),
                None => continue,
            }
        } else {
            match int_str(s) {
                Some(n) => n,
                None => continue,
            }
        };
        let end = if !s.is_empty() && !e.is_empty() {
            match int_str(e) {
                Some(n) if n < size => n + 1,
                Some(_) => size,
                None => continue,
            }
        } else {
            size
        };
        out.push((start, end));
    }
    out
}

/// Starlette `FileResponse._parse_range_header` (golden-tested against Starlette: tests/static_ranges.rs).
pub fn parse_range_header(http_range: &str, size: u64) -> RangeOutcome {
    let Some((units, range_)) = http_range.split_once('=') else {
        return RangeOutcome::Malformed("Malformed range header.");
    };
    if hub_core::py::text::strip(units).to_lowercase() != "bytes" {
        return RangeOutcome::Malformed("Only support bytes range");
    }
    if range_.matches(',').count() + 1 > MAX_RANGES {
        return RangeOutcome::Ranges(Vec::new());
    }
    let size_i = i128::from(size);
    let mut ranges = parse_ranges(range_, size_i);
    if ranges.is_empty() {
        return RangeOutcome::Malformed("Range header: range must be requested");
    }
    if ranges.iter().any(|(s, _)| !(0..size_i).contains(s)) {
        return RangeOutcome::Unsatisfiable;
    }
    if ranges.iter().any(|(s, e)| s >= e) {
        return RangeOutcome::Malformed("Range header: start must be less than end");
    }
    if ranges.len() > 1 {
        ranges.sort_unstable();
        let mut merged: Vec<(i128, i128)> = Vec::with_capacity(ranges.len());
        for (s, e) in ranges {
            match merged.last_mut() {
                Some(last) if s <= last.1 => last.1 = last.1.max(e),
                _ => merged.push((s, e)),
            }
        }
        ranges = merged;
    }
    // every start is in 0..size and every end in start+1..=size here, so the conversions cannot fail
    RangeOutcome::Ranges(
        ranges
            .into_iter()
            .map(|(s, e)| (u64::try_from(s).unwrap_or(0), u64::try_from(e).unwrap_or(0)))
            .collect(),
    )
}

/// The `multipart/byteranges` body of Starlette's `_handle_multiple_ranges` (`generate_multipart` + the parts).
pub fn multipart_body(data: &[u8], ranges: &[(u64, u64)], boundary: &str, size: u64, ctype: &str) -> Vec<u8> {
    let mut out = Vec::new();
    for (s, e) in ranges {
        out.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Type: {ctype}\r\nContent-Range: bytes {s}-{}/{size}\r\n\r\n",
                e - 1
            )
            .as_bytes(),
        );
        out.extend_from_slice(slice(data, *s, *e));
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(format!("--{boundary}--").as_bytes());
    out
}

fn slice(data: &[u8], s: u64, e: u64) -> &[u8] {
    let s = usize::try_from(s).unwrap_or(usize::MAX);
    let e = usize::try_from(e).unwrap_or(usize::MAX).min(data.len());
    data.get(s..e).unwrap_or(&[])
}

fn set(h: &mut http::HeaderMap, name: header::HeaderName, value: &str) {
    if let Ok(v) = HeaderValue::from_str(value) {
        h.insert(name, v);
    }
}

/// `PlainTextResponse(content, status_code, headers)`.
fn plain(status: u16, text: &str, content_range: Option<String>) -> Response<Body> {
    let mut r = super::errors::raw(status, "text/plain; charset=utf-8", text.as_bytes().to_vec());
    if let Some(cr) = content_range {
        set(r.headers_mut(), header::CONTENT_RANGE, &cr);
    }
    r
}

/// Starlette `FileResponse` (whole file, one range or several) + the 304 short-cut of `_file_response`.
pub fn file_response(req: &Req, target: &Path) -> Response<Body> {
    let Ok(meta) = std::fs::metadata(target) else {
        return not_found("no such file");
    };
    #[allow(clippy::cast_precision_loss)]
    let mtime = meta.mtime() as f64 + meta.mtime_nsec() as f64 * 1e-9;
    let size = meta.len();
    let etag = format!(
        "\"{}\"",
        hex::encode(md5::Md5::digest(format!("{}-{size}", float::repr(mtime)).as_bytes()))
    );
    let sent: Vec<String> = req
        .header("if-none-match")
        .unwrap_or_default()
        .split(',')
        .map(|t| t.trim().trim_start_matches("W/").to_string())
        .collect();
    if sent.contains(&etag) {
        let mut r = Response::new(Body::empty());
        *r.status_mut() = StatusCode::NOT_MODIFIED;
        set(r.headers_mut(), header::ETAG, &etag);
        r.headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static(STATIC_CACHE));
        return r;
    }
    let lm = httpdate::fmt_http_date(
        std::time::UNIX_EPOCH + std::time::Duration::from_secs(u64::try_from(meta.mtime()).unwrap_or(0)),
    );
    let ctype = content_type(target);
    // Range / If-Range: a stale If-Range (neither the Last-Modified nor the ETag) gets the whole file
    let mut ranges: Vec<(u64, u64)> = Vec::new();
    if let Some(hr) = req.header("range")
        && req.header("if-range").is_none_or(|ir| ir == lm || ir == etag)
    {
        match parse_range_header(&hr, size) {
            RangeOutcome::Malformed(msg) => return plain(400, msg, None),
            RangeOutcome::Unsatisfiable => return plain(416, "", Some(format!("bytes */{size}"))),
            RangeOutcome::Ranges(v) => ranges = v,
        }
    }
    let Ok(data) = std::fs::read(target) else {
        return not_found("no such file");
    };
    let (status, body, body_ctype, content_range) = match ranges.as_slice() {
        [] => (StatusCode::OK, data, ctype, None),
        [(s, e)] => (
            StatusCode::PARTIAL_CONTENT,
            slice(&data, *s, *e).to_vec(),
            ctype,
            Some(format!("bytes {s}-{}/{size}", e - 1)),
        ),
        many => {
            let boundary = hub_core::common::token_hex(13);
            let body = multipart_body(&data, many, &boundary, size, &ctype);
            (
                StatusCode::PARTIAL_CONTENT,
                body,
                format!("multipart/byteranges; boundary={boundary}"),
                None,
            )
        }
    };
    let len = body.len();
    let mut r = Response::new(Body::from(body));
    *r.status_mut() = status;
    let h = r.headers_mut();
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static(STATIC_CACHE));
    set(h, header::CONTENT_TYPE, &body_ctype);
    h.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    set(h, header::CONTENT_LENGTH, &len.to_string());
    set(h, header::LAST_MODIFIED, &lm);
    set(h, header::ETAG, &etag);
    if let Some(cr) = content_range {
        set(h, header::CONTENT_RANGE, &cr);
    }
    r
}

/// GET /{path:path}.
pub async fn spa(s: Arc<AppState>, r: Req) -> Response<Body> {
    let Some(root) = s.frontend.clone() else {
        return not_found("frontend not built");
    };
    let path = r.param("path");
    if path.starts_with("api/") || path == "api" {
        return not_found("no such API route");
    }
    let top = path.split('/').next().unwrap_or("");
    if STATIC_DIRS.contains(&top) && path.contains('/') {
        let base = realpath(&root.join(top));
        let target = realpath(&root.join(&path));
        if target.is_file() && target.starts_with(&base) && target != base {
            return file_response(&r, &target);
        }
        return not_found("no such file");
    }
    let last = path.rsplit('/').next().unwrap_or("");
    if last.contains('.') && path != "index.html" {
        return not_found("no such file");
    }
    let index = root.join("index.html");
    if index.is_file() {
        return file_response(&r, &index);
    }
    not_found("frontend not built")
}
