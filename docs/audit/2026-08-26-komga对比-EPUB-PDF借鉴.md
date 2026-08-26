# komga-cn (riir-dev) vs reader-dev:EPUB/PDF 解析可借鉴点

调研分支:komga-cn 的 `riir-dev`(克隆成功,Rust 重写在 `komga-rust/` 下,20+ crate,是**真实实现**不是占位)。以下借鉴点中标 ✅ 的已对照 reader-dev 源码核实。

## 前提:两个项目定位不同
- **komga = 图片/漫画型服务端**:PDF 用 **pdfium 把整页渲染成 JPEG 图片**,EPUB 的 spine 当资源。**它完全不做 PDF 文本提取**(全仓 grep `extract_text/ocr` 零命中)。
- **reader-dev = 文本重排阅读器(legado 风格)**:把 PDF/EPUB 转成文本章节。

→ 所以 **PDF「文本质量」komga 一无可借鉴**(它压根不提文本);能借鉴的是 **EPUB 元数据/目录的 XML 工程质量** 和 **封面缩略图**。

## 可借鉴点(按价值排序)

### ★★★★★ 1. EPUB2 `meta name="cover"` 间接封面(✅ 已证 reader-dev 缺)
- komga:`komga-rust/crates/epub/src/parse.rs:141` `parse_epub_metadata_cover_id` 读 `<meta name="cover" content="ID">`,再回 manifest 用该 ID 查 href。
- reader-dev 现状:`epub.rs:66-78 parse_opf` 只处理 guide reference / `item id=cover` / `properties=cover-image`,**没有 meta name=cover 这条**——Calibre 导出的 EPUB2 主流封面方式会漏。
- 改法:在封面回退链加一步「读 `<meta name=cover>` 的 content → manifest 按 id 查 href」。纯逻辑、零依赖、修真 bug。**最高性价比。**

### ★★★★★ 2. OPF 元数据改用 quick-xml(已在依赖里)(✅ 已证短板)
- komga:`media-metadata/src/refresh/epub.rs` 全程 quick-xml 状态机;标签匹配 `xml_name_matches` 用 `actual.ends_with(expected)`(命名空间无关);属性用 `normalized_value` 自动实体解码。
- reader-dev 现状:`epub.rs` 手写 `extract_tag/extract_attr`,**写死 `dc:` 前缀**(非 dc 前缀或默认命名空间整条丢失);`decode_entities`(161)**只解 7 个命名实体,不支持数字实体** `&#233;`/`&#xE9;`;属性匹配对顺序/空格敏感。
- 改法:把 epub.rs 的字符串提取换成 quick-xml 事件循环(可直接搬 komga 的 `xml_name_matches_local` + `attribute_value` 两个小函数)。`quick-xml 0.41` 已是依赖,零新增。顺带能低成本引入作者角色(opf:role/marc:relators)、系列(belongs-to-collection)、阅读方向(page-progression-direction)。

### ★★★★☆ 3. EPUB 资源路径归一化(✅ 已证 resolve_opf_path naive)
- komga:`parse.rs:286 normalize_epub_resource_href` + `:329 normalize_epub_zip_path` + `:306 percent_decode`(先去 `#` 锚点 → percent-decode → 折叠 `.`/`..` → 反斜杠转正斜杠)。
- reader-dev 现状:`local_book.rs:2788 resolve_opf_path` 只 `目录+/+href`、去 `#`,**不折叠 `../`、不 percent-decode**。OPF/nav 在子目录时的父级相对路径(`../images/x.jpg`)与 `%20` 编码路径都读不到 → 封面/章节丢失。
- 改法:用这套归一化统一处理 cover_href / nav href / ncx src / spine href。komga 有现成单测可搬。

### ★★★★☆ 4. PDF 封面缩略图(能力缺口:reader-dev PDF cover: None)
- komga:`media-metadata/src/refresh/artwork_support.rs:157 render_pdf_thumbnail` 用 pdfium 渲染首页→JPEG;`spawn_blocking` + `OnceLock` 单例加载 pdfium。
- reader-dev 现状:`local_book.rs:1501 parse_pdf` `cover: None`——PDF 导入无任何封面。
- 改法二选一:
  - (a) 引入 `pdfium-render`(可选 feature)渲染首页——效果最好,但带原生库 `libpdfium`(打包/部署负担)。
  - (b) 纯 lopdf 兜底:从首页 `Resources/XObject` 抽第一个 `/Subtype /Image` 内嵌图当封面——零原生依赖,但对矢量/扫描页无效。
- 按部署对原生依赖的容忍度取舍。

### ★★★☆☆ 5. ncx 目录用真正 XML 解析(层级 + 正确取 src)
- komga:`analysis.rs:758 ncx_link` 用 DOM,`content/@src` 取链接、`navLabel/text` 取标题,递归保层级。
- reader-dev 现状:`local_book.rs:235 ncx_nav_points` 手写字符串扫描,`stack` 声明未用(层级被拍平),`src` 用 `find("content")` 后取首个引号(属性乱序会取错、`<content id=.. src=..>` 会读到 id)。
- 改法:quick-xml 重写,按 `<content>` 的 `src` 属性正确取值;层级可选保留(reader-dev 章节最终扁平,取值正确性才是刚需)。

### ★★★☆☆ 6. 缩略图 AVIF/WebP 编码失败回退 JPEG
- komga:`page_rendering.rs:365 encode_image_with_jpeg_fallback`。若 reader-dev 封面走 AVIF/WebP,套一层 JPEG 回退更稳。

## reader-dev 已经更好、**不要倒退借鉴**的地方
- **解压炸弹防护**:reader-dev 有 `read_zip_limited`(local_book.rs:2694,单条目上限)+ PDF 每页文本 8MB 上限;**komga EPUB 分析是无上限 `read_to_end`**。保持。
- **nav.xhtml 用 html5ever(scraper)容错解析**,比 komga 的严格 XML DOM 对畸形 nav 更宽容。
- **PDF 文本提取 + 中文分章规则**(`PDF_TOC_RULES`):komga 完全没有,reader-dev 领先。

## 落地建议
先做 #1(meta cover)、#2(quick-xml OPF)、#3(路径归一化)——三项都是纯逻辑、零/极小依赖、直接修「有封面识别不到 / 字段丢失 / 章节读不到」的真实 bug;#4(PDF 封面)按是否接受 pdfium 原生依赖再定。PDF 文本质量想提升只能自行换库(pdf-extract/mupdf/pdfium 文本 API),komga 帮不上。
