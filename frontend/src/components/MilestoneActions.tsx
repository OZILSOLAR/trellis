import { useRef, useState } from 'react'
import { nativeToScVal, xdr } from '@stellar/stellar-sdk'
import { useContractInvoke } from '../hooks/useContractInvoke'
import { useWallet } from '../context/WalletContext'
import { useToastActions } from '../hooks/useToast'
import TransactionStatus from './TransactionStatus'
import { formatCountdown, hexToBytes } from '../lib/format'
import { useMilestoneDeadline } from '../hooks/useMilestoneDeadline'
import { remainingAmount, type Agreement, type Milestone } from '../lib/soroban'

interface MilestoneActionsProps {
  milestone: Milestone
  agreement: Agreement
  onSuccess?: () => void
}

/**
 * Encodes `proof_uri` for the contract's `Option<String>` parameter.
 * A blank input means "no proof link" and maps to None (ScVal void), never
 * to an empty string.
 */
export function proofUriToScVal(proofUri: string): xdr.ScVal {
  const trimmed = proofUri.trim()
  return trimmed ? nativeToScVal(trimmed, { type: 'string' }) : xdr.ScVal.scvVoid()
}

/** Matches a non-negative integer amount in the token's base units (stroops). */
const BASE_UNIT_AMOUNT = /^\d+$/

/**
 * Validates a resolver's split of a disputed milestone before it is signed.
 *
 * Mirrors `resolve_dispute_split`'s on-chain checks: both legs must be
 * non-negative integers (base units) and together equal exactly what is still
 * escrowed for the milestone. Returns an error message, or `null` when valid.
 */
export function validateSplit(payerAmount: string, payeeAmount: string, remaining: bigint): string | null {
  const payer = payerAmount.trim()
  const payee = payeeAmount.trim()
  if (!payer || !payee) return 'Enter an amount for both the payer and the payee.'
  if (!BASE_UNIT_AMOUNT.test(payer) || !BASE_UNIT_AMOUNT.test(payee)) {
    return 'Amounts must be non-negative whole numbers.'
  }
  if (BigInt(payer) + BigInt(payee) !== remaining) {
    return `Payer and payee amounts must sum to the remaining escrow (${remaining.toString()}).`
  }
  return null
}

/**
 * Validates a payer's partial release before it is signed. Mirrors
 * `release_partial`: the amount must be a positive whole number (base units)
 * no larger than what is still escrowed. Returns an error message or `null`.
 */
export function validatePartialRelease(amount: string, remaining: bigint): string | null {
  const value = amount.trim()
  if (!value) return 'Enter an amount to release.'
  if (!BASE_UNIT_AMOUNT.test(value)) return 'Amount must be a positive whole number.'
  const parsed = BigInt(value)
  if (parsed <= 0n) return 'Amount must be greater than zero.'
  if (parsed > remaining) return `Amount cannot exceed the remaining escrow (${remaining.toString()}).`
  return null
}

/**
 * Converts a `<input type="datetime-local">` value (local time) to the Unix
 * seconds `set_milestone_deadline` expects, rejecting anything not strictly in
 * the future — the contract fails such deadlines with `DeadlineInPast`.
 */
export function parseDeadlineInput(value: string, nowSeconds: number): { deadline: number } | { error: string } {
  if (!value) return { error: 'Pick a deadline.' }
  const ms = new Date(value).getTime()
  if (Number.isNaN(ms)) return { error: 'Invalid date.' }
  const deadline = Math.floor(ms / 1000)
  if (deadline <= nowSeconds) return { error: 'Deadline must be in the future.' }
  return { deadline }
}

/** Formats a Date as the local-time string a `datetime-local` input expects. */
function toDateTimeLocal(date: Date): string {
  const pad = (n: number) => String(n).padStart(2, '0')
  return `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())}T${pad(date.getHours())}:${pad(date.getMinutes())}`
}

const MAX_RETRIES = 3
const BASE_FEE = 100_000

export default function MilestoneActions({ milestone, agreement, onSuccess }: MilestoneActionsProps) {
  const wallet = useWallet()
  const toast = useToastActions()
  const { invoke, status, txHash, error, reset } = useContractInvoke()
  const [showProofInput, setShowProofInput] = useState(false)
  const [proofUri, setProofUri] = useState('')
  const [showConfirm, setShowConfirm] = useState<string | null>(null)
  const [isRetrying, setIsRetrying] = useState(false)
  const [showSplitForm, setShowSplitForm] = useState(false)
  const [splitPayer, setSplitPayer] = useState('')
  const [splitPayee, setSplitPayee] = useState('')
  const [showReleaseForm, setShowReleaseForm] = useState(false)
  const [releaseAmount, setReleaseAmount] = useState('')
  const [showDeadlineForm, setShowDeadlineForm] = useState(false)
  const [deadlineInput, setDeadlineInput] = useState('')
  const pendingAction = useRef<string | null>(null)

  const isLoading = status === 'building' || status === 'signing' || status === 'submitting'
  const isUserPayer = wallet.publicKey === agreement.payer
  const isUserPayee = wallet.publicKey === agreement.payee
  const isUserResolver = !!wallet.publicKey && wallet.publicKey === agreement.dispute_resolver
  // What is still escrowed after any release_partial payouts — rulings and
  // approvals only ever move this remainder.
  const remainingEscrow = remainingAmount(milestone)
  const splitError = validateSplit(splitPayer, splitPayee, remainingEscrow)
  const releaseError = validatePartialRelease(releaseAmount, remainingEscrow)

  // Deadlines only matter while a milestone can still be expired.
  const isExpirable = milestone.status === 'Pending' || milestone.status === 'Funded'
  const {
    deadline,
    now,
    isPastDeadline,
    refetch: refetchDeadline,
  } = useMilestoneDeadline(agreement.agreement_id, milestone.id, isExpirable)
  const parsedDeadline = parseDeadlineInput(deadlineInput, now)
  const deadlineError = 'error' in parsedDeadline ? parsedDeadline.error : null

  // lock_funds, submit_work and approve_and_release take no caller argument —
  // the contract derives the signer from agreement.payer / agreement.payee via
  // require_auth(). Only raise_dispute takes an explicit leading `caller`.
  const handleLockFunds = async (fee?: string) => {
    if (!wallet.publicKey) return

    pendingAction.current = 'lock_funds'
    try {
      const idBytes = hexToBytes(agreement.agreement_id)
      const args = [
        nativeToScVal(idBytes, { type: 'bytes' }),
        nativeToScVal(milestone.id, { type: 'u32' }),
      ]

      await invoke('lock_funds', args, wallet.publicKey, fee)
      toast.success({ title: 'Funds locked successfully' })
      setShowConfirm(null)
      setRetryCount(0)
      pendingAction.current = null
      onSuccess?.()
    } catch (err) {
      toast.error({ title: 'Lock funds failed', message: err instanceof Error ? err.message : 'Unknown error' })
    }
  }

  const handleSubmitWork = async (fee?: string) => {
    if (!wallet.publicKey) return

    pendingAction.current = 'submit_work'
    try {
      const idBytes = hexToBytes(agreement.agreement_id)
      const args = [
        nativeToScVal(idBytes, { type: 'bytes' }),
        nativeToScVal(milestone.id, { type: 'u32' }),
        proofUriToScVal(proofUri),
      ]

      await invoke('submit_work', args, wallet.publicKey, fee)
      toast.success({ title: 'Work submitted successfully' })
      setShowProofInput(false)
      setProofUri('')
      setShowConfirm(null)
      setRetryCount(0)
      pendingAction.current = null
      onSuccess?.()
    } catch (err) {
      toast.error({ title: 'Submit work failed', message: err instanceof Error ? err.message : 'Unknown error' })
    }
  }

  const handleApproveRelease = async (fee?: string) => {
    if (!wallet.publicKey) return

    pendingAction.current = 'approve_and_release'
    try {
      const idBytes = hexToBytes(agreement.agreement_id)
      const args = [
        nativeToScVal(idBytes, { type: 'bytes' }),
        nativeToScVal(milestone.id, { type: 'u32' }),
      ]

      await invoke('approve_and_release', args, wallet.publicKey, fee)
      toast.success({ title: 'Milestone approved and funds released' })
      setShowConfirm(null)
      setRetryCount(0)
      pendingAction.current = null
      onSuccess?.()
    } catch (err) {
      toast.error({ title: 'Approve failed', message: err instanceof Error ? err.message : 'Unknown error' })
    }
  }

  const handleRaiseDispute = async (fee?: string) => {
    if (!wallet.publicKey) return

    pendingAction.current = 'raise_dispute'
    try {
      const idBytes = hexToBytes(agreement.agreement_id)
      const args = [
        nativeToScVal(wallet.publicKey, { type: 'address' }),
        nativeToScVal(idBytes, { type: 'bytes' }),
        nativeToScVal(milestone.id, { type: 'u32' }),
        xdr.ScVal.scvVoid(), // reason_uri: Option<String> = None
      ]

      await invoke('raise_dispute', args, wallet.publicKey, fee)
      toast.success({ title: 'Dispute raised' })
      setShowConfirm(null)
      setRetryCount(0)
      pendingAction.current = null
      onSuccess?.()
    } catch (err) {
      toast.error({ title: 'Raise dispute failed', message: err instanceof Error ? err.message : 'Unknown error' })
    }
  }

  // cancel_unfunded_milestone and set_milestone_deadline take no caller
  // argument — the contract requires the payer's auth.
  const handleCancelUnfunded = async (fee?: string) => {
    if (!wallet.publicKey) return

    pendingAction.current = 'cancel_unfunded_milestone'
    try {
      const idBytes = hexToBytes(agreement.agreement_id)
      const args = [
        nativeToScVal(idBytes, { type: 'bytes' }),
        nativeToScVal(milestone.id, { type: 'u32' }),
      ]

      await invoke('cancel_unfunded_milestone', args, wallet.publicKey, fee)
      toast.success({ title: 'Milestone cancelled' })
      setShowConfirm(null)
      pendingAction.current = null
      onSuccess?.()
    } catch (err) {
      toast.error({ title: 'Cancel milestone failed', message: err instanceof Error ? err.message : 'Unknown error' })
    }
  }

  const handleSetDeadline = async (fee?: string) => {
    if (!wallet.publicKey) return
    // Re-check against the current clock: a past deadline is rejected on-chain.
    const parsed = parseDeadlineInput(deadlineInput, Math.floor(Date.now() / 1000))
    if ('error' in parsed) return

    pendingAction.current = 'set_milestone_deadline'
    try {
      const idBytes = hexToBytes(agreement.agreement_id)
      const args = [
        nativeToScVal(idBytes, { type: 'bytes' }),
        nativeToScVal(milestone.id, { type: 'u32' }),
        nativeToScVal(BigInt(parsed.deadline), { type: 'u64' }),
      ]

      await invoke('set_milestone_deadline', args, wallet.publicKey, fee)
      toast.success({ title: 'Deadline set' })
      setShowDeadlineForm(false)
      setDeadlineInput('')
      pendingAction.current = null
      refetchDeadline()
      onSuccess?.()
    } catch (err) {
      toast.error({ title: 'Set deadline failed', message: err instanceof Error ? err.message : 'Unknown error' })
    }
  }

  // expire_milestone is permissionless but takes an explicit leading `caller`
  // that must authorise the call.
  const handleExpire = async (fee?: string) => {
    if (!wallet.publicKey) return

    pendingAction.current = 'expire_milestone'
    try {
      const idBytes = hexToBytes(agreement.agreement_id)
      const args = [
        nativeToScVal(wallet.publicKey, { type: 'address' }),
        nativeToScVal(idBytes, { type: 'bytes' }),
        nativeToScVal(milestone.id, { type: 'u32' }),
      ]

      await invoke('expire_milestone', args, wallet.publicKey, fee)
      toast.success({ title: 'Milestone expired' })
      setShowConfirm(null)
      pendingAction.current = null
      onSuccess?.()
    } catch (err) {
      toast.error({ title: 'Expire milestone failed', message: err instanceof Error ? err.message : 'Unknown error' })
    }
  }

  // release_partial takes no caller argument — the contract requires the
  // payer's auth.
  const handleReleasePartial = async (fee?: string) => {
    if (!wallet.publicKey) return
    // Never ask the wallet to sign a release the contract would reject.
    if (releaseError) return

    pendingAction.current = 'release_partial'
    try {
      const idBytes = hexToBytes(agreement.agreement_id)
      const args = [
        nativeToScVal(idBytes, { type: 'bytes' }),
        nativeToScVal(milestone.id, { type: 'u32' }),
        nativeToScVal(BigInt(releaseAmount.trim()), { type: 'i128' }),
      ]

      await invoke('release_partial', args, wallet.publicKey, fee)
      toast.success({ title: 'Partial release sent to payee' })
      setShowReleaseForm(false)
      setReleaseAmount('')
      pendingAction.current = null
      onSuccess?.()
    } catch (err) {
      toast.error({ title: 'Partial release failed', message: err instanceof Error ? err.message : 'Unknown error' })
    }
  }

  // resolve_dispute and resolve_dispute_split take no caller argument — the
  // contract gates both on agreement.dispute_resolver.require_auth().
  const handleResolveDispute = async (refundToPayer: boolean, fee?: string) => {
    if (!wallet.publicKey) return

    pendingAction.current = refundToPayer ? 'resolve_dispute_refund' : 'resolve_dispute_pay'
    try {
      const idBytes = hexToBytes(agreement.agreement_id)
      const args = [
        nativeToScVal(idBytes, { type: 'bytes' }),
        nativeToScVal(milestone.id, { type: 'u32' }),
        nativeToScVal(refundToPayer, { type: 'bool' }),
      ]

      await invoke('resolve_dispute', args, wallet.publicKey, fee)
      toast.success({ title: refundToPayer ? 'Dispute resolved: payer refunded' : 'Dispute resolved: payee paid' })
      setShowConfirm(null)
      pendingAction.current = null
      onSuccess?.()
    } catch (err) {
      toast.error({ title: 'Resolve dispute failed', message: err instanceof Error ? err.message : 'Unknown error' })
    }
  }

  const handleResolveSplit = async (fee?: string) => {
    if (!wallet.publicKey) return
    // Never ask the wallet to sign a split the contract would reject.
    if (splitError) return

    pendingAction.current = 'resolve_dispute_split'
    try {
      const idBytes = hexToBytes(agreement.agreement_id)
      const args = [
        nativeToScVal(idBytes, { type: 'bytes' }),
        nativeToScVal(milestone.id, { type: 'u32' }),
        nativeToScVal(BigInt(splitPayer.trim()), { type: 'i128' }),
        nativeToScVal(BigInt(splitPayee.trim()), { type: 'i128' }),
      ]

      await invoke('resolve_dispute_split', args, wallet.publicKey, fee)
      toast.success({ title: 'Dispute resolved with split' })
      setShowSplitForm(false)
      setSplitPayer('')
      setSplitPayee('')
      pendingAction.current = null
      onSuccess?.()
    } catch (err) {
      toast.error({ title: 'Split resolution failed', message: err instanceof Error ? err.message : 'Unknown error' })
    }
  }

  const handleRetry = async () => {
    if (isRetrying) return
    setIsRetrying(true)
    try {
      reset()
      if (pendingAction.current === 'lock_funds') await handleLockFunds()
      else if (pendingAction.current === 'submit_work') await handleSubmitWork()
      else if (pendingAction.current === 'approve_and_release') await handleApproveRelease()
      else if (pendingAction.current === 'raise_dispute') await handleRaiseDispute()
      else if (pendingAction.current === 'release_partial') await handleReleasePartial()
      else if (pendingAction.current === 'cancel_unfunded_milestone') await handleCancelUnfunded()
      else if (pendingAction.current === 'set_milestone_deadline') await handleSetDeadline()
      else if (pendingAction.current === 'expire_milestone') await handleExpire()
      else if (pendingAction.current === 'resolve_dispute_refund') await handleResolveDispute(true)
      else if (pendingAction.current === 'resolve_dispute_pay') await handleResolveDispute(false)
      else if (pendingAction.current === 'resolve_dispute_split') await handleResolveSplit()
    } catch (err) {
      toast.error({ title: 'Retry failed', message: err instanceof Error ? err.message : 'Unknown error' })
    } finally {
      setIsRetrying(false)
    }
  }

  // Determine available actions based on milestone status and user role
  const actions: Array<{ label: string; action: () => void; color: string }> = []

  if (milestone.status === 'Pending' && isUserPayer) {
    actions.push({
      label: 'Lock Funds',
      action: () => setShowConfirm('lock'),
      color: 'bg-cyan-500 hover:bg-cyan-400',
    })
  }

  // Cancelling and setting a deadline are payer-only terms changes, allowed
  // only before the milestone is funded.
  if (milestone.status === 'Pending' && isUserPayer) {
    actions.push(
      {
        label: deadline !== null ? 'Change Deadline' : 'Set Deadline',
        action: () => setShowDeadlineForm(true),
        color: 'bg-indigo-500 hover:bg-indigo-400',
      },
      {
        label: 'Cancel Milestone',
        action: () => setShowConfirm('cancel'),
        color: 'bg-slate-500 hover:bg-slate-400',
      },
    )
  }

  // expire_milestone is permissionless once the deadline has passed, but only
  // closes Pending or Funded milestones.
  if (isExpirable && isPastDeadline && wallet.connected) {
    actions.push({
      label: 'Expire',
      action: () => setShowConfirm('expire'),
      color: 'bg-amber-600 hover:bg-amber-500',
    })
  }

  if (milestone.status === 'Funded' && isUserPayee) {
    actions.push({
      label: 'Submit Work',
      action: () => setShowProofInput(true),
      color: 'bg-emerald-500 hover:bg-emerald-400',
    })
  }

  if (milestone.status === 'WorkSubmitted' && isUserPayer) {
    actions.push({
      label: 'Approve & Release',
      action: () => setShowConfirm('approve'),
      color: 'bg-green-500 hover:bg-green-400',
    })
  }

  const isDisputable = milestone.status === 'Funded' || milestone.status === 'WorkSubmitted'

  // release_partial is payer-only and needs funds still in escrow.
  if (isDisputable && isUserPayer && remainingEscrow > 0n) {
    actions.push({
      label: 'Release Part',
      action: () => setShowReleaseForm(true),
      color: 'bg-teal-500 hover:bg-teal-400',
    })
  }
  // raise_dispute rejects any caller other than the payer or payee.
  const isUserParty = isUserPayer || isUserPayee

  if (isDisputable && wallet.connected && isUserParty) {
    actions.push({
      label: 'Dispute',
      action: () => setShowConfirm('dispute'),
      color: 'bg-red-500 hover:bg-red-400',
    })
  }

  // Only the agreement's dispute resolver can settle a Disputed milestone.
  if (milestone.status === 'Disputed' && wallet.connected && isUserResolver) {
    actions.push(
      {
        label: 'Refund Payer',
        action: () => setShowConfirm('resolve_refund'),
        color: 'bg-orange-500 hover:bg-orange-400',
      },
      {
        label: 'Pay Payee',
        action: () => setShowConfirm('resolve_pay'),
        color: 'bg-green-500 hover:bg-green-400',
      },
      {
        label: 'Split',
        action: () => setShowSplitForm(true),
        color: 'bg-purple-500 hover:bg-purple-400',
      },
    )
  }

  const deadlineBadge =
    isExpirable && deadline !== null ? (
      <p
        className={`text-xs ${isPastDeadline ? 'text-amber-400' : 'text-gray-400 dark:text-gray-400 light:text-gray-600'}`}
        data-testid="milestone-deadline"
        title={new Date(deadline * 1000).toLocaleString()}
      >
        {isPastDeadline ? 'Deadline passed' : `Deadline in ${formatCountdown(deadline - now)}`}
      </p>
    ) : null

  if (actions.length === 0) {
    if (deadlineBadge) return deadlineBadge
    if (isDisputable && wallet.connected && !isUserParty) {
      return (
        <span className="text-gray-500 dark:text-gray-500 light:text-gray-600 text-xs">
          Only the payer or payee can act on this milestone.
        </span>
      )
    }
    return <span className="text-gray-500 dark:text-gray-500 light:text-gray-600 text-sm">—</span>
  }

  return (
    <div className="space-y-2">
      {deadlineBadge}
      <div className="flex gap-2 flex-wrap">
        {actions.map((action) => (
          <button
            key={action.label}
            onClick={action.action}
            disabled={isLoading || !wallet.connected}
            className={`px-3 py-1 ${action.color} text-white dark:text-white light:text-gray-900 font-semibold text-xs rounded transition-colors disabled:opacity-50 disabled:cursor-not-allowed`}
          >
            {action.label}
          </button>
        ))}
      </div>

      <TransactionStatus
        status={status}
        txHash={txHash}
        error={error}
        onRetry={handleRetry}
        method={pendingAction.current || ''}
        retrying={isRetrying}
      />

      {/* Proof URI Input */}
      {showProofInput && (
        <div className="space-y-2 p-3 bg-navy-700 dark:bg-navy-700 light:bg-gray-100 rounded">
          {/* maxLength mirrors the contract's MAX_PROOF_URI_LEN (512 bytes) —
              longer proofs are rejected on-chain with ProofUriTooLong. */}
          <input
            type="text"
            placeholder="Proof URI (optional, e.g., GitHub PR, IPFS link)"
            maxLength={512}
            value={proofUri}
            onChange={(e) => setProofUri(e.target.value)}
            className="w-full px-3 py-2 bg-navy-800 dark:bg-navy-800 light:bg-white border border-navy-600 dark:border-navy-600 light:border-gray-300 text-white dark:text-white light:text-gray-900 text-sm rounded focus:outline-none focus:border-cyan-400"
          />
          <div className="flex gap-2">
            <button
              onClick={handleSubmitWork}
              disabled={isLoading}
              className="px-3 py-1 bg-emerald-500 hover:bg-emerald-400 text-white dark:text-white light:text-gray-900 text-xs rounded disabled:opacity-50"
            >
              {isLoading ? 'Submitting...' : 'Submit'}
            </button>
            <button
              onClick={() => {
                setShowProofInput(false)
                setProofUri('')
              }}
              className="px-3 py-1 bg-navy-600 dark:bg-navy-600 light:bg-gray-200 hover:bg-navy-500 dark:hover:bg-navy-500 light:hover:bg-gray-300 text-white dark:text-white light:text-gray-900 text-xs rounded"
            >
              Cancel
            </button>
          </div>
        </div>
      )}

      {/* Payer deadline picker */}
      {showDeadlineForm && (
        <div className="space-y-2 p-3 bg-navy-700 dark:bg-navy-700 light:bg-gray-100 rounded">
          <label className="flex flex-col text-xs text-gray-400 dark:text-gray-400 light:text-gray-600">
            Deadline (local time)
            <input
              type="datetime-local"
              aria-label="Milestone deadline"
              min={toDateTimeLocal(new Date())}
              value={deadlineInput}
              onChange={(e) => setDeadlineInput(e.target.value)}
              className="mt-1 px-3 py-2 bg-navy-800 dark:bg-navy-800 light:bg-white border border-navy-600 dark:border-navy-600 light:border-gray-300 text-white dark:text-white light:text-gray-900 text-sm rounded focus:outline-none focus:border-cyan-400"
            />
          </label>
          {deadlineInput && deadlineError && (
            <p className="text-red-400 text-xs" role="alert">{deadlineError}</p>
          )}
          <div className="flex gap-2">
            <button
              onClick={() => handleSetDeadline()}
              disabled={isLoading || !!deadlineError}
              className="px-3 py-1 bg-indigo-500 hover:bg-indigo-400 text-white dark:text-white light:text-gray-900 text-xs rounded disabled:opacity-50 disabled:cursor-not-allowed"
            >
              {isLoading ? 'Processing...' : 'Save Deadline'}
            </button>
            <button
              onClick={() => {
                setShowDeadlineForm(false)
                setDeadlineInput('')
              }}
              disabled={isLoading}
              className="px-3 py-1 bg-navy-600 dark:bg-navy-600 light:bg-gray-200 hover:bg-navy-500 dark:hover:bg-navy-500 light:hover:bg-gray-300 text-white dark:text-white light:text-gray-900 text-xs rounded disabled:opacity-50"
            >
              Cancel
            </button>
          </div>
        </div>
      )}

      {/* Payer partial-release form */}
      {showReleaseForm && (
        <div className="space-y-2 p-3 bg-navy-700 dark:bg-navy-700 light:bg-gray-100 rounded">
          <p className="text-xs text-gray-300 dark:text-gray-300 light:text-gray-700">
            Release part of the remaining <span className="font-mono">{remainingEscrow.toString()}</span> to the payee.
            Partial releases are final.
          </p>
          <input
            type="text"
            inputMode="numeric"
            placeholder="Amount to release"
            aria-label="Amount to release"
            value={releaseAmount}
            onChange={(e) => setReleaseAmount(e.target.value)}
            className="w-full px-3 py-2 bg-navy-800 dark:bg-navy-800 light:bg-white border border-navy-600 dark:border-navy-600 light:border-gray-300 text-white dark:text-white light:text-gray-900 text-sm rounded focus:outline-none focus:border-cyan-400"
          />
          {releaseAmount && releaseError && (
            <p className="text-red-400 text-xs" role="alert">{releaseError}</p>
          )}
          <div className="flex gap-2">
            <button
              onClick={() => handleReleasePartial()}
              disabled={isLoading || !!releaseError}
              className="px-3 py-1 bg-teal-500 hover:bg-teal-400 text-white dark:text-white light:text-gray-900 text-xs rounded disabled:opacity-50 disabled:cursor-not-allowed"
            >
              {isLoading ? 'Processing...' : 'Release'}
            </button>
            <button
              onClick={() => {
                setShowReleaseForm(false)
                setReleaseAmount('')
              }}
              disabled={isLoading}
              className="px-3 py-1 bg-navy-600 dark:bg-navy-600 light:bg-gray-200 hover:bg-navy-500 dark:hover:bg-navy-500 light:hover:bg-gray-300 text-white dark:text-white light:text-gray-900 text-xs rounded disabled:opacity-50"
            >
              Cancel
            </button>
          </div>
        </div>
      )}

      {/* Resolver split form */}
      {showSplitForm && (
        <div className="space-y-2 p-3 bg-navy-700 dark:bg-navy-700 light:bg-gray-100 rounded">
          <p className="text-xs text-gray-300 dark:text-gray-300 light:text-gray-700">
            Split the remaining escrow of <span className="font-mono">{remainingEscrow.toString()}</span> between the parties.
          </p>
          <div className="flex gap-2 flex-wrap">
            <label className="flex flex-col text-xs text-gray-400 dark:text-gray-400 light:text-gray-600">
              To payer
              <input
                type="text"
                inputMode="numeric"
                placeholder="0"
                aria-label="Amount to payer"
                value={splitPayer}
                onChange={(e) => setSplitPayer(e.target.value)}
                className="mt-1 w-32 px-3 py-2 bg-navy-800 dark:bg-navy-800 light:bg-white border border-navy-600 dark:border-navy-600 light:border-gray-300 text-white dark:text-white light:text-gray-900 text-sm rounded focus:outline-none focus:border-cyan-400"
              />
            </label>
            <label className="flex flex-col text-xs text-gray-400 dark:text-gray-400 light:text-gray-600">
              To payee
              <input
                type="text"
                inputMode="numeric"
                placeholder="0"
                aria-label="Amount to payee"
                value={splitPayee}
                onChange={(e) => setSplitPayee(e.target.value)}
                className="mt-1 w-32 px-3 py-2 bg-navy-800 dark:bg-navy-800 light:bg-white border border-navy-600 dark:border-navy-600 light:border-gray-300 text-white dark:text-white light:text-gray-900 text-sm rounded focus:outline-none focus:border-cyan-400"
              />
            </label>
          </div>
          {(splitPayer || splitPayee) && splitError && (
            <p className="text-red-400 text-xs" role="alert">{splitError}</p>
          )}
          <div className="flex gap-2">
            <button
              onClick={() => handleResolveSplit()}
              disabled={isLoading || !!splitError}
              className="px-3 py-1 bg-purple-500 hover:bg-purple-400 text-white dark:text-white light:text-gray-900 text-xs rounded disabled:opacity-50 disabled:cursor-not-allowed"
            >
              {isLoading ? 'Processing...' : 'Resolve Split'}
            </button>
            <button
              onClick={() => {
                setShowSplitForm(false)
                setSplitPayer('')
                setSplitPayee('')
              }}
              disabled={isLoading}
              className="px-3 py-1 bg-navy-600 dark:bg-navy-600 light:bg-gray-200 hover:bg-navy-500 dark:hover:bg-navy-500 light:hover:bg-gray-300 text-white dark:text-white light:text-gray-900 text-xs rounded disabled:opacity-50"
            >
              Cancel
            </button>
          </div>
        </div>
      )}

      {/* Confirmation Modal */}
      {showConfirm && (
        <div className="p-3 bg-navy-700 dark:bg-navy-700 light:bg-gray-100 rounded border border-yellow-500/50">
          <p className="text-sm text-gray-300 dark:text-gray-300 light:text-gray-700 mb-2">
            {showConfirm === 'lock' && `Lock ${milestone.amount} tokens for milestone ${milestone.id}?`}
            {showConfirm === 'approve' && `Approve and release ${remainingEscrow.toString()} tokens to payee?`}
            {showConfirm === 'dispute' && 'Raise a dispute for this milestone?'}
            {showConfirm === 'cancel' && `Cancel unfunded milestone ${milestone.id}? This cannot be undone.`}
            {showConfirm === 'expire' &&
              (milestone.status === 'Funded'
                ? `Expire milestone ${milestone.id} and refund the locked funds to the payer?`
                : `Expire unfunded milestone ${milestone.id}?`)}
            {showConfirm === 'resolve_refund' && `Refund the remaining ${remainingEscrow.toString()} tokens to the payer?`}
            {showConfirm === 'resolve_pay' && `Release the remaining ${remainingEscrow.toString()} tokens to the payee?`}
          </p>
          <div className="flex gap-2">
            <button
              onClick={() => {
                if (showConfirm === 'lock') handleLockFunds()
                else if (showConfirm === 'approve') handleApproveRelease()
                else if (showConfirm === 'dispute') handleRaiseDispute()
                else if (showConfirm === 'cancel') handleCancelUnfunded()
                else if (showConfirm === 'expire') handleExpire()
                else if (showConfirm === 'resolve_refund') handleResolveDispute(true)
                else if (showConfirm === 'resolve_pay') handleResolveDispute(false)
              }}
              disabled={isLoading}
              className="px-3 py-1 bg-cyan-500 hover:bg-cyan-400 text-white dark:text-white light:text-gray-900 text-xs rounded disabled:opacity-50"
            >
              {isLoading ? 'Processing...' : 'Confirm'}
            </button>
            <button
              onClick={() => setShowConfirm(null)}
              disabled={isLoading}
              className="px-3 py-1 bg-navy-600 dark:bg-navy-600 light:bg-gray-200 hover:bg-navy-500 dark:hover:bg-navy-500 light:hover:bg-gray-300 text-white dark:text-white light:text-gray-900 text-xs rounded disabled:opacity-50"
            >
              Cancel
            </button>
          </div>
        </div>
      )}
    </div>
  )
}
