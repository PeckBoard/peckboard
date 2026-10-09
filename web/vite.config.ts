import { copyFileSync, mkdirSync } from 'node:fs'
import path from 'node:path'
import { fileURLToPath } from 'node:url'
import { defineConfig, type Plugin } from 'vite'
import react from '@vitejs/plugin-react'

const here = path.dirname(fileURLToPath(import.meta.url))

/**
 * Copy xterm's prebuilt UMD bundles into `dist/vendor/xterm/` so the release
 * binary serves them at `/vendor/xterm/*`. Plugin pages (sandboxed iframes
 * with an opaque origin, e.g. SSH Fleet's terminal) load them from there:
 * they can't import from the app bundle, and shipping offline means no CDN.
 */
function vendorXterm(): Plugin {
  return {
    name: 'peckboard-vendor-xterm',
    apply: 'build',
    closeBundle() {
      const out = path.join(here, 'dist', 'vendor', 'xterm')
      mkdirSync(out, { recursive: true })
      const files: Array<[string, string]> = [
        ['@xterm/xterm/lib/xterm.js', 'xterm.js'],
        ['@xterm/xterm/css/xterm.css', 'xterm.css'],
        ['@xterm/addon-fit/lib/addon-fit.js', 'addon-fit.js'],
      ]
      for (const [src, dst] of files) {
        copyFileSync(path.join(here, 'node_modules', src), path.join(out, dst))
      }
    },
  }
}

export default defineConfig({
  plugins: [react(), vendorXterm()],
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
