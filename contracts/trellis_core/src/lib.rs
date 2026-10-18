#![no_std]

mod errors;
mod events;
mod storage;
pub mod types;

#[cfg(test)]
mod test;

#[cfg(test)]
mod test_utils;

#[cfg(test)]
mod test_properties;

#[cfg(test)]
mod test_panic_boundaries;

#[cfg(test)]
mod test_storage;

#[cfg(test)]
mod test_settlement;

use soroban_sdk::{contract, contractimpl, token, Address, BytesN, Env, Map, String, Vec};

use errors::TrellisError;
use types::{Agreement, AgreementHeader, EscrowStatus, Milestone};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const MAX_MILESTONES: u32 = 50;

/// Maximum length, in bytes, of the `proof_uri` accepted by `submit_work`.
///
/// A milestone lives in persistent storage for the lifetime of its agreement,
/// so an unbounded proof URI would let a payee permanently inflate the
/// agreement's storage footprint (and rent) with no real cost beyond the
/// transaction fee. 512 bytes comfortably fits an `ipfs://` CID or a long
/// HTTPS URL, and the CLI enforces the same cap client-side.
pub const MAX_PROOF_URI_LEN: u32 = 512;

// ---------------------------------------------------------------------------
// Contract struct
// ---------------------------------------------------------------------------

#[contract]
pub struct TrellisContract;

// ---------------------------------------------------------------------------
// Contract entrypoints
// ---------------------------------------------------------------------------
//
// Panic safety (#154): a panic in a Soroban contract traps the host — the
// transaction reverts but the caller is still charged the fee, and an
// unexpected trap can leave callers reasoning about inconsistent state. Every
// entrypoint below is therefore panic-free by construction:
//
//   * milestone lookups go through `storage::read_milestone`, which maps a
//     missing/out-of-range index to `TrellisError::InvalidMilestone` via
//     `Option::ok_or` — never `Vec::get(id).unwrap()` or index syntax;
//   * agreement reads go through `storage::read_header` / `read_agreement`,
//     which map a missing entry to `TrellisError::AgreementNotFound` via
//     `Option::ok_or`;
//   * every fallible entrypoint returns `Result<_, TrellisError>` so failures
//     propagate as typed on-chain errors, not panics.
//
// There are no `unwrap()` / `expect()` calls anywhere in the contract crate's
// non-test sources. The `token::Client` transfer calls can still trap inside
// the SDK (e.g. the payer does not hold enough balance) — that is the token
// contract's own boundary and is intentionally left to it. A custom
// `#[panic_handler]` is not added: `soroban-sdk` already provides one for the
// wasm build and a second definition is a duplicate-lang-item error.
// `test_panic_boundaries.rs` fuzzes these paths to keep the property enforced.

#[contractimpl]
impl TrellisContract {
    /// Create a new escrow agreement.
    ///
    /// The payer authorises this call.  Every milestone in `milestones` must
    /// arrive with `status = EscrowStatus::Pending` — the contract rejects any
    /// other initial status with
    /// [`TrellisError::InvalidInitialMilestoneStatus`] rather than silently
    /// accepting it.
    ///
    /// `Pending` is the only valid starting state because it is the sole
    /// entry point of the state machine: `Funded` is reached by
    /// `lock_funds`, and every status after that is reached by transitioning
    /// a funded milestone. All agreements using a given token share one
    /// pooled contract balance, so a milestone created in a pre-advanced
    /// state would be a claim on tokens that were never escrowed for it —
    /// `WorkSubmitted` could go straight to `approve_and_release` and
    /// `Disputed` straight to `resolve_dispute`, either draining the pool
    /// without anything having been locked.
    ///
    /// `milestones` must be non-empty and `dispute_resolver` must be distinct
    /// from both `payer` and `payee` — see the `Errors` below.
    ///
    /// # Errors
    /// - [`TrellisError::AlreadyInitialized`] if an agreement with this ID
    ///   already exists in storage.
    /// - [`TrellisError::EmptyMilestoneSet`] if `milestones` is empty — such
    ///   an agreement could never transition through any state.
    /// - [`TrellisError::InvalidInitialMilestoneStatus`] if any milestone's
    ///   `status` is not `Pending`.
    /// - [`TrellisError::ResolverCannotBeParty`] if `dispute_resolver` equals
    ///   `payer` or `payee` — the resolver must be a neutral third party.
    /// - [`TrellisError::PayerEqualsPayee`] if `payer == payee`.
    /// - [`TrellisError::MilestoneCountExceeded`] if more than
    ///   `MAX_MILESTONES` milestones are supplied.
    /// - [`TrellisError::InvalidToken`] if `token` is not a live token contract.
    /// - [`TrellisError::InvalidMilestone`] if any milestone amount is zero
    ///   or negative.
    /// - [`TrellisError::TotalAmountOverflow`] if the milestone amounts sum to
    ///   more than `i128::MAX`.
    pub fn init(
        env: Env,
        agreement_id: BytesN<32>,
        payer: Address,
        payee: Address,
        token: Address,
        milestones: Vec<Milestone>,
        dispute_resolver: Address,
    ) -> Result<(), TrellisError> {
        payer.require_auth();

        if storage::has_agreement(&env, &agreement_id) {
            return Err(TrellisError::AlreadyInitialized);
        }

        if milestones.is_empty() {
            return Err(TrellisError::EmptyMilestoneSet);
        }

        if milestones.len() > MAX_MILESTONES {
            return Err(TrellisError::MilestoneCountExceeded);
        }

        if payer == payee {
            return Err(TrellisError::PayerEqualsPayee);
        }

        if dispute_resolver == payer || dispute_resolver == payee {
            return Err(TrellisError::ResolverCannotBeParty);
        }

        // Liveness probe: the token address must be a live token contract, so
        // `symbol()` has to succeed. Both failure modes — a host trap from a
        // non-contract address and a decode failure from a contract that does
        // not return a symbol — map to `InvalidToken` rather than propagating.
        token::Client::new(&env, &token)
            .try_symbol()
            .map_err(|_| TrellisError::InvalidToken)?
            .map_err(|_| TrellisError::InvalidToken)?;

        let total_amount = validate_milestones(&milestones)?;

        let agreement = Agreement {
            agreement_id: agreement_id.clone(),
            payer: payer.clone(),
            payee: payee.clone(),
            token,
            milestones,
            dispute_resolver,
            total_amount,
            released_amounts: Map::new(&env),
        };

        storage::write_agreement(&env, &agreement_id, &agreement)?;
        events::agreement_created(&env, agreement_id, payer, payee);

        Ok(())
    }

    /// Lock funds for a single milestone into the contract.
    ///
    /// The payer authorises this call with `require_auth()`. The contract then
    /// pulls the tokens itself, via a single
    /// `token::Client::transfer(payer → this contract)` call — the payer's
    /// authorization *is* the authorization for that transfer, because the
    /// token contract sees the transfer as invoked by this contract on the
    /// payer's behalf and re-checks the payer's signature.
    ///
    /// No prior approval or allowance step is involved. There is no
    /// `approve` / `set_allowance` call anywhere in this crate, so callers
    /// must **not** pre-approve the escrow contract on the token contract
    /// before calling this — doing so is unnecessary, and for SAC-style
    /// tokens there is no allowance to set in the first place. The only
    /// precondition is that the payer holds at least `milestone.amount` of
    /// `agreement.token`.
    ///
    /// The milestone must be `Pending`; any other status returns
    /// [`TrellisError::InvalidStateTransition`]. If a deadline was set via
    /// [`Self::set_milestone_deadline`] it must not have passed yet: funding a
    /// milestone that is already expirable would let anyone call
    /// [`Self::expire_milestone`] and pull the escrow straight back.
    ///
    /// # Errors
    /// - [`TrellisError::AgreementNotFound`] – unknown agreement ID.
    /// - [`TrellisError::InvalidMilestone`] – `milestone_id` out of range.
    /// - [`TrellisError::InvalidStateTransition`] – milestone not `Pending`.
    /// - [`TrellisError::DeadlinePassed`] – the milestone's deadline is
    ///   strictly before the current ledger timestamp.
    pub fn lock_funds(
        env: Env,
        agreement_id: BytesN<32>,
        milestone_id: u32,
    ) -> Result<(), TrellisError> {
        // #401: the header and the one milestone being transitioned are read
        // separately, so this call is O(1) in ledger reads and writes no matter
        // how many milestones the agreement has.
        let header = storage::read_header(&env, &agreement_id)?;
        header.payer.require_auth();

        // Read the milestone value, mutate it, and write it back to the same
        // slot without cloning the original entry.
        let mut milestone = storage::read_milestone(&env, &agreement_id, milestone_id)?;

        if milestone.status != EscrowStatus::Pending {
            return Err(TrellisError::InvalidStateTransition);
        }
        ensure_deadline_not_passed(&env, &agreement_id, milestone_id)?;

        // Mutate the milestone value and persist it back to storage before any
        // external calls (checks-effects-interactions pattern).
        let amount = milestone.amount;
        milestone.status = EscrowStatus::Funded;
        storage::write_milestone(&env, &agreement_id, milestone_id, &milestone);

        // Transfer tokens from payer → this contract.
        pull_funds(&env, &header.token, &header.payer, amount, amount)?;

        events::funds_locked(&env, agreement_id, milestone_id, amount);

        Ok(())
    }

    /// Submit proof of work for a funded milestone.
    ///
    /// The payee authorises this call.
    ///
    /// Pass `Some(uri)` to attach delivery proof, or `None` to advance the
    /// milestone to `WorkSubmitted` without one. Empty-string proofs are
    /// normalized to `None` to ensure semantic consistency — indexers pattern
    /// match on `Some(uri)` and must never see an empty string.
    ///
    /// `proof_uri` is stored verbatim in the agreement's persistent entry, so
    /// it is capped at [`MAX_PROOF_URI_LEN`] (512 bytes) to bound the storage
    /// and rent a single submission can lock in for the agreement's lifetime.
    ///
    /// # Errors
    /// - [`TrellisError::AgreementNotFound`] – unknown agreement ID.
    /// - [`TrellisError::InvalidMilestone`] – `milestone_id` out of range.
    /// - [`TrellisError::InvalidStateTransition`] – milestone not `Funded`.
    /// - [`TrellisError::ProofUriTooLong`] – `proof_uri` exceeds
    ///   [`MAX_PROOF_URI_LEN`] bytes.
    pub fn submit_work(
        env: Env,
        agreement_id: BytesN<32>,
        milestone_id: u32,
        proof_uri: Option<String>,
    ) -> Result<(), TrellisError> {
        // #401: header + single milestone only — O(1) reads and one write.
        let header = storage::read_header(&env, &agreement_id)?;
        header.payee.require_auth();

        let mut milestone = storage::read_milestone(&env, &agreement_id, milestone_id)?;

        if milestone.status != EscrowStatus::Funded {
            return Err(TrellisError::InvalidStateTransition);
        }

        let proof_uri = proof_uri.filter(|s| !s.is_empty());

        // Reject oversized proofs before touching storage: the URI is written
        // verbatim and kept for the agreement's lifetime, so an unbounded
        // length would be a permanent storage/rent cost imposed by the payee.
        if let Some(uri) = &proof_uri {
            if uri.len() > MAX_PROOF_URI_LEN {
                return Err(TrellisError::ProofUriTooLong);
            }
        }

        milestone.status = EscrowStatus::WorkSubmitted;
        milestone.proof_uri = proof_uri.clone();
        storage::write_milestone(&env, &agreement_id, milestone_id, &milestone);

        events::work_submitted(&env, agreement_id, milestone_id, proof_uri);

        Ok(())
    }

    /// Approve submitted work and release funds to the payee.
    ///
    /// The payer authorises this call.
    ///
    /// Transfers whatever is still escrowed for the milestone — its full
    /// `amount` minus anything already paid out via [`Self::release_partial`]
    /// — and moves it to `Completed`. If that was the agreement's last
    /// non-terminal milestone, `agreement_completed` is emitted as well.
    ///
    /// # Errors
    /// - [`TrellisError::AgreementNotFound`] – unknown agreement ID.
    /// - [`TrellisError::InvalidMilestone`] – `milestone_id` out of range.
    /// - [`TrellisError::InvalidStateTransition`] – milestone not `WorkSubmitted`.
    pub fn approve_and_release(
        env: Env,
        agreement_id: BytesN<32>,
        milestone_id: u32,
    ) -> Result<(), TrellisError> {
        // #401: header + single milestone only — O(1) reads and one write.
        let mut header = storage::read_header(&env, &agreement_id)?;
        header.payer.require_auth();

        let mut milestone = storage::read_milestone(&env, &agreement_id, milestone_id)?;

        if milestone.status != EscrowStatus::WorkSubmitted {
            return Err(TrellisError::InvalidStateTransition);
        }

        let amount = remaining_amount(&header, milestone_id, &milestone);
        milestone.status = EscrowStatus::Completed;
        storage::write_milestone(&env, &agreement_id, milestone_id, &milestone);
        close_milestone(&mut header, false);
        storage::write_header(&env, &agreement_id, &header);

        // Transfer tokens from this contract → payee after state change.
        token::Client::new(&env, &header.token).transfer(
            &env.current_contract_address(),
            &header.payee,
            &amount,
        );

        events::funds_released(&env, agreement_id.clone(), milestone_id, amount);
        emit_if_agreement_completed(&env, &agreement_id, &header);

        Ok(())
    }

    /// Release part of a milestone's escrowed amount to the payee as a
    /// progress (or retainer) payment, without approving the milestone.
    ///
    /// The payer authorises this call. The milestone must be `Funded` or
    /// `WorkSubmitted` and stays in that status while any amount remains in
    /// escrow; the release that brings the remainder to zero moves it to
    /// `Completed` (emitting `agreement_completed` if it was the last
    /// non-terminal milestone).
    ///
    /// Partially-released funds are final. A later [`Self::raise_dispute`] /
    /// [`Self::resolve_dispute`] only covers what is still escrowed, and
    /// [`Self::approve_and_release`] pays out only the remainder.
    ///
    /// Returns the amount still held in escrow for the milestone afterwards.
    ///
    /// # Errors
    /// - [`TrellisError::AgreementNotFound`] – unknown agreement ID.
    /// - [`TrellisError::InvalidMilestone`] – `milestone_id` out of range.
    /// - [`TrellisError::InvalidStateTransition`] – milestone is not `Funded`
    ///   or `WorkSubmitted`.
    /// - [`TrellisError::InvalidReleaseAmount`] – `amount` is zero, negative,
    ///   or exceeds what is still escrowed for the milestone.
    pub fn release_partial(
        env: Env,
        agreement_id: BytesN<32>,
        milestone_id: u32,
        amount: i128,
    ) -> Result<i128, TrellisError> {
        // #401: reads the header plus the single milestone it is about to pay out,
        // never the whole milestone vector. The cumulative release counter is
        // agreement-level data that lives on the header, so this is the one
        // transition that writes two entries — and only ever two, regardless of
        // how many milestones the agreement has.
        let mut header = storage::read_header(&env, &agreement_id)?;
        header.payer.require_auth();

        let mut milestone = storage::read_milestone(&env, &agreement_id, milestone_id)?;

        if milestone.status != EscrowStatus::Funded
            && milestone.status != EscrowStatus::WorkSubmitted
        {
            return Err(TrellisError::InvalidStateTransition);
        }

        let remaining_before = remaining_amount(&header, milestone_id, &milestone);
        if amount <= 0 || amount > remaining_before {
            return Err(TrellisError::InvalidReleaseAmount);
        }

        // `amount <= remaining_before` bounds the new total by
        // `milestone.amount`, so neither operation can overflow.
        let released = milestone.amount - remaining_before + amount;
        let remaining = remaining_before - amount;

        header.released_amounts.set(milestone_id, released);
        let completed = remaining == 0;
        if completed {
            milestone.status = EscrowStatus::Completed;
            close_milestone(&mut header, false);
        }

        // Record the release before moving tokens (checks-effects-interactions).
        storage::write_header(&env, &agreement_id, &header);
        if completed {
            storage::write_milestone(&env, &agreement_id, milestone_id, &milestone);
        }

        // Transfer tokens from this contract → payee after state change.
        token::Client::new(&env, &header.token).transfer(
            &env.current_contract_address(),
            &header.payee,
            &amount,
        );

        events::funds_partially_released(
            &env,
            agreement_id.clone(),
            milestone_id,
            amount,
            remaining,
        );
        if completed {
            emit_if_agreement_completed(&env, &agreement_id, &header);
        }

        Ok(remaining)
    }

    /// Raise a dispute on a milestone that is currently `Funded` or `WorkSubmitted`.
    ///
    /// Either the payer or the payee may call this — the `caller` arg is
    /// checked against both roles so either party can autonomously trigger
    /// the dispute window.  This prevents a malicious payer from silently
    /// refusing to approve work AND refusing to raise a dispute, which would
    /// permanently lock the freelancer's funds.
    ///
    /// `reason_uri` optionally points at the disputing party's reason or
    /// evidence, mirroring `submit_work`'s `proof_uri`. An empty string is
    /// normalized to `None` and the value is emitted in the `dispute_raised`
    /// event only; it is not stored on the milestone.
    ///
    /// # Errors
    /// - [`TrellisError::AgreementNotFound`] – unknown agreement ID.
    /// - [`TrellisError::Unauthorized`] – `caller` is neither payer nor payee.
    /// - [`TrellisError::InvalidMilestone`] – `milestone_id` out of range.
    /// - [`TrellisError::InvalidStateTransition`] – milestone has no funds at
    ///   stake (status is not `Funded` or `WorkSubmitted`).
    pub fn raise_dispute(
        env: Env,
        caller: Address,
        agreement_id: BytesN<32>,
        milestone_id: u32,
        reason_uri: Option<String>,
    ) -> Result<(), TrellisError> {
        // #401: header + single milestone only — O(1) reads and one write.
        let header = storage::read_header(&env, &agreement_id)?;

        // Check the caller is an authorised party before requiring their sig.
        if caller != header.payer && caller != header.payee {
            return Err(TrellisError::Unauthorized);
        }
        // Require the on-chain signature of whichever party is calling.
        caller.require_auth();

        let mut milestone = storage::read_milestone(&env, &agreement_id, milestone_id)?;

        // Only milestones with funds at stake can be disputed.
        if milestone.status != EscrowStatus::Funded
            && milestone.status != EscrowStatus::WorkSubmitted
        {
            return Err(TrellisError::InvalidStateTransition);
        }

        let amount = remaining_amount(&header, milestone_id, &milestone);
        milestone.status = EscrowStatus::Disputed;
        storage::write_milestone(&env, &agreement_id, milestone_id, &milestone);

        events::dispute_raised(
            &env,
            agreement_id,
            milestone_id,
            caller,
            amount,
            reason_uri.filter(|s| !s.is_empty()),
        );

        Ok(())
    }

    /// Settle a disputed milestone with an explicit split between the parties.
    ///
    /// Pass `payer_amount` and `payee_amount` to split the milestone's still-
    /// escrowed amount between the two parties. The two amounts must be
    /// non-negative and sum exactly to that remainder, so every unit of locked
    /// funds is accounted for exactly once. Passing the whole remainder to one
    /// side reproduces [`Self::resolve_dispute`]'s all-or-nothing behaviour.
    ///
    /// If this ruling settles the agreement's last non-terminal milestone,
    /// `agreement_completed` is emitted after `milestone_resolved`.
    ///
    /// # Auth
    /// `agreement.dispute_resolver.require_auth()` — see
    /// [`Self::resolve_dispute`] for why that alone is sufficient.
    ///
    /// # Errors
    /// - [`TrellisError::AgreementNotFound`] – unknown agreement ID.
    /// - [`TrellisError::InvalidMilestone`] – `milestone_id` out of range.
    /// - [`TrellisError::InvalidStateTransition`] – milestone is not `Disputed`.
    /// - [`TrellisError::InvalidSplitAmounts`] – `payer_amount` or
    ///   `payee_amount` is negative, or the two do not sum to the remainder.
    pub fn resolve_dispute_split(
        env: Env,
        agreement_id: BytesN<32>,
        milestone_id: u32,
        payer_amount: i128,
        payee_amount: i128,
    ) -> Result<(), TrellisError> {
        // #401: header + single milestone only — O(1) reads and one write.
        let mut header = storage::read_header(&env, &agreement_id)?;

        // `require_auth` is the enforcement gate — see `resolve_dispute`.
        header.dispute_resolver.require_auth();

        let mut milestone = storage::read_milestone(&env, &agreement_id, milestone_id)?;

        if milestone.status != EscrowStatus::Disputed {
            return Err(TrellisError::InvalidStateTransition);
        }

        // A prior `release_partial` is final, so a ruling only ever covers what
        // is still held in escrow for this milestone.
        let amount = remaining_amount(&header, milestone_id, &milestone);

        // Validate the split: both legs must be non-negative and together
        // account for exactly what is still escrowed. This is checked before any
        // state write or token movement so an invalid split cannot leave the
        // milestone in a half-resolved state.
        if payer_amount < 0 || payee_amount < 0 {
            return Err(TrellisError::InvalidSplitAmounts);
        }
        let total = payer_amount
            .checked_add(payee_amount)
            .ok_or(TrellisError::InvalidSplitAmounts)?;
        if total != amount {
            return Err(TrellisError::InvalidSplitAmounts);
        }

        // Status reflects the dominant outcome for indexers: a full refund to
        // the payer is `Refunded`, anything else (including a full award to
        // the payee or a genuine split) is `Completed`.
        if payee_amount == 0 {
            milestone.status = EscrowStatus::Refunded;
        } else {
            milestone.status = EscrowStatus::Completed;
        }

        // The ruling itself is published by the `milestone_resolved` event
        // below, which carries both legs — so an indexer can reconstruct the
        // split without an extra field on the milestone.
        storage::write_milestone(&env, &agreement_id, milestone_id, &milestone);
        close_milestone(&mut header, payee_amount == 0);
        storage::write_header(&env, &agreement_id, &header);

        // Settle both legs in a single transaction. Zero-amount legs are
        // skipped so we never issue a no-op transfer to the token contract.
        let token = token::Client::new(&env, &header.token);
        if payer_amount > 0 {
            token.transfer(
                &env.current_contract_address(),
                &header.payer,
                &payer_amount,
            );
        }
        if payee_amount > 0 {
            token.transfer(
                &env.current_contract_address(),
                &header.payee,
                &payee_amount,
            );
        }

        events::milestone_resolved(
            &env,
            agreement_id.clone(),
            milestone_id,
            payer_amount,
            payee_amount,
        );
        // A ruling moves the milestone into a terminal state, so it can be the
        // transition that settles the whole agreement — mirror
        // `approve_and_release` and emit `agreement_completed` after the
        // milestone-level event.
        emit_if_agreement_completed(
            &env,
            &agreement_id,
            &storage::read_agreement(&env, &agreement_id)?,
        );

        Ok(())
    }

    /// Settle a disputed milestone in full, to one side.
    ///
    /// `refund_to_payer = true` returns the still-escrowed amount to the payer
    /// (ruling against the payee); `false` releases it to the payee (ruling
    /// against the payer).
    ///
    /// This is the original all-or-nothing entrypoint and is kept because
    /// callers that have not migrated still invoke it — including the CLI's
    /// `trellis resolve --refund-to-payer <bool>`, which is why its name and
    /// arguments must not change. For a genuine partial outcome use
    /// [`Self::resolve_dispute_split`], which expresses what this one cannot.
    ///
    /// If this ruling settles the agreement's last non-terminal milestone,
    /// `agreement_completed` is emitted after `milestone_resolved`.
    ///
    /// # Auth
    /// `agreement.dispute_resolver.require_auth()` is the sole enforcement
    /// mechanism — the Soroban host automatically traps if the invoker's
    /// signature does not match the resolver address stored on-chain. No
    /// additional manual check is needed beyond `require_auth()`, which is why
    /// this entrypoint has no resolver-mismatch error variant: an unauthorised
    /// caller never reaches contract code at all.
    ///
    /// # Errors
    /// - [`TrellisError::AgreementNotFound`] – unknown agreement ID.
    /// - [`TrellisError::InvalidMilestone`] – `milestone_id` out of range.
    /// - [`TrellisError::InvalidStateTransition`] – milestone is not `Disputed`.
    pub fn resolve_dispute(
        env: Env,
        agreement_id: BytesN<32>,
        milestone_id: u32,
        refund_to_payer: bool,
    ) -> Result<(), TrellisError> {
        // #401: header + single milestone only — O(1) reads and one write.
        let mut header = storage::read_header(&env, &agreement_id)?;

        // `require_auth` is the enforcement gate — see the doc comment above.
        header.dispute_resolver.require_auth();

        let mut milestone = storage::read_milestone(&env, &agreement_id, milestone_id)?;

        if milestone.status != EscrowStatus::Disputed {
            return Err(TrellisError::InvalidStateTransition);
        }

        let amount = remaining_amount(&header, milestone_id, &milestone);

        milestone.status = if refund_to_payer {
            EscrowStatus::Refunded
        } else {
            EscrowStatus::Completed
        };
        storage::write_milestone(&env, &agreement_id, milestone_id, &milestone);
        close_milestone(&mut header, refund_to_payer);
        storage::write_header(&env, &agreement_id, &header);

        // Transfer tokens from this contract after the state change.
        token::Client::new(&env, &header.token).transfer(
            &env.current_contract_address(),
            if refund_to_payer {
                &header.payer
            } else {
                &header.payee
            },
            &amount,
        );

        // The event carries the split shape the merged event signature expects:
        // one leg is the full remainder, the other is zero.
        events::milestone_resolved(
            &env,
            agreement_id.clone(),
            milestone_id,
            if refund_to_payer { amount } else { 0 },
            if refund_to_payer { 0 } else { amount },
        );
        // See `resolve_dispute_split`: a ruling can settle the last
        // non-terminal milestone, so check for agreement completion.
        emit_if_agreement_completed(
            &env,
            &agreement_id,
            &storage::read_agreement(&env, &agreement_id)?,
        );

        Ok(())
    }

    /// Replace the agreement's dispute resolver.
    ///
    /// `dispute_resolver` is otherwise fixed at [`Self::init`] for the whole
    /// lifetime of the agreement. If that resolver's signing key is lost, it is
    /// compromised, or the operator simply stops answering, every milestone
    /// that reaches [`EscrowStatus::Disputed`] is permanently unsettleable:
    /// [`Self::resolve_dispute`] and [`Self::resolve_dispute_split`] are gated
    /// solely by `agreement.dispute_resolver.require_auth()`. This entrypoint
    /// closes that single point of failure.
    ///
    /// # Auth
    /// **Both** the payer and the payee must authorise the call
    /// (`header.payer.require_auth()` followed by `header.payee.require_auth()`).
    ///
    /// Requiring both parties is what stops the rotation being an attack in its
    /// own right. A single-party gate would let either side install a resolver
    /// it controls and then rule on the resulting dispute — the resolver would
    /// be adjudicating an escrow it has a stake in. With two signatures, a
    /// colluding resolver-plus-one-party cannot swap in a friendly arbiter.
    ///
    /// The **outgoing resolver is deliberately not required to sign.** That is
    /// the entire point of this entrypoint: the situations it exists for —
    /// a lost key, a compromised key, an operator that has stopped responding —
    /// are all cases where the outgoing resolver's signature is either
    /// unavailable or worthless. Requiring it would make rotation impossible in
    /// precisely the scenario it was written for. The incoming resolver does
    /// not sign at rotation time either; it acquires the authority to settle
    /// disputes the moment this call succeeds.
    ///
    /// # Effect
    /// Only `dispute_resolver` changes. Milestone statuses, escrowed amounts,
    /// `total_amount`, `released_amounts`, deadlines and every other header
    /// field are untouched, and no tokens move — so the rotation is safe to
    /// perform at any point in the agreement's life, **including while a
    /// milestone is actively `Disputed`**. That is the rescue path: a dispute
    /// raised against a resolver that has since gone dark can be settled by the
    /// replacement as soon as the rotation lands.
    ///
    /// The rotation is immediate and not retroactive. A `Disputed` milestone
    /// stays `Disputed` — rotation does not settle it — it just changes *who*
    /// is able to settle it. Milestones already settled are final and are never
    /// re-opened; the outgoing resolver's past rulings stand.
    ///
    /// # Events
    /// Emits `("resolver_rotated", agreement_id)` carrying both the old and the
    /// new resolver address, so the full resolver history of an agreement is
    /// reconstructible from the event log alone.
    ///
    /// # Errors
    /// - [`TrellisError::AgreementNotFound`] – unknown agreement ID.
    /// - [`TrellisError::ResolverCannotBeParty`] – `new_resolver` equals
    ///   `payer` or `payee`. This mirrors the identical guard in [`Self::init`],
    ///   so an address can never become a party's own resolver no matter how
    ///   many rotations happen.
    pub fn rotate_resolver(
        env: Env,
        agreement_id: BytesN<32>,
        new_resolver: Address,
    ) -> Result<(), TrellisError> {
        // #401: header only — one read and one write, never the milestone vector.
        let mut header = storage::read_header(&env, &agreement_id)?;

        // Both parties consent. See the "Auth" section above for why the
        // outgoing resolver is not asked to sign.
        header.payer.require_auth();
        header.payee.require_auth();

        // Same neutrality invariant `init` enforces, re-checked on every
        // rotation so a party address can never be installed as the resolver.
        if new_resolver == header.payer || new_resolver == header.payee {
            return Err(TrellisError::ResolverCannotBeParty);
        }

        let old_resolver = header.dispute_resolver.clone();
        header.dispute_resolver = new_resolver.clone();
        storage::write_header(&env, &agreement_id, &header);

        // Record both addresses: `get_agreement` only ever exposes the current
        // resolver, so this event is the only record of who held the role
        // before.
        events::resolver_rotated(&env, agreement_id, old_resolver, new_resolver);

        Ok(())
    }

    /// Cancel a milestone that was never funded (status = `Pending`).
    ///
    /// Only the payer may withdraw a milestone proposal that has not yet had
    /// funds locked against it.  If any funds were ever locked the payer must
    /// go through the dispute flow instead.
    ///
    /// # Events
    /// Emits `("cancelled", agreement_id)` — **not** the `("resolved", …)`
    /// event used by [`Self::resolve_dispute`]. No tokens move here, so
    /// off-chain consumers must not treat a cancellation as a dispute ruling.
    ///
    /// The milestone transitions to [`EscrowStatus::Cancelled`] — never
    /// [`EscrowStatus::Refunded`], which is reserved for dispute rulings
    /// (`resolve_dispute`) where real funds were locked and then returned.
    ///
    /// # Errors
    /// - [`TrellisError::AgreementNotFound`] – unknown agreement ID.
    /// - [`TrellisError::InvalidMilestone`] – `milestone_id` out of range.
    /// - [`TrellisError::InvalidStateTransition`] – milestone is not `Pending`
    ///   (i.e. funds exist or the milestone is already resolved). This is a
    ///   state machine violation, not an economic one — use the dispute flow
    ///   instead once a milestone has left `Pending`.
    pub fn cancel_unfunded_milestone(
        env: Env,
        agreement_id: BytesN<32>,
        milestone_id: u32,
    ) -> Result<(), TrellisError> {
        // #401: header + single milestone only — O(1) reads and one write.
        let header = storage::read_header(&env, &agreement_id)?;
        header.payer.require_auth();

        let mut milestone = storage::read_milestone(&env, &agreement_id, milestone_id)?;

        if milestone.status != EscrowStatus::Pending {
            // Funds exist or milestone already resolved — use dispute flow.
            return Err(TrellisError::InvalidStateTransition);
        }

        // Mark the milestone closed with no token movement required.
        //
        // `Cancelled`, not `Refunded`: this milestone was never funded, so no
        // tokens ever moved. `Refunded` is reserved for a dispute ruling that
        // actually returned escrowed funds, and a reader of `get_agreement`
        // must be able to tell the two apart without replaying the event log —
        // which is what this entrypoint's own doc comment promises.
        let amount = milestone.amount;
        milestone.status = EscrowStatus::Cancelled;
        storage::write_milestone(&env, &agreement_id, milestone_id, &milestone);

        // Emit the dedicated cancellation event rather than milestone_resolved:
        // no arbitration happened and no tokens moved, so indexers must be able
        // to tell this apart from a dispute ruling.
        events::milestone_cancelled(
            &env,
            agreement_id.clone(),
            milestone_id,
            header.payer.clone(),
            header.payer,
            amount,
        );
        // `Cancelled` is not a settled state, so the milestone stays counted in
        // `open_milestones` and this transition can never complete the agreement.

        Ok(())
    }

    /// Return the full [`Agreement`] struct for the given ID.
    ///
    /// This is a view call — no auth is required and no agreement data is
    /// modified, but it is **not** free of side effects: the read goes through
    /// `storage::read_agreement`, which renews the entry's ledger TTL whenever
    /// the remaining lifetime has dropped below the renewal threshold, and the
    /// caller pays the rent for that extension.  An agreement that is merely
    /// being watched — a long dispute window, a milestone awaiting delivery —
    /// is still in active use and must not be archived out from under its
    /// parties, which is why the renewal happens on read rather than only on
    /// write.  A bump-free variant was considered for callers that want a
    /// strictly side-effect-free probe, but it would make a single careless
    /// read (for example from an indexer polling every block) silently
    /// responsible for the agreement's lifetime.
    ///
    /// It exists primarily so the CLI `status` command can display the current
    /// agreement state (including per-milestone statuses) via
    /// `stellar contract invoke … -- get_agreement --agreement-id <hex>`.
    ///
    /// # Errors
    /// Returns [`TrellisError::AgreementNotFound`] if no agreement exists for
    /// the given `agreement_id`.
    pub fn get_agreement(env: Env, agreement_id: BytesN<32>) -> Result<Agreement, TrellisError> {
        storage::read_agreement(&env, &agreement_id)
    }

    /// Return the pre-computed total value of an agreement.
    ///
    /// Equivalent to summing `amount` over every milestone in
    /// [`Self::get_agreement`], but avoids the O(n) iteration for callers who
    /// only need the total — see [`Agreement::total_amount`].
    ///
    /// # Errors
    /// Returns [`TrellisError::AgreementNotFound`] if no agreement exists for
    /// the given `agreement_id`.
    pub fn get_total_amount(env: Env, agreement_id: BytesN<32>) -> Result<i128, TrellisError> {
        storage::read_header(&env, &agreement_id).map(|header| header.total_amount)
    }

    /// Fund multiple milestones in a single transaction.
    ///
    /// Iterates `milestone_ids` in order, applying the same logic as
    /// [`Self::lock_funds`] for each entry.  The entire call is atomic: if any
    /// milestone fails (out of range, wrong status), the transaction reverts and
    /// no tokens are transferred.  Individual `funds_locked` events are emitted
    /// for each successfully funded milestone so off-chain indexers retain the
    /// same per-milestone event granularity as sequential calls.
    ///
    /// The payer authorises this call once and the auth covers all transfers
    /// within the batch.
    ///
    /// # Empty input
    /// An empty `milestone_ids` is a no-op that returns `Ok(0)`. No state is
    /// written, no event is emitted and no token moves. The agreement is still
    /// read (so an unknown ID returns
    /// [`TrellisError::AgreementNotFound`]) and the payer's authorisation is
    /// still required — an empty batch is a well-formed call, not a bypass of
    /// either check.
    ///
    /// # Errors
    /// - [`TrellisError::AgreementNotFound`] – unknown agreement ID.
    /// - [`TrellisError::InvalidMilestone`] – any ID in `milestone_ids` is out of range.
    /// - [`TrellisError::InvalidStateTransition`] – any milestone is not `Pending`.
    /// - [`TrellisError::DeadlinePassed`] – any milestone's deadline has
    ///   already passed (see [`Self::lock_funds`]).
    pub fn batch_lock_funds(
        env: Env,
        agreement_id: BytesN<32>,
        milestone_ids: Vec<u32>,
    ) -> Result<u32, TrellisError> {
        // #401: a batch costs one write per *requested* milestone, not one
        // write of the whole agreement — the header is read once and never
        // rewritten, and milestones outside `milestone_ids` are untouched.
        let header = storage::read_header(&env, &agreement_id)?;
        header.payer.require_auth();

        // An empty batch funds nothing, so there is no state change to persist.
        // Returning here skips any milestone write, which would otherwise be a
        // redundant operation that costs the caller gas.
        if milestone_ids.is_empty() {
            return Ok(0);
        }

        let mut funded: u32 = 0;
        // #611: each milestone is read exactly once; its amount is summed here
        // and moved in a single transfer below instead of re-reading every
        // milestone and making one cross-contract call per milestone.
        let mut total: i128 = 0;

        for milestone_id in milestone_ids.iter() {
            let mut milestone = storage::read_milestone(&env, &agreement_id, milestone_id)?;

            if milestone.status != EscrowStatus::Pending {
                return Err(TrellisError::InvalidStateTransition);
            }
            ensure_deadline_not_passed(&env, &agreement_id, milestone_id)?;

            let amount = milestone.amount;
            total = total
                .checked_add(amount)
                .ok_or(TrellisError::TotalAmountOverflow)?;
            milestone.status = EscrowStatus::Funded;
            storage::write_milestone(&env, &agreement_id, milestone_id, &milestone);

            events::funds_locked(&env, agreement_id.clone(), milestone_id, amount);
            funded += 1;
        }

        let mut total: i128 = 0;
        for milestone_id in milestone_ids.iter() {
            let milestone = storage::read_milestone(&env, &agreement_id, milestone_id)?;
            total = total
                .checked_add(milestone.amount)
                .ok_or(TrellisError::TransferShortfall)?;
        }
        // One balance check around the whole batch: the delta must cover every
        // milestone just marked `Funded`.
        pull_funds(&env, &header.token, &header.payer, total, total)?;

        Ok(funded)
    }

    /// Return a single [`Milestone`] by its index within the agreement.
    ///
    /// This is a view call — no auth is required and no agreement data is
    /// modified, but, as with [`Self::get_agreement`], the read renews the
    /// entry's ledger TTL when the remaining lifetime is below the renewal
    /// threshold (see `storage::read_agreement`).  It lets callers query one
    /// milestone's current status without deserializing the full [`Agreement`]
    /// struct, which reduces ledger read cost for agreements with many
    /// milestones.
    ///
    /// # Return type
    /// Like its siblings [`Self::get_agreement`] and [`Self::get_total_amount`],
    /// this returns [`Result`], so the two failure modes stay distinguishable:
    ///
    /// - [`TrellisError::AgreementNotFound`] – no agreement exists for the
    ///   given `agreement_id`.
    /// - `Ok(None)` – the agreement exists but `milestone_id` is out of range.
    ///
    /// Returning a bare `Option<Milestone>` would collapse those two very
    /// different situations into a single `None`, forcing every caller to
    /// handle this one entrypoint differently from its siblings for no
    /// functional benefit.
    pub fn get_milestone(
        env: Env,
        agreement_id: BytesN<32>,
        milestone_id: u32,
    ) -> Option<Milestone> {
        storage::read_milestone(&env, &agreement_id, milestone_id).ok()
    }

    /// Renew the ledger TTL of an agreement without changing its state.
    ///
    /// Persistent entries are archived once their TTL runs out, which would
    /// destroy the agreement record. State-mutating entrypoints renew the TTL
    /// automatically, but an agreement that sits idle — a long delivery
    /// window, a stalled dispute — receives no writes and will eventually
    /// expire. This entrypoint exists so an external keeper service can renew
    /// it on a schedule.
    ///
    /// Still permissionless — any address, including a third-party keeper,
    /// may renew any agreement — but `caller` must authorise its own call
    /// (`caller.require_auth()`). `caller` is published in the `ttl_extended`
    /// event as an audit trail of keeper activity, so without the check anyone
    /// could log an arbitrary address as the keeper. A keeper already signs
    /// the transaction it submits, so this costs it nothing, and it matches
    /// [`Self::expire_milestone`], which authenticates its `caller` for the
    /// same reason.
    ///
    /// # Errors
    /// - [`TrellisError::AgreementNotFound`] – unknown agreement ID.
    pub fn extend_agreement_ttl(
        env: Env,
        agreement_id: BytesN<32>,
        caller: Address,
    ) -> Result<(), TrellisError> {
        // Authenticate before the TTL bump so the emitted `caller` is
        // guaranteed to be the real signer.
        caller.require_auth();

        storage::extend_agreement_ttl(&env, &agreement_id)?;
        events::ttl_extended(&env, agreement_id, caller);
        Ok(())
    }

    /// Set a deadline on a milestone that has not been funded yet.
    ///
    /// `deadline` is a ledger timestamp (Unix seconds, compared against
    /// `env.ledger().timestamp()`). Once it passes, anyone may call
    /// [`Self::expire_milestone`] to close the milestone if it is still
    /// stalled — see that entrypoint for exactly which states are covered.
    ///
    /// Only the payer may set a deadline, and only while the milestone is
    /// `Pending`: the deadline is part of the terms the payee sees *before*
    /// any funds are locked and any work starts, so it can never be imposed
    /// on (or moved under) a payee who is already working against escrowed
    /// funds. Calling it again while still `Pending` replaces the deadline.
    ///
    /// Deadlines are stored beside the agreement rather than as a field on
    /// [`Milestone`], so the `init` argument layout and `get_agreement` /
    /// `get_milestone` return types are unchanged for existing callers. Use
    /// [`Self::get_milestone_deadline`] to read one back.
    ///
    /// # Errors
    /// - [`TrellisError::AgreementNotFound`] – unknown agreement ID.
    /// - [`TrellisError::InvalidMilestone`] – `milestone_id` out of range.
    /// - [`TrellisError::InvalidStateTransition`] – milestone not `Pending`.
    /// - [`TrellisError::DeadlineInPast`] – `deadline` is not strictly after
    ///   the current ledger timestamp.
    pub fn set_milestone_deadline(
        env: Env,
        agreement_id: BytesN<32>,
        milestone_id: u32,
        deadline: u64,
    ) -> Result<(), TrellisError> {
        let header = storage::read_header(&env, &agreement_id)?;
        header.payer.require_auth();

        let milestone = storage::read_milestone(&env, &agreement_id, milestone_id)?;

        if milestone.status != EscrowStatus::Pending {
            return Err(TrellisError::InvalidStateTransition);
        }

        if deadline <= env.ledger().timestamp() {
            return Err(TrellisError::DeadlineInPast);
        }

        storage::write_deadline(&env, &agreement_id, milestone_id, deadline);
        events::deadline_set(&env, agreement_id, milestone_id, deadline);

        Ok(())
    }

    /// Return the deadline set for a milestone, if any.
    ///
    /// Read-only view — no auth required, no state modified. Returns `None`
    /// when the agreement or milestone does not exist, or when no deadline
    /// was ever set (mirroring [`Self::get_milestone`]).
    pub fn get_milestone_deadline(
        env: Env,
        agreement_id: BytesN<32>,
        milestone_id: u32,
    ) -> Option<u64> {
        storage::read_deadline(&env, &agreement_id, milestone_id)
    }

    /// Close a stalled milestone whose deadline has passed.
    ///
    /// Permissionless: `caller` only has to authorise its own call (so the
    /// emitted event names who really triggered it). The payer, the payee
    /// or a third-party keeper can all use it — the outcome is fixed by the
    /// milestone's state, not by who calls.
    ///
    /// Covered stalled states, once `env.ledger().timestamp() > deadline`:
    /// - `Pending` (never funded) → `Cancelled`. No tokens move; this is the
    ///   automatic equivalent of [`Self::cancel_unfunded_milestone`], and
    ///   writes the same status it does.
    /// - `Funded` (payee never submitted work) → `Refunded`, and the locked
    ///   `amount` is returned to the payer.
    ///
    /// The split mirrors the rest of the contract: `Refunded` always means
    /// escrowed tokens really went back to the payer, while `Cancelled` marks a
    /// milestone that was closed before anything was ever locked.
    ///
    /// Deliberately **not** covered:
    /// - `WorkSubmitted` — the payee has delivered, so an unresponsive payer
    ///   must not be able to win by default. Either party can still
    ///   [`Self::raise_dispute`].
    /// - `Disputed` — the resolver owns the outcome; a timeout would let
    ///   either party run out the clock on arbitration.
    /// - `Completed` / `Refunded` / `Cancelled` — already terminal.
    ///
    /// All of these return [`TrellisError::InvalidStateTransition`].
    ///
    /// # Errors
    /// - [`TrellisError::AgreementNotFound`] – unknown agreement ID.
    /// - [`TrellisError::InvalidMilestone`] – `milestone_id` out of range.
    /// - [`TrellisError::DeadlineNotReached`] – no deadline set, or the
    ///   current ledger timestamp is not yet past it.
    /// - [`TrellisError::InvalidStateTransition`] – milestone is not
    ///   `Pending` or `Funded`.
    pub fn expire_milestone(
        env: Env,
        caller: Address,
        agreement_id: BytesN<32>,
        milestone_id: u32,
    ) -> Result<(), TrellisError> {
        caller.require_auth();

        let mut header = storage::read_header(&env, &agreement_id)?;

        let mut milestone = storage::read_milestone(&env, &agreement_id, milestone_id)?;

        match storage::read_deadline(&env, &agreement_id, milestone_id) {
            Some(deadline) if env.ledger().timestamp() > deadline => {}
            _ => return Err(TrellisError::DeadlineNotReached),
        }

        // A never-funded milestone is `Cancelled` (no tokens moved), exactly as
        // `cancel_unfunded_milestone` would leave it; `Refunded` is reserved
        // for the case where escrowed funds really go back to the payer.
        let (refunded_amount, new_status) = match milestone.status {
            EscrowStatus::Pending => (0, EscrowStatus::Cancelled),
            EscrowStatus::Funded => (milestone.amount, EscrowStatus::Refunded),
            _ => return Err(TrellisError::InvalidStateTransition),
        };

        // Checks-effects-interactions: persist the terminal state before any
        // token transfer.
        milestone.status = EscrowStatus::Refunded;
        storage::write_milestone(&env, &agreement_id, milestone_id, &milestone);
        close_milestone(&mut header, true);
        storage::write_header(&env, &agreement_id, &header);

        if refunded_amount > 0 {
            token::Client::new(&env, &header.token).transfer(
                &env.current_contract_address(),
                &header.payer,
                &refunded_amount,
            );
        }

        events::milestone_expired(&env, agreement_id, milestone_id, refunded_amount, caller);

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Amount still held in escrow for `milestone`: its configured `amount` minus
/// whatever [`TrellisContract::release_partial`] has already paid out.
///
/// Takes the header rather than a whole [`Agreement`] so callers on the
/// single-milestone path (#401) do not have to reassemble the milestone
/// vector just to learn one number.
fn remaining_amount(header: &AgreementHeader, milestone_id: u32, milestone: &Milestone) -> i128 {
    milestone.amount - header.released_amounts.get(milestone_id).unwrap_or(0)
}

/// Record that one milestone reached a settled state (`Completed` or
/// `Refunded`). The caller persists the header.
fn close_milestone(header: &mut AgreementHeader, refunded: bool) {
    header.open_milestones = header.open_milestones.saturating_sub(1);
    if refunded {
        header.refunded_milestones += 1;
    }
}

/// Emit `agreement_completed` once no milestone is open any more.
///
/// Called only right after an entrypoint settled one milestone and updated
/// `header`. Settled states have no outgoing transitions, so the counter can
/// reach zero exactly once per agreement — the event fires at most once, on
/// the transition that settled the last milestone.
fn emit_if_agreement_completed(env: &Env, agreement_id: &BytesN<32>, header: &AgreementHeader) {
    if header.open_milestones == 0 {
        events::agreement_completed(
            env,
            agreement_id.clone(),
            header.milestone_count - header.refunded_milestones,
            header.refunded_milestones,
        );
    }
}

/// Move `amount` of `token` from `payer` into this contract and require that
/// the contract's balance grew by at least `expected`.
///
/// A fee-on-transfer token would otherwise leave the pooled balance short of
/// what milestones were credited, silently draining other agreements.
fn pull_funds(
    env: &Env,
    token: &Address,
    payer: &Address,
    amount: i128,
    expected: i128,
) -> Result<(), TrellisError> {
    let client = token::Client::new(env, token);
    let this = env.current_contract_address();
    let before = client.balance(&this);
    client.transfer(payer, &this, &amount);
    if client.balance(&this) - before < expected {
        return Err(TrellisError::TransferShortfall);
    }
    Ok(())
}

/// Validate incoming milestones and sum their amounts into a total.
///
/// Runs once, in `init`, before the agreement is written to storage — so an
/// invalid milestone list never consumes storage, and every later reader gets
/// the sum for free via [`Agreement::total_amount`] instead of iterating
/// `milestones` on every query.
///
/// Each milestone must be in [`EscrowStatus::Pending`]: it is the sole entry
/// point of the state machine, and every later status is reached by locking
/// funds first. Accepting a pre-advanced milestone would let its owner skip
/// `lock_funds` entirely and then release from the shared contract balance
/// tokens that were never escrowed for that milestone.
///
/// # Errors
/// Returns [`TrellisError::InvalidMilestone`] on the first milestone whose
/// `amount` is zero or negative.
/// Returns [`TrellisError::InvalidInitialMilestoneStatus`] on the first
/// milestone whose `status` is not `Pending`.
/// Returns [`TrellisError::TotalAmountOverflow`] if the sum of milestone
/// amounts exceeds i128::MAX.
fn validate_milestones(milestones: &Vec<Milestone>) -> Result<i128, TrellisError> {
    let mut total: i128 = 0;
    for m in milestones.iter() {
        if m.status != EscrowStatus::Pending {
            return Err(TrellisError::InvalidInitialMilestoneStatus);
        }
        if m.amount <= 0 {
            return Err(TrellisError::InvalidMilestone);
        }
        total = total
            .checked_add(m.amount)
            .ok_or(TrellisError::TotalAmountOverflow)?;
    }
    Ok(total)
}
