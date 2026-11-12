import js from '@eslint/js'
import globals from 'globals'
import reactHooks from 'eslint-plugin-react-hooks'
import reactRefresh from 'eslint-plugin-react-refresh'
import tseslint from 'typescript-eslint'
import prettier from 'eslint-config-prettier'
import { defineConfig, globalIgnores } from 'eslint/config'

export default defineConfig([
  globalIgnores(['dist']),
  {
    files: ['**/*.{ts,tsx}'],
    extends: [
      js.configs.recommended,
      tseslint.configs.recommended,
      reactHooks.configs.flat.recommended,
      reactRefresh.configs.vite,
      prettier,
    ],
    languageOptions: {
      globals: globals.browser,
    },
    rules: {
      // Throws on plain-http origins (LAN / Tailscale installs) and crashes
      // the page — use `uuid()` from src/lib/uuid.ts.
      'no-restricted-properties': [
        'error',
        {
          object: 'crypto',
          property: 'randomUUID',
          message: 'Not available on plain-http origins; use uuid() from src/lib/uuid.ts.',
        },
      ],
    },
  },
  {
    // The guarded fallbacks themselves.
    files: ['src/lib/uuid.ts', 'src/lib/widgetGrid.ts'],
    rules: { 'no-restricted-properties': 'off' },
  },
])
