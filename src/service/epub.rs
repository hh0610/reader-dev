//! EPUB/OPF 元数据解析（本地书导入前置 + OPDS 元数据）
//!
//! 解析 content.opf 的 OPF 2.0 元数据（对齐样本：identifier/title/creator/language/
//! date/description/publisher/subject 等全字段，不丢字段）

use serde::Serialize;

/// OPF 元数据（全字段）
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OpfMeta {
    pub title: String,
    /// 作者列表（dc:creator，取全部）
    pub authors: Vec<String>,
    /// 作者（第一个，file-as 优先）
    pub author: String,
    pub language: Option<String>,
    pub publisher: Option<String>,
    pub published_at: Option<String>,
    pub description: Option<String>,
    pub subjects: Vec<String>,
    pub identifiers: Vec<String>,
    /// 封面路径（guide reference 或 manifest 中 cover）
    pub cover_href: Option<String>,
}

/// 解析 OPF XML（quick-xml 事件流）。
///
/// 相比旧的手写字符串提取：
/// - **命名空间无关**：标签按 local-name 匹配（`dc:title` / `title` / `opf:title` 均命中），
///   此前写死 `<dc:` 前缀，默认命名空间或其它前缀的 OPF 整条字段丢失。
/// - **完整实体解码**：quick-xml `unescape` 支持数字实体（`&#233;` / `&#xE9;`），
///   此前只覆盖 7 个命名实体。
/// - **属性顺序/空白无关**：属性按解析结果读取，不再依赖 `key="val"` 子串匹配。
pub fn parse_opf(xml: &str) -> OpfMeta {
    use quick_xml::events::Event;

    let mut meta = OpfMeta::default();
    // manifest：id → href（EPUB2 `<meta name="cover" content="ID">` 间接封面用）
    let mut manifest: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    // 封面候选（优先级：guide reference > properties=cover-image > meta name=cover > id=cover）
    let mut cover_guide: Option<String> = None;
    let mut cover_properties: Option<String> = None;
    let mut cover_meta_id: Option<String> = None;
    let mut cover_id_item: Option<String> = None;

    let mut reader = quick_xml::Reader::from_str(xml);
    reader.config_mut().trim_text(false);
    let mut buf = Vec::new();
    // 当前正在收集文本的 dc 字段（local-name）
    let mut cur_field: Option<String> = None;
    let mut cur_text = String::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                let name_owned = e.name();
                let local = local_name(name_owned.as_ref());
                match local.as_str() {
                    "title" | "creator" | "language" | "publisher" | "date" | "description"
                    | "subject" | "identifier" => {
                        cur_field = Some(local);
                        cur_text.clear();
                    }
                    "item" => {
                        let id = attr_of(&e, "id").unwrap_or_default();
                        let href = attr_of(&e, "href").unwrap_or_default();
                        let props = attr_of(&e, "properties").unwrap_or_default();
                        if !id.is_empty() && !href.is_empty() {
                            manifest.insert(id.clone(), href.clone());
                        }
                        // EPUB3：properties 含 cover-image（空白分隔多值）
                        if !href.is_empty()
                            && props.split_whitespace().any(|p| p == "cover-image")
                            && cover_properties.is_none()
                        {
                            cover_properties = Some(href.clone());
                        }
                        // EPUB2 习惯：id="cover"
                        if !href.is_empty() && id.eq_ignore_ascii_case("cover")
                            && cover_id_item.is_none()
                        {
                            cover_id_item = Some(href);
                        }
                    }
                    "reference" => {
                        // guide：<reference type="cover" href="...">
                        let ty = attr_of(&e, "type").unwrap_or_default();
                        let href = attr_of(&e, "href").unwrap_or_default();
                        if ty.eq_ignore_ascii_case("cover") && !href.is_empty()
                            && cover_guide.is_none()
                        {
                            cover_guide = Some(href);
                        }
                    }
                    "meta" => {
                        // EPUB2 主流封面：<meta name="cover" content="封面 item 的 id"/>
                        let name = attr_of(&e, "name").unwrap_or_default();
                        let content = attr_of(&e, "content").unwrap_or_default();
                        if name.eq_ignore_ascii_case("cover")
                            && !content.is_empty()
                            && cover_meta_id.is_none()
                        {
                            cover_meta_id = Some(content);
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::Text(t)) => {
                if cur_field.is_some() {
                    // 先按编码解码，再解 XML 实体（含数字实体 &#233;/&#xE9;——
                    // decode() 只做编码转换，不解实体）
                    if let Ok(raw) = t.decode() {
                        match quick_xml::escape::unescape(&raw) {
                            Ok(s) => cur_text.push_str(&s),
                            // 含未知实体（如 HTML &nbsp;）→ 保留原文，不丢字段
                            Err(_) => cur_text.push_str(&raw),
                        }
                    }
                }
            }
            // quick-xml 0.41 把实体引用作为独立事件发出（不并入 Text）：
            // 数字实体 `&#233;`/`&#xE9;` 经 resolve_char_ref 还原为字符；
            // 命名实体（&amp;/&lt;/&nbsp; 等）按名映射，未知实体保留原文不丢字段。
            Ok(Event::GeneralRef(r)) => {
                if cur_field.is_some() {
                    match r.resolve_char_ref() {
                        Ok(Some(c)) => cur_text.push(c),
                        _ => {
                            let name = String::from_utf8_lossy(r.as_ref()).to_string();
                            let resolved = match name.as_str() {
                                "amp" => "&",
                                "lt" => "<",
                                "gt" => ">",
                                "quot" => "\"",
                                "apos" => "'",
                                "nbsp" => " ",
                                _ => "",
                            };
                            if resolved.is_empty() {
                                cur_text.push('&');
                                cur_text.push_str(&name);
                                cur_text.push(';');
                            } else {
                                cur_text.push_str(resolved);
                            }
                        }
                    }
                }
            }
            Ok(Event::CData(t)) => {
                if cur_field.is_some() {
                    cur_text.push_str(&String::from_utf8_lossy(t.as_ref()));
                }
            }
            Ok(Event::End(e)) => {
                let name_owned = e.name();
                let local = local_name(name_owned.as_ref());
                if cur_field.as_deref() == Some(local.as_str()) {
                    let text = cur_text.trim().to_string();
                    if !text.is_empty() {
                        match local.as_str() {
                            "title" => {
                                if meta.title.is_empty() {
                                    meta.title = text;
                                }
                            }
                            "creator" => {
                                if meta.author.is_empty() {
                                    meta.author = text.clone();
                                }
                                meta.authors.push(text);
                            }
                            "language" => {
                                meta.language.get_or_insert(text);
                            }
                            "publisher" => {
                                meta.publisher.get_or_insert(text);
                            }
                            "date" => {
                                meta.published_at.get_or_insert(text);
                            }
                            "description" => {
                                meta.description.get_or_insert(text);
                            }
                            "subject" => {
                                meta.subjects.push(text);
                            }
                            "identifier" => {
                                meta.identifiers.push(text);
                            }
                            _ => {}
                        };
                    }
                    cur_field = None;
                    cur_text.clear();
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break, // 畸形 OPF：保留已解析字段，不 panic
            _ => {}
        }
        buf.clear();
    }

    // 封面回退链：guide > EPUB3 properties > EPUB2 meta name=cover（经 manifest 查 href）> id=cover
    meta.cover_href = cover_guide
        .or(cover_properties)
        .or_else(|| cover_meta_id.and_then(|id| manifest.get(&id).cloned()))
        .or(cover_id_item);

    meta
}

/// 取标签 local-name（去命名空间前缀，小写）——命名空间无关匹配
fn local_name(qname: &[u8]) -> String {
    let s = String::from_utf8_lossy(qname);
    let local = s.rsplit(':').next().unwrap_or(&s);
    local.to_ascii_lowercase()
}

/// 取属性值（按 local-name 匹配、自动实体解码；未找到返回 None）
fn attr_of(e: &quick_xml::events::BytesStart<'_>, want: &str) -> Option<String> {
    for attr in e.attributes().flatten() {
        if local_name(attr.key.as_ref()) == want {
            return attr
                .unescape_value()
                .ok()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty());
        }
    }
    None
}

/// 提取单个标签文本（跨行、含 CDATA）
pub(crate) fn extract_tag(xml: &str, tag: &str) -> Option<String> {
    extract_all_tags(xml, tag).into_iter().next()
}

/// 提取全部同名标签文本
pub(crate) fn extract_all_tags(xml: &str, tag: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = xml;
    loop {
        let Some(start) = rest.find(&format!("<{tag}")) else {
            break;
        };
        let after = &rest[start..];
        let Some(gt) = after.find('>') else { break };
        // 跳过自闭合
        if after[..gt].ends_with('/') {
            rest = &after[gt + 1..];
            continue;
        }
        let close = format!("</{tag}>");
        let Some(end) = after[gt + 1..].find(&close) else {
            break;
        };
        let content = &after[gt + 1..gt + 1 + end];
        let text = content
            .trim()
            .trim_start_matches("<![CDATA[")
            .trim_end_matches("]]>")
            .trim()
            .to_string();
        if !text.is_empty() {
            out.push(text);
        }
        rest = &after[gt + 1 + end + close.len()..];
    }
    out
}

/// 提取带属性条件的标签 href：<item id="cover" href="...">
fn extract_attr(
    xml: &str,
    tag: &str,
    attr_key: &str,
    attr_val: &str,
    want: &str,
) -> Option<String> {
    let mut rest = xml;
    loop {
        let Some(start) = rest.find(&format!("<{tag}")) else {
            return None;
        };
        let after = &rest[start..];
        let Some(gt) = after.find('>') else {
            return None;
        };
        let tag_block = &after[..gt + 1];
        // 检查 attr_key="attr_val"（宽容：引号单双）
        let pattern_attr = format!("{attr_key}=\"{attr_val}\"");
        let pattern_attr2 = format!("{attr_key}='{attr_val}'");
        if tag_block.contains(&pattern_attr) || tag_block.contains(&pattern_attr2) {
            let want_pattern = format!("{want}=\"");
            let want_pattern2 = format!("{want}='");
            if let Some(i) = tag_block.find(&want_pattern) {
                let rest2 = &tag_block[i + want_pattern.len()..];
                return rest2.split('"').next().map(str::to_string);
            }
            if let Some(i) = tag_block.find(&want_pattern2) {
                let rest2 = &tag_block[i + want_pattern2.len()..];
                return rest2.split('\'').next().map(str::to_string);
            }
            return None;
        }
        rest = &after[gt + 1..];
    }
}

/// HTML/XML 实体解码（常用）
pub(crate) fn decode_entities(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&nbsp;", " ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_opf_sample() {
        // 用户提供的测试样本（metadata.opf 全字段）
        let xml = r#"<?xml version='1.0' encoding='utf-8'?>
<package xmlns="http://www.idpf.org/2007/opf" unique-identifier="uuid_id" version="2.0">
    <metadata xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:opf="http://www.idpf.org/2007/opf">
        <dc:identifier opf:scheme="calibre" id="calibre_id">39</dc:identifier>
        <dc:identifier opf:scheme="uuid" id="uuid_id">088054ed-61ad-440c-8ba4-90f97710015b</dc:identifier>
        <dc:title>我的化身正在成为最终BOSS</dc:title>
        <dc:creator opf:file-as="汐尺" opf:role="aut">汐尺</dc:creator>
        <dc:contributor opf:file-as="calibre" opf:role="bkp">calibre (9.11.0) [https://calibre-ebook.com]</dc:contributor>
        <dc:date>2025-02-13T00:00:00+00:00</dc:date>
        <dc:description>平平无奇地生活了十多年后，某天夜里，姬明欢觉醒了一个允许他在现实世界"创建游戏角色"的异能。</dc:description>
        <dc:publisher>起点中文网</dc:publisher>
        <dc:identifier opf:scheme="QIDIAN_URL">https://www.qidian.com/book/1042464636</dc:identifier>
        <dc:identifier opf:scheme="QIDIAN">1042464636</dc:identifier>
        <dc:language>zho</dc:language>
        <dc:subject>轻小说</dc:subject>
        <dc:subject>原生幻想</dc:subject>
        <dc:subject>都市异能</dc:subject>
        <dc:subject>完本</dc:subject>
    </metadata>
    <guide>
        <reference type="cover" title="封面" href="cover.jpg"/>
    </guide>
</package>"#;
        let meta = parse_opf(xml);
        assert_eq!(meta.title, "我的化身正在成为最终BOSS");
        assert_eq!(meta.author, "汐尺");
        assert_eq!(meta.language.as_deref(), Some("zho"));
        assert_eq!(meta.publisher.as_deref(), Some("起点中文网"));
        assert!(meta
            .published_at
            .as_deref()
            .unwrap()
            .starts_with("2025-02-13"));
        assert!(meta.description.as_deref().unwrap().contains("姬明欢"));
        assert_eq!(meta.subjects.len(), 4, "多 subject 不丢");
        assert_eq!(meta.identifiers.len(), 4, "多 identifier 不丢");
        assert_eq!(meta.cover_href.as_deref(), Some("cover.jpg"));
    }

    #[test]
    fn test_parse_opf_cdata() {
        let xml = r#"<package><metadata><dc:title><![CDATA[书名 <特殊> & 符号]]></dc:title></metadata></package>"#;
        let meta = parse_opf(xml);
        assert_eq!(meta.title, "书名 <特殊> & 符号");
    }

    /// EPUB2 主流封面：<meta name="cover" content="ID"/> → manifest[ID].href
    /// （Calibre 导出的标准做法；此前不支持 → 有封面也识别不到）
    #[test]
    fn test_parse_opf_epub2_meta_cover_indirect() {
        let xml = r#"<package><metadata>
            <dc:title>书</dc:title>
            <meta name="cover" content="cover-img"/>
          </metadata>
          <manifest>
            <item id="cover-img" href="images/cover.jpg" media-type="image/jpeg"/>
            <item id="ch1" href="text/ch1.xhtml" media-type="application/xhtml+xml"/>
          </manifest></package>"#;
        let meta = parse_opf(xml);
        assert_eq!(meta.cover_href.as_deref(), Some("images/cover.jpg"));
    }

    /// 命名空间无关：非 dc: 前缀 / 默认命名空间的 OPF 不再丢字段
    #[test]
    fn test_parse_opf_namespace_agnostic() {
        // 默认命名空间（无前缀）
        let xml = r#"<package xmlns="http://www.idpf.org/2007/opf"><metadata>
            <title>无前缀书名</title><creator>无前缀作者</creator><language>zh</language>
          </metadata></package>"#;
        let meta = parse_opf(xml);
        assert_eq!(meta.title, "无前缀书名");
        assert_eq!(meta.author, "无前缀作者");
        assert_eq!(meta.language.as_deref(), Some("zh"));
        // 自定义前缀
        let xml2 = r#"<opf:package><opf:metadata>
            <opf:title>前缀书名</opf:title></opf:metadata></opf:package>"#;
        assert_eq!(parse_opf(xml2).title, "前缀书名");
    }

    /// 数字实体解码（&#233; / &#xE9;）——此前只解 7 个命名实体
    #[test]
    fn test_parse_opf_numeric_entities() {
        let xml = r#"<package><metadata>
            <dc:title>caf&#233; &#x4E2D;&#25991; &amp; more</dc:title></metadata></package>"#;
        let meta = parse_opf(xml);
        assert_eq!(meta.title, "café 中文 & more");
    }

    /// 属性顺序/单引号无关（此前依赖 key="val" 精确子串）
    #[test]
    fn test_parse_opf_attr_order_and_quotes() {
        let xml = r#"<package><manifest>
            <item media-type='image/png' href='cov.png' properties='cover-image' id='x'/>
          </manifest></package>"#;
        assert_eq!(parse_opf(xml).cover_href.as_deref(), Some("cov.png"));
    }
}
