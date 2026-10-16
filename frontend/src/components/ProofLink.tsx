import { safeProofHref } from '../lib/proofUri'

interface ProofLinkProps {
  /** Payee-supplied, untrusted `proof_uri`. */
  uri: string
  /** Link text shown when the URI is safe to link. */
  label: string
  /** Classes for the rendered `<a>`. */
  className?: string
}

/**
 * Renders a milestone's `proof_uri` as a link only when it passes
 * `safeProofHref`; anything else is shown as inert, clearly-flagged text so
 * the payer can still read (and manually verify) what the payee submitted.
 */
export default function ProofLink({ uri, label, className }: ProofLinkProps) {
  const href = safeProofHref(uri)

  if (href) {
    return (
      <a href={href} target="_blank" rel="noopener noreferrer" className={className}>
        {label}
      </a>
    )
  }

  return (
    <span
      className="text-amber-400 text-sm break-all"
      title="This proof link uses an unsupported or unsafe scheme and was not made clickable."
      data-testid="untrusted-proof"
    >
      <span className="font-semibold">Untrusted link: </span>
      <code>{uri}</code>
    </span>
  )
}
