/**
 * Sanitising payee-supplied `proof_uri` values before they become an `href`.
 *
 * `proof_uri` is free text written on-chain by the payee, so it is untrusted.
 * Rendering it straight into `<a href>` lets a payee plant a `javascript:` or
 * `data:` link that runs in the wallet-connected page when the payer clicks
 * it — `rel="noopener"` does nothing against that.
 *
 * Policy:
 * - `https:` URLs are allowed as-is (after WHATWG normalisation).
 * - `ipfs://<cid>[/path]` is allowed by rewriting it to the public HTTPS
 *   gateway, so the browser never sees a non-HTTPS scheme. The CLI docs
 *   suggest `ipfs://` proofs, so rejecting them outright would break the
 *   common case.
 * - Everything else (`http:`, `javascript:`, `data:`, `vbscript:`, `file:`,
 *   protocol-relative `//host`, relative paths, …) is rejected.
 *
 * Inputs containing ASCII whitespace or control characters are rejected
 * before parsing: the URL parser silently strips tabs/newlines, which is how
 * `java\tscript:` sneaks past naive prefix checks.
 */

export const IPFS_GATEWAY = 'https://ipfs.io/ipfs/'

// CIDv0 (base58btc `Qm…`) or CIDv1 (base32/base36 lowercase), followed by an
// optional path of URL-safe characters.
const IPFS_RE = /^ipfs:\/\/([A-Za-z0-9]{46,100})(\/[A-Za-z0-9._~!$&'()*+,;=:@%/-]*)?$/i

// eslint-disable-next-line no-control-regex
const UNSAFE_CHARS = /[\u0000- \u007f-\u009f  ]/

/**
 * Return a safe `href` for `uri`, or `null` if it must not be linked.
 */
export function safeProofHref(uri: string | null | undefined): string | null {
  if (typeof uri !== 'string' || uri.length === 0 || UNSAFE_CHARS.test(uri)) {
    return null
  }

  const ipfs = IPFS_RE.exec(uri)
  if (ipfs) {
    return `${IPFS_GATEWAY}${ipfs[1]}${ipfs[2] ?? ''}`
  }

  let url: URL
  try {
    // No base URL: relative and protocol-relative (`//host`) inputs throw.
    url = new URL(uri)
  } catch {
    return null
  }

  if (url.protocol !== 'https:' || url.hostname === '') {
    return null
  }
  // Credentials in the authority (`https://bank.com@evil.com`) are a classic
  // phishing disguise and never legitimate for a proof link.
  if (url.username !== '' || url.password !== '') {
    return null
  }

  return url.href
}
