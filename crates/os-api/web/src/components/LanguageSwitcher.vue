<script setup lang="ts">
import { computed } from 'vue';
import { useI18n } from 'vue-i18n';
import { SUPPORTED_LOCALES, setLocale } from '@/i18n';

// variant：
//   - default：常规表单场景（浅色表面，如设置页）
//   - bar：顶栏（深色半透明状态栏）紧凑胶囊样式
withDefaults(defineProps<{ variant?: 'default' | 'bar' }>(), { variant: 'default' });

const SUPPORTED = SUPPORTED_LOCALES.map((value) => ({
    value,
    label: {
        'zh-CN': '简体中文',
        'zh-TW': '繁體中文',
        'en-US': 'English',
        'ja-JP': '日本語',
    }[value],
}));

const { locale } = useI18n();
// 启动恢复已由 i18n/index.ts 初始化完成（读 os.locale），这里只需双向绑定。

const current = computed<string>({
    get: () => locale.value,
    set: (val: string) => {
        // setLocale：更新 vue-i18n + 持久化 localStorage(os.locale) + 同步 <html lang>
        setLocale(val);
    },
});
</script>

<template>
    <label class="lang-switcher" :class="`lang-switcher--${variant}`" aria-label="Language">
        <span class="lang-switcher__icon" aria-hidden="true">🌐</span>
        <select v-model="current" class="lang-switcher__select" data-test="language-select">
            <option v-for="opt in SUPPORTED" :key="opt.value" :value="opt.value">
                {{ opt.label }}
            </option>
        </select>
    </label>
</template>

<style scoped>
.lang-switcher {
    display: inline-flex;
    align-items: center;
    gap: 0.35rem;
    font-size: 0.85rem;
}
.lang-switcher__icon {
    line-height: 1;
}
.lang-switcher__select {
    appearance: none;
    border: 1px solid var(--border, #d0d0d0);
    border-radius: 6px;
    background: var(--surface, #fff);
    color: var(--text, #2B2B2B);
    padding: 0.25rem 0.55rem;
    cursor: pointer;
    outline: none;
    font-family: inherit;
}
.lang-switcher__select:hover {
    border-color: var(--accent, #007aff);
}
.lang-switcher__select:focus {
    border-color: var(--accent, #007aff);
    box-shadow: 0 0 0 2px rgba(0, 122, 255, 0.2);
}

/* —— bar 变体：深色顶栏（rgba 黑底白字）内的紧凑胶囊 —— */
.lang-switcher--bar {
    font-size: 12px;
    gap: 0.3rem;
}
.lang-switcher--bar .lang-switcher__select {
    background: rgba(255, 255, 255, 0.14);
    color: #fff;
    border-color: rgba(255, 255, 255, 0.28);
    border-radius: 999px;
    padding: 0.14rem 0.5rem;
    font-size: 11.5px;
    transition: background 0.14s ease, border-color 0.14s ease;
}
.lang-switcher--bar .lang-switcher__select:hover,
.lang-switcher--bar .lang-switcher__select:focus {
    background: rgba(255, 255, 255, 0.24);
    border-color: rgba(255, 255, 255, 0.6);
    box-shadow: none;
}
/* 深色 select 的 option 菜单在部分浏览器继承白字不可读，显式回深色 */
.lang-switcher--bar .lang-switcher__select option {
    color: #2B2B2B;
    background: #fff;
}
</style>
