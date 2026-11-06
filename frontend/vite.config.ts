import CodexProxyUI from '@codex-proxy/ui/vite'
import tailwindcss from '@tailwindcss/vite'
import vue from '@vitejs/plugin-vue'
import { defineConfig } from 'vite'

export default defineConfig({
  base: './',
  plugins: [
    vue(),
    tailwindcss(),
    CodexProxyUI(),
    {
      name: 'plugin-classic-script',
      // 隔离页面使用经典脚本，资源引用仍由 Vite 生成并交给宿主校验
      transformIndexHtml: {
        order: 'post',
        handler: html => html.replace('type="module" crossorigin', 'defer'),
      },
    },
  ],
  build: {
    modulePreload: false,
    cssCodeSplit: false,
    rolldownOptions: {
      output: {
        format: 'iife',
        codeSplitting: false,
        entryFileNames: 'app.js',
        assetFileNames: 'app.[ext]',
      },
    },
  },
})
