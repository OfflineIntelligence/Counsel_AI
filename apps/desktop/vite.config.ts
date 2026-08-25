import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'

function removeCrossorigin() {
  return {
    name: 'remove-crossorigin',
    transformIndexHtml(html: string) {
      return html
        .replace(/\s+crossorigin/g, '')
        .replace(/crossorigin\s+/g, '')
    }
  }
}

export default defineConfig({
  plugins: [react(), removeCrossorigin()],
  clearScreen: false,
  server: {
    port: 3000,
    strictPort: true,
    host: 'localhost',
  },
  base: './',
  build: {
    chunkSizeWarningLimit: 700,
    target: 'esnext',
    cssTarget: 'chrome80',
    rollupOptions: {
      output: {
        entryFileNames: 'assets/[name].js',
        chunkFileNames: 'assets/[name].js',
        assetFileNames: 'assets/[name].[ext]'
      }
    }
  },
  esbuild: {
    target: 'esnext'
  }
})
