import { useEffect, useMemo, useState } from 'react'
import { nativeToScVal, scValToNative, type xdr } from '@stellar/stellar-sdk'
import { useContractRead } from './useContractRead'
import { hexToBytes } from '../lib/format'

/**
 * Reads a milestone's deadline via `get_milestone_deadline` and keeps a
 * once-per-second clock running while one is set, so countdowns and the
 * Expire action update without a refetch.
 *
 * `deadline` is a Unix timestamp in seconds (the contract compares it against
 * the ledger timestamp), or `null` when no deadline has been set.
 */
export function useMilestoneDeadline(agreementId: string, milestoneId: number | undefined, enabled = true) {
  const args = useMemo(() => {
    if (milestoneId === undefined) return []
    return [
      nativeToScVal(hexToBytes(agreementId), { type: 'bytes' }),
      nativeToScVal(milestoneId, { type: 'u32' }),
    ]
  }, [agreementId, milestoneId])

  const { data, refetch } = useContractRead<xdr.ScVal>(
    'get_milestone_deadline',
    args,
    enabled && milestoneId !== undefined,
  )

  const deadline = useMemo<number | null>(() => {
    if (!data) return null
    try {
      const native: unknown = scValToNative(data)
      if (typeof native === 'bigint' || typeof native === 'number') return Number(native)
    } catch {
      // Malformed result — treat as "no deadline".
    }
    return null
  }, [data])

  const [now, setNow] = useState(() => Math.floor(Date.now() / 1000))
  useEffect(() => {
    if (deadline === null) return
    const timer = setInterval(() => setNow(Math.floor(Date.now() / 1000)), 1000)
    return () => clearInterval(timer)
  }, [deadline])

  return {
    deadline: enabled ? deadline : null,
    now,
    /** True once the deadline is strictly in the past, matching `expire_milestone`. */
    isPastDeadline: enabled && deadline !== null && now > deadline,
    refetch,
  }
}
