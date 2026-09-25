// =============================================================================
// design/theme.ts —— 深浅主题状态（v0.1.7；试点页顶栏切换钮消费）。
//
// 挂载点：<html data-theme="light|dark">——只有本包 design/dark.css 读该属性，
// 旧页/宿主样式不消费它，切换对旧视图零影响。偏好 localStorage 记忆
// （key nexos.film.theme），缺省浅色（与宿主 Yaru 浅色主题连续）。
// color-scheme 同步给原生控件/滚动条。
// =============================================================================
import { ref } from 'vue'

export type NxTheme = 'light' | 'dark'

const THEME_KEY = 'nexos.film.theme'

/** 当前主题（模块级响应式——多切换钮共享同一份）。 */
export const nxTheme = ref<NxTheme>('light')

/** 读偏好（localStorage → 环境缺省；隐私模式等异常静默）。
 *  缺省口径：桌面嵌入=light（宿主 Yaru 浅色主题连续）；独立模式=dark
 *  （standalone 壳自绘暗色基座，旧页即暗色——试点页跟随免跳变）。 */
export function readNxTheme(): NxTheme {
  try {
    const v = localStorage.getItem(THEME_KEY)
    if (v === 'light' || v === 'dark') return v
  } catch {
    /* ignore */
  }
  const standalone = Boolean(
    (globalThis as { __NEXOS_STANDALONE__?: boolean }).__NEXOS_STANDALONE__,
  )
  return standalone ? 'dark' : 'light'
}

/** 应用主题到 <html>（attr + color-scheme + localStorage 记忆）。 */
export function applyNxTheme(t: NxTheme): void {
  nxTheme.value = t
  try {
    document.documentElement.setAttribute('data-theme', t)
  } catch {
    /* SSR/无 document：忽略 */
  }
  try {
    localStorage.setItem(THEME_KEY, t)
  } catch {
    /* 隐私模式等：仅本次内存生效 */
  }
}

/** 初始化（FilmStudio setup 调一次；幂等）。 */
export function initNxTheme(): void {
  applyNxTheme(readNxTheme())
}

/** 切换（浅↔深；返回切换后主题）。 */
export function toggleNxTheme(): NxTheme {
  applyNxTheme(nxTheme.value === 'dark' ? 'light' : 'dark')
  return nxTheme.value
}
