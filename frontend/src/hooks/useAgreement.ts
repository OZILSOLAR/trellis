import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import { nativeToScVal, scValToNative, xdr } from '@stellar/stellar-sdk'
import { useContractRead } from './useContractRead'
import { hexToBytes } from '../lib/format'
import type { Agreement, RawAgreementResponse, RawMilestone } from '../lib/soroban'
import { isValidAgreementResponse, parseEscrowStatus, releasedAmountFor } from '../lib/soroban'

const MAX_RETRIES = 3

/**
 * Hook to fetch agreement data from the contract using get_agreement entrypoint.
 *
 * Returns a standardised { data, loading, error } shape alongside typed
 * helpers so consuming components can render skeleton loaders on `isLoading`
 * and surface error banners on `isError`.
 *
 * Retry logic: up to MAX_RETRIES automatic retries are attempted on failure
 * before the error state is surfaced to the UI.  Retries use incremental back-
 * off (1 s, 2 s, 3 s).  The retry counter resets whenever the agreementId
 * changes or the consumer calls `refetch()`.
 */
export function useAgreement(agreementId: string | null) {
  const [retryCount, setRetryCount] = useState(0)
  // Used to keep the auto-retry effect stable across renders.
  const retryCountRef = useRef(retryCount)
  retryCountRef.current = retryCount

  const args = useMemo(() => {
    if (!agreementId) return []

    // Convert agreement ID (hex string) to ScVal bytes
    const idBytes = hexToBytes(agreementId)
    return [nativeToScVal(idBytes, { type: 'bytes' })]
  }, [agreementId])

  const { data, status, loading, error, refetch: _refetch } = useContractRead<xdr.ScVal>(
    'get_agreement',
    args,
    !!agreementId,
  )

  // Auto-retry on transient failures before surfacing the error to the UI.
  useEffect(() => {
    if (status !== 'error') return
    if (retryCountRef.current >= MAX_RETRIES) return

    const delay = 1000 * (retryCountRef.current + 1) // 1 s, 2 s, 3 s
    const timer = setTimeout(() => {
      setRetryCount((c) => c + 1)
      _refetch()
    }, delay)

    return () => clearTimeout(timer)
  }, [status, _refetch])

  // Reset retry counter when the queried ID changes.
  useEffect(() => {
    setRetryCount(0)
  }, [agreementId])

  /**
   * Manually re-fetch and reset the retry counter so the user can try again
   * after the auto-retries have been exhausted.
   */
  const refetch = useCallback(() => {
    setRetryCount(0)
    _refetch()
  }, [_refetch])

  const agreement = useMemo<Agreement | null>(() => {
    if (!data) return null

    try {
      // Parse the ScVal result into Agreement structure
      const native = scValToNative(data)

      // Validate the response structure
      if (!isValidAgreementResponse(native)) {
        console.error('[useAgreement] Invalid agreement response structure:', native)
        return null
      }

      const response = native as RawAgreementResponse
      const milestonesArray = response.milestones || []

      return {
        agreement_id: agreementId!,
        payer: response.payer || '',
        payee: response.payee || '',
        token: response.token || '',
        dispute_resolver: response.dispute_resolver || '',
        milestones: milestonesArray.map((m: RawMilestone, index: number) => ({
          id: index,
          amount: typeof m.amount === 'string' ? m.amount : (m.amount?.toString() || '0'),
          status: parseEscrowStatus(m.status),
          proof_uri: m.proof_uri || '',
          released: releasedAmountFor(response.released_amounts, index),
        })),
      }
    } catch (err) {
      console.error('[useAgreement] Failed to parse agreement data:', err)
      return null
    }
  }, [data, agreementId])

  return {
    /** Parsed agreement or null while loading / on error. */
    agreement,
    /** True while the RPC call is in-flight (maps to status === 'loading'). */
    isLoading: loading,
    /** True when the last attempt ended in error and all retries are exhausted. */
    isError: status === 'error' && retryCount >= MAX_RETRIES,
    /** Human-readable error message, or null. */
    error,
    /** Granular status string for consumers that need it. */
    status,
    /** Manually re-fetch and reset the retry counter. */
    refetch,
  }
}
