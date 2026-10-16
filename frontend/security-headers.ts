/**
 * Content-Security-Policy and security response headers for the production
 * build (#596).
 *
 * The app is a wallet-connected page, so any XSS (e.g. a malicious
 * payee-supplied link) must not be able to load or eval arbitrary script or
 * exfiltrate data to an arbitrary host. `connect-src` is therefore derived
 * from the same `VITE_*` endpoints Vite inlines into the bundle, so a deploy
 * pointed at a different RPC/indexer gets a matching policy automatically.
 *
 * Kept free of Node imports so it can be unit-tested from `src/` by Vitest
 * and type-checked under both tsconfig projects.
 *
 * Freighter talks to the page via `window.postMessage` from its content
 * script, which CSP does not govern, so the wallet needs no allowance here.
 */

export interface SecurityHeaderEnv {
  VITE_RPC_URL?: string
  VITE_INDEXER_URL?: string
}

/** Same default as `src/lib/config.ts` DEV_FALLBACKS. */
export const DEFAULT_RPC_URL = 'https://soroban-testnet.stellar.org'

/** Directive values that must never appear in the policy. */
export const FORBIDDEN_CSP_TOKENS = ["'unsafe-eval'", "'wasm-unsafe-eval'"] as const

/**
 * Origin (`scheme://host[:port]`) of an endpoint URL. Throws on anything that
 * is not an absolute http(s) URL so a typo fails the build instead of
 * silently producing a policy that blocks every RPC call.
 */
export function endpointOrigin(name: string, value: string): string {
  let url: URL
  try {
    url = new URL(value)
  } catch {
    throw new Error(`${name} is not an absolute URL: ${JSON.stringify(value)}`)
  }
  if (url.protocol !== 'https:' && url.protocol !== 'http:') {
    throw new Error(`${name} must be an http(s) URL, got ${url.protocol}`)
  }
  return url.origin
}

/** Origins the app is allowed to `fetch` / `EventSource` from. */
export function connectSources(env: SecurityHeaderEnv): string[] {
  const sources = ["'self'", endpointOrigin('VITE_RPC_URL', env.VITE_RPC_URL || DEFAULT_RPC_URL)]
  if (env.VITE_INDEXER_URL) {
    sources.push(endpointOrigin('VITE_INDEXER_URL', env.VITE_INDEXER_URL))
  }
  return [...new Set(sources)]
}

/**
 * Build the CSP directives. `includeFrameAncestors` is false for the
 * `<meta>` fallback, where browsers ignore `frame-ancestors`.
 */
export function buildCsp(env: SecurityHeaderEnv, includeFrameAncestors = true): string {
  const directives: [string, string[]][] = [
    ['default-src', ["'self'"]],
    ['script-src', ["'self'"]],
    ['style-src', ["'self'", 'https://fonts.googleapis.com']],
    ['font-src', ["'self'", 'https://fonts.gstatic.com']],
    // data: for the locally generated agreement-ID QR code.
    ['img-src', ["'self'", 'data:']],
    ['connect-src', connectSources(env)],
    ['object-src', ["'none'"]],
    ['base-uri', ["'self'"]],
    ['form-action', ["'self'"]],
  ]
  if (includeFrameAncestors) {
    directives.push(['frame-ancestors', ["'none'"]])
  }
  const csp = directives.map(([name, values]) => `${name} ${values.join(' ')}`).join('; ')
  assertCspSafe(csp)
  return csp
}

/** Throw if the policy allows eval-style script execution. */
export function assertCspSafe(csp: string): void {
  const lower = csp.toLowerCase()
  for (const token of FORBIDDEN_CSP_TOKENS) {
    if (lower.includes(token)) {
      throw new Error(`Content-Security-Policy must not contain ${token}: ${csp}`)
    }
  }
}

/** Header name → value, applied to every path. */
export function securityHeaders(env: SecurityHeaderEnv): Record<string, string> {
  return {
    'Content-Security-Policy': buildCsp(env),
    'X-Content-Type-Options': 'nosniff',
    'Referrer-Policy': 'strict-origin-when-cross-origin',
    // Legacy counterpart of frame-ancestors 'none' for older browsers.
    'X-Frame-Options': 'DENY',
    'Permissions-Policy': 'camera=(), microphone=(), geolocation=(), payment=()',
  }
}

/** Render a Netlify / Cloudflare Pages `_headers` file. */
export function buildHeadersFile(env: SecurityHeaderEnv): string {
  const lines = ['/*']
  for (const [name, value] of Object.entries(securityHeaders(env))) {
    lines.push(`  ${name}: ${value}`)
  }
  return `${lines.join('\n')}\n`
}
