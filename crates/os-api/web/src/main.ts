import { createApp } from 'vue'
import { createPinia } from 'pinia'
import App from './App.vue'
import router from './router'
import i18n from './i18n'

// 设计令牌 + 主样式（全局）
import './styles/tokens.css'
import './styles/main.css'

const app = createApp(App)
app.use(createPinia())
app.use(router)
// i18n：LanguageSwitcher / Settings 语言切换依赖（启动时从 os.locale 恢复）
app.use(i18n)
app.mount('#app')
