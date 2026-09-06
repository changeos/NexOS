import { defineConfig } from 'vite'
import vue from '@vitejs/plugin-vue'
import { fileURLToPath, URL } from 'node:url'

// https://vite.dev/config/
// os-api 网关默认监听 127.0.0.1:8080；开发时把 /api /status /healthz /shares /discover
// 反向代理到 os-api，前端 SPA 走 vite dev server（默认 5173）。
export default defineConfig({
  plugins: [vue()],
  resolve: {
    alias: {
      '@': fileURLToPath(new URL('./src', import.meta.url)),
    },
  },
  build: {
    // 构建产物输出到 crates/os-api/static-dist/，供 os-api 网关内嵌/托管。
    outDir: '../static-dist',
    emptyOutDir: true,
  },
  server: {
    port: 5173,
    proxy: {
      '/api': { target: 'http://127.0.0.1:8080', changeOrigin: true },
      '/status': { target: 'http://127.0.0.1:8080', changeOrigin: true },
      '/healthz': { target: 'http://127.0.0.1:8080', changeOrigin: true },
      '/shares': { target: 'http://127.0.0.1:8080', changeOrigin: true },
      '/discover': { target: 'http://127.0.0.1:8080', changeOrigin: true },
    },
  },
})
