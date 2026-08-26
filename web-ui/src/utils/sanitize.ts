/**
 * P1-4：RSS 正文 HTML 净化（纯函数，无外部依赖——不引入 DOMPurify CDN）。
 *
 * 在原有轻量清洗（去 script/style/嵌入标签/事件属性）基础上加固：
 * - 实体解码（数字实体 + 常见命名实体，最多 3 轮——覆盖 `&amp;#106;` 双重编码绕过）后校验；
 * - 移除 javascript:/data:/vbscript: 协议的 href/src/xlink:href 属性（含 `java\tscript:`、
 *   `&#106;avascript:` 等变体——协议内空白/控制字符剥离后前缀匹配）；
 * - xlink:href 与 href/src 同规则处理（SVG `<a xlink:href="javascript:...">` 是常见注入面）。
 */

/** 常见命名实体（仅 URL 协议判定相关子集；未知命名实体原样保留） */
const NAMED_ENTITIES: Record<string, string> = {
  amp: '&',
  lt: '<',
  gt: '>',
  quot: '"',
  apos: "'",
  nbsp: ' ',
  colon: ':',
  sol: '/',
  Tab: '\t',
  NewLine: '\n',
}

/** 单轮实体解码（数字实体 + 常见命名实体） */
function decodeEntitiesOnce(s: string): string {
  return s
    .replace(/&#x([0-9a-f]+);/gi, (_, h: string) => {
      const cp = parseInt(h, 16)
      return cp >= 0 && cp <= 0x10ffff ? String.fromCodePoint(cp) : ''
    })
    .replace(/&#(\d+);/g, (_, d: string) => {
      const cp = parseInt(d, 10)
      return cp >= 0 && cp <= 0x10ffff ? String.fromCodePoint(cp) : ''
    })
    .replace(/&([a-zA-Z][a-zA-Z0-9]*);/g, (m, name: string) => NAMED_ENTITIES[name] ?? m)
}

/**
 * 完整实体解码（最多 3 轮——覆盖 `&amp;#106;` 双重编码；无变化提前结束）。
 * 返回的是解码后字符串，仅用于协议判定；不改变原始 HTML 内容本身。
 */
export function decodeEntities(s: string): string {
  let cur = s
  for (let i = 0; i < 3; i++) {
    const next = decodeEntitiesOnce(cur)
    if (next === cur) return cur
    cur = next
  }
  return cur
}

/**
 * 危险 URL 协议判定：剥离空白/控制字符（浏览器 URL 解析对 scheme 内 tab/换行/C0 控制
 * 字符等同忽略，`java\tscript:` 与 `javascript:` 等价）后前缀匹配。
 */
export function isDangerousUrl(value: string): boolean {
  const stripped = value.replace(/[\u0000-\u0020\u007f]/g, '').toLowerCase()
  if (stripped.startsWith('javascript:') || stripped.startsWith('vbscript:')) return true
  if (stripped.startsWith('data:')) {
    // data: 默认危险，但**放行 base64 位图**：本地书（EPUB 内联图 / CBZ / 扫描版 PDF）
    // 的正文图片就是 data URI，一律拦截会让图片全部裂掉。
    // 仅白名单位图格式；svg+xml 可内嵌脚本，继续拦截。
    return !/^data:image\/(png|jpe?g|gif|webp|bmp|avif);base64,/.test(stripped)
  }
  return false
}

/**
 * 轻量 HTML 净化：RSS 正文按 HTML 渲染前的安全清洗。
 * 1) 删除 script/style/iframe/object/embed/form 整块；
 * 2) 删除全部事件属性（on*）；
 * 3) href/src/xlink:href 值实体解码后做危险协议校验，命中则整个属性删除。
 */
export function sanitizeHtml(html: string): string {
  return (
    html
      // 成对块：整块删除
      .replace(/<script[\s\S]*?<\/script>/gi, '')
      .replace(/<style[\s\S]*?<\/style>/gi, '')
      // 未闭合的 script/style：删至末尾（此前只处理成对标签，
      // `<style>body{display:none}` 这类未闭合块会原样输出并全局生效）
      .replace(/<script\b[\s\S]*$/i, '')
      .replace(/<style\b[\s\S]*$/i, '')
      .replace(/<(iframe|object|embed|form)[\s\S]*?<\/(?:iframe|object|embed|form)>/gi, '')
      .replace(/<(iframe|object|embed|form)\b[^>]*\/?>/gi, '')
      // 事件属性：分隔符用 [\s/] —— HTML 解析器把 `/` 也当属性分隔符，
      // 仅用 \s 会漏掉 `<img/onerror=alert(1)>`、`<img src=x/onerror=...>`（实测可绕过）
      .replace(/[\s/]+on\w+\s*=\s*("[^"]*"|'[^']*'|[^\s>]+)/gi, '')
      .replace(
        /[\s/]+(?:href|src|xlink:href)\s*=\s*("[^"]*"|'[^']*'|[^\s>]+)/gi,
        (m, raw: string) => {
          const unquoted = raw.replace(/^["']|["']$/g, '')
          return isDangerousUrl(decodeEntities(unquoted)) ? '' : m
        },
      )
  )
}
