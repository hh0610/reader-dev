//! 封面缩略图端点 `GET /cover-thumb/{ns}/{file}`
//!
//! 书架网格里一屏可能有几十本书，每本都拉 1000×1500 的完整封面是纯浪费——
//! 格子实际只有 100~200px 宽。这里按需生成 250×350（方形源图 250×250，
//! 有声书封面用的就是方图）的缩略图并落盘缓存。
//!
//! 缓存位置：`assets/{ns}/covers/thumbs/{file}`。
//! 源图更新（换封面会换文件名）时缩略图自然失效；同名源图被就地覆盖的情况
//! 按 mtime 比对重生成。
//!
//! 鉴权：与 `/assets` 静态目录一致（当前不鉴权）。刻意保持一致——
//! 封面本就经 `/assets/{ns}/covers/*` 明文可取，这里再加一道并不能提高保密性，
//! 反而会让 `<img>` 加载失败。`/assets` 的鉴权是既有待办，届时两处一起改。

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Response;

use crate::api::router::AppState;

/// 缩略图缓存子目录名
const THUMB_DIR: &str = "thumbs";

/// 由封面地址推导缩略图地址；非本地封面（远程 URL / 空）原样返回。
///
/// 只认 `/assets/{ns}/covers/{file}` 这一种形态——那是导入路径唯一会写出的形态。
pub fn thumb_url_for(cover_url: &str) -> Option<String> {
    let rest = cover_url.strip_prefix("/assets/")?;
    let (ns, rest) = rest.split_once('/')?;
    let file = rest.strip_prefix("covers/")?;
    if ns.is_empty() || file.is_empty() || file.contains('/') {
        return None;
    }
    Some(format!("/cover-thumb/{ns}/{file}"))
}

/// `GET /cover-thumb/{ns}/{file}`
pub async fn cover_thumb(
    State(state): State<AppState>,
    Path((ns, file)): Path<(String, String)>,
) -> Response {
    // 路径分量校验：ns/file 都必须是单段普通名字（防穿越）
    if !is_plain_segment(&ns) || !is_plain_segment(&file) {
        return not_found();
    }
    let covers = state
        .storage
        .config
        .storage_dir()
        .join("assets")
        .join(&ns)
        .join("covers");
    let src = covers.join(&file);
    if !src.is_file() {
        return not_found();
    }
    let cache = covers.join(THUMB_DIR).join(&file);

    let cached_ok = match (std::fs::metadata(&cache), std::fs::metadata(&src)) {
        // 缩略图不比源图旧才算有效（同名源图被就地覆盖时会重生成）
        (Ok(c), Ok(s)) => match (c.modified(), s.modified()) {
            (Ok(ct), Ok(st)) => ct >= st,
            _ => false,
        },
        _ => false,
    };

    let bytes = if cached_ok {
        match tokio::fs::read(&cache).await {
            Ok(b) if !b.is_empty() => b,
            _ => return serve_source_fallback(&src).await,
        }
    } else {
        let src2 = src.clone();
        let cache2 = cache.clone();
        match tokio::task::spawn_blocking(move || generate(&src2, &cache2)).await {
            Ok(Some(b)) => b,
            // 生成不了（非常见格式/解码失败）→ 回退原图，而不是让书架缺图
            _ => return serve_source_fallback(&src).await,
        }
    };

    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "image/jpeg")
        // 地址随封面文件名走，换封面即换地址，可放心长缓存
        .header("Cache-Control", "public, max-age=31536000, immutable")
        .header("Content-Length", bytes.len().to_string())
        .body(axum::body::Body::from(bytes))
        .unwrap_or_else(|_| not_found())
}

/// 生成缩略图并落盘（阻塞 IO + 图像解码，调用方放 spawn_blocking）
fn generate(src: &std::path::Path, cache: &std::path::Path) -> Option<Vec<u8>> {
    let raw = std::fs::read(src).ok()?;
    let (jpg, _, _) = crate::service::imaging::make_thumbnail(&raw)?;
    if let Some(parent) = cache.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // 原子落盘：先写临时文件再 rename——直接 write 的话，并发的另一请求可能在
    // 半截文件上通过 mtime 校验，把截断的 JPEG 当缓存端出去（书架出现裂图）。
    // 写失败只是下次再生成一遍，不影响本次返回。
    let tmp = cache.with_extension(format!("tmp{}", std::process::id()));
    if std::fs::write(&tmp, &jpg).is_ok() {
        let _ = std::fs::rename(&tmp, cache);
    }
    Some(jpg)
}

/// 缩略图不可用时回退原图（书架宁可慢一点，也不能出现裂图）
async fn serve_source_fallback(src: &std::path::Path) -> Response {
    let Ok(bytes) = tokio::fs::read(src).await else {
        return not_found();
    };
    let ct = crate::service::local_book::image_ext_of(&bytes)
        .map(|e| match e {
            "png" => "image/png",
            "gif" => "image/gif",
            "webp" => "image/webp",
            "bmp" => "image/bmp",
            "svg" => "image/svg+xml",
            _ => "image/jpeg",
        })
        .unwrap_or("image/jpeg");
    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", ct)
        .header("Cache-Control", "public, max-age=86400")
        .body(axum::body::Body::from(bytes))
        .unwrap_or_else(|_| not_found())
}

/// 单段普通名字：非空、无路径分隔符、无 `..`、无盘符
fn is_plain_segment(s: &str) -> bool {
    !s.is_empty()
        && s != "."
        && s != ".."
        && !s.contains('/')
        && !s.contains('\\')
        && !s.contains(':')
        && !s.contains('\0')
}

fn not_found() -> Response {
    Response::builder()
        .status(StatusCode::NOT_FOUND)
        .body(axum::body::Body::empty())
        .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 缩略图地址推导() {
        assert_eq!(
            thumb_url_for("/assets/alice/covers/abc.jpg").as_deref(),
            Some("/cover-thumb/alice/abc.jpg")
        );
        // 远程封面 / 其它资源类型 / 空值 → None（调用方保持原地址）
        assert_eq!(thumb_url_for("https://x.com/a.jpg"), None);
        assert_eq!(thumb_url_for("/assets/alice/uploads/a.jpg"), None);
        assert_eq!(thumb_url_for(""), None);
        // 多级路径不接受——covers 下是平铺的 uuid 文件名
        assert_eq!(thumb_url_for("/assets/alice/covers/sub/a.jpg"), None);
    }

    #[test]
    fn 路径分量校验() {
        assert!(is_plain_segment("abc.jpg"));
        assert!(!is_plain_segment(".."));
        assert!(!is_plain_segment("a/b"));
        assert!(!is_plain_segment("a\\b"));
        assert!(!is_plain_segment("C:x"));
        assert!(!is_plain_segment(""));
    }
}
