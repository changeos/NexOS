import { createI18n } from 'vue-i18n';
import zhCN from './locales/zh-CN.json';
import zhTW from './locales/zh-TW.json';
import enUS from './locales/en-US.json';
import jaJP from './locales/ja-JP.json';

/** 语言偏好持久化 key（与 LanguageSwitcher / Settings 共用）。 */
export const LOCALE_STORAGE_KEY = 'os.locale';

/** 支持的语言列表。 */
export const SUPPORTED_LOCALES = ['zh-CN', 'zh-TW', 'en-US', 'ja-JP'] as const;

export type SupportedLocale = (typeof SUPPORTED_LOCALES)[number];

function isSupported(lang: string): lang is SupportedLocale {
    return (SUPPORTED_LOCALES as readonly string[]).includes(lang);
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
    locale: restoreLocale(),
    fallbackLocale: 'en-US',
    messages: { 'zh-CN': zhCN, 'zh-TW': zhTW, 'en-US': enUS, 'ja-JP': jaJP },
});

/**
 * 切换语言（唯一入口）：立即更新 vue-i18n 当前 locale，持久化到
 * localStorage(os.locale)，并同步 <html lang>。非受支持的值将被忽略。
 */
export function setLocale(lang: string): void {
    if (!isSupported(lang)) return;
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
