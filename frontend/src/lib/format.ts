/** Shorten a 64-character hex agreement ID to `first 8…last 6`. */
export function truncateAgreementId(id: string): string {
  if (id.length <= 16) return id
  return `${id.slice(0, 8)}…${id.slice(-6)}`
}

const UNITS: Array<[Intl.RelativeTimeFormatUnit, number]> = [
  ['year', 365 * 24 * 60 * 60 * 1000],
  ['month', 30 * 24 * 60 * 60 * 1000],
  ['day', 24 * 60 * 60 * 1000],
  ['hour', 60 * 60 * 1000],
  ['minute', 60 * 1000],
]

const rtf = new Intl.RelativeTimeFormat('en', { numeric: 'auto' })

/** Render an ISO timestamp as relative time, e.g. "2 hours ago". */
export function formatRelativeTime(isoTimestamp: string, now: number = Date.now()): string {
  const timestamp = new Date(isoTimestamp).getTime()
  if (Number.isNaN(timestamp)) return 'unknown'

  const elapsed = now - timestamp
  if (elapsed < 0) return 'just now'
  for (const [unit, ms] of UNITS) {
    if (elapsed >= ms) {
      return rtf.format(-Math.round(elapsed / ms), unit)
    }
  }
  return 'just now'
}

/**
 * Decode a hex string into a Uint8Array without relying on Node.js Buffer.
 *
 * This is a browser-safe replacement for `Buffer.from(hex, 'hex')`.
 * It is intentionally strict: the string must consist of an even number of
 * hexadecimal characters (0-9 / a-f / A-F).
 *
 * @throws {Error} If `hex` contains non-hex characters or has an odd length.
 *
 * @example
 * hexToBytes('deadbeef') // Uint8Array [0xde, 0xad, 0xbe, 0xef]
 */
export function hexToBytes(hex: string): Uint8Array {
  if (hex.length % 2 !== 0) {
    throw new Error(`hexToBytes: expected even-length hex string, got length ${hex.length}`)
  }
  if (hex.length > 0 && !/^[0-9a-fA-F]+$/.test(hex)) {
    throw new Error('hexToBytes: input contains non-hexadecimal characters')
  }

  const bytes = new Uint8Array(hex.length / 2)
  for (let i = 0; i < bytes.length; i++) {
    bytes[i] = parseInt(hex.slice(i * 2, i * 2 + 2), 16)
  }
  return bytes
}

/**
 * Format a duration in seconds as a compact countdown, e.g. `2d 3h 4m` or
 * `4m 05s`. Seconds are only shown once less than an hour remains.
 */
export function formatCountdown(totalSeconds: number): string {
  const seconds = Math.max(0, Math.floor(totalSeconds))
  const days = Math.floor(seconds / 86_400)
  const hours = Math.floor((seconds % 86_400) / 3_600)
  const minutes = Math.floor((seconds % 3_600) / 60)
  const secs = seconds % 60

  if (days > 0) return `${days}d ${hours}h ${minutes}m`
  if (hours > 0) return `${hours}h ${minutes}m`
  return `${minutes}m ${String(secs).padStart(2, '0')}s`
}
