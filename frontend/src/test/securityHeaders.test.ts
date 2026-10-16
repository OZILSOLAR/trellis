import { describe, expect, it } from 'vitest'

import {
  assertCspSafe,
  buildCsp,
  buildHeadersFile,
  connectSources,
  DEFAULT_RPC_URL,
  securityHeaders,
} from '../../security-headers.ts'

const env = {
  VITE_RPC_URL: 'https://soroban-testnet.stellar.org/rpc',
  VITE_INDEXER_URL: 'https://indexer.example.com:8443/api',
}

describe('security headers (#596)', () => {
  it('never allows unsafe-eval', () => {
    const csp = buildCsp(env)
    expect(csp).not.toMatch(/unsafe-eval/i)
    expect(buildHeadersFile(env)).not.toMatch(/unsafe-eval/i)
  })

  it('fails if the policy gains unsafe-eval', () => {
    expect(() => assertCspSafe("script-src 'self' 'unsafe-eval'")).toThrow(/unsafe-eval/)
    expect(() => assertCspSafe("script-src 'self' 'UNSAFE-EVAL'")).toThrow()
    expect(() => assertCspSafe("script-src 'self' 'wasm-unsafe-eval'")).toThrow()
  })

  it('derives connect-src from the VITE_* endpoint origins', () => {
    expect(connectSources(env)).toEqual([
      "'self'",
      'https://soroban-testnet.stellar.org',
      'https://indexer.example.com:8443',
    ])
    expect(buildCsp(env)).toContain(
      "connect-src 'self' https://soroban-testnet.stellar.org https://indexer.example.com:8443",
    )
  })

  it('falls back to the default RPC and omits an unset indexer', () => {
    expect(connectSources({})).toEqual(["'self'", new URL(DEFAULT_RPC_URL).origin])
  })

  it('rejects malformed endpoint URLs instead of emitting a broken policy', () => {
    expect(() => buildCsp({ VITE_RPC_URL: 'not a url' })).toThrow(/VITE_RPC_URL/)
    expect(() => buildCsp({ VITE_INDEXER_URL: 'javascript:alert(1)' })).toThrow(/VITE_INDEXER_URL/)
  })

  it('locks down scripts, objects and framing', () => {
    const csp = buildCsp(env)
    expect(csp).toContain("script-src 'self'")
    expect(csp).not.toMatch(/script-src[^;]*unsafe-inline/)
    expect(csp).toContain("object-src 'none'")
    expect(csp).toContain("frame-ancestors 'none'")
    expect(buildCsp(env, false)).not.toContain('frame-ancestors')
  })

  it('emits nosniff, Referrer-Policy and frame protection headers', () => {
    const headers = securityHeaders(env)
    expect(headers['X-Content-Type-Options']).toBe('nosniff')
    expect(headers['Referrer-Policy']).toBe('strict-origin-when-cross-origin')
    expect(headers['X-Frame-Options']).toBe('DENY')

    const file = buildHeadersFile(env)
    expect(file.startsWith('/*\n')).toBe(true)
    expect(file).toContain('  Content-Security-Policy: ')
  })
})
