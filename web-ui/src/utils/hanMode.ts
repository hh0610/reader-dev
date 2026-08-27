/**
 * 全站共享简繁模式（响应式状态）+ 词典懒加载代理。
 *
 * 背景：utils/chinese.ts 内嵌整套 OpenCC 词典（约 1MB 源码 / 438KB gzip）。
 * 此前本模块**静态** import 它——于是搜索/探索/阅读等 6 个视图的路由 chunk
 * 全部被词典下载卡住才渲染。注意 `auto`（默认）模式**也会转换**（auto/simp 均转简体，
 * 短标题检测不可靠，见 chinese.ts applyHan 注释），所以词典不能「不用就不加载」，
 * 只能把加载从「阻塞路由」改成「并行下载」：
 *
 *   - 视图 chunk 立即渲染，转换函数在词典就绪前**原样返回文本**；
 *   - applyHan/hanText 在渲染期读取 hanDictReady（ref）→ 组件自动订阅，
 *     词典到位后 ref 翻转，所有用到转换的组件自动重渲染出转换结果；
 *   - 词典 chunk 由 HTTP 缓存兜底，后续访问无感。
 *
 * 其它职责不变：单一响应式 hanMode ref、localStorage 持久化、跨标签页同步。
 * 阅读页（ReaderView）另有 per-book 覆盖的独立 hanMode ref，其全局写入路径
 * （saveSetting → setGlobalHanMode）同样会更新本状态，保证全站一致。
 */

import { ref } from 'vue'
// 只引类型：type-only import 编译期擦除，不会把词典拉回同步依赖
import type { HanMode } from './chinese.ts'

export type { HanMode }

const HAN_MODE_KEY = 'reader_han_mode'

/** 读取持久化模式（与 chinese.ts getHanMode 同键同语义；在此重复实现以避免静态引入词典模块） */
export function getHanMode(): HanMode {
  try {
    const raw = localStorage.getItem(HAN_MODE_KEY)
    if (raw === 'simp' || raw === 'trad') return raw
  } catch {
    /* ignore */
  }
  return 'auto'
}

function persistHanMode(mode: HanMode) {
  try {
    localStorage.setItem(HAN_MODE_KEY, mode)
  } catch {
    /* ignore */
  }
}

/* ---------------- 词典懒加载 ---------------- */

type Dict = typeof import('./chinese.ts')

/** 词典是否就绪（响应式：渲染期在 applyHan 里被读取 → 就绪时相关组件自动重渲染）。
 *  需要在词典就绪后补做一次性副作用的地方（如 ReaderView 的繁体检测）可 watch 它。 */
export const hanDictReady = ref(false)

let dict: Dict | null = null
let dictLoading: Promise<void> | null = null

/** 触发词典加载（幂等；失败可重试——置空 promise 供下次调用再试） */
export function ensureHanDict(): Promise<void> {
  if (dict) return Promise.resolve()
  if (!dictLoading) {
    dictLoading = import('./chinese.ts')
      .then((m) => {
        dict = m
        hanDictReady.value = true
      })
      .catch((e) => {
        dictLoading = null // 网络抖动等：下次调用重试，期间原样显示
        console.warn('[hanMode] 简繁词典加载失败（将在下次转换时重试）:', e)
      })
  }
  return dictLoading
}

/** 按模式转换文本。词典未就绪：触发加载并原样返回（渲染期读 hanDictReady 建立订阅）。 */
export function applyHan(text: string, mode: HanMode = hanMode.value): string {
  if (!text) return text
  // 渲染期访问 → 组件订阅 hanDictReady，词典就绪后自动重渲染
  if (!hanDictReady.value) {
    void ensureHanDict()
    return text
  }
  return dict ? dict.applyHan(text, mode) : text
}

/** 检测文本是否繁体（词典未就绪返回 false 并触发加载；调用方如需就绪后重检，watch hanDictReady） */
export function detectTraditional(text: string): boolean {
  if (!hanDictReady.value) {
    void ensureHanDict()
    return false
  }
  return dict ? dict.detectTraditional(text) : false
}

/* ---------------- 响应式模式状态（原有职责） ---------------- */

/** 全站共享简繁模式（响应式；初始值取自 localStorage reader_han_mode） */
export const hanMode = ref<HanMode>(getHanMode())

/** 视图内使用：const mode = useHanMode() 后即可响应式读取 */
export function useHanMode() {
  return hanMode
}

/** 设置全局简繁模式：更新响应式状态 + 写 localStorage（阅读页/书海等写入方统一走这里） */
export function setGlobalHanMode(m: HanMode) {
  hanMode.value = m
  persistHanMode(m)
}

/** 按当前全局模式转换文本（模板中直接调用，响应式跟随 hanMode 与词典就绪状态） */
export function hanText(text: string): string {
  return applyHan(text, hanMode.value)
}

/** 从 localStorage 重新同步（视图挂载 / 服务器配置下发后调用，覆盖同标签页直写场景） */
export function syncHanMode() {
  hanMode.value = getHanMode()
}

/** 跨标签页响应：其他标签页修改 reader_han_mode 时同步本状态 */
if (typeof window !== 'undefined') {
  window.addEventListener('storage', (e) => {
    if (e.key === 'reader_han_mode') syncHanMode()
  })
}
