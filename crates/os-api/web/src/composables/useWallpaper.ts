/**
 * useWallpaper —— 桌面壁纸（图片 + CSS 渐变）状态管理。
 *
 * 预置 4 张 AI 生成图片壁纸（public/wallpapers/ 下的 1920x1080 JPEG，
 * css 采用多层 background：半透明暗色 scrim（保证白字可读）+ 图片层
 * `url(...) center/cover no-repeat` + 渐变兜底），另保留 6 套 Ubuntu
 * Yaru 风纯 CSS 渐变壁纸（不依赖图片文件）。
 * 通过 localStorage key `os-wallpaper` 持久化当前壁纸 id，
 * MainLayout 用 cssValue 计算属性应用到 .desktop-wallpaper。
 *
 * textLight=true 表示壁纸为深色（前景文字用白色）；
 * textLight=false 表示浅色壁纸（前景文字需改为深色）。
 */
import { computed, ref } from 'vue'

export interface Wallpaper {
    id: string
    name: string
    /** 完整 CSS background 值（可含多层渐变 / 图片层） */
    css: string
    /** 预览用的背景（设置面板小色块；图片壁纸直接展示图片层） */
    preview: string
    /** true=深色壁纸（白字）；false=浅色壁纸（深字） */
    textLight: boolean
    /** 图片壁纸的图片 URL（纯渐变壁纸无此字段） */
    image?: string
}

const WALLPAPER_KEY = 'os-wallpaper'

/** 预置壁纸集合（顺序即展示顺序）。 */
export const WALLPAPERS: Wallpaper[] = [
    // ---- 图片壁纸（AI 生成，置于列表头部）----
    {
        id: 'nexos-aubergine',
        name: '流光紫韵',
        css: 'linear-gradient(rgba(24,6,18,0.42), rgba(24,6,18,0.52)), url(/wallpapers/nexos-aubergine.jpg) center/cover no-repeat, linear-gradient(135deg, #2C001E, #4A0E3C)',
        preview: 'url(/wallpapers/nexos-aubergine.jpg) center/cover no-repeat, linear-gradient(135deg, #2C001E, #4A0E3C)',
        textLight: true,
        image: '/wallpapers/nexos-aubergine.jpg',
    },
    {
        id: 'nexos-ember',
        name: '暖橙流焰',
        css: 'linear-gradient(rgba(28,10,4,0.38), rgba(28,10,4,0.50)), url(/wallpapers/nexos-ember.jpg) center/cover no-repeat, linear-gradient(135deg, #772953, #B83A1A)',
        preview: 'url(/wallpapers/nexos-ember.jpg) center/cover no-repeat, linear-gradient(135deg, #772953, #B83A1A)',
        textLight: true,
        image: '/wallpapers/nexos-ember.jpg',
    },
    {
        id: 'nexos-abyss',
        name: '深海微光',
        css: 'linear-gradient(rgba(8,18,38,0.38), rgba(8,18,38,0.52)), url(/wallpapers/nexos-abyss.jpg) center/cover no-repeat, linear-gradient(135deg, #0C1E3E, #1B3A5C)',
        preview: 'url(/wallpapers/nexos-abyss.jpg) center/cover no-repeat, linear-gradient(135deg, #0C1E3E, #1B3A5C)',
        textLight: true,
        image: '/wallpapers/nexos-abyss.jpg',
    },
    {
        id: 'nexos-dawn',
        name: '晨曦简约',
        css: 'linear-gradient(rgba(255,255,255,0.22), rgba(248,246,242,0.34)), url(/wallpapers/nexos-dawn.jpg) center/cover no-repeat, linear-gradient(135deg, #F5F5F5, #E8E8E8)',
        preview: 'url(/wallpapers/nexos-dawn.jpg) center/cover no-repeat, linear-gradient(135deg, #F5F5F5, #E8E8E8)',
        textLight: false,
        image: '/wallpapers/nexos-dawn.jpg',
    },
    // ---- 科幻风纯 CSS 渐变壁纸（P0 新增，可选作默认）----
    {
        id: 'nexos-cyber',
        name: '赛博矩阵',
        css: 'repeating-linear-gradient(0deg, rgba(0,229,255,0.06) 0 1px, transparent 1px 26px), repeating-linear-gradient(90deg, rgba(0,229,255,0.06) 0 1px, transparent 1px 26px), radial-gradient(ellipse 75% 60% at 50% 118%, rgba(0,200,255,0.20), transparent 60%), linear-gradient(135deg, #040814 0%, #0a1430 55%, #0f1c44 100%)',
        preview: 'linear-gradient(135deg, #040814, #0f1c44)',
        textLight: true,
    },
    {
        id: 'nexos-nebula',
        name: '星云深空',
        css: 'radial-gradient(ellipse 60% 50% at 25% 28%, rgba(191,90,242,0.38), transparent 60%), radial-gradient(ellipse 60% 50% at 78% 74%, rgba(64,210,255,0.30), transparent 60%), radial-gradient(ellipse 85% 60% at 50% 112%, rgba(233,84,32,0.14), transparent 55%), linear-gradient(135deg, #0a0726 0%, #181043 55%, #2a1060 100%)',
        preview: 'radial-gradient(ellipse 60% 50% at 25% 28%, rgba(191,90,242,0.38), transparent 60%), linear-gradient(135deg, #0a0726, #2a1060)',
        textLight: true,
    },
    // ---- 纯 CSS 渐变壁纸（保留旧 id，localStorage 旧值仍有效）----
    {
        id: 'aubergine',
        name: 'Aubergine 深紫',
        css: 'radial-gradient(ellipse 70% 60% at 85% 12%, rgba(233,84,32,0.18), transparent 60%), radial-gradient(ellipse 60% 50% at 8% 92%, rgba(119,41,83,0.35), transparent 65%), linear-gradient(135deg, #2C001E 0%, #3A0A2C 45%, #4A0E3C 100%)',
        preview: 'linear-gradient(135deg, #2C001E, #4A0E3C)',
        textLight: true,
    },
    {
        id: 'orange',
        name: 'Ubuntu 暖橙',
        css: 'radial-gradient(ellipse 60% 50% at 80% 15%, rgba(255,200,150,0.25), transparent 60%), linear-gradient(135deg, #E95420 0%, #B83A1A 50%, #772953 100%)',
        preview: 'linear-gradient(135deg, #E95420, #772953)',
        textLight: true,
    },
    {
        id: 'ocean',
        name: '深蓝海洋',
        css: 'radial-gradient(ellipse 65% 55% at 85% 10%, rgba(100,160,220,0.18), transparent 60%), linear-gradient(135deg, #0C1E3E 0%, #15304F 50%, #1B3A5C 100%)',
        preview: 'linear-gradient(135deg, #0C1E3E, #1B3A5C)',
        textLight: true,
    },
    {
        id: 'black',
        name: '暗夜纯黑',
        css: 'radial-gradient(ellipse 60% 50% at 80% 12%, rgba(255,255,255,0.05), transparent 60%), linear-gradient(135deg, #1A1A1A 0%, #232323 50%, #2D2D2D 100%)',
        preview: 'linear-gradient(135deg, #1A1A1A, #2D2D2D)',
        textLight: true,
    },
    {
        id: 'forest',
        name: '森林墨绿',
        css: 'radial-gradient(ellipse 65% 55% at 82% 12%, rgba(150,200,140,0.15), transparent 60%), linear-gradient(135deg, #1B3A2B 0%, #244735 50%, #2D5A40 100%)',
        preview: 'linear-gradient(135deg, #1B3A2B, #2D5A40)',
        textLight: true,
    },
    {
        id: 'light',
        name: '浅色简约',
        css: 'radial-gradient(ellipse 70% 60% at 85% 12%, rgba(233,84,32,0.08), transparent 60%), linear-gradient(135deg, #F5F5F5 0%, #EFEFEF 50%, #E8E8E8 100%)',
        preview: 'linear-gradient(135deg, #F5F5F5, #E8E8E8)',
        textLight: false,
    },
]

/** 默认壁纸 id。 */
const DEFAULT_WALLPAPER_ID = 'nexos-cyber'

/** 单例状态：当前壁纸 id（跨组件共享）。 */
const currentId = ref<string>(DEFAULT_WALLPAPER_ID)

/** 是否已从 localStorage 初始化（避免 SSR/首帧闪烁判断）。 */
let initialized = false

/** 从 localStorage 读取壁纸 id（仅合法值才接受）。 */
function loadFromStorage(): string {
    try {
        const raw = window.localStorage.getItem(WALLPAPER_KEY)
        if (raw && WALLPAPERS.some((w) => w.id === raw)) return raw
    } catch {
        /* localStorage 不可用：保持默认 */
    }
    return DEFAULT_WALLPAPER_ID
}

/** 确保单例已初始化（懒加载，首次调用时读 localStorage）。 */
function ensureInit(): void {
    if (initialized) return
    currentId.value = loadFromStorage()
    initialized = true
}

export function useWallpaper() {
    ensureInit()

    /** 当前壁纸对象（computed，id 变化自动跟随）。 */
    const current = computed<Wallpaper>(
        () => WALLPAPERS.find((w) => w.id === currentId.value) ?? WALLPAPERS[0],
    )

    /** 当前壁纸完整 CSS background 值（直接赋给元素 style.background）。 */
    const cssValue = computed<string>(() => current.value.css)

    /** 当前壁纸是否为浅色（用于决定前景文字深/浅）。 */
    const isLight = computed<boolean>(() => !current.value.textLight)

    /** 切换壁纸：写入 localStorage + 更新单例 id。 */
    function setWallpaper(id: string): void {
        if (!WALLPAPERS.some((w) => w.id === id)) return
        currentId.value = id
        try {
            window.localStorage.setItem(WALLPAPER_KEY, id)
        } catch {
            /* 写入失败（隐私模式等）：仅内存生效 */
        }
    }

    return { current, currentId, cssValue, isLight, setWallpaper, wallpapers: WALLPAPERS }
}
