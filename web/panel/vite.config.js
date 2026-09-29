import process from 'node:process'
import vue from '@vitejs/plugin-vue'
import { defineConfig } from 'vite'

export default defineConfig({
  plugins: [vue()],
  base: '/manage/panel/',
  server: {
    proxy: {
      '/manage/api': {
        target: process.env.HERDEX_DEV_PROXY || 'http://127.0.0.1:8317',
        changeOrigin: true,
      },
    },
  },
})
