import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'

// 构建产物直接落在 dist/，Rust 侧用 include_dir! 在编译期内嵌进二进制，
// 这样单容器就能提供管理台，不需要额外拷文件。
export default defineConfig({
  plugins: [react()],
  build: {
    outDir: 'dist',
    emptyOutDir: true,
    chunkSizeWarningLimit: 1200,
  },
  server: {
    port: 5173,
    proxy: {
      // 开发时前端跑 5173，API 转发到本地后端
      '/api': 'http://127.0.0.1:8080',
      '/v1': 'http://127.0.0.1:8080',
    },
  },
})
