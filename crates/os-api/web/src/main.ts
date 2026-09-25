import { createApp } from 'vue'
import { createPinia } from 'pinia'
import App from './App.vue'
import router from './router'
import i18n, { initLocale } from './i18n'

// 设计令牌 + 主样式（全局）
import './styles/tokens.css'
import './styles/main.css'

// i18n：LanguageSwitcher / Settings 语言切换依赖（启动时从 os.locale 恢复）。
// 懒加载：恢复语言非 zh-CN 时先补载其 chunk 再挂载（期间无渲染，不闪烁）。
async function bootstrap(): Promise<void> {
    const app = createApp(App)
    app.use(createPinia())
    app.use(router)
    app.use(i18n)
    await initLocale()
    app.mount('#app')
}

void bootstrap()
