<script setup lang="ts">
/**
 * Launchpad —— 全屏应用启动台。
 *
 * 按业务域（备份 / 监控 / 媒体 / 安全 / 文件 / 开发者工具 / 电源 / 系统）分组展示
 * 全部桌面应用；点击即打开对应浮窗（与桌面图标 / Dock 行为一致）。
 * 点击空白遮罩或按 Esc 关闭。
 */
import { computed, onMounted, onUnmounted } from 'vue'
import { useRouter } from 'vue-router'
import { useWindowManager } from '@/composables/useWindowManager'
import {
    appRegistry,
    desktopApps,
    getAppCategory,
    CATEGORY_META,
    getAppName,
} from '@/appRegistry'
import AppIcon from '@/components/AppIcon.vue'

const props = defineProps<{ open: boolean }>()
const emit = defineEmits<{ (e: 'close'): void }>()

const wm = useWindowManager()
const router = useRouter()

/** 按业务域分组的桌面应用（内置 + 运行时应用包；过滤掉空分组）。 */
const groups = computed(() =>
    CATEGORY_META.map((cat) => ({
        ...cat,
        apps: desktopApps.value.filter((a) => getAppCategory(a.id) === cat.key),
    })).filter((g) => g.apps.length > 0),
)

function openApp(appId: string): void {
    const meta = desktopApps.value.find((a) => a.id === appId)
    if (!meta) return
    if (!appRegistry[appId]) {
        void router.push(meta.route)
    } else {
        wm.openWindow({ id: appId, title: getAppName(appId), icon: appId })
    }
    emit('close')
}

function onKey(e: KeyboardEvent): void {
    if (e.key === 'Escape') emit('close')
}
onMounted(() => document.addEventListener('keydown', onKey))
onUnmounted(() => document.removeEventListener('keydown', onKey))
</script>

<template>
    <transition name="lp-fade">
        <div v-if="open" class="launchpad" @click.self="emit('close')">
            <div class="lp-inner">
                <div class="lp-head">
                    <span class="lp-title">启动台</span>
                    <button class="lp-close" type="button" aria-label="关闭" @click="emit('close')">×</button>
                </div>
                <div class="lp-scroll">
                    <section v-for="g in groups" :key="g.key" class="lp-group">
                        <h3 class="lp-group-title">{{ g.label }}</h3>
                        <div class="lp-grid">
                            <button
                                v-for="app in g.apps"
                                :key="app.id"
                                class="lp-app"
                                type="button"
                                :title="getAppName(app.id)"
                                @click="openApp(app.id)"
                            >
                                <span class="lp-tile" :style="{ background: app.gradient }">
                                    <AppIcon :name="app.icon" :size="40" />
                                </span>
                                <span class="lp-label">{{ getAppName(app.id) }}</span>
                            </button>
                        </div>
                    </section>
                </div>
            </div>
        </div>
    </transition>
</template>

<style scoped>
.launchpad {
    position: fixed;
    inset: 0;
    z-index: 350;
    background: rgba(8, 8, 14, 0.62);
    backdrop-filter: blur(18px);
    -webkit-backdrop-filter: blur(18px);
    display: flex;
    align-items: center;
    justify-content: center;
    padding: 40px;
}
.lp-inner {
    width: min(960px, 94vw);
    max-height: 86vh;
    display: flex;
    flex-direction: column;
    background: rgba(22, 22, 30, 0.92);
    border: 1px solid rgba(255, 255, 255, 0.12);
    border-radius: 18px;
    box-shadow: 0 24px 70px rgba(0, 0, 0, 0.55);
    overflow: hidden;
    font-family: var(--font);
}
.lp-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: 16px 20px;
    border-bottom: 1px solid rgba(255, 255, 255, 0.08);
}
.lp-title {
    color: #fff;
    font-size: 16px;
    font-weight: 700;
    letter-spacing: 0.3px;
}
.lp-close {
    width: 30px;
    height: 30px;
    border: none;
    border-radius: 50%;
    background: rgba(255, 255, 255, 0.1);
    color: #fff;
    font-size: 20px;
    line-height: 1;
    cursor: pointer;
}
.lp-close:hover {
    background: rgba(255, 255, 255, 0.2);
}
.lp-scroll {
    overflow: auto;
    padding: 18px 20px 24px;
}
.lp-group {
    margin-bottom: 22px;
}
.lp-group-title {
    color: rgba(255, 255, 255, 0.55);
    font-size: 12px;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 1px;
    margin: 0 0 12px;
}
.lp-grid {
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(96px, 1fr));
    gap: 16px 12px;
}
.lp-app {
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 8px;
    background: transparent;
    border: none;
    cursor: pointer;
    border-radius: 14px;
    padding: 8px 4px;
    transition: transform 0.12s ease, background 0.12s ease;
}
.lp-app:hover {
    transform: translateY(-4px);
    background: rgba(255, 255, 255, 0.08);
}
.lp-tile {
    width: 60px;
    height: 60px;
    border-radius: 16px;
    display: flex;
    align-items: center;
    justify-content: center;
    color: #fff;
    box-shadow: 0 6px 16px rgba(0, 0, 0, 0.34);
}
.lp-label {
    color: #fff;
    font-size: 11px;
    text-align: center;
    max-width: 92px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    text-shadow: 0 1px 3px rgba(0, 0, 0, 0.6);
}
.lp-fade-enter-active,
.lp-fade-leave-active {
    transition: opacity 0.18s ease;
}
.lp-fade-enter-from,
.lp-fade-leave-to {
    opacity: 0;
}
</style>
