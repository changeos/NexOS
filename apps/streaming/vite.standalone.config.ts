// =============================================================================
// vite.standalone.config.ts —— 独立运行宿主构建（第二个 lib 产物）。
//
// 与 vite.config.ts（entry.js）的区别：**不挂 host-externals**——standalone-host
// 就是真宿主，vue / vue-i18n 完整打进产物（应用包完全自包含，内网离线可跑）。
//
// 产物：
//   dist/web/standalone-host.js   —— 宿主桥 + ctx 适配器 + 应用全量（ESM 单文件）
//   dist/standalone.html          —— 独立入口页（引用相对 ./web/standalone-host.js；
//                                      发布时并入包根 web/，经 /apps-assets 剥 web/ 段命中）
//
// npm 脚本：`vite build && vite build --config vite.standalone.config.ts`
//（先 entry 清空 dist/web，再本构建 emptyOutDir=false 追加）。
// （照 apps/film/vite.standalone.config.ts 同款，改 id / styleId。）
// =============================================================================
import { defineConfig, type Plugin } from 'vite'
import vue from '@vitejs/plugin-vue'
import { fileURLToPath, URL } from 'node:url'
import { copyFileSync, mkdirSync, readdirSync, readFileSync, unlinkSync, writeFileSync } from 'node:fs'

/**
 * post-process：构建收尾——
 *   1. standalone/standalone.html 落到 dist/（发布根形态）；
 *   2. dist/manifest.json 与包根同步；
 *   3. lib 构建抽出的 standalone CSS（nexos-app-streaming.css）**内联**进
 *      standalone-host.js 头部（与 entry.js 的 inlineCss 同款注入器；独立
 *      模式没有 entry.js，组件样式必须随宿主自带）；
 *   4. 残余 `process.env.NODE_ENV` 替换为 "production"（vue / vue-i18n
 *      esm-bundler 源码引用它——浏览器无 process 全局，lib 构建不做依赖
 *      预打包，define 在 lib 模式不生效，实测必须显式替换）。
 */
function postProcessStandaloneHost(): Plugin {
  return {
    name: 'nexos-standalone-post',
    closeBundle() {
      const dest = fileURLToPath(new URL('./dist/standalone.html', import.meta.url))
      const distDir = fileURLToPath(new URL('./dist', import.meta.url))
      mkdirSync(distDir, { recursive: true })
      copyFileSync(
        fileURLToPath(new URL('./standalone/standalone.html', import.meta.url)),
        dest,
      )
      copyFileSync(
        fileURLToPath(new URL('./manifest.json', import.meta.url)),
        fileURLToPath(new URL('./dist/manifest.json', import.meta.url)),
      )
      const webDir = fileURLToPath(new URL('./dist/web', import.meta.url))
      const host = fileURLToPath(new URL('./dist/web/standalone-host.js', import.meta.url))
      // 3. CSS 内联（id 与 entry.js 的 inlineCss 一致：重复注入先移除旧节点）
      const cssFiles = readdirSync(webDir).filter((n) => n.endsWith('.css'))
      if (cssFiles.length) {
        const css = cssFiles
          .map((n) => readFileSync(`${webDir}/${n}`, 'utf8'))
          .join('\n')
        for (const n of cssFiles) unlinkSync(`${webDir}/${n}`)
        const injector =
          `(function(){if(typeof document==="undefined")return;var d=document;var i=d.getElementById("nexos-app-streaming-style");if(i)i.remove();var s=d.createElement("style");s.id="nexos-app-streaming-style";s.textContent=${JSON.stringify(
            css,
          )};d.head.appendChild(s)})();\n`
        writeFileSync(host, injector + readFileSync(host, 'utf8'))
      }
      // 4. process.env.NODE_ENV → "production"
      writeFileSync(
        host,
        readFileSync(host, 'utf8').replaceAll('process.env.NODE_ENV', '"production"'),
      )
    },
  }
}

export default defineConfig({
  plugins: [vue(), postProcessStandaloneHost()],
  resolve: {
    alias: {
      '@': fileURLToPath(new URL('./src', import.meta.url)),
      // @nexos/app-sdk：standalone 宿主自包含——把裸导入指到主前端 sdk/
      // 唯一事实源（crates/os-api/web/src/sdk/index.ts）整包打进本产物。
      // 桌面嵌入模式的 entry.js 构建走 vite.config.ts 的 host-externals
      // 重写（__NEXOS_HOST__.sdk 零打包）——两载体不同路径、同一源码。
      '@nexos/app-sdk': fileURLToPath(
        new URL('../../crates/os-api/web/src/sdk/index.ts', import.meta.url),
      ),
    },
  },
  build: {
    lib: {
      entry: fileURLToPath(new URL('./standalone/standalone-host.ts', import.meta.url)),
      formats: ['es'],
      fileName: () => 'standalone-host.js',
    },
    outDir: 'dist/web',
    // entry 构建先行清空过 dist/web——本构建只追加 standalone-host.js。
    emptyOutDir: false,
    cssCodeSplit: false,
    target: 'es2020',
    minify: 'esbuild',
  },
})
