/**
 * Lazy environment configuration.
 *
 * Values are validated on first access rather than at module evaluation time,
 * so a missing env var no longer crashes the app before React mounts — the
 * error can be caught by an error boundary and shown as a friendly UI message.
 *
 * Dev-mode fallbacks (console.warn) let the app load in a non-configured
 * environment without a hard crash.
 */

const DEV_FALLBACKS: Record<string, string> = {
  VITE_CONTRACT_ID: 'CAUAO7CYKULE2K4EJMQ6LLRUHP7Y7JYOH6G2VBXKYG7PTETE3UZ3DU7Q',
  VITE_RPC_URL: 'https://soroban-testnet.stellar.org',
  VITE_NETWORK_PASSPHRASE: 'Test SDF Network ; September 2015',
}

function getEnv(name: keyof typeof DEV_FALLBACKS): string {
  const value = import.meta.env[name]

  if (typeof value === 'string' && value) {
    return value
  }

  const fallback = DEV_FALLBACKS[name]

  if (import.meta.env.DEV) {
    console.warn(
      `[config] ${name} is not set. Using dev fallback: "${fallback}". ` +
        'Copy .env.example to .env and fill in real values.',
    )
    return fallback
  }

  throw new Error(
    `Environment variable ${name} is not set. Copy .env.example to .env and fill in values.`,
  )
}

export type NetworkId = 'testnet' | 'mainnet' | 'futurenet' | 'custom'

export const NETWORK_IDS: NetworkId[] = ['testnet', 'mainnet', 'futurenet', 'custom']

/**
 * Presets mirroring the CLI's `--network` flag (cli/trellis_cli/src/config.rs).
 * `custom` has no preset: it uses the build-time VITE_* values.
 */
const PRESETS: Record<Exclude<NetworkId, 'custom'>, { rpcUrl: string; passphrase: string }> = {
  testnet: {
    rpcUrl: 'https://soroban-testnet.stellar.org',
    passphrase: 'Test SDF Network ; September 2015',
  },
  mainnet: {
    rpcUrl: 'https://mainnet.sorobanrpc.com',
    passphrase: 'Public Global Stellar Network ; September 2015',
  },
  futurenet: {
    rpcUrl: 'https://rpc-futurenet.stellar.org',
    passphrase: 'Test SDF Future Network ; October 2022',
  },
}

const STORAGE_KEY = 'trellis.network'

function isNetworkId(value: unknown): value is NetworkId {
  return typeof value === 'string' && (NETWORK_IDS as string[]).includes(value)
}

/** The network the user picked in the UI, or null to use build-time env values. */
export function getSelectedNetwork(): NetworkId | null {
  try {
    const stored = localStorage.getItem(STORAGE_KEY)
    return isNetworkId(stored) ? stored : null
  } catch {
    return null
  }
}

/** Persists the choice. Consumers read config at import time, so callers should reload. */
export function setSelectedNetwork(network: NetworkId): void {
  try {
    localStorage.setItem(STORAGE_KEY, network)
  } catch {
    // Storage unavailable (private mode): the selection just won't persist.
  }
}

/** Network currently in effect, inferred from the build-time passphrase when nothing is selected. */
export function getActiveNetwork(): NetworkId {
  const selected = getSelectedNetwork()
  if (selected) return selected
  const passphrase = getEnv('VITE_NETWORK_PASSPHRASE')
  const match = (Object.keys(PRESETS) as Array<keyof typeof PRESETS>).find(
    (id) => PRESETS[id].passphrase === passphrase,
  )
  return match ?? 'custom'
}

function preset(): (typeof PRESETS)[keyof typeof PRESETS] | null {
  const selected = getSelectedNetwork()
  return selected && selected !== 'custom' ? PRESETS[selected] : null
}

export function getContractId(): string {
  // A non-default preset may have its own deployment: VITE_CONTRACT_ID_MAINNET etc.
  const selected = getSelectedNetwork()
  if (selected && selected !== 'custom') {
    const perNetwork = import.meta.env[`VITE_CONTRACT_ID_${selected.toUpperCase()}`]
    if (typeof perNetwork === 'string' && perNetwork) return perNetwork
  }
  return getEnv('VITE_CONTRACT_ID')
}

export function getRpcUrl(): string {
  return preset()?.rpcUrl ?? getEnv('VITE_RPC_URL')
}

export function getNetworkPassphrase(): string {
  return preset()?.passphrase ?? getEnv('VITE_NETWORK_PASSPHRASE')
}

/**
 * Base URL of the optional Trellis indexer/relay service (see indexer/ and
 * DEPLOYMENT.md). Unlike the variables above this one is optional: when it is
 * unset the event hooks poll Soroban RPC directly, exactly as before.
 */
export function getIndexerUrl(): string | null {
  const value = import.meta.env.VITE_INDEXER_URL
  return typeof value === 'string' && value ? value.replace(/\/+$/, '') : null
}

/**
 * Named re-exports kept for backwards compatibility with existing consumers.
 * The module-level constants are now derived via the lazy getters, so they
 * are still evaluated lazily on the first import rather than unconditionally
 * at bundle evaluation time.
 *
 * @deprecated Prefer the explicit getter functions (getContractId, getRpcUrl,
 *   getNetworkPassphrase) at new call sites.
 */
export const CONTRACT_ID = getContractId()
export const RPC_URL = getRpcUrl()
export const NETWORK_PASSPHRASE = getNetworkPassphrase()
