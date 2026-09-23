import vue from '@vitejs/plugin-vue'
import { defineConfig } from 'vite'

export default defineConfig({
  plugins: [vue()],
  base: '/manage/panel/',
  server: {
    proxy: {
      '/manage/api': {
        target: 'http://192.168.168.254:8088',
        changeOrigin: true,
      },
    },
  },
})
