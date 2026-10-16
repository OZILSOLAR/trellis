// defineConfig comes from vitest/config rather than vite so the `test` block
// below is typed; it is a superset of vite's own defineConfig.
import { defineConfig } from 'vitest/config'
import { loadEnv, type Plugin } from 'vite'
import react from '@vitejs/plugin-react'
import { readFileSync } from 'node:fs'
import { resolve } from 'node:path'
import { buildCsp, buildHeadersFile, type SecurityHeaderEnv } from './security-headers.ts'

// Read version from package.json so the string is always consistent with the
// published package version, regardless of whether npm_package_version is set
// in the environment (it is only injected by npm/pnpm run scripts).
function resolveAppVersion(): string {
  try {
    const pkg = JSON.parse(
      readFileSync(resolve(import.meta.dirname, 'package.json'), 'utf-8'),
    ) as { version?: string }
    return pkg.version ?? 'dev'
  } catch {
    return 'dev'
  }
}

// Emits `dist/_headers` (CSP + security headers, honoured by Netlify and
// Cloudflare Pages) and a matching `<meta>` CSP in index.html for hosts that
// ignore `_headers`. Build-only: the dev server needs inline scripts and an
// HMR websocket that the production policy deliberately forbids. Both
// builders throw if the policy ever gains 'unsafe-eval', failing the build.
function securityHeadersPlugin(env: SecurityHeaderEnv): Plugin {
  return {
    name: 'trellis-security-headers',
    apply: 'build',
    transformIndexHtml() {
      return [
        {
          tag: 'meta',
          attrs: { 'http-equiv': 'Content-Security-Policy', content: buildCsp(env, false) },
          injectTo: 'head-prepend',
        },
      ]
    },
    generateBundle() {
      this.emitFile({ type: 'asset', fileName: '_headers', source: buildHeadersFile(env) })
    },
  }
}

// https://vite.dev/config/
export default defineConfig(({ mode }) => {
  // Load environment variables including .env.test for test mode
  const env = loadEnv(mode, process.cwd(), 'VITE_')

  return {
    // 'spa' makes the dev server fall back to index.html for any path that
    // isn't a real file, so a hard refresh on a client-side route (e.g.
    // /agreement/123) resolves through React Router instead of 404ing.
    appType: 'spa',
    plugins: [react(), securityHeadersPlugin(env)],
    define: {
      // __APP_VERSION__ is declared as a global in src/vite-env.d.ts so
      // TypeScript knows the type without a window/globalThis cast.
      __APP_VERSION__: JSON.stringify(resolveAppVersion()),
    },
    test: {
      // Components render against the DOM, so the whole suite runs in jsdom
      // rather than splitting into separate node/jsdom projects.
      environment: 'jsdom',
      globals: true,
      setupFiles: ['./src/test/setup.ts'],
      include: ['src/**/*.{test,spec}.{ts,tsx}'],
      css: false,
      // Set environment variables for tests from .env.test
      env: {
        VITE_CONTRACT_ID:
          env.VITE_CONTRACT_ID ||
          'CAUAO7CYKULE2K4EJMQ6LLRUHP7Y7JYOH6G2VBXKYG7PTETE3UZ3DU7Q',
        VITE_RPC_URL: env.VITE_RPC_URL || 'https://soroban-testnet.stellar.org',
        VITE_NETWORK_PASSPHRASE:
          env.VITE_NETWORK_PASSPHRASE || 'Test SDF Network ; September 2015',
      },
      coverage: {
        provider: 'v8',
        reporter: ['text', 'lcov'],
        include: ['src/**/*.{ts,tsx}'],
        // Entry point, generated assets and the test harness itself carry no
        // logic worth covering.
        exclude: ['src/main.tsx', 'src/vite-env.d.ts', 'src/test/**'],
      },
    },
  }
})
