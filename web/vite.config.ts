import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'

export default defineConfig({
  plugins: [react()],
  server: {
    port: 5173,
    proxy: {
      '/api': 'http://localhost:3333',
      '/ws': {
        target: 'http://localhost:3333',
        ws: true,
      },
    },
  },
  build: {
    outDir: 'dist',
    // Off for shipped builds — the release binary embeds web/dist, and we
    // don't ship our sources to users. On only for the impact-map run
    // (scripts/e2e-impact-map.sh), which needs to trace V8 coverage of the
    // minified bundle back to web/src/** files.
    sourcemap: process.env.PECKBOARD_E2E_COVERAGE === '1',
  },
})
