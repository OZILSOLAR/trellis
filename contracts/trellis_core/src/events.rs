use soroban_sdk::{symbol_short, Address, BytesN, Env, String};

// ---------------------------------------------------------------------------
// Event emitters — the only place that calls env.events().publish().
//
// Convention:
//   • topics: a tuple of (Symbol, agreement_id) — enables indexed filtering
//     by event name and/or agreement ID from off-chain indexers.
//   • data:   a tuple of typed Soroban primitives (Symbol, i128, Address, u32,
//     bool, Option<String>) so off-chain consumers can decode payloads without
//     importing the contract WASM types.
//
// Symbol length limits:
//   • `symbol_short!` accepts ≤9 characters and stores the symbol as an
//     inline 64-bit integer — no heap allocation on the guest WASM side.
//   • `symbol_short!` is used where the prefixed name is exactly 9 chars or
//     fewer.  Any name that exceeds 9 chars uses `Symbol::new(env, "…")`,
//     which stores the string in the host's symbol table.
//
// Prefixed topic names and their lengths:
//   1. "trls_crte"  ( 9 chars) → symbol_short! — agreement_created  (abbreviated to stay ≤9)
//   2. "trls_lckd"  ( 9 chars) → symbol_short! — funds_locked       (abbreviated)
//   3. "trls_sbmt"  ( 9 chars) → symbol_short! — work_submitted     (abbreviated)
//   4. "trls_rlsd"  ( 9 chars) → symbol_short! — funds_released     (abbreviated)
//   5. "trls_dspt"  ( 9 chars) → symbol_short! — dispute_raised     (abbreviated)
//   6. "trls_rslv"  ( 9 chars) → symbol_short! — milestone_resolved (abbreviated)
//   7. "trls_cncl"  ( 9 chars) → symbol_short! — milestone_cancelled(abbreviated)
//   8. "trls_ttle"  ( 9 chars) → symbol_short! — ttl_extended       (abbreviated)
//   9. "trls_prtl"  ( 9 chars) → symbol_short! — funds_partially_released (abbreviated)
//  10. "trls_cmpl"  ( 9 chars) → symbol_short! — agreement_completed (abbreviated)
//  11. "trls_ddln"  ( 9 chars) → symbol_short! — deadline_set       (abbreviated)
//  12. "trls_expd"  ( 9 chars) → symbol_short! — milestone_expired  (abbreviated)
//  13. "trls_rrot"  ( 9 chars) → symbol_short! — resolver_rotated   (abbreviated)
//
// All abbreviations follow the pattern: drop the vowels from the root word
// while keeping enough consonants to be unambiguous.  They are intentional
// and documented here so indexers can rely on them as stable identifiers.
//
// Event schema (topics → data):
//   1. "created"   → (payer: Address, payee: Address)
//   2. "locked"    → (milestone_id: u32, amount: i128)
//   3. "submitted" → (milestone_id: u32, proof_uri: Option<String>)
//   4. "released"  → (milestone_id: u32, amount: i128)
//   5. "disputed"  → (milestone_id: u32, caller: Address)
//   6. "resolved"  → (milestone_id: u32, payer_amount: i128, payee_amount: i128)
//   7. "cancelled" → (milestone_id: u32, payer: Address, cancelled_by: Address)
//   8. "ttl_extended" → (caller: Address)
//   9. "partially_released" → (milestone_id: u32, amount: i128, remaining: i128)
//  10. "agreement_completed" → (completed_count: u32, refunded_count: u32)
//  11. "deadline_set" → (milestone_id: u32, deadline: u64)
//  12. "expired"   → (milestone_id: u32, refunded_amount: i128, caller: Address)
//  13. "resolver_rotated" → (old_resolver: Address, new_resolver: Address)
//
// Schema changes: the `amount` field on "disputed", "resolved" and
// "cancelled" was appended as the last tuple element, so an indexer that
// decodes those payloads positionally keeps reading the earlier fields at
// the same indices. Every fund-movement event now carries an amount.
//
// `amount` semantics: on "released", "disputed" and "resolved" it is the
// amount still held in escrow for the milestone at the time of the call —
// the milestone's configured `amount` minus anything already paid out via
// `release_partial`. With no partial releases this equals the milestone's
// stored amount. On "cancelled" it is the proposed (never funded) amount.
//
// "trls_rslv" and "trls_cncl" are deliberately distinct: an indexer must be
// able to tell an arbitrated dispute outcome apart from a payer walking back a
// milestone that was never funded, since only the former involves token
// movement and resolver involvement.
//
// "trls_rrot" is the only event that carries two `Address` values describing
// the same role before and after a state change. An indexer must treat
// `old_resolver` as the address that could sign `resolve_dispute` *before* this
// event and `new_resolver` as the one that can sign it *after*, so the pair is
// the authoritative resolver-history record for an agreement — `get_agreement`
// only ever exposes the current holder.
// ---------------------------------------------------------------------------

/// Emitted when a new escrow agreement is created.
///
/// Topics: `("trls_crte", agreement_id)`
/// Data:   `(payer, payee)`
pub fn agreement_created(env: &Env, agreement_id: BytesN<32>, payer: Address, payee: Address) {
    env.events().publish(
        (symbol_short!("trls_crte"), agreement_id.clone()),
        (payer, payee),
    );
}

/// Emitted when funds for a specific milestone are locked into the escrow.
///
/// Topics: `("trls_lckd", agreement_id)`
/// Data:   `(milestone_id, amount)`
pub fn funds_locked(env: &Env, agreement_id: BytesN<32>, milestone_id: u32, amount: i128) {
    env.events().publish(
        (symbol_short!("trls_lckd"), agreement_id.clone()),
        (milestone_id, amount),
    );
}

/// Emitted when a payee submits proof of work for a milestone.
///
/// Topics: `("trls_sbmt", agreement_id)`
/// Data:   `(milestone_id, proof_uri)`
///
/// `proof_uri` is `None` when the milestone was submitted without a proof
/// link; it is never an empty string.
pub fn work_submitted(
    env: &Env,
    agreement_id: BytesN<32>,
    milestone_id: u32,
    proof_uri: Option<String>,
) {
    env.events().publish(
        (symbol_short!("trls_sbmt"), agreement_id.clone()),
        (milestone_id, proof_uri),
    );
}

/// Emitted when a payer approves a milestone and funds are released to the payee.
///
/// Topics: `("trls_rlsd", agreement_id)`
/// Data:   `(milestone_id, amount)`
///
/// `amount` is what this call actually transferred: the milestone's amount
/// minus anything already paid out through [`funds_partially_released`].
pub fn funds_released(env: &Env, agreement_id: BytesN<32>, milestone_id: u32, amount: i128) {
    env.events().publish(
        (symbol_short!("trls_rlsd"), agreement_id.clone()),
        (milestone_id, amount),
    );
}

/// Emitted when either party raises a dispute on a funded or work-submitted milestone.
///
/// Topics: `("trls_dspt", agreement_id)`
/// Data:   `(milestone_id, caller, amount, reason_uri)`
///
/// `reason_uri` is the disputing party's optional pointer at their reason or
/// evidence (`None` when omitted or empty). It is carried in the event only —
/// it is never stored on the milestone.
///
/// `caller` is the party (payer or payee) that triggered the dispute, provided
/// as a typed `Address` so indexers can identify the initiating party without
/// importing contract WASM types.
///
/// `amount` is the escrowed amount at stake in the dispute — the milestone's
/// amount minus anything already partially released — so a dispute dashboard
/// does not need to cross-reference the earlier `funds_locked` event.
pub fn dispute_raised(
    env: &Env,
    agreement_id: BytesN<32>,
    milestone_id: u32,
    caller: Address,
    amount: i128,
    reason_uri: Option<String>,
) {
    env.events().publish(
        (symbol_short!("trls_dspt"), agreement_id.clone()),
        (milestone_id, caller, amount, reason_uri),
    );
}

/// Emitted when the dispute resolver settles a disputed milestone.
///
/// This event means an arbitration ruling was made and tokens moved. It is
/// **not** emitted for cancellations — see [`milestone_cancelled`].
///
/// Topics: `("trls_rslv", agreement_id)`
/// Data:   `(milestone_id, payer_amount, payee_amount)`
///
/// `payer_amount` is the portion of the locked milestone amount returned to
/// the payer; `payee_amount` is the portion awarded to the payee. The two
/// amounts always sum to the milestone's locked total, so indexers can
/// reconstruct the split without reading contract state. All-or-nothing
/// outcomes are represented as `(payer_amount = total, payee_amount = 0)` or
/// `(payer_amount = 0, payee_amount = total)`.
pub fn milestone_resolved(
    env: &Env,
    agreement_id: BytesN<32>,
    milestone_id: u32,
    payer_amount: i128,
    payee_amount: i128,
) {
    env.events().publish(
        (symbol_short!("trls_rslv"), agreement_id.clone()),
        (milestone_id, payer_amount, payee_amount),
    );
}

/// Emitted when a payer cancels a milestone that was never funded.
///
/// No tokens move: the milestone simply leaves the `Pending` state without
/// ever having been escrowed. Indexers should treat this as a withdrawal of a
/// proposal, not as a dispute outcome.
///
/// Topics: `("trls_cncl", agreement_id)`
/// Data:   `(milestone_id, payer, cancelled_by, amount)`
///
/// `payer` is the agreement's payer; `cancelled_by` is the address that
/// authorised the cancellation. These are the same address today (only the
/// payer may cancel) but are reported separately so the event stays
/// self-describing if delegated cancellation is ever added.
///
/// `amount` is the milestone's proposed amount. It was never escrowed, so no
/// tokens move — it lets an indexer show "a $X milestone was withdrawn"
/// without a separate query. Any future cancellation-adjacent event (e.g. a
/// payee-side withdrawal) should carry the same trailing `amount` field.
pub fn milestone_cancelled(
    env: &Env,
    agreement_id: BytesN<32>,
    milestone_id: u32,
    payer: Address,
    cancelled_by: Address,
    amount: i128,
) {
    env.events().publish(
        (symbol_short!("trls_cncl"), agreement_id.clone()),
        (milestone_id, payer, cancelled_by, amount),
    );
}

/// Emitted when the payer releases part of a milestone's escrowed amount to
/// the payee ahead of (or instead of) full approval.
///
/// Topics: `("trls_prtl", agreement_id)`
/// Data:   `(milestone_id, amount, remaining)`
///
/// `amount` is what this call transferred; `remaining` is what is still held
/// in escrow for the milestone afterwards. `remaining == 0` means the
/// milestone moved to `Completed` in the same call.
pub fn funds_partially_released(
    env: &Env,
    agreement_id: BytesN<32>,
    milestone_id: u32,
    amount: i128,
    remaining: i128,
) {
    env.events().publish(
        (symbol_short!("trls_prtl"), agreement_id.clone()),
        (milestone_id, amount, remaining),
    );
}

/// Emitted once per agreement, when its last non-terminal milestone reaches a
/// terminal state.
///
/// "Completed" here means *settled*: every milestone is either `Completed`
/// (paid to the payee, by approval, full partial release, or a ruling for
/// the payee) or `Refunded` (returned to the payer by a ruling, or cancelled
/// before funding). No milestone can transition again, and the agreement
/// holds no escrowed funds. It does **not** mean every milestone was paid
/// out — use the counts to tell a fully delivered agreement from one that
/// was partly or wholly refunded.
///
/// Topics: `("trls_cmpl", agreement_id)`
/// Data:   `(completed_count, refunded_count)`
///
/// It is emitted after the milestone-level event of the transition that
/// triggered it, in the same invocation.
pub fn agreement_completed(
    env: &Env,
    agreement_id: BytesN<32>,
    completed_count: u32,
    refunded_count: u32,
) {
    env.events().publish(
        (symbol_short!("trls_cmpl"), agreement_id.clone()),
        (completed_count, refunded_count),
    );
}

/// Emitted when a keeper extends the TTL of an agreement to prevent expiry.
///
/// No state is modified beyond the ledger TTL. This event creates an audit
/// trail for off-chain monitoring of keeper activity. `caller` has authorised
/// the invocation (`extend_agreement_ttl` calls `caller.require_auth()`), so
/// it is always the real signer and cannot be forged.
///
/// Topics: `("trls_ttle", agreement_id)`
/// Data:   `(caller)`
pub fn ttl_extended(env: &Env, agreement_id: BytesN<32>, caller: Address) {
    env.events().publish(
        (symbol_short!("trls_ttle"), agreement_id.clone()),
        (caller,),
    );
}

/// Emitted when the payer sets (or moves) a milestone's deadline.
///
/// Topics: `("trls_ddln", agreement_id)`
/// Data:   `(milestone_id, deadline)`
///
/// `deadline` is a ledger timestamp (Unix seconds).
pub fn deadline_set(env: &Env, agreement_id: BytesN<32>, milestone_id: u32, deadline: u64) {
    env.events().publish(
        (symbol_short!("trls_ddln"), agreement_id.clone()),
        (milestone_id, deadline),
    );
}

/// Emitted when a milestone is closed by the deadline fallback.
///
/// Topics: `("trls_expd", agreement_id)`
/// Data:   `(milestone_id, refunded_amount, caller)`
///
/// `refunded_amount` is `0` when the milestone expired while still `Pending`
/// (nothing was ever escrowed; it is now `Cancelled`), and the milestone
/// amount when it expired while `Funded` (the locked funds went back to the
/// payer; it is now `Refunded`). `caller` is
/// whoever triggered the expiry — the entrypoint is permissionless, so this
/// may be a keeper rather than either party.
pub fn milestone_expired(
    env: &Env,
    agreement_id: BytesN<32>,
    milestone_id: u32,
    refunded_amount: i128,
    caller: Address,
) {
    env.events().publish(
        (symbol_short!("trls_expd"), agreement_id.clone()),
        (milestone_id, refunded_amount, caller),
    );
}

/// Emitted when an agreement's dispute resolver is replaced.
///
/// Topics: `("trls_rrot", agreement_id)`
/// Data:   `(old_resolver, new_resolver)`
///
/// Both addresses are recorded, not just the new one: `get_agreement` only ever
/// exposes the current resolver, so without this event an indexer could not
/// reconstruct who was able to rule on a dispute that was raised before the
/// rotation. No tokens move and no milestone changes status — the only effect
/// is that `resolve_dispute` / `resolve_dispute_split` now gate on
/// `new_resolver` instead of `old_resolver`, including for milestones that are
/// already `Disputed`.
pub fn resolver_rotated(
    env: &Env,
    agreement_id: BytesN<32>,
    old_resolver: Address,
    new_resolver: Address,
) {
    env.events().publish(
        (symbol_short!("trls_rrot"), agreement_id.clone()),
        (old_resolver, new_resolver),
    );
}
