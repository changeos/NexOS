// =============================================================================
// apps/qrtransfer —— NexOS 应用包「二维码传输」构建配置。
//
// 产物：dist/web/entry.js（ESM 单文件：组件 + i18n + 图标 + CSS 内联；
//       vue / vue-i18n 不打进包，运行时经宿主桥 globalThis.__NEXOS_HOST__ 取主
//       前端同一份实例——保证响应式系统 / useI18n 与宿主共享，避免双 Vue 副本
//       导致的失活与注入失败）+ dist/manifest.json（构建后由脚本同步拷贝）。
//
// 宿主桥协议（主前端 crates/os-api/web/src/appRuntime.ts）：
//   __NEXOS_HOST__.vue     —— 主前端 vue 模块命名空间（ref/computed/…）
//   __NEXOS_HOST__.vueI18n —— 主前端 vue-i18n 模块命名空间（useI18n/…）
//   __NEXOS_HOST__.api     —— 主前端 api client 原语（get/post/del/request）
// （照 apps/film/vite.config.ts 同款，改 id / styleId。）
// =============================================================================
import { defineConfig, type Plugin } from 'vite'
import vue from '@vitejs/plugin-vue'
import { fileURLToPath, URL } from 'node:url'
import { readFileSync, readdirSync, unlinkSync, writeFileSync } from 'node:fs'
import * as vueExports from 'vue'
import * as vueI18nExports from 'vue-i18n'

/** 宿主桥上的命名空间路径。 */
const BRIDGE: Record<string, string> = {
  vue: 'vue',
  'vue-i18n': 'vueI18n',
  // @nexos/app-sdk（v0.1.28）：重写到宿主就绪 SDK 实例对象——该对象既是
  // ctx.sdk 同一实例，也携带工厂面（createSdk / SDK_VERSION），应用源码
  // `import { createSdk } from '@nexos/app-sdk'` 照常写、零打包。
  '@nexos/app-sdk': 'sdk',
}

/** 构建期导出名清单（从真实模块命名空间取，随版本演进自动覆盖新 API）。 */
const EXPORT_NAMES: Record<string, string[]> = {
  vue: Object.keys(vueExports),
  'vue-i18n': Object.keys(vueI18nExports),
  // SDK 运行时导出面（与 crates/os-api/web/src/sdk/index.ts 的 export 同步；
  // 类型导出构建期擦除，不在此列）
  '@nexos/app-sdk': ['createSdk', 'SDK_VERSION'],
}

/**
 * host-externals：把 `import {...} from 'vue' / 'vue-i18n'` 重写到宿主桥。
 * 输出形如：
 *   const __m = (globalThis.__NEXOS_HOST__ || {}).vue || {};
 *   export const { ref, computed, ... } = __m;
 * （宿主缺失时各导出为 undefined——仅影响宿主外直接运行，导入本身不抛错。）
 */
function hostExternals(): Plugin {
  const VIRTUAL_PREFIX = '\0virtual:host:'
  return {
    name: 'nexos-host-externals',
    enforce: 'pre',
    resolveId(source) {
      if (source in BRIDGE) return VIRTUAL_PREFIX + source
      return null
    },
    load(id) {
      if (!id.startsWith(VIRTUAL_PREFIX)) return null
      const pkg = id.slice(VIRTUAL_PREFIX.length)
      const path = BRIDGE[pkg]
      if (!path) return null
      const names = EXPORT_NAMES[pkg] ?? []
      return [
        `const __m = ((typeof globalThis !== 'undefined' ? globalThis : window).__NEXOS_HOST__ || {}).${path} || {};`,
        `export const { ${names.join(', ')} } = __m;`,
        '',
      ].join('\n')
    },
  }
}

/**
 * inline-css：lib 模式默认把 SFC 样式抽成独立 .css——单文件要求下，
 * 构建收尾（closeBundle，落盘后）把 dist/web/*.css 内联为 entry.js 头部的
 * <style> 注入语句并删除 css 文件。id 固定（先按 id 移除旧节点，重复注入安全）。
 */
function inlineCss(outDir: string, styleId: string): Plugin {
  return {
    name: 'nexos-inline-css',
    closeBundle() {
      const files = readdirSync(outDir).filter((n) => n.endsWith('.css'))
      if (!files.length) return
      const css = files
        .map((n) => readFileSync(`${outDir}/${n}`, 'utf8'))
        .join('\n')
      for (const n of files) unlinkSync(`${outDir}/${n}`)
      const entry = `${outDir}/entry.js`
      const js = readFileSync(entry, 'utf8')
      const injector =
        `(function(){if(typeof document==="undefined")return;var d=document;var i=d.getElementById(${JSON.stringify(
          styleId,
        )});if(i)i.remove();var s=d.createElement('style');s.id=${JSON.stringify(
          styleId,
        )};s.textContent=${JSON.stringify(css)};d.head.appendChild(s)})();\n`
      writeFileSync(entry, injector + js)
    },
  }
}

export default defineConfig({
  plugins: [
    hostExternals(),
    vue(),
    inlineCss(
      fileURLToPath(new URL('./dist/web', import.meta.url)),
      'nexos-app-qrtransfer-style',
    ),
  ],
  resolve: {
    alias: {
      '@': fileURLToPath(new URL('./src', import.meta.url)),
    },
  },
  build: {
    // lib 模式：ESM 单入口，固定文件名 entry.js（manifest.entry = web/entry.js）。
    lib: {
      entry: fileURLToPath(new URL('./src/entry.ts', import.meta.url)),
      formats: ['es'],
      fileName: () => 'entry.js',
    },
    outDir: 'dist/web',
    emptyOutDir: true,
    // 样式不拆分（配合 inline-css 插件收尾内联进 entry.js）。
    cssCodeSplit: false,
    // vue / vue-i18n 不设 rollupOptions.external：host-externals 的 resolveId
    // （enforce:pre）先接管，改写到宿主桥虚拟模块——若走 external 会残留浏览器
    // 无法解析的裸导入 'vue'。
    target: 'es2020',
    minify: 'esbuild',
  },
})
