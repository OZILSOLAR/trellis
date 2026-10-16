import { remainingAmount, toBaseUnits, type Milestone } from '../lib/soroban'

interface MilestoneAmountProps {
  milestone: Milestone
  className?: string
}

/**
 * A milestone's amount, plus how much has been paid out via `release_partial`
 * and how much is still escrowed once any partial release has happened — so
 * the table never overstates what is still locked.
 */
export default function MilestoneAmount({ milestone, className = '' }: MilestoneAmountProps) {
  const released = toBaseUnits(milestone.released)

  return (
    <div className={className}>
      <span>{milestone.amount}</span>
      {released > 0n && (
        <p className="text-xs text-gray-400 dark:text-gray-400 light:text-gray-600" data-testid="milestone-released">
          Released <span className="font-mono">{released.toString()}</span> · Remaining{' '}
          <span className="font-mono">{remainingAmount(milestone).toString()}</span>
        </p>
      )}
    </div>
  )
}
