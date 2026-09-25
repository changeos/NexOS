import { createI18n } from 'vue-i18n';
import zhCN from './locales/zh-CN.json';

/** 语言偏好持久化 key（与 LanguageSwitcher / Settings 共用）。 */
export const LOCALE_STORAGE_KEY = 'os.locale';

/** 支持的语言列表。 */
export const SUPPORTED_LOCALES = ['zh-CN', 'zh-TW', 'en-US', 'ja-JP'] as const;

export type SupportedLocale = (typeof SUPPORTED_LOCALES)[number];

function isSupported(lang: string): lang is SupportedLocale {
    return (SUPPORTED_LOCALES as readonly string[]).includes(lang);
}

/**
 * locale 懒加载（首屏只带当前语言，其余按需加载）：
 * - zh-CN（默认语言）静态打进首屏 chunk，兼作消息体类型锚；
 * - 其余语言由 vite 拆为独立 chunk，setLocale 前置动态 import 加载，
 *   setLocaleMessage 注入后缓存——再次切换零请求；
 * - 启动恢复 os.locale 为非默认语言时，main.ts 挂载前 initLocale() 补载，
 *   不产生语言闪烁。
 */

/** 懒加载 loader 表（动态 import → 独立 chunk；返回类型以 zh-CN 为锚）。 */
const localeLoaders: Record<Exclude<SupportedLocale, 'zh-CN'>, () => Promise<{ default: typeof zhCN }>> = {
    'zh-TW': () => import('./locales/zh-TW.json'),
    'en-US': () => import('./locales/en-US.json'),
    'ja-JP': () => import('./locales/ja-JP.json'),
};

/** 已注入 vue-i18n 的语言集合（zh-CN 静态即载）。 */
const loadedLocales = new Set<SupportedLocale>(['zh-CN']);

/** 进行中的加载 Promise（并发调用去重，不重复发起 import）。 */
const pendingLoads = new Map<SupportedLocale, Promise<void>>();

/**
 * 加载指定语言的 messages 并注入 vue-i18n（幂等：已加载则直接返回）。
 * 加载失败向上抛出，由调用方决定保持当前语言。
 */
export function loadLocale(lang: SupportedLocale): Promise<void> {
    if (loadedLocales.has(lang)) return Promise.resolve();
    let pending = pendingLoads.get(lang);
    if (!pending) {
        const loader = lang === 'zh-CN' ? undefined : localeLoaders[lang];
        if (!loader) return Promise.resolve();
        pending = loader()
            .then((mod) => {
                i18n.global.setLocaleMessage(lang, mod.default);
                loadedLocales.add(lang);
            })
            .finally(() => {
                pendingLoads.delete(lang);
            });
        pendingLoads.set(lang, pending);
    }
    return pending;
}

/**
 * 启动恢复：从 localStorage(os.locale) 读取上次选择的语言。
 * 未设置 / 值非法 / localStorage 不可用时回退默认 zh-CN。
 */
function restoreLocale(): SupportedLocale {
    try {
        const saved = localStorage.getItem(LOCALE_STORAGE_KEY);
        if (saved && isSupported(saved)) return saved;
    } catch {
        // localStorage 不可用（隐私模式等）时忽略
    }
    return 'zh-CN';
}

const i18n = createI18n({
    legacy: false,
    // 起步 locale 即恢复值；若为非 zh-CN，其 messages 由 initLocale()（挂载前）
    // 补载——期间无渲染，不闪烁。
    locale: restoreLocale(),
    fallbackLocale: 'en-US',
    // messages 类型显式放宽为 Record（否则 locale 被推断收窄为 'zh-CN'，
    // 无法赋值懒加载语言）；值类型仍以 zh-CN 为锚。
    messages: { 'zh-CN': zhCN } as Record<string, typeof zhCN>,
});

/**
 * 启动补载：恢复语言非 zh-CN 时在其 chunk 到位后再挂载（main.ts 调用）。
 * 加载失败回退 zh-CN，保证可渲染。
 */
export async function initLocale(): Promise<void> {
    const lang = i18n.global.locale.value as SupportedLocale;
    if (lang !== 'zh-CN') {
        try {
            await loadLocale(lang);
        } catch (err) {
            console.error('[i18n] 启动语言 chunk 加载失败，回退 zh-CN:', lang, err);
            i18n.global.locale.value = 'zh-CN';
        }
    }
    if (typeof document !== 'undefined') {
        document.documentElement.lang = i18n.global.locale.value;
    }
}

/**
 * 切换语言（唯一入口）：先加载目标语言 chunk（已加载则零开销），再更新
 * vue-i18n 当前 locale、持久化 localStorage(os.locale) 并同步 <html lang>。
 * 非受支持的值 / chunk 加载失败时忽略（保持当前语言）。
 */
export async function setLocale(lang: string): Promise<void> {
    if (!isSupported(lang)) return;
    try {
        await loadLocale(lang);
    } catch (err) {
        console.error('[i18n] 语言 chunk 加载失败，保持当前语言:', lang, err);
        return;
    }
    i18n.global.locale.value = lang;
    try {
        localStorage.setItem(LOCALE_STORAGE_KEY, lang);
    } catch {
        // localStorage 不可用时忽略（仅本次会话生效）
    }
    if (typeof document !== 'undefined') {
        document.documentElement.lang = lang;
    }
}

// 挂载时同步一次 <html lang>（index.html 默认 zh-CN，恢复其他语言时纠正）
if (typeof document !== 'undefined') {
    document.documentElement.lang = i18n.global.locale.value;
}

export default i18n;
