import { describe, expect, it } from 'vitest'

import { IPFS_GATEWAY, safeProofHref } from './proofUri'

const CID_V0 = 'QmYwAPJzv5CZsnA625s3Xf2nemtYgPpHdWEz79ojWnPbdG'
const CID_V1 = 'bafybeigdyrzt5sfp7udm7hu76uh7y26nf3efuylqabf3oclgtqy55fbzdi'

describe('safeProofHref', () => {
  it('allows https URLs', () => {
    expect(safeProofHref('https://github.com/org/repo/pull/1')).toBe(
      'https://github.com/org/repo/pull/1',
    )
    expect(safeProofHref('HTTPS://Example.com/a?b=c#d')).toBe('https://example.com/a?b=c#d')
  })

  it('rewrites ipfs:// URIs to the HTTPS gateway', () => {
    expect(safeProofHref(`ipfs://${CID_V0}`)).toBe(`${IPFS_GATEWAY}${CID_V0}`)
    expect(safeProofHref(`ipfs://${CID_V1}/report.pdf`)).toBe(
      `${IPFS_GATEWAY}${CID_V1}/report.pdf`,
    )
  })

  it.each([
    ['javascript:', 'javascript:alert(document.cookie)'],
    ['mixed-case javascript:', 'JaVaScRiPt:alert(1)'],
    ['tab-split javascript:', 'java\tscript:alert(1)'],
    ['newline-split javascript:', 'java\nscript:alert(1)'],
    ['leading-space javascript:', ' javascript:alert(1)'],
    ['entity-free vbscript:', 'vbscript:msgbox(1)'],
    ['data:', 'data:text/html;base64,PHNjcmlwdD5hbGVydCgxKTwvc2NyaXB0Pg=='],
    ['protocol-relative', '//evil.example/x'],
    ['backslash protocol-relative', '\\\\evil.example/x'],
    ['relative path', '/proof.pdf'],
    ['http:', 'http://example.com/proof'],
    ['file:', 'file:///etc/passwd'],
    ['credentials in authority', 'https://bank.example@evil.example/'],
    ['ipfs with a bogus CID', 'ipfs://not-a-cid'],
    ['ipfs with script payload', `ipfs://${CID_V0}/"><script>`],
    ['empty string', ''],
  ])('rejects %s', (_label, input) => {
    expect(safeProofHref(input)).toBeNull()
  })

  it('rejects null and undefined', () => {
    expect(safeProofHref(null)).toBeNull()
    expect(safeProofHref(undefined)).toBeNull()
  })
})
