//! HTTP Range（RFC 9110 §14）文件响应：按需分段 + 流式发送。
//!
//! 动机（借鉴 booklore，见 docs/audit/2026-08-27-booklore对比-导入与阅读借鉴.md）：
//! 原先本地书原文件下载是 `std::fs::read` 整本进内存再 `Body::from(bytes)`——
//! 一个 500MB 的 PDF 就是 500MB 常驻，并发几路直接把内存打满；
//! 且客户端（KOReader、pdf.js、支持断点续传的下载器）**无法只取需要的那几百 KB**。
//!
//! 本模块提供：
//! - 无 `Range` 头 → 200 + `Accept-Ranges: bytes`，**流式**发送（不整本进内存）
//! - 单段 `Range: bytes=a-b` / `bytes=a-` / `bytes=-n` → 206 + `Content-Range`
//! - 越界或无法满足 → 416 + `Content-Range: bytes */len`
//! - 多段 range（`bytes=0-9,20-29`）→ 按 RFC 允许的做法**降级为 200 整文件**，
//!   不实现 multipart/byteranges（客户端极少用，实现成本与出错面都不划算）

use axum::body::Body;
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;

/// 解析后的单段区间（闭区间，均为字节偏移）
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ByteRange {
    pub start: u64,
    /// 含末字节
    pub end: u64,
}

/// Range 头解析结果
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RangeSpec {
    /// 无 Range 头，或多段/不认识的单位 → 整文件
    Full,
    /// 单段
    Single(ByteRange),
    /// 语法合法但无法满足（起点越界）→ 416
    Unsatisfiable,
}

/// 解析 `Range` 头。`total` 为文件总长度。
///
/// 只认 `bytes=` 单位。三种形态：
/// - `bytes=a-b`：a..=min(b, total-1)
/// - `bytes=a-`：a..=total-1
/// - `bytes=-n`：最后 n 字节
pub(crate) fn parse_range(headers: &HeaderMap, total: u64) -> RangeSpec {
    let Some(raw) = headers
        .get(axum::http::header::RANGE)
        .and_then(|v| v.to_str().ok())
    else {
        return RangeSpec::Full;
    };
    let Some(spec) = raw.trim().strip_prefix("bytes=") else {
        // 其它单位（RFC 允许忽略）→ 整文件
        return RangeSpec::Full;
    };
    // 多段：不实现 multipart/byteranges，降级整文件
    if spec.contains(',') {
        return RangeSpec::Full;
    }
    let spec = spec.trim();
    let Some((a, b)) = spec.split_once('-') else {
        return RangeSpec::Full;
    };
    let (a, b) = (a.trim(), b.trim());

    // 长度为 0 的文件：任何区间都不可满足
    if total == 0 {
        return RangeSpec::Unsatisfiable;
    }

    if a.is_empty() {
        // bytes=-n：最后 n 字节
        let Ok(n) = b.parse::<u64>() else {
            return RangeSpec::Full;
        };
        if n == 0 {
            return RangeSpec::Unsatisfiable;
        }
        let n = n.min(total);
        return RangeSpec::Single(ByteRange {
            start: total - n,
            end: total - 1,
        });
    }

    let Ok(start) = a.parse::<u64>() else {
        return RangeSpec::Full;
    };
    if start >= total {
        return RangeSpec::Unsatisfiable;
    }
    let end = if b.is_empty() {
        total - 1
    } else {
        match b.parse::<u64>() {
            Ok(e) => e.min(total - 1),
            Err(_) => return RangeSpec::Full,
        }
    };
    if end < start {
        return RangeSpec::Unsatisfiable;
    }
    RangeSpec::Single(ByteRange { start, end })
}

/// 以流式 + Range 支持返回磁盘文件。
///
/// `extra_headers` 里的头会原样附加（如 `Content-Disposition`、`Cache-Control`）。
/// 文件打不开 → 返回 None，由调用方决定 404 文案。
pub(crate) async fn serve_file(
    path: &std::path::Path,
    content_type: &str,
    headers: &HeaderMap,
    extra_headers: &[(&str, String)],
) -> Option<Response> {
    let file = tokio::fs::File::open(path).await.ok()?;
    let total = file.metadata().await.ok()?.len();

    let mut builder = Response::builder()
        .header("Content-Type", content_type)
        // 必须始终声明：客户端据此判断能否发 Range（不声明就只会整本下载）
        .header(axum::http::header::ACCEPT_RANGES, "bytes");
    for (k, v) in extra_headers {
        builder = builder.header(*k, v.clone());
    }

    match parse_range(headers, total) {
        RangeSpec::Unsatisfiable => Some(
            builder
                .status(StatusCode::RANGE_NOT_SATISFIABLE)
                .header(axum::http::header::CONTENT_RANGE, format!("bytes */{total}"))
                .header(axum::http::header::CONTENT_LENGTH, "0")
                .body(Body::empty())
                .ok()?,
        ),
        RangeSpec::Full => Some(
            builder
                .status(StatusCode::OK)
                .header(axum::http::header::CONTENT_LENGTH, total.to_string())
                .body(stream_body(file, total))
                .ok()?,
        ),
        RangeSpec::Single(r) => {
            use tokio::io::AsyncSeekExt as _;
            let mut file = file;
            file.seek(std::io::SeekFrom::Start(r.start)).await.ok()?;
            let len = r.end - r.start + 1;
            Some(
                builder
                    .status(StatusCode::PARTIAL_CONTENT)
                    .header(
                        axum::http::header::CONTENT_RANGE,
                        format!("bytes {}-{}/{}", r.start, r.end, total),
                    )
                    .header(axum::http::header::CONTENT_LENGTH, len.to_string())
                    .body(stream_body(file, len))
                    .ok()?,
            )
        }
    }
}

/// 从已定位好的文件句柄流式读取 `len` 字节
fn stream_body(file: tokio::fs::File, len: u64) -> Body {
    let stream = tokio_util::io::ReaderStream::new(tokio::io::AsyncReadExt::take(file, len));
    Body::from_stream(stream)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hdr(v: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(axum::http::header::RANGE, v.parse().unwrap());
        h
    }

    #[test]
    fn 无range头返回整文件() {
        assert_eq!(parse_range(&HeaderMap::new(), 100), RangeSpec::Full);
    }

    #[test]
    fn 闭区间与开区间() {
        assert_eq!(
            parse_range(&hdr("bytes=0-9"), 100),
            RangeSpec::Single(ByteRange { start: 0, end: 9 })
        );
        assert_eq!(
            parse_range(&hdr("bytes=10-"), 100),
            RangeSpec::Single(ByteRange { start: 10, end: 99 })
        );
        // 末端越界按 RFC 截到 total-1，而不是 416
        assert_eq!(
            parse_range(&hdr("bytes=90-999"), 100),
            RangeSpec::Single(ByteRange { start: 90, end: 99 })
        );
    }

    #[test]
    fn 后缀区间取末尾n字节() {
        assert_eq!(
            parse_range(&hdr("bytes=-10"), 100),
            RangeSpec::Single(ByteRange { start: 90, end: 99 })
        );
        // n 超过总长 → 整文件区间（不是 416）
        assert_eq!(
            parse_range(&hdr("bytes=-500"), 100),
            RangeSpec::Single(ByteRange { start: 0, end: 99 })
        );
        assert_eq!(parse_range(&hdr("bytes=-0"), 100), RangeSpec::Unsatisfiable);
    }

    #[test]
    fn 起点越界与倒置区间是416() {
        assert_eq!(parse_range(&hdr("bytes=100-"), 100), RangeSpec::Unsatisfiable);
        assert_eq!(parse_range(&hdr("bytes=200-300"), 100), RangeSpec::Unsatisfiable);
        // 空文件：任何区间都不可满足
        assert_eq!(parse_range(&hdr("bytes=0-0"), 0), RangeSpec::Unsatisfiable);
    }

    #[test]
    fn 多段与非法语法降级整文件() {
        // 不实现 multipart/byteranges：按 RFC 允许的做法整本返回，而不是报错
        assert_eq!(parse_range(&hdr("bytes=0-9,20-29"), 100), RangeSpec::Full);
        assert_eq!(parse_range(&hdr("items=0-9"), 100), RangeSpec::Full);
        assert_eq!(parse_range(&hdr("bytes=abc-def"), 100), RangeSpec::Full);
        assert_eq!(parse_range(&hdr("garbage"), 100), RangeSpec::Full);
    }

    #[tokio::test]
    async fn 端到端_206与416与整文件() {
        let dir = std::env::temp_dir().join(format!("range-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("a.bin");
        std::fs::write(&p, b"0123456789").unwrap();

        // 整文件
        let resp = serve_file(&p, "application/octet-stream", &HeaderMap::new(), &[])
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.headers()[axum::http::header::ACCEPT_RANGES], "bytes");
        assert_eq!(resp.headers()[axum::http::header::CONTENT_LENGTH], "10");
        let body = axum::body::to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
        assert_eq!(&body[..], b"0123456789");

        // 206：中间一段
        let resp = serve_file(&p, "application/octet-stream", &hdr("bytes=2-5"), &[])
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(resp.headers()[axum::http::header::CONTENT_RANGE], "bytes 2-5/10");
        assert_eq!(resp.headers()[axum::http::header::CONTENT_LENGTH], "4");
        let body = axum::body::to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
        assert_eq!(&body[..], b"2345", "206 必须只回该区间，多回一字节客户端就拼错");

        // 206：末尾 3 字节
        let resp = serve_file(&p, "application/octet-stream", &hdr("bytes=-3"), &[])
            .await
            .unwrap();
        let body = axum::body::to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
        assert_eq!(&body[..], b"789");

        // 416
        let resp = serve_file(&p, "application/octet-stream", &hdr("bytes=99-"), &[])
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::RANGE_NOT_SATISFIABLE);
        assert_eq!(resp.headers()[axum::http::header::CONTENT_RANGE], "bytes */10");

        // 附加头原样透传
        let resp = serve_file(
            &p,
            "application/pdf",
            &HeaderMap::new(),
            &[("Content-Disposition", "attachment; filename=\"a.pdf\"".into())],
        )
        .await
        .unwrap();
        assert_eq!(resp.headers()["Content-Disposition"], "attachment; filename=\"a.pdf\"");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
