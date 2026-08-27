//! 本地书资源端点 `GET /book-asset?url={bookUrl}&path={压缩包内条目}`
//!
//! 借鉴 booklore 的「服务端虚拟文件系统」（见 docs/audit/2026-08-27-booklore对比-导入与阅读借鉴.md）：
//! 正文里不再把图片 base64 内联进章节文本，而只存一个地址，图片按需从**原始压缩包**流出。
//!
//! 换来的好处：
//! - 章节文本从「一张图几 MB 的 base64」变回几十字节，DB 与响应都瘦一个数量级
//!   （base64 本身还要多占 33%）
//! - 浏览器能缓存单张图、能并发拉、能懒加载；此前整章一次性传完才显示
//! - 不再受内联预算上限约束——超过预算的图**此前是被静默丢弃的**
//!
//! 安全：
//! - 条目路径不落文件系统，只在压缩包内查找，**压缩包自身的条目表就是白名单**
//!   （booklore 用预计算的 manifest Set 做同一件事）。仍复核一次 zip-slip 规则。
//! - secure 模式要求命名空间匹配：token 来自 query，或 `getBookContent` 下发的
//!   路径限定 cookie（浏览器发 `<img>` 子请求时不会带 Authorization 头）。
//! - 只服务图片类型；其它条目（HTML/CSS/字体）一律 404，避免这个端点变成
//!   任意读取压缩包内容的通道。

use std::collections::HashMap;

use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;

use crate::api::router::AppState;

/// 资源鉴权 cookie 名（与 /epub、/book-assets 静态资源共用同一个名字，但 Path 不同）
pub const ASSET_COOKIE: &str = "reader_asset_token";

/// 单个资源条目大小上限（超大条目直接拒绝——正文图片不该有几百 MB）
const MAX_ASSET_BYTES: u64 = 64 * 1024 * 1024;

/// 生成某本书的资源地址前缀：`/book-asset?url=<percent-encoded bookUrl>`
pub fn asset_base_for(book_url: &str) -> String {
    format!("/book-asset?url={}", urlencoding::encode(book_url))
}

/// `GET /book-asset?url=&path=`
pub async fn book_asset(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let book_url = params.get("url").cloned().unwrap_or_default();
    let entry = params.get("path").cloned().unwrap_or_default();
    if book_url.is_empty() || entry.is_empty() {
        return not_found();
    }
    // 命名空间：secure 模式取 token 对应用户（query token 或 cookie 回退）
    let ns = match resolve_ns(&state, &params, &headers).await {
        Some(ns) => ns,
        None => return not_found(),
    };
    // 书必须属于该命名空间——否则可用别人的 bookUrl 读别人的书
    let Ok(Some(book)) = state.storage.find_book(&ns, &book_url).await else {
        return not_found();
    };
    let Some(archive) = archive_path_of(&state, &ns, &book) else {
        return not_found();
    };
    // 条目路径复核（压缩包内查找不碰文件系统，这里防的是畸形路径进日志/缓存键）
    if !crate::service::local_book::is_safe_zip_entry_path_pub(&entry) {
        return not_found();
    }
    let Some(mime) = crate::service::local_book::image_mime_pub(&entry) else {
        // 只服务图片：不让这个端点变成任意读取压缩包内容的通道
        return not_found();
    };
    let entry2 = entry.clone();
    let bytes = match tokio::task::spawn_blocking(move || read_archive_entry(&archive, &entry2))
        .await
    {
        Ok(Some(b)) => b,
        _ => return not_found(),
    };
    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", mime)
        // 地址由 (bookUrl, 条目路径) 唯一确定，内容随原文件走；
        // 原文件被替换时对账会重扫章节、地址不变——故用较长但非 immutable 的缓存
        .header("Cache-Control", "private, max-age=604800")
        .header("Content-Length", bytes.len().to_string())
        .body(axum::body::Body::from(bytes))
        .unwrap_or_else(|_| not_found())
}

/// 命名空间解析：非 secure 一律 default；secure 下先 query/header token，再回退 cookie
async fn resolve_ns(
    state: &AppState,
    params: &HashMap<String, String>,
    headers: &HeaderMap,
) -> Option<String> {
    if !state.storage.config.secure {
        return Some("default".to_string());
    }
    if let Ok(u) = crate::api::router::resolve_current_user(state, params, headers).await {
        return Some(u.username);
    }
    // 浏览器发 <img> 子请求不带 Authorization —— 回退到 getBookContent 下发的路径限定 cookie
    let tok = crate::api::router::cookie_value_pub(headers, ASSET_COOKIE)?;
    let mut p = HashMap::new();
    p.insert("accessToken".to_string(), tok);
    crate::api::router::resolve_current_user(state, &p, &HeaderMap::new())
        .await
        .ok()
        .map(|u| u.username)
}

/// 把正文里指向本服务资源端点的图片地址还原成 data URI（导出/生成 epub 用）。
///
/// 正文里的 `![alt](/book-asset?url=..&path=..)` 只在本服务内有效——导出的 EPUB
/// 拿到别处打开就是一张取不到的图。导出前在这里读回原始字节内嵌回去，
/// 导出物因此重新变成自包含的（顺带修好了一个更早就存在的问题：
/// 导出此前把整行 base64 当**普通文本**塞进 `<p>`，图片一张都没有）。
///
/// 找不到原文件/条目时保留原样，不让导出整体失败。
pub fn inline_asset_urls(
    state: &AppState,
    ns: &str,
    book: &crate::model::Book,
    content: &str,
) -> String {
    if !content.contains("/book-asset?") {
        return content.to_string();
    }
    let Some(archive) = archive_path_of(state, ns, book) else {
        return content.to_string();
    };
    use base64::Engine;
    content
        .lines()
        .map(|line| {
            let Some(entry) = entry_of_asset_line(line) else {
                return line.to_string();
            };
            let Some(mime) = crate::service::local_book::image_mime_pub(&entry) else {
                return line.to_string();
            };
            match read_archive_entry(&archive, &entry) {
                Some(bytes) => {
                    let alt = line
                        .trim()
                        .strip_prefix("![")
                        .and_then(|r| r.split_once("]("))
                        .map(|(a, _)| a)
                        .unwrap_or("图片");
                    let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
                    format!("![{alt}](data:{mime};base64,{b64})")
                }
                None => line.to_string(),
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// 从 `![alt](/book-asset?url=..&path=..)` 取出条目路径（非该形态返回 None）
fn entry_of_asset_line(line: &str) -> Option<String> {
    let rest = line.trim().strip_prefix("![")?;
    let (_, rest) = rest.split_once("](")?;
    let url = rest.strip_suffix(')')?.trim();
    let query = url.strip_prefix("/book-asset?")?;
    for kv in query.split('&') {
        if let Some(v) = kv.strip_prefix("path=") {
            return urlencoding::decode(v).ok().map(|c| c.into_owned());
        }
    }
    None
}

/// 书 → 原始压缩包路径。
///
/// **先查 opds_files（上传时落盘的原件），再回退 local_file** —— 顺序不能反。
/// 双轨同步会给「只有 DB 记录、没有关联文件」的书**自动生成一个 epub** 落到书仓，
/// 并把 local_file 指向那个生成物。生成的 epub 是按章节文本重建的，内部条目路径
/// 与原始 EPUB 完全不同，拿它去找 `OEBPS/Images/cover.jpg` 只会 404（实测踩到）。
/// 正文里的资源地址是按**原始**压缩包的条目写的，所以必须优先回到原件。
fn archive_path_of(
    state: &AppState,
    ns: &str,
    book: &crate::model::Book,
) -> Option<std::path::PathBuf> {
    if let Some(id) = book.book_url.strip_prefix("local://") {
        let dir = state
            .storage
            .config
            .storage_dir()
            .join("data")
            .join(ns)
            .join("opds_files");
        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.flatten() {
                let p = e.path();
                if p.is_file()
                    && p.file_stem()
                        .map(|s| s.to_string_lossy() == id)
                        .unwrap_or(false)
                {
                    return Some(p);
                }
            }
        }
    }
    // 扫描导入的书（local://store/{hash}）没有 opds_files 副本，原件就是 local_file
    book.local_file
        .as_deref()
        .filter(|p| !p.is_empty())
        .map(std::path::PathBuf::from)
        .filter(|p| p.is_file())
}

/// 从压缩包读取条目（阻塞 IO，调用方放 spawn_blocking）。
/// 条目名可能是 GBK 等非 UTF-8 编码，故精确名查不到时按解码名再找一次。
fn read_archive_entry(archive: &std::path::Path, entry: &str) -> Option<Vec<u8>> {
    use std::io::Read as _;
    let file = std::fs::File::open(archive).ok()?;
    let mut zip = zip::ZipArchive::new(std::io::BufReader::new(file)).ok()?;
    let idx = crate::service::local_book::zip_index_of_pub(&mut zip, entry)?;
    let mut f = zip.by_index(idx).ok()?;
    if f.size() > MAX_ASSET_BYTES {
        return None;
    }
    let mut buf = Vec::with_capacity(f.size().min(MAX_ASSET_BYTES) as usize);
    f.take(MAX_ASSET_BYTES).read_to_end(&mut buf).ok()?;
    Some(buf).filter(|b| !b.is_empty())
}

fn not_found() -> Response {
    Response::builder()
        .status(StatusCode::NOT_FOUND)
        .body(axum::body::Body::empty())
        .unwrap()
}
