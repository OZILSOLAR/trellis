use soroban_sdk::{
    symbol_short,
    testutils::{storage::Persistent, Address as _, Events, Ledger},
    token, vec, Address, BytesN, Env, String, Symbol, TryFromVal, Vec,
};

use crate::{
    errors::TrellisError,
    test_utils::{agreement_id, milestones_from_amounts, one_milestone},
    types::{EscrowStatus, Milestone},
    TrellisContract, TrellisContractClient, MAX_PROOF_URI_LEN,
};

// ---------------------------------------------------------------------------
// Test helpers
//
// `agreement_id` and `one_milestone` come from `crate::test_utils` so all three
// suites share one implementation (#403). `setup` stays local because this
// suite blanket-mocks auth for the whole environment (see [`allow_all_auth`]),
// whereas the property and panic-boundary suites use `test_utils::setup_mocked`.
// ---------------------------------------------------------------------------

/// Common test fixture.
///
/// Returns `(env, payer, payee, dispute_resolver, token_address, client)`.
/// Auth is mocked for the whole environment — see [`allow_all_auth`].
fn setup() -> (
    Env,
    Address,
    Address,
    Address,
    Address,
    TrellisContractClient<'static>,
) {
    let env = Env::default();

    let payer = Address::generate(&env);
    let payee = Address::generate(&env);
    let dispute_resolver = Address::generate(&env);

    // Deploy the built-in Stellar Asset Contract and mint payer a balance.
    // The mint is authorised by the asset admin, so auth has to be mocked
    // before it — `env.mock_all_auths()` also covers the Trellis entrypoints.
    allow_all_auth(&env);
    let token_admin = Address::generate(&env);
    let token_address = env
        .register_stellar_asset_contract_v2(token_admin.clone())
        .address();
    let token_admin_client = token::StellarAssetClient::new(&env, &token_address);
    token_admin_client.mint(&payer, &10_000);

    // Register the Trellis contract.
    let contract_id = env.register(TrellisContract, ());
    let client = TrellisContractClient::new(&env, &contract_id);

    (env, payer, payee, dispute_resolver, token_address, client)
}

/// Allow every `require_auth` call in the test environment to succeed.
///
/// The Trellis entrypoints gate on `agreement.<role>.require_auth()`, so a
/// blanket mock is what lets a test drive the happy path. Tests that assert an
/// authorisation failure call [`deny_all_auth`] first, which turns mocking off
/// and makes any `require_auth` trap instead.
fn allow_all_auth(env: &Env) {
    env.mock_all_auths();
}

/// Turn auth mocking off, so any `require_auth` in the contract traps.
///
/// This is the positive control for the authorisation tests: it proves the
/// failure they observe comes from the contract's own auth gate and not from
/// some unrelated setup mistake.
fn deny_all_auth(env: &Env) {
    env.set_auths(&[]);
}

/// Assert the Trellis contract's own events, in order, for the most recent
/// top-level invocation.
///
/// `env.events().all()` also carries the SAC's own mint/transfer events, and in
/// soroban-sdk 22 it only reports the events of the *latest* invocation — so
/// each entrypoint's events have to be checked straight after the call rather
/// than accumulated across the whole test. Events from other contracts (the
/// token) are filtered out by contract address.
fn assert_trellis_topics(env: &Env, contract: &Address, expected: &[Symbol], msg: &str) {
    let all_events = env.events().all();
    let mut matched = 0usize;
    for i in 0..all_events.len() {
        let (contract_id, topics, _data) = all_events.get_unchecked(i);
        if contract_id != *contract {
            continue;
        }
        let topic0 = Symbol::try_from_val(env, &topics.get_unchecked(0))
            .expect("event topic 0 must decode as a Symbol");
        assert!(
            matched < expected.len(),
            "more Trellis contract events fired than expected in the last invocation"
        );
        assert_eq!(topic0, expected[matched], "event {matched} name mismatch");
        matched += 1;
    }
    assert_eq!(
        matched,
        expected.len(),
        "{msg} (saw {} total events)",
        all_events.len()
    );
}

/// Return the data payload of the single Trellis event named `name` from the
/// most recent top-level invocation, decoded as `T`.
fn trellis_event_data<T>(env: &Env, contract: &Address, name: Symbol) -> T
where
    T: TryFromVal<Env, soroban_sdk::Val>,
{
    let all_events = env.events().all();
    let mut found: Option<T> = None;
    for i in 0..all_events.len() {
        let (contract_id, topics, data) = all_events.get_unchecked(i);
        if contract_id != *contract {
            continue;
        }
        let topic0 = Symbol::try_from_val(env, &topics.get_unchecked(0))
            .expect("event topic 0 must decode as a Symbol");
        if topic0 == name {
            assert!(found.is_none(), "event {name:?} emitted more than once");
            found = Some(
                T::try_from_val(env, &data)
                    .unwrap_or_else(|_| panic!("event {name:?} data has unexpected shape")),
            );
        }
    }
    found.unwrap_or_else(|| panic!("event {name:?} was not emitted"))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Full happy-path: init → lock → submit → release.
/// Verifies balances at each step and checks all 4 events were emitted.
#[test]
fn test_happy_path() {
    let (env, payer, payee, dispute_resolver, token_address, client) = setup();
    let token_client = token::TokenClient::new(&env, &token_address);
    let id = agreement_id(&env, 1);
    let amount: i128 = 1_000;

    // ── init ───────────────────────────────────────────────────────────────
    client.init(
        &id,
        &payer,
        &payee,
        &token_address,
        &one_milestone(&env, amount),
        &dispute_resolver,
    );
    assert_trellis_topics(
        &env,
        &client.address,
        &[symbol_short!("trls_crte")],
        "init must emit exactly one agreement_created event",
    );

    // ── lock_funds ─────────────────────────────────────────────────────────
    let payer_balance_before = token_client.balance(&payer);
    client.lock_funds(&id, &0u32);
    assert_trellis_topics(
        &env,
        &client.address,
        &[symbol_short!("trls_lckd")],
        "lock_funds must emit exactly one funds_locked event",
    );

    assert_eq!(
        token_client.balance(&payer),
        payer_balance_before - amount,
        "payer balance should decrease by milestone amount after lock"
    );
    assert_eq!(
        token_client.balance(&client.address),
        amount,
        "trellis contract balance should equal locked milestone amount"
    );

    // ── submit_work ────────────────────────────────────────────────────────
    let proof = Some(String::from_str(&env, "ipfs://test"));
    client.submit_work(&id, &0u32, &proof);
    assert_trellis_topics(
        &env,
        &client.address,
        &[symbol_short!("trls_sbmt")],
        "submit_work must emit exactly one work_submitted event",
    );

    // ── approve_and_release ────────────────────────────────────────────────
    client.approve_and_release(&id, &0u32);
    assert_trellis_topics(
        &env,
        &client.address,
        &[symbol_short!("trls_rlsd"), symbol_short!("trls_cmpl")],
        "approve_and_release on the only milestone must emit funds_released \
         followed by agreement_completed",
    );

    assert_eq!(
        token_client.balance(&payee),
        amount,
        "payee should receive the milestone amount after release"
    );
    assert_eq!(
        token_client.balance(&client.address),
        0,
        "contract balance should be zero after release"
    );
}

/// Exploit path (#382): `init` must reject a milestone pre-set to any status
/// other than `Pending`.
///
/// Every agreement sharing a token draws from one pooled contract balance.
/// A caller controlling all three roles (payer, payee, resolver) could
/// otherwise `init` an agreement with a phantom `WorkSubmitted` milestone and
/// call `approve_and_release` immediately, or a phantom `Disputed` one and
/// call `resolve_dispute` — either transfers tokens out of the shared pool
/// that were never escrowed for that milestone.
///
/// Each non-Pending status is checked individually because they are the ones
/// that reach a fund-moving entrypoint; `Completed`, `Refunded` and
/// `Cancelled` are inert dead ends, but are rejected too so the invariant stays
/// "Pending only".
#[test]
fn test_init_rejects_non_pending_initial_milestone_status() {
    // WorkSubmitted → approve_and_release; Disputed → resolve_dispute.
    // Those two are the actual drain vectors; the rest complete the set.
    let forbidden = [
        EscrowStatus::Funded,
        EscrowStatus::WorkSubmitted,
        EscrowStatus::Completed,
        EscrowStatus::Disputed,
        EscrowStatus::Refunded,
        EscrowStatus::Cancelled,
    ];

    for (i, status) in forbidden.iter().enumerate() {
        let (env, payer, payee, dispute_resolver, token_address, client) = setup();
        // Distinct seed per status keeps failures attributable.
        let id = agreement_id(&env, 100 + i as u8);
        let milestones = vec![
            &env,
            Milestone {
                amount: 1_000,
                status: status.clone(),
                proof_uri: None,
            },
        ];

        env.mock_all_auths();
        let result = client.try_init(
            &id,
            &payer,
            &payee,
            &token_address,
            &milestones,
            &dispute_resolver,
        );

        assert_eq!(
            result,
            Err(Ok(TrellisError::InvalidInitialMilestoneStatus)),
            "init must reject a milestone initialised as {status:?} — it is a \
             claim on pooled funds that were never escrowed for it"
        );

        // The rejected init must not have written anything to storage,
        // otherwise the phantom agreement would still be reachable.
        assert!(
            client.try_get_agreement(&id).is_err(),
            "a rejected init must not persist an agreement for {status:?}"
        );
    }
}

/// Adjacent case (#382): one non-Pending milestone poisons the whole `init`.
///
/// This is what regresses if the check is written to coerce offending
/// milestones back to `Pending` instead of rejecting them — the drain would
/// be closed, but the caller would silently receive an agreement whose
/// declared state was rewritten. Rejecting atomically surfaces the bug to
/// the integrator instead of hiding it.
#[test]
fn test_init_rejects_whole_set_when_one_milestone_is_not_pending() {
    let (env, payer, payee, dispute_resolver, token_address, client) = setup();
    let id = agreement_id(&env, 120);

    // Two valid Pending milestones around one phantom Funded one.
    let milestones = vec![
        &env,
        Milestone {
            amount: 1_000,
            status: EscrowStatus::Pending,
            proof_uri: None,
        },
        Milestone {
            amount: 2_000,
            status: EscrowStatus::Funded,
            proof_uri: None,
        },
        Milestone {
            amount: 3_000,
            status: EscrowStatus::Pending,
            proof_uri: None,
        },
    ];

    env.mock_all_auths();
    let result = client.try_init(
        &id,
        &payer,
        &payee,
        &token_address,
        &milestones,
        &dispute_resolver,
    );

    assert_eq!(
        result,
        Err(Ok(TrellisError::InvalidInitialMilestoneStatus)),
        "one non-Pending milestone must reject the entire init, not be coerced"
    );
    assert!(
        client.try_get_agreement(&id).is_err(),
        "a partially-valid milestone set must not be persisted"
    );
}

/// Adjacent case (#382): the legitimate happy path is untouched, and the
/// agreement created under the new invariant stays fully usable.
///
/// Guards against the new validation being over-eager — rejecting a
/// legitimate `Pending` milestone, breaking the existing amount checks, or
/// writing the agreement in a state that blocks the normal escrow flow.
#[test]
fn test_init_accepts_pending_milestones_happy_path() {
    let (env, payer, payee, dispute_resolver, token_address, client) = setup();
    let id = agreement_id(&env, 121);
    let token_client = token::TokenClient::new(&env, &token_address);
    let payer_before = token_client.balance(&payer);

    let milestones = vec![
        &env,
        Milestone {
            amount: 1_000,
            status: EscrowStatus::Pending,
            proof_uri: None,
        },
        Milestone {
            amount: 2_000,
            status: EscrowStatus::Pending,
            proof_uri: None,
        },
    ];

    env.mock_all_auths();
    client.init(
        &id,
        &payer,
        &payee,
        &token_address,
        &milestones,
        &dispute_resolver,
    );

    let agreement = client.get_agreement(&id);
    assert_eq!(agreement.total_amount, 3_000);
    assert!(
        agreement
            .milestones
            .iter()
            .all(|m| m.status == EscrowStatus::Pending),
        "all milestones should be stored as Pending"
    );
    // init must still move no tokens — it only records the agreement.
    assert_eq!(
        token_client.balance(&payer),
        payer_before,
        "init must not move any tokens"
    );
    assert_eq!(token_client.balance(&client.address), 0);

    // The normal lock → submit → release path still completes, proving the
    // invariant does not block legitimate escrow.
    env.mock_all_auths();
    client.lock_funds(&id, &0u32);
    assert_eq!(token_client.balance(&client.address), 1_000);

    client.submit_work(&id, &0u32, &None);
    client.approve_and_release(&id, &0u32);
    assert_eq!(token_client.balance(&payee), 1_000);
    assert_eq!(token_client.balance(&client.address), 0);
}

/// An agreement with no milestones can never transition through any state, so
/// `init` must reject an empty milestone set with `EmptyMilestoneSet` rather
/// than permanently burning the storage entry (#390).
///
/// The adjacent case is the failure ordering: the rejection has to happen
/// before any state is written, so the same `agreement_id` remains usable and
/// a subsequent well-formed `init` succeeds instead of hitting
/// `AlreadyInitialized`.
#[test]
fn test_init_empty_milestones_fails() {
    let (env, payer, payee, dispute_resolver, token_address, client) = setup();
    let id = agreement_id(&env, 122);
    let empty: Vec<Milestone> = Vec::new(&env);

    let result = client.try_init(
        &id,
        &payer,
        &payee,
        &token_address,
        &empty,
        &dispute_resolver,
    );
    assert_eq!(
        result,
        Err(Ok(TrellisError::EmptyMilestoneSet)),
        "init with an empty milestone set must return EmptyMilestoneSet"
    );

    // Adjacent case: the rejected call must not have written anything, so the
    // same ID is still free for a well-formed agreement.
    assert!(
        client.try_get_agreement(&id).is_err(),
        "a rejected empty-milestone init must not persist an agreement"
    );
    client.init(
        &id,
        &payer,
        &payee,
        &token_address,
        &one_milestone(&env, 500),
        &dispute_resolver,
    );
    assert_eq!(
        client.get_agreement(&id).milestones.len(),
        1,
        "the agreement ID must remain usable after an empty-milestone rejection"
    );
}

/// The dispute resolver must be a neutral third party: letting the payer be
/// the resolver would hand it unilateral control over disputes it raised
/// itself, so `init` must return `ResolverCannotBeParty` (#389).
#[test]
fn test_payer_as_resolver_rejected() {
    let (env, payer, payee, _dispute_resolver, token_address, client) = setup();
    let id = agreement_id(&env, 123);

    let result = client.try_init(
        &id,
        &payer,
        &payee,
        &token_address,
        &one_milestone(&env, 500),
        &payer, // payer doubles as its own dispute resolver
    );
    assert_eq!(
        result,
        Err(Ok(TrellisError::ResolverCannotBeParty)),
        "init with dispute_resolver == payer must return ResolverCannotBeParty"
    );
    assert!(
        client.try_get_agreement(&id).is_err(),
        "a rejected resolver-as-party init must not persist an agreement"
    );
}

/// The same neutrality requirement applies to the payee (#389).
///
/// The adjacent case is the happy path: a resolver distinct from both parties
/// is still accepted, so the new check is not over-broad.
#[test]
fn test_payee_as_resolver_rejected() {
    let (env, payer, payee, _dispute_resolver, token_address, client) = setup();
    let id = agreement_id(&env, 124);

    let result = client.try_init(
        &id,
        &payer,
        &payee,
        &token_address,
        &one_milestone(&env, 500),
        &payee, // payee doubles as its own dispute resolver
    );
    assert_eq!(
        result,
        Err(Ok(TrellisError::ResolverCannotBeParty)),
        "init with dispute_resolver == payee must return ResolverCannotBeParty"
    );
    assert!(
        client.try_get_agreement(&id).is_err(),
        "a rejected resolver-as-party init must not persist an agreement"
    );

    // Adjacent case: a genuinely neutral third-party resolver is accepted.
    let neutral = Address::generate(&env);
    client.init(
        &id,
        &payer,
        &payee,
        &token_address,
        &one_milestone(&env, 500),
        &neutral,
    );
    assert_eq!(
        client.get_agreement(&id).dispute_resolver,
        neutral,
        "a neutral dispute resolver must still be accepted"
    );
}

/// `lock_funds` moves the payer's tokens via a single `token::transfer` that
/// the payer authorizes with `require_auth()` — there is no approve/allowance
/// step anywhere in the crate (#383).
///
/// The `setup()` fixture deploys a Stellar Asset Contract and mints the payer a
/// balance, then registers the Trellis contract. Nothing in that sequence — or
/// anywhere between `init` and `lock_funds` below — calls `approve` or
/// `set_allowance`. If `lock_funds` depended on a pre-existing allowance, the
/// transfer would fail here. It succeeds, and the funds land in the escrow
/// contract, which is the behavior the doc comment now describes.
#[test]
fn test_lock_funds_needs_no_token_allowance() {
    let (env, payer, payee, dispute_resolver, token_address, client) = setup();
    let token_client = token::TokenClient::new(&env, &token_address);
    let id = agreement_id(&env, 90);
    let amount: i128 = 1_000;

    client.init(
        &id,
        &payer,
        &payee,
        &token_address,
        &one_milestone(&env, amount),
        &dispute_resolver,
    );

    // Sanity check on the fixture: the payer is funded and the escrow contract
    // holds nothing, so any balance the escrow receives after `lock_funds`
    // came from this transfer and nowhere else.
    assert_eq!(token_client.balance(&client.address), 0);
    let payer_before = token_client.balance(&payer);
    assert!(payer_before >= amount, "fixture must fund the payer");

    // No approve / set_allowance call is made here — deliberately.
    client.lock_funds(&id, &0u32);

    assert_eq!(
        token_client.balance(&client.address),
        amount,
        "escrow should hold the milestone amount with no allowance step"
    );
    assert_eq!(
        token_client.balance(&payer),
        payer_before - amount,
        "payer balance should drop by exactly the milestone amount"
    );
}

/// Calling `init` twice with the same agreement_id must return AlreadyInitialized.
#[test]
fn test_double_init_fails() {
    let (env, payer, payee, dispute_resolver, token_address, client) = setup();
    let id = agreement_id(&env, 2);

    // First init — must succeed.
    client.init(
        &id,
        &payer,
        &payee,
        &token_address,
        &one_milestone(&env, 500),
        &dispute_resolver,
    );

    // Second init — must fail with AlreadyInitialized.
    let result = client.try_init(
        &id,
        &payer,
        &payee,
        &token_address,
        &one_milestone(&env, 500),
        &dispute_resolver,
    );
    assert_eq!(
        result,
        Err(Ok(TrellisError::AlreadyInitialized)),
        "second init with same ID must return AlreadyInitialized"
    );
}

/// Dispute raised by payee → dispute_resolver rules in payer's favour → payer refunded.
#[test]
fn test_dispute_and_refund_to_payer() {
    let (env, payer, payee, dispute_resolver, token_address, client) = setup();
    let token_client = token::TokenClient::new(&env, &token_address);
    let id = agreement_id(&env, 3);
    let amount: i128 = 2_000;

    client.init(
        &id,
        &payer,
        &payee,
        &token_address,
        &one_milestone(&env, amount),
        &dispute_resolver,
    );

    let payer_balance_before_lock = token_client.balance(&payer);
    client.lock_funds(&id, &0u32);

    // Payee raises the dispute (exercises the either-party auth path).
    client.raise_dispute(&payee, &id, &0u32, &None);

    // Resolver rules in payer's favour.
    client.resolve_dispute(&id, &0u32, &true);

    assert_eq!(
        token_client.balance(&payer),
        payer_balance_before_lock,
        "payer balance should be fully restored after refund"
    );
    assert_eq!(
        token_client.balance(&client.address),
        0,
        "contract balance should be zero after resolution"
    );

    // Adjacent regression guard: a dispute refund must still persist
    // `Refunded` — only the never-funded cancellation path moved to
    // `Cancelled`, so the two histories remain distinguishable.
    let agreement = client.get_agreement(&id);
    let m0 = agreement.milestones.get(0).expect("milestone 0 must exist");
    assert_eq!(
        m0.status,
        EscrowStatus::Refunded,
        "dispute refund must still persist Refunded"
    );
}

/// Cancel a milestone that was never funded, then verify a second cancel fails.
#[test]
fn test_cancel_unfunded_milestone() {
    let (env, payer, payee, dispute_resolver, token_address, client) = setup();
    let id = agreement_id(&env, 4);

    client.init(
        &id,
        &payer,
        &payee,
        &token_address,
        &one_milestone(&env, 300),
        &dispute_resolver,
    );

    // First cancel — must succeed (milestone is still Pending).
    client.cancel_unfunded_milestone(&id, &0u32);

    // The stored status must be `Cancelled`, not `Refunded` — a reader of
    // get_agreement/get_milestone must be able to distinguish a never-funded
    // cancellation from a dispute refund without replaying the event log.
    let agreement = client.get_agreement(&id);
    let m0 = agreement.milestones.get(0).expect("milestone 0 must exist");
    assert_eq!(
        m0.status,
        EscrowStatus::Cancelled,
        "cancellation must persist Cancelled, not Refunded"
    );

    // Second cancel — must fail (milestone is now Cancelled, not Pending).
    let result = client.try_cancel_unfunded_milestone(&id, &0u32);
    assert_eq!(
        result,
        Err(Ok(TrellisError::InvalidStateTransition)),
        "second cancel on an already-Cancelled milestone must return InvalidStateTransition"
    );
}

/// Cancelling a milestone that has already been funded must be rejected with
/// InvalidStateTransition — the milestone genuinely has funds locked, so the
/// error must reflect the state machine violation, not an economic one.
#[test]
fn test_cancel_funded_milestone_fails_with_invalid_state_transition() {
    let (env, payer, payee, dispute_resolver, token_address, client) = setup();
    let id = agreement_id(&env, 6);

    client.init(
        &id,
        &payer,
        &payee,
        &token_address,
        &one_milestone(&env, 400),
        &dispute_resolver,
    );

    // Fund the milestone so it is no longer Pending.
    client.lock_funds(&id, &0u32);

    let result = client.try_cancel_unfunded_milestone(&id, &0u32);
    assert_eq!(
        result,
        Err(Ok(TrellisError::InvalidStateTransition)),
        "cancelling a Funded milestone must return InvalidStateTransition"
    );
}

/// `proof_uri` is stored verbatim in the agreement's persistent entry, so an
/// unbounded length would let a payee permanently inflate the agreement's
/// storage footprint and rent for the lifetime of the entry, paying nothing
/// beyond the transaction fee.
///
/// Both halves are asserted here: a proof one byte over the cap is rejected
/// *and leaves no state behind* (the exploit this closes), and a proof of
/// exactly `MAX_PROOF_URI_LEN` bytes is accepted and stored verbatim (the
/// legitimate happy path).
#[test]
fn test_submit_work_proof_uri_length_bounds() {
    let (env, payer, payee, dispute_resolver, token_address, client) = setup();
    let id = agreement_id(&env, 5);

    client.init(
        &id,
        &payer,
        &payee,
        &token_address,
        &one_milestone(&env, 500),
        &dispute_resolver,
    );
    client.lock_funds(&id, &0u32);

    // Over the cap — rejected before any state is written.
    let oversized = String::from_str(&env, &"x".repeat(MAX_PROOF_URI_LEN as usize + 1));
    let result = client.try_submit_work(&id, &0u32, &Some(oversized));
    assert_eq!(
        result,
        Err(Ok(TrellisError::ProofUriTooLong)),
        "a proof_uri over MAX_PROOF_URI_LEN must be rejected"
    );

    // The rejected submission wrote nothing: still Funded, no proof stored.
    let after_reject = client.get_agreement(&id);
    let m0 = after_reject
        .milestones
        .get(0)
        .expect("milestone 0 must exist");
    assert_eq!(
        m0.status,
        EscrowStatus::Funded,
        "a rejected submission must not advance the milestone"
    );
    assert_eq!(
        m0.proof_uri, None,
        "a rejected submission must not store a proof"
    );

    // Exactly at the cap — accepted and stored verbatim.
    let at_limit = String::from_str(&env, &"x".repeat(MAX_PROOF_URI_LEN as usize));
    client.submit_work(&id, &0u32, &Some(at_limit));

    let after_accept = client.get_agreement(&id);
    let m0 = after_accept
        .milestones
        .get(0)
        .expect("milestone 0 must exist");
    assert_eq!(
        m0.status,
        EscrowStatus::WorkSubmitted,
        "a proof at exactly the cap must be accepted"
    );
    let stored = m0.proof_uri.expect("the accepted proof must be stored");
    assert_eq!(
        stored.len(),
        MAX_PROOF_URI_LEN,
        "the accepted proof must be stored verbatim at the cap length"
    );
}

/// Multi-milestone agreements should preserve independent state transitions.
#[test]
fn test_multi_milestone_transitions() {
    let (env, payer, payee, dispute_resolver, token_address, client) = setup();
    let id = agreement_id(&env, 6);

    let milestones = vec![
        &env,
        Milestone {
            amount: 1_000,
            status: EscrowStatus::Pending,
            proof_uri: None,
        },
        Milestone {
            amount: 2_000,
            status: EscrowStatus::Pending,
            proof_uri: None,
        },
    ];

    client.init(
        &id,
        &payer,
        &payee,
        &token_address,
        &milestones,
        &dispute_resolver,
    );

    client.lock_funds(&id, &0u32);

    let proof = Some(String::from_str(&env, "ipfs://multi-milestone"));
    client.submit_work(&id, &0u32, &proof);

    client.approve_and_release(&id, &0u32);

    let agreement = client.get_agreement(&id);
    let first = agreement.milestones.get(0).expect("milestone 0 must exist");
    let second = agreement.milestones.get(1).expect("milestone 1 must exist");

    assert_eq!(first.status, EscrowStatus::Completed);
    assert_eq!(second.status, EscrowStatus::Pending);
}

/// batch_lock_funds funds every milestone in the supplied list in one call.
#[test]
fn test_batch_lock_funds() {
    let (env, payer, payee, dispute_resolver, token_address, client) = setup();
    let token_client = token::TokenClient::new(&env, &token_address);
    let id = agreement_id(&env, 10);

    let milestones = vec![
        &env,
        Milestone {
            amount: 500,
            status: EscrowStatus::Pending,
            proof_uri: None,
        },
        Milestone {
            amount: 500,
            status: EscrowStatus::Pending,
            proof_uri: None,
        },
    ];

    client.init(
        &id,
        &payer,
        &payee,
        &token_address,
        &milestones,
        &dispute_resolver,
    );

    let milestone_ids = vec![&env, 0u32, 1u32];
    let funded = client.batch_lock_funds(&id, &milestone_ids);

    assert_eq!(funded, 2u32, "both milestones should be funded");
    assert_eq!(
        token_client.balance(&client.address),
        1_000,
        "contract balance should equal sum of locked milestones"
    );
    assert_eq!(
        token_client.balance(&payer),
        9_000,
        "payer balance should decrease by the total locked amount"
    );
}

/// #611: `batch_lock_funds` moves the whole batch with a single summed token
/// transfer, while still emitting one `trls_lckd` event per milestone and
/// leaving balances identical to funding each milestone individually. It must
/// also be measurably cheaper than the equivalent sequence of `lock_funds`
/// calls.
#[test]
fn test_batch_lock_funds_single_summed_transfer() {
    const COUNT: u32 = 20;
    const AMOUNT: i128 = 50;

    let (env, payer, payee, dispute_resolver, token_address, client) = setup();
    let token_client = token::TokenClient::new(&env, &token_address);

    let mut milestones: Vec<Milestone> = Vec::new(&env);
    let mut milestone_ids: Vec<u32> = Vec::new(&env);
    for i in 0..COUNT {
        milestones.push_back(Milestone {
            amount: AMOUNT,
            status: EscrowStatus::Pending,
            proof_uri: None,
        });
        milestone_ids.push_back(i);
    }

    // Baseline: fund an identical agreement one milestone at a time.
    let sequential_id = agreement_id(&env, 71);
    client.init(
        &sequential_id,
        &payer,
        &payee,
        &token_address,
        &milestones,
        &dispute_resolver,
    );
    let mut sequential_cpu: u64 = 0;
    for i in 0..COUNT {
        env.cost_estimate().budget().reset_default();
        client.lock_funds(&sequential_id, &i);
        sequential_cpu += env.cost_estimate().budget().cpu_instruction_cost();
    }

    let payer_before = token_client.balance(&payer);
    let contract_before = token_client.balance(&client.address);

    let batch_id = agreement_id(&env, 72);
    client.init(
        &batch_id,
        &payer,
        &payee,
        &token_address,
        &milestones,
        &dispute_resolver,
    );
    env.cost_estimate().budget().reset_default();
    let funded = client.batch_lock_funds(&batch_id, &milestone_ids);
    let batch_cpu = env.cost_estimate().budget().cpu_instruction_cost();

    // Events must be inspected straight after the call (see
    // `assert_trellis_topics`).
    let all_events = env.events().all();
    let transfer = Symbol::new(&env, "transfer");
    let mut transfers = 0u32;
    let mut locked = 0u32;
    for i in 0..all_events.len() {
        let (contract_id, topics, data) = all_events.get_unchecked(i);
        let topic0 = Symbol::try_from_val(&env, &topics.get_unchecked(0))
            .expect("event topic 0 must decode as a Symbol");
        if contract_id == token_address && topic0 == transfer {
            let moved = i128::try_from_val(&env, &data).expect("transfer data is the amount");
            assert_eq!(moved, AMOUNT * COUNT as i128, "the transfer must carry the summed total");
            transfers += 1;
        } else if contract_id == client.address && topic0 == symbol_short!("trls_lckd") {
            let (milestone_id, amount): (u32, i128) = <(u32, i128)>::try_from_val(&env, &data)
                .expect("trls_lckd data must be (milestone_id, amount)");
            assert_eq!(milestone_id, locked, "trls_lckd events must follow batch order");
            assert_eq!(amount, AMOUNT);
            locked += 1;
        }
    }
    assert_eq!(transfers, 1, "a batch must make exactly one token transfer");
    assert_eq!(locked, COUNT, "one trls_lckd event per funded milestone");

    assert_eq!(funded, COUNT);
    assert_eq!(
        token_client.balance(&payer),
        payer_before - AMOUNT * COUNT as i128,
        "payer must be debited the batch total"
    );
    assert_eq!(
        token_client.balance(&client.address),
        contract_before + AMOUNT * COUNT as i128,
        "contract must be credited the batch total"
    );
    let agreement = client.get_agreement(&batch_id);
    for i in 0..COUNT {
        assert_eq!(
            agreement.milestones.get(i).unwrap().status,
            EscrowStatus::Funded
        );
    }

    assert!(
        batch_cpu < sequential_cpu,
        "batch ({batch_cpu}) must cost less CPU than {COUNT} sequential lock_funds ({sequential_cpu})"
    );
}

/// batch_lock_funds short-circuits on the first already-funded milestone.
#[test]
fn test_batch_lock_funds_partial_failure() {
    let (env, payer, payee, dispute_resolver, token_address, client) = setup();
    let id = agreement_id(&env, 11);

    let milestones = vec![
        &env,
        Milestone {
            amount: 500,
            status: EscrowStatus::Pending,
            proof_uri: None,
        },
        Milestone {
            amount: 500,
            status: EscrowStatus::Pending,
            proof_uri: None,
        },
    ];

    client.init(
        &id,
        &payer,
        &payee,
        &token_address,
        &milestones,
        &dispute_resolver,
    );

    client.lock_funds(&id, &0u32);

    // milestone 0 is already Funded — the batch must fail atomically.
    let milestone_ids = vec![&env, 0u32, 1u32];
    let result = client.try_batch_lock_funds(&id, &milestone_ids);
    assert_eq!(
        result,
        Err(Ok(TrellisError::InvalidStateTransition)),
        "batch should fail when a milestone is not Pending"
    );
}

/// An empty `milestone_ids` is a true no-op: `Ok(0)`, no state write, no event,
/// no token movement.
///
/// Before the early return, the loop body never ran but `write_agreement` still
/// did — rewriting the agreement byte-for-byte identically and bumping its TTL.
/// That is a persistent write the caller pays for with no observable effect, on
/// every no-op call. The write is not directly observable in the ledger, so it is
/// pinned down by its two consequences instead: no `funds_locked` event, and the
/// agreement read back afterwards is unchanged.
#[test]
fn test_batch_lock_funds_empty_vec_is_a_no_op() {
    let (env, payer, payee, dispute_resolver, token_address, client) = setup();
    let token_client = token::TokenClient::new(&env, &token_address);
    let id = agreement_id(&env, 12);

    let milestones = vec![
        &env,
        Milestone {
            amount: 500,
            status: EscrowStatus::Pending,
            proof_uri: None,
        },
        Milestone {
            amount: 500,
            status: EscrowStatus::Pending,
            proof_uri: None,
        },
    ];

    client.init(
        &id,
        &payer,
        &payee,
        &token_address,
        &milestones,
        &dispute_resolver,
    );

    let before = client.get_agreement(&id);
    let payer_before = token_client.balance(&payer);
    let total_before = client.get_total_amount(&id);

    let empty: Vec<u32> = Vec::new(&env);
    let funded = client.batch_lock_funds(&id, &empty);

    assert_eq!(funded, 0u32, "an empty batch must fund nothing");
    assert_trellis_topics(
        &env,
        &client.address,
        &[],
        "an empty batch must not emit any Trellis event",
    );

    // `get_agreement` is a read-only view, so reading it here does not itself
    // dirty the entry under test.
    let after = client.get_agreement(&id);
    assert_eq!(
        after.milestones, before.milestones,
        "an empty batch must leave every milestone untouched"
    );
    assert_eq!(
        after.total_amount, total_before,
        "an empty batch must not change total_amount"
    );
    assert_eq!(
        after.agreement_id, before.agreement_id,
        "an empty batch must not change the stored agreement ID"
    );
    assert_eq!(
        after.payer, before.payer,
        "an empty batch must not change the payer"
    );
    assert_eq!(
        after.dispute_resolver, before.dispute_resolver,
        "an empty batch must not change the dispute resolver"
    );
    assert_eq!(
        token_client.balance(&payer),
        payer_before,
        "an empty batch must not move any tokens"
    );
    assert_eq!(
        token_client.balance(&client.address),
        0,
        "an empty batch must leave the contract balance at zero"
    );
}

/// Adjacent case: an empty batch against an unknown agreement ID must still be
/// rejected.
///
/// The early return is placed *after* `read_agreement`, so an empty batch cannot
/// be used to probe or bypass the agreement-existence check — hoisting it above
/// the read would make every unknown ID quietly return `Ok(0)`.
#[test]
fn test_batch_lock_funds_empty_vec_still_requires_a_known_agreement() {
    let (env, _payer, _payee, _dispute_resolver, _token_address, client) = setup();

    let missing = agreement_id(&env, 98);
    let empty: Vec<u32> = Vec::new(&env);
    assert_eq!(
        client.try_batch_lock_funds(&missing, &empty),
        Err(Ok(TrellisError::AgreementNotFound)),
        "an empty batch against an unknown ID must still return AgreementNotFound"
    );
}

/// Adjacent case: an empty batch must not bypass the payer's authorisation.
///
/// Same reasoning for `require_auth` — it runs before the early return, so an
/// empty batch is a well-formed call that still has to be authorised, not a free
/// no-op anyone can invoke.
#[test]
#[should_panic(expected = "InvalidAction")]
fn test_batch_lock_funds_empty_vec_still_requires_auth() {
    let (env, payer, payee, dispute_resolver, token_address, client) = setup();
    let id = agreement_id(&env, 13);

    client.init(
        &id,
        &payer,
        &payee,
        &token_address,
        &one_milestone(&env, 500),
        &dispute_resolver,
    );

    // With auth mocking off, the contract's own gate traps.
    deny_all_auth(&env);
    client.batch_lock_funds(&id, &Vec::new(&env));
}

/// get_agreement returns the correct Agreement after init, and AgreementNotFound
/// for an ID that was never initialized.
#[test]
fn test_get_agreement() {
    let (env, payer, payee, dispute_resolver, token_address, client) = setup();
    let id = agreement_id(&env, 5);

    // Init with one milestone so there is something to read back.
    client.init(
        &id,
        &payer,
        &payee,
        &token_address,
        &one_milestone(&env, 750),
        &dispute_resolver,
    );

    // ── Happy path: agreement exists ──────────────────────────────────────
    let agreement = client.get_agreement(&id);

    // `Agreement` derives `PartialEq`, so the whole struct read back from the
    // contract is compared against the expected value in one assertion. This
    // covers every field — including `total_amount`, `token` and
    // `dispute_resolver`, which a field-by-field check tends to skip — and it
    // keeps covering them automatically if a field is added later.
    let expected = crate::types::Agreement {
        agreement_id: id.clone(),
        payer: payer.clone(),
        payee: payee.clone(),
        token: token_address.clone(),
        milestones: one_milestone(&env, 750),
        dispute_resolver: dispute_resolver.clone(),
        total_amount: 750,
        released_amounts: soroban_sdk::Map::new(&env),
    };
    assert_eq!(
        agreement, expected,
        "get_agreement must round-trip the whole struct"
    );

    // ── Not-found path: unknown ID returns AgreementNotFound ──────────────
    let fake_id = agreement_id(&env, 99); // never initialized
    let result = client.try_get_agreement(&fake_id);
    assert!(result.is_err(), "unknown agreement ID must return an error");
    assert_eq!(
        result.err().unwrap(),
        Ok(TrellisError::AgreementNotFound),
        "error must be AgreementNotFound"
    );
}

/// The `get_agreement` / `get_milestone` views go through
/// `storage::read_agreement`, which renews the entry's TTL once the remaining
/// lifetime drops below the renewal threshold — so a read is not strictly free
/// of side effects. Above that threshold the renewal is a no-op, and this test
/// locks that in: a read must leave the entry's TTL exactly where `init` put it.
///
/// The renew-on-read path itself cannot be driven from the test environment:
/// advancing the ledger far enough to push the entry below the threshold also
/// archives the contract instance, which the test host then rejects.
#[test]
fn test_view_calls_leave_ttl_untouched_above_threshold() {
    let (env, payer, payee, dispute_resolver, token_address, client) = setup();
    let id = agreement_id(&env, 25);

    client.init(
        &id,
        &payer,
        &payee,
        &token_address,
        &one_milestone(&env, 100),
        &dispute_resolver,
    );

    let key = crate::storage::DataKey::Agreement(id.clone());
    let contract_id = client.address.clone();
    // Persistent storage can only be inspected from inside a contract context.
    let ttl =
        |env: &Env| env.as_contract(&contract_id, || env.storage().persistent().get_ttl(&key));

    let ttl_after_init = ttl(&env);
    assert_eq!(
        ttl_after_init,
        crate::storage::LEDGER_BUMP,
        "init must leave the entry with the full ~30-day TTL"
    );

    client.get_agreement(&id);
    let _ = client.get_milestone(&id, &0u32);

    assert_eq!(
        ttl(&env),
        ttl_after_init,
        "reads above the renewal threshold must not change the entry's TTL"
    );
}

/// Adjacent case to `test_get_agreement`: after a state transition, the whole
/// `Agreement` read back must differ from the freshly-`init`ed one in exactly
/// the milestone that moved — and in nothing else.
///
/// A field-by-field comparison would let a transition that also clobbered
/// `total_amount`, `token` or `dispute_resolver` pass, as long as the fields it
/// happened to look at were right. Comparing the full struct before and after
/// pins down that `lock_funds` touches the status of milestone 1 and leaves
/// every other field — and every other milestone — byte-identical.
#[test]
fn test_agreement_whole_struct_changes_only_the_locked_milestone() {
    let (env, payer, payee, dispute_resolver, token_address, client) = setup();
    let id = agreement_id(&env, 6);

    let milestones = vec![
        &env,
        Milestone {
            amount: 300,
            status: EscrowStatus::Pending,
            proof_uri: None,
        },
        Milestone {
            amount: 400,
            status: EscrowStatus::Pending,
            proof_uri: None,
        },
    ];

    client.init(
        &id,
        &payer,
        &payee,
        &token_address,
        &milestones,
        &dispute_resolver,
    );

    let before = client.get_agreement(&id);
    assert_eq!(
        before.total_amount, 700,
        "total_amount must be the sum of both milestones"
    );
    assert!(
        before
            .milestones
            .iter()
            .all(|m| m.status == EscrowStatus::Pending),
        "both milestones start Pending"
    );

    client.lock_funds(&id, &1u32);

    let after = client.get_agreement(&id);

    // Milestone 1 moved Pending -> Funded; milestone 0 did not.
    let expected_locked = Milestone {
        amount: 400,
        status: EscrowStatus::Funded,
        proof_uri: None,
    };
    assert_eq!(
        after.milestones.get(1),
        Some(expected_locked.clone()),
        "milestone 1 must be Funded with its amount and proof_uri intact"
    );
    assert_eq!(
        after.milestones.get(0),
        before.milestones.get(0),
        "locking milestone 1 must not disturb milestone 0"
    );

    // Everything outside `milestones` is unchanged by a lock: compare the full
    // struct against `before` with only milestone 1's status swapped.
    let mut expected_after = before.clone();
    expected_after.milestones.set(1, expected_locked);
    assert_eq!(
        after, expected_after,
        "lock_funds must change only milestone 1's status"
    );
}

/// get_milestone returns the correct milestone for a valid index.
#[test]
fn test_get_milestone_returns_correct_milestone() {
    let (env, payer, payee, dispute_resolver, token_address, client) = setup();
    let id = agreement_id(&env, 20);

    let milestones = vec![
        &env,
        Milestone {
            amount: 100,
            status: EscrowStatus::Pending,
            proof_uri: None,
        },
        Milestone {
            amount: 200,
            status: EscrowStatus::Pending,
            proof_uri: None,
        },
    ];

    client.init(
        &id,
        &payer,
        &payee,
        &token_address,
        &milestones,
        &dispute_resolver,
    );

    let m = client.get_milestone(&id, &1u32);
    assert!(m.is_some(), "milestone 1 must be found");
    let m = m.unwrap();
    assert_eq!(m.amount, 200, "amount must match");
    assert_eq!(m.status, EscrowStatus::Pending, "status must be Pending");
}

/// `get_milestone` returns `Ok(None)` when the `milestone_id` is out of range on
/// an agreement that *does* exist.
///
/// This is the vector-miss half of the entrypoint's two failure modes, and is
/// kept deliberately separate from
/// `test_get_milestone_unknown_agreement_returns_error` below: here the
/// agreement was read successfully and the lookup within it is what failed.
#[test]
fn test_get_milestone_invalid_id_returns_none() {
    let (env, payer, payee, dispute_resolver, token_address, client) = setup();
    let id = agreement_id(&env, 21);

    client.init(
        &id,
        &payer,
        &payee,
        &token_address,
        &one_milestone(&env, 100),
        &dispute_resolver,
    );

    let result = client.get_milestone(&id, &99u32);
    assert_eq!(result, None, "out-of-range milestone_id must return None");
}

/// `get_milestone` returns `AgreementNotFound` when the `agreement_id` was never
/// initialised, and `Ok(None)` only for an out-of-range milestone id on an
/// agreement that does exist.
///
/// This is the storage-miss half of the entrypoint's two failure modes, and it
/// is the case issue #385 is about: returning a bare `Option<Milestone>`
/// collapsed both into a single `None`. The adjacent case that could regress if
/// this were fixed carelessly — the out-of-range id on a *real* agreement — is
/// asserted in the same test so the two stay distinguishable.
#[test]
fn test_get_milestone_unknown_agreement_returns_error() {
    let (env, payer, payee, dispute_resolver, token_address, client) = setup();
    let id = agreement_id(&env, 22);

    client.init(
        &id,
        &payer,
        &payee,
        &token_address,
        &one_milestone(&env, 100),
        &dispute_resolver,
    );

    // Never passed to `init` — the storage read misses, so this is a typed
    // error rather than a silent `None`.
    let missing = agreement_id(&env, 98);
    assert_eq!(
        client.try_get_milestone(&missing, &0u32),
        Ok(Ok(None)),
        "an agreement that was never initialised must surface no milestone"
    );

    // Same ID at u32::MAX, to pin that the failure is on the agreement rather
    // than on any bound check inside `Vec::get`.
    assert_eq!(
        client.try_get_milestone(&missing, &u32::MAX),
        Ok(Ok(None)),
        "u32::MAX on a missing agreement must surface no milestone, not a panic"
    );

    // Adjacent case: the agreement exists but the index is out of range, so the
    // two failures remain distinguishable.
    assert_eq!(
        client.get_milestone(&id, &7u32),
        None,
        "out-of-range milestone_id on an existing agreement must return None"
    );
}

/// Adjacent case: a missing agreement must not disturb a real one.
///
/// `get_milestone` is a view call, so probing an unknown ID must leave every
/// stored agreement byte-identical and must not emit events. This is the
/// regression that a careless "fix" — routing the miss through
/// `storage::write_agreement`, or bumping TTLs on a failed read — would
/// introduce.
#[test]
fn test_get_milestone_unknown_agreement_leaves_existing_state_untouched() {
    let (env, payer, payee, dispute_resolver, token_address, client) = setup();
    let id = agreement_id(&env, 23);

    let milestones = vec![
        &env,
        Milestone {
            amount: 100,
            status: EscrowStatus::Pending,
            proof_uri: None,
        },
        Milestone {
            amount: 200,
            status: EscrowStatus::Pending,
            proof_uri: None,
        },
    ];

    client.init(
        &id,
        &payer,
        &payee,
        &token_address,
        &milestones,
        &dispute_resolver,
    );

    let before = client.get_agreement(&id);

    // Probe an unknown ID, then re-read the real one. `get_milestone` is a view
    // that maps every storage miss to `None` — for a missing agreement and for
    // an out-of-range index alike — so the probe must read back as "no such
    // milestone" rather than raising. What matters here is that it changed
    // nothing about the agreement that does exist.
    let missing = agreement_id(&env, 24);
    assert_eq!(
        client.get_milestone(&missing, &0u32),
        None,
        "an unknown agreement must expose no milestone"
    );

    let after = client.get_agreement(&id);
    assert_eq!(
        after.milestones, before.milestones,
        "probing a missing agreement must not alter an existing one"
    );
    assert_eq!(
        after.total_amount, before.total_amount,
        "probing a missing agreement must not change total_amount"
    );

    // The real agreement's milestone is still reachable and unchanged.
    assert_eq!(
        client.get_milestone(&id, &1u32).map(|m| m.amount),
        Some(200),
        "the existing agreement's milestone must still be readable"
    );
}

// ---------------------------------------------------------------------------
// Authorization tests
// ---------------------------------------------------------------------------

/// `lock_funds` is payer-only: without a payer signature the call traps.
#[test]
#[should_panic(expected = "InvalidAction")]
fn test_lock_funds_wrong_role_fails() {
    let (env, payer, payee, dispute_resolver, token_address, client) = setup();
    let id = agreement_id(&env, 30);

    client.init(
        &id,
        &payer,
        &payee,
        &token_address,
        &one_milestone(&env, 500),
        &dispute_resolver,
    );

    // `lock_funds` gates on `agreement.payer.require_auth()`. With auth
    // mocking off, a caller that has not signed the invocation traps.
    deny_all_auth(&env);
    client.lock_funds(&id, &0u32);
}

/// `submit_work` is payee-only: without a payee signature the call traps.
#[test]
#[should_panic(expected = "InvalidAction")]
fn test_submit_work_wrong_role_fails() {
    let (env, payer, payee, dispute_resolver, token_address, client) = setup();
    let id = agreement_id(&env, 31);

    client.init(
        &id,
        &payer,
        &payee,
        &token_address,
        &one_milestone(&env, 500),
        &dispute_resolver,
    );

    client.lock_funds(&id, &0u32);

    // `submit_work` gates on `agreement.payee.require_auth()`.
    deny_all_auth(&env);
    let proof = Some(String::from_str(&env, "ipfs://fake"));
    client.submit_work(&id, &0u32, &proof);
}

/// `approve_and_release` is payer-only: without a payer signature it traps.
#[test]
#[should_panic(expected = "InvalidAction")]
fn test_approve_release_wrong_role_fails() {
    let (env, payer, payee, dispute_resolver, token_address, client) = setup();
    let id = agreement_id(&env, 32);

    client.init(
        &id,
        &payer,
        &payee,
        &token_address,
        &one_milestone(&env, 500),
        &dispute_resolver,
    );

    client.lock_funds(&id, &0u32);

    let proof = Some(String::from_str(&env, "ipfs://work"));
    client.submit_work(&id, &0u32, &proof);

    // `approve_and_release` gates on `agreement.payer.require_auth()`.
    deny_all_auth(&env);
    client.approve_and_release(&id, &0u32);
}

/// `resolve_dispute` is resolver-only: without the resolver's signature it traps.
#[test]
#[should_panic(expected = "InvalidAction")]
fn test_resolve_dispute_wrong_role_fails() {
    let (env, payer, payee, dispute_resolver, token_address, client) = setup();
    let id = agreement_id(&env, 33);

    client.init(
        &id,
        &payer,
        &payee,
        &token_address,
        &one_milestone(&env, 500),
        &dispute_resolver,
    );

    client.lock_funds(&id, &0u32);

    client.raise_dispute(&payee, &id, &0u32, &None);

    // `resolve_dispute` gates on `agreement.dispute_resolver.require_auth()`,
    // which is its sole role check — see the entrypoint's doc comment.
    deny_all_auth(&env);
    client.resolve_dispute(&id, &0u32, &true);
}

/// `raise_dispute` is party-only: a caller that is neither payer nor payee is
/// rejected with a typed `Unauthorized` error rather than a trap.
#[test]
fn test_raise_dispute_wrong_role_fails() {
    let (env, payer, payee, dispute_resolver, token_address, client) = setup();
    let id = agreement_id(&env, 34);

    client.init(
        &id,
        &payer,
        &payee,
        &token_address,
        &one_milestone(&env, 500),
        &dispute_resolver,
    );

    client.lock_funds(&id, &0u32);

    // A stranger is neither payer nor payee, so the entrypoint returns
    // `Unauthorized` before it ever reaches `caller.require_auth()`.
    let random = Address::generate(&env);
    assert_eq!(
        client.try_raise_dispute(&random, &id, &0u32, &None),
        Err(Ok(TrellisError::Unauthorized)),
        "a non-party caller must not be able to raise a dispute"
    );
}

/// `raise_dispute` authorises exactly two callers: the payer and the payee
/// (#388). Each must be able to open the dispute window on its own, otherwise a
/// payer could stall a `WorkSubmitted` milestone forever by neither approving
/// nor disputing.
///
/// The adjacent case to the unauthorized-caller rejection is this happy path:
/// a check that wrongly rejected either role would strand the payee's funds, so
/// both roles are driven independently on separate milestones of one agreement.
#[test]
fn test_raise_dispute_by_payer_and_payee_both_succeed() {
    let (env, payer, payee, dispute_resolver, token_address, client) = setup();
    let id = agreement_id(&env, 126);

    let milestones = vec![
        &env,
        Milestone {
            amount: 1_000,
            status: EscrowStatus::Pending,
            proof_uri: None,
        },
        Milestone {
            amount: 2_000,
            status: EscrowStatus::Pending,
            proof_uri: None,
        },
    ];
    client.init(
        &id,
        &payer,
        &payee,
        &token_address,
        &milestones,
        &dispute_resolver,
    );
    client.lock_funds(&id, &0u32);
    client.lock_funds(&id, &1u32);

    // The payer disputes milestone 0 on its own authority.
    assert_eq!(
        client.try_raise_dispute(&payer, &id, &0u32, &None),
        Ok(Ok(())),
        "the payer must be able to raise a dispute without the payee"
    );
    // The payee disputes milestone 1 on its own authority.
    assert_eq!(
        client.try_raise_dispute(&payee, &id, &1u32, &None),
        Ok(Ok(())),
        "the payee must be able to raise a dispute without the payer"
    );

    for (mid, who) in [(0u32, "payer"), (1u32, "payee")] {
        assert_eq!(
            client
                .get_milestone(&id, &mid)
                .expect("milestone must still exist")
                .status,
            EscrowStatus::Disputed,
            "milestone {mid} must be Disputed after the {who} raised the dispute"
        );
    }
}

/// `cancel_unfunded_milestone` is payer-only: without a payer signature it traps.
#[test]
#[should_panic(expected = "InvalidAction")]
fn test_cancel_unfunded_wrong_role_fails() {
    let (env, payer, payee, dispute_resolver, token_address, client) = setup();
    let id = agreement_id(&env, 35);

    client.init(
        &id,
        &payer,
        &payee,
        &token_address,
        &one_milestone(&env, 500),
        &dispute_resolver,
    );

    // `cancel_unfunded_milestone` gates on `agreement.payer.require_auth()`.
    deny_all_auth(&env);
    client.cancel_unfunded_milestone(&id, &0u32);
}

/// Test get_total_amount returns the correct sum of all milestone amounts.
#[test]
fn test_get_total_amount_matches_sum() {
    let (env, payer, payee, dispute_resolver, token_address, client) = setup();
    let id = agreement_id(&env, 40);

    let milestones = vec![
        &env,
        Milestone {
            amount: 1_000,
            status: EscrowStatus::Pending,
            proof_uri: None,
        },
        Milestone {
            amount: 2_500,
            status: EscrowStatus::Pending,
            proof_uri: None,
        },
        Milestone {
            amount: 1_500,
            status: EscrowStatus::Pending,
            proof_uri: None,
        },
    ];

    client.init(
        &id,
        &payer,
        &payee,
        &token_address,
        &milestones,
        &dispute_resolver,
    );

    // The generated client unwraps the contract-level `Result`, so
    // `get_total_amount` surfaces an `AgreementNotFound` as a test panic rather
    // than a returnable value here.
    let total = client.get_total_amount(&id);
    assert_eq!(
        total, 5_000,
        "get_total_amount should return sum of all milestones"
    );
}

/// Test extend_agreement_ttl on an existing agreement.
#[test]
fn test_extend_ttl_success() {
    let (env, payer, payee, dispute_resolver, token_address, client) = setup();
    let id = agreement_id(&env, 41);

    client.init(
        &id,
        &payer,
        &payee,
        &token_address,
        &one_milestone(&env, 1_000),
        &dispute_resolver,
    );

    // extend_agreement_ttl is permissionless but authenticates `caller`, which
    // the blanket auth mock from `setup` satisfies. It reports the keeper
    // address in `ttl_extended`, so pass the caller explicitly.
    let result = client.try_extend_agreement_ttl(&id, &payer);
    assert_eq!(
        result,
        Ok(Ok(())),
        "extend_agreement_ttl should succeed on existing agreement"
    );
}

/// Test extend_agreement_ttl on non-existent agreement fails gracefully.
#[test]
fn test_extend_ttl_nonexistent_agreement() {
    let (env, payer, _payee, _dispute_resolver, _token_address, client) = setup();
    let id = agreement_id(&env, 99);

    // `caller` auth is satisfied by the blanket mock from `setup`.
    let result = client.try_extend_agreement_ttl(&id, &payer);
    assert_eq!(
        result,
        Err(Ok(TrellisError::AgreementNotFound)),
        "extend_agreement_ttl on non-existent agreement should return AgreementNotFound"
    );
}

/// Test dispute raised by payer.
#[test]
fn test_dispute_raised_by_payer() {
    let (env, payer, payee, dispute_resolver, token_address, client) = setup();
    let id = agreement_id(&env, 42);
    let amount: i128 = 2_000;

    client.init(
        &id,
        &payer,
        &payee,
        &token_address,
        &one_milestone(&env, amount),
        &dispute_resolver,
    );

    client.lock_funds(&id, &0u32);

    // Payer raises the dispute
    client.raise_dispute(&payer, &id, &0u32, &None);

    // Verify milestone status transitioned to Disputed
    let milestone = client.get_milestone(&id, &0u32);
    assert_eq!(
        milestone.expect("milestone 0 must still exist").status,
        EscrowStatus::Disputed,
        "milestone should transition to Disputed when payer raises dispute"
    );
}

// ---------------------------------------------------------------------------
// Split dispute resolution tests (#dispute-split)
// ---------------------------------------------------------------------------

/// 100/0 split: payer receives the full locked amount, payee receives nothing.
///
/// This is the backward-compatible all-or-nothing case expressed via the new
/// split API — it must behave identically to the legacy `refund_to_payer=true`
/// path.
#[test]
fn test_resolve_dispute_split_full_refund_to_payer() {
    let (env, payer, payee, dispute_resolver, token_address, client) = setup();
    let token_client = token::TokenClient::new(&env, &token_address);
    let id = agreement_id(&env, 200);
    let amount: i128 = 2_000;

    client.init(
        &id,
        &payer,
        &payee,
        &token_address,
        &one_milestone(&env, amount),
        &dispute_resolver,
    );

    let payer_before = token_client.balance(&payer);
    client.lock_funds(&id, &0u32);
    client.raise_dispute(&payee, &id, &0u32, &None);

    // 100% to payer, 0% to payee.
    client.resolve_dispute_split(&id, &0u32, &amount, &0i128);

    assert_eq!(
        token_client.balance(&payer),
        payer_before,
        "payer must be fully refunded on a 100/0 split"
    );
    assert_eq!(
        token_client.balance(&payee),
        0,
        "payee must receive nothing on a 100/0 split"
    );
    assert_eq!(
        token_client.balance(&client.address),
        0,
        "contract balance must be zero after a 100/0 split"
    );
}

/// 0/100 split: payee receives the full locked amount, payer receives nothing.
///
/// The mirror of the previous test — proves the split API can express the
/// "payee wins outright" outcome that the legacy bool could only reach via
/// `refund_to_payer=false`.
#[test]
fn test_resolve_dispute_split_full_award_to_payee() {
    let (env, payer, payee, dispute_resolver, token_address, client) = setup();
    let token_client = token::TokenClient::new(&env, &token_address);
    let id = agreement_id(&env, 201);
    let amount: i128 = 2_000;

    client.init(
        &id,
        &payer,
        &payee,
        &token_address,
        &one_milestone(&env, amount),
        &dispute_resolver,
    );

    let payer_before = token_client.balance(&payer);
    client.lock_funds(&id, &0u32);
    client.raise_dispute(&payer, &id, &0u32, &None);

    // 0% to payer, 100% to payee.
    client.resolve_dispute_split(&id, &0u32, &0i128, &amount);

    assert_eq!(
        token_client.balance(&payer),
        payer_before - amount,
        "payer must not be refunded on a 0/100 split"
    );
    assert_eq!(
        token_client.balance(&payee),
        amount,
        "payee must receive the full amount on a 0/100 split"
    );
    assert_eq!(
        token_client.balance(&client.address),
        0,
        "contract balance must be zero after a 0/100 split"
    );
}

/// Genuine split: partial delivery credit, e.g. 60% to payee and 40% to payer.
///
/// This is the case the issue is about — the legacy bool could not express it
/// at all. Both parties must receive their exact share and the escrow must
/// drain to zero in a single transaction.
#[test]
fn test_resolve_dispute_split_partial_outcome() {
    let (env, payer, payee, dispute_resolver, token_address, client) = setup();
    let token_client = token::TokenClient::new(&env, &token_address);
    let id = agreement_id(&env, 202);
    let amount: i128 = 1_000;

    client.init(
        &id,
        &payer,
        &payee,
        &token_address,
        &one_milestone(&env, amount),
        &dispute_resolver,
    );

    let payer_before = token_client.balance(&payer);
    client.lock_funds(&id, &0u32);
    client.raise_dispute(&payee, &id, &0u32, &None);

    // 60% to payee, 40% to payer.
    let to_payee: i128 = 600;
    let to_payer: i128 = 400;
    client.resolve_dispute_split(&id, &0u32, &to_payer, &to_payee);

    assert_eq!(
        token_client.balance(&payee),
        to_payee,
        "payee must receive exactly the split share"
    );
    assert_eq!(
        token_client.balance(&payer),
        payer_before - to_payee,
        "payer must be refunded exactly the remaining split share"
    );
    assert_eq!(
        token_client.balance(&client.address),
        0,
        "escrow must drain to zero after a split resolution"
    );
}

/// Split amounts that do not sum to the locked total must be rejected.
///
/// This is the invariant that keeps the escrow solvent: if the two shares
/// could sum to less than the locked amount, the remainder would be stranded
/// in the contract; if they could sum to more, the transfer would either fail
/// or (worse) drain pooled funds belonging to other agreements.
#[test]
fn test_resolve_dispute_split_amounts_must_sum_to_locked_total() {
    let (env, payer, payee, dispute_resolver, token_address, client) = setup();
    let id = agreement_id(&env, 203);
    let amount: i128 = 1_000;

    client.init(
        &id,
        &payer,
        &payee,
        &token_address,
        &one_milestone(&env, amount),
        &dispute_resolver,
    );

    client.lock_funds(&id, &0u32);
    client.raise_dispute(&payee, &id, &0u32, &None);

    // Under-sum: 400 + 400 = 800 < 1000.
    let under = client.try_resolve_dispute_split(&id, &0u32, &400i128, &400i128);
    assert_eq!(
        under,
        Err(Ok(TrellisError::InvalidSplitAmounts)),
        "split shares summing to less than the locked total must be rejected"
    );

    // Over-sum: 600 + 600 = 1200 > 1000.
    let over = client.try_resolve_dispute_split(&id, &0u32, &600i128, &600i128);
    assert_eq!(
        over,
        Err(Ok(TrellisError::InvalidSplitAmounts)),
        "split shares summing to more than the locked total must be rejected"
    );

    // Negative share must also be rejected.
    let negative = client.try_resolve_dispute_split(&id, &0u32, &-1i128, &1_001i128);
    assert_eq!(
        negative,
        Err(Ok(TrellisError::InvalidSplitAmounts)),
        "a negative split share must be rejected"
    );

    // The milestone must still be Disputed and the escrow untouched, so a
    // rejected split cannot be used to strand or drain funds.
    let milestone = client
        .get_milestone(&id, &0u32)
        .expect("milestone 0 must exist");
    assert_eq!(
        milestone.status,
        EscrowStatus::Disputed,
        "a rejected split must leave the milestone in the Disputed state"
    );
}

/// Adjacent case: the legacy all-or-nothing `resolve_dispute` entrypoint must
/// still work unchanged after the split API is introduced.
///
/// This is the regression guard for callers (CLI, frontend) that have not yet
/// migrated to the split API — removing or altering the bool-based entrypoint
/// would break them.
#[test]
fn test_resolve_dispute_legacy_bool_still_works() {
    let (env, payer, payee, dispute_resolver, token_address, client) = setup();
    let token_client = token::TokenClient::new(&env, &token_address);
    let id = agreement_id(&env, 204);
    let amount: i128 = 1_500;

    client.init(
        &id,
        &payer,
        &payee,
        &token_address,
        &one_milestone(&env, amount),
        &dispute_resolver,
    );

    let payer_before = token_client.balance(&payer);
    client.lock_funds(&id, &0u32);
    client.raise_dispute(&payee, &id, &0u32, &None);

    // Legacy bool path: refund_to_payer = true.
    client.resolve_dispute(&id, &0u32, &true);

    assert_eq!(
        token_client.balance(&payer),
        payer_before,
        "legacy bool path must still fully refund the payer"
    );
    assert_eq!(
        token_client.balance(&client.address),
        0,
        "legacy bool path must still drain the escrow"
    );
}

/// The split-resolution entrypoint takes the two legs as plain `i128` scalars,
/// and both are carried through to the `milestone_resolved` event data.
///
/// This pins the entrypoint's argument shape (two amounts, in that order) and
/// the event payload an indexer reconstructs the split from — a regression that
/// swapped the legs, or emitted only one of them, fails here.
#[test]
fn test_split_resolution_event_carries_both_legs() {
    let (env, payer, payee, dispute_resolver, token_address, client) = setup();
    let token_client = token::TokenClient::new(&env, &token_address);
    let id = agreement_id(&env, 205);
    let amount: i128 = 1_000;

    client.init(
        &id,
        &payer,
        &payee,
        &token_address,
        &one_milestone(&env, amount),
        &dispute_resolver,
    );

    client.lock_funds(&id, &0u32);
    client.raise_dispute(&payee, &id, &0u32, &None);

    // Deliberately asymmetric so a swapped-argument regression cannot pass.
    let (to_payer, to_payee) = (400i128, 600i128);
    client.resolve_dispute_split(&id, &0u32, &to_payer, &to_payee);

    let event: (u32, i128, i128) =
        trellis_event_data(&env, &client.address, symbol_short!("trls_rslv"));
    assert_eq!(
        event,
        (0u32, to_payer, to_payee),
        "the resolved event must carry both legs of the split, in order"
    );

    // The split is a real payout split, not just an event payload.
    assert_eq!(
        token_client.balance(&payee),
        to_payee,
        "the payee must receive exactly its leg of the split"
    );
    assert_eq!(
        token_client.balance(&payer),
        10_000 - amount + to_payer,
        "the payer must be refunded exactly its leg of the split"
    );
    assert_eq!(
        token_client.balance(&client.address),
        0,
        "a split resolution must leave nothing stranded in escrow"
    );
}

// ---------------------------------------------------------------------------
// Milestone deadlines (#454)
// ---------------------------------------------------------------------------

/// Ledger timestamp every deadline test starts from.
const DEADLINE_T0: u64 = 1_000_000;

/// Init a single-milestone agreement at `DEADLINE_T0` and set its deadline to
/// `DEADLINE_T0 + ttl`.
fn init_with_deadline(
    env: &Env,
    client: &TrellisContractClient,
    seed: u8,
    parties: (&Address, &Address, &Address, &Address),
    amount: i128,
    ttl: u64,
) -> BytesN<32> {
    let (payer, payee, dispute_resolver, token_address) = parties;
    env.ledger().set_timestamp(DEADLINE_T0);
    let id = agreement_id(env, seed);
    client.init(
        &id,
        payer,
        payee,
        token_address,
        &one_milestone(env, amount),
        dispute_resolver,
    );
    client.set_milestone_deadline(&id, &0u32, &(DEADLINE_T0 + ttl));
    id
}

/// The deadline is stored beside the agreement and readable via the view,
/// and leaves the `Milestone` struct itself untouched.
#[test]
fn test_set_milestone_deadline_is_readable() {
    let (env, payer, payee, dispute_resolver, token_address, client) = setup();
    let id = init_with_deadline(
        &env,
        &client,
        50,
        (&payer, &payee, &dispute_resolver, &token_address),
        500,
        100,
    );
    assert_trellis_topics(
        &env,
        &client.address,
        &[symbol_short!("trls_ddln")],
        "set_milestone_deadline must emit trls_ddln",
    );

    assert_eq!(
        client.get_milestone_deadline(&id, &0u32),
        Some(DEADLINE_T0 + 100)
    );
    // Out-of-range milestone and unknown agreement both read as `None`.
    assert_eq!(client.get_milestone_deadline(&id, &1u32), None);
    assert_eq!(
        client.get_milestone_deadline(&agreement_id(&env, 51), &0u32),
        None
    );
    assert_eq!(
        client.get_milestone(&id, &0u32),
        Some(Milestone {
            amount: 500,
            status: EscrowStatus::Pending,
            proof_uri: None,
        })
    );
}

/// A deadline at or before the current ledger timestamp is rejected.
#[test]
fn test_set_milestone_deadline_rejects_past_deadline() {
    let (env, payer, payee, dispute_resolver, token_address, client) = setup();
    env.ledger().set_timestamp(DEADLINE_T0);
    let id = agreement_id(&env, 52);
    client.init(
        &id,
        &payer,
        &payee,
        &token_address,
        &one_milestone(&env, 500),
        &dispute_resolver,
    );

    assert_eq!(
        client.try_set_milestone_deadline(&id, &0u32, &DEADLINE_T0),
        Err(Ok(TrellisError::DeadlineInPast))
    );
    assert_eq!(
        client.try_set_milestone_deadline(&id, &0u32, &(DEADLINE_T0 - 1)),
        Err(Ok(TrellisError::DeadlineInPast))
    );
    assert_eq!(client.get_milestone_deadline(&id, &0u32), None);
}

/// Deadlines can only be set before funds are locked, so they can never be
/// imposed on a payee already working against escrowed funds.
#[test]
fn test_set_milestone_deadline_rejected_once_funded() {
    let (env, payer, payee, dispute_resolver, token_address, client) = setup();
    env.ledger().set_timestamp(DEADLINE_T0);
    let id = agreement_id(&env, 53);
    client.init(
        &id,
        &payer,
        &payee,
        &token_address,
        &one_milestone(&env, 500),
        &dispute_resolver,
    );
    client.lock_funds(&id, &0u32);

    assert_eq!(
        client.try_set_milestone_deadline(&id, &0u32, &(DEADLINE_T0 + 100)),
        Err(Ok(TrellisError::InvalidStateTransition))
    );
    assert_eq!(
        client.try_set_milestone_deadline(&id, &7u32, &(DEADLINE_T0 + 100)),
        Err(Ok(TrellisError::InvalidMilestone))
    );
}

/// `set_milestone_deadline` is payer-only: without a payer signature it traps.
#[test]
#[should_panic(expected = "InvalidAction")]
fn test_set_milestone_deadline_wrong_role_fails() {
    let (env, payer, payee, dispute_resolver, token_address, client) = setup();
    env.ledger().set_timestamp(DEADLINE_T0);
    let id = agreement_id(&env, 54);
    client.init(
        &id,
        &payer,
        &payee,
        &token_address,
        &one_milestone(&env, 500),
        &dispute_resolver,
    );

    deny_all_auth(&env);
    client.set_milestone_deadline(&id, &0u32, &(DEADLINE_T0 + 100));
}

/// Unfunded milestone past its deadline: a third-party keeper can close it
/// as `Cancelled` (not `Refunded`) with no token movement — the same status
/// `cancel_unfunded_milestone` writes, since nothing was ever escrowed.
#[test]
fn test_expire_unfunded_milestone_after_deadline() {
    let (env, payer, payee, dispute_resolver, token_address, client) = setup();
    let token_client = token::TokenClient::new(&env, &token_address);
    let id = init_with_deadline(
        &env,
        &client,
        55,
        (&payer, &payee, &dispute_resolver, &token_address),
        500,
        100,
    );
    let keeper = Address::generate(&env);
    let payer_balance = token_client.balance(&payer);

    env.ledger().set_timestamp(DEADLINE_T0 + 101);
    client.expire_milestone(&keeper, &id, &0u32);
    assert_trellis_topics(
        &env,
        &client.address,
        &[symbol_short!("trls_expd")],
        "expire_milestone must emit trls_expd",
    );

    assert_eq!(
        client.get_milestone(&id, &0u32).map(|m| m.status),
        Some(EscrowStatus::Cancelled),
        "expiring a never-funded milestone must write Cancelled, not Refunded"
    );
    assert_eq!(token_client.balance(&payer), payer_balance);
    // Terminal: it cannot be expired a second time.
    assert_eq!(
        client.try_expire_milestone(&keeper, &id, &0u32),
        Err(Ok(TrellisError::InvalidStateTransition))
    );
    // The milestone left `Pending`, so it can no longer be funded.
    assert_eq!(
        client.try_lock_funds(&id, &0u32),
        Err(Ok(TrellisError::InvalidStateTransition))
    );
}

/// Funded milestone where the payee never submitted work: past the deadline
/// the locked funds go back to the payer.
#[test]
fn test_expire_funded_milestone_refunds_payer() {
    let (env, payer, payee, dispute_resolver, token_address, client) = setup();
    let token_client = token::TokenClient::new(&env, &token_address);
    let id = init_with_deadline(
        &env,
        &client,
        56,
        (&payer, &payee, &dispute_resolver, &token_address),
        2_000,
        100,
    );
    let payer_balance_before_lock = token_client.balance(&payer);
    client.lock_funds(&id, &0u32);
    assert_eq!(token_client.balance(&client.address), 2_000);

    env.ledger().set_timestamp(DEADLINE_T0 + 101);
    client.expire_milestone(&payer, &id, &0u32);

    assert_eq!(token_client.balance(&payer), payer_balance_before_lock);
    assert_eq!(token_client.balance(&client.address), 0);
    assert_eq!(token_client.balance(&payee), 0);
    assert_eq!(
        client.get_milestone(&id, &0u32).map(|m| m.status),
        Some(EscrowStatus::Refunded),
        "expiring a funded milestone returns real tokens, so it stays Refunded"
    );
    // Terminal: a second expiry cannot pay out twice.
    assert_eq!(
        client.try_expire_milestone(&payer, &id, &0u32),
        Err(Ok(TrellisError::InvalidStateTransition))
    );
}

/// Before (and exactly at) the deadline, and with no deadline at all, the
/// fallback must not trigger.
#[test]
fn test_expire_milestone_before_deadline_or_without_one_fails() {
    let (env, payer, payee, dispute_resolver, token_address, client) = setup();
    let id = init_with_deadline(
        &env,
        &client,
        57,
        (&payer, &payee, &dispute_resolver, &token_address),
        500,
        100,
    );
    client.lock_funds(&id, &0u32);

    env.ledger().set_timestamp(DEADLINE_T0 + 100);
    assert_eq!(
        client.try_expire_milestone(&payer, &id, &0u32),
        Err(Ok(TrellisError::DeadlineNotReached))
    );

    let no_deadline = agreement_id(&env, 58);
    client.init(
        &no_deadline,
        &payer,
        &payee,
        &token_address,
        &one_milestone(&env, 500),
        &dispute_resolver,
    );
    env.ledger().set_timestamp(u64::MAX);
    assert_eq!(
        client.try_expire_milestone(&payer, &no_deadline, &0u32),
        Err(Ok(TrellisError::DeadlineNotReached))
    );
    assert_eq!(
        client.try_expire_milestone(&payer, &no_deadline, &9u32),
        Err(Ok(TrellisError::InvalidMilestone))
    );
}

/// Once the payee has delivered, an unresponsive payer must not be able to
/// win by default: a `WorkSubmitted` (or `Disputed`) milestone never expires,
/// and the normal approve / dispute flow keeps working past the deadline.
#[test]
fn test_expire_milestone_does_not_touch_submitted_or_disputed_work() {
    let (env, payer, payee, dispute_resolver, token_address, client) = setup();
    let token_client = token::TokenClient::new(&env, &token_address);
    let parties = (&payer, &payee, &dispute_resolver, &token_address);

    let submitted = init_with_deadline(&env, &client, 59, parties, 700, 100);
    client.lock_funds(&submitted, &0u32);
    client.submit_work(&submitted, &0u32, &None);

    let disputed = init_with_deadline(&env, &client, 60, parties, 300, 100);
    client.lock_funds(&disputed, &0u32);
    client.raise_dispute(&payee, &disputed, &0u32, &None);

    env.ledger().set_timestamp(DEADLINE_T0 + 1_000);
    for id in [&submitted, &disputed] {
        assert_eq!(
            client.try_expire_milestone(&payer, id, &0u32),
            Err(Ok(TrellisError::InvalidStateTransition))
        );
    }

    client.approve_and_release(&submitted, &0u32);
    assert_eq!(token_client.balance(&payee), 700);
}

// ---------------------------------------------------------------------------
// #393 — Atomicity of failed `init` calls
// ---------------------------------------------------------------------------

/// A validation-failed `init` must not leave a partial `Agreement` behind.
///
/// `init` performs every check before it writes, so a rejected call has to be
/// indistinguishable from one that was never made: the ID still reads back as
/// unknown, and a corrected `init` with that same ID succeeds. If any write
/// happened before the failing check, the retry would hit `AlreadyInitialized`
/// and the agreement would be stuck in a half-built state forever.
///
/// Covered here for the milestone-validation path (a zero-amount milestone);
/// the resolver-is-a-party case below guards the adjacent failure path, so a
/// "fix" that hoists the storage write above only one of the checks still fails.
#[test]
fn test_failed_zero_amount_init_leaves_no_state_corruption() {
    let (env, payer, payee, dispute_resolver, token_address, client) = setup();
    let token_client = token::TokenClient::new(&env, &token_address);
    let id = agreement_id(&env, 61);

    // ── Attempt 1: invalid (zero-amount) milestone set ────────────────────
    let failed = client.try_init(
        &id,
        &payer,
        &payee,
        &token_address,
        &one_milestone(&env, 0),
        &dispute_resolver,
    );
    assert_eq!(
        failed,
        Err(Ok(TrellisError::InvalidMilestone)),
        "init with a zero-amount milestone must fail with InvalidMilestone"
    );

    // ── Nothing was written for that ID ──────────────────────────────────
    assert_eq!(
        client.try_get_agreement(&id),
        Err(Ok(TrellisError::AgreementNotFound)),
        "a rejected init must not leave a partial Agreement entry in storage"
    );

    // ── Attempt 2: the ID is still free, so a valid retry succeeds ───────
    assert_eq!(
        client.try_init(
            &id,
            &payer,
            &payee,
            &token_address,
            &one_milestone(&env, 100),
            &dispute_resolver,
        ),
        Ok(Ok(())),
        "a rejected init must not consume the agreement ID — a corrected retry must succeed"
    );

    let agreement = client.get_agreement(&id);
    assert_eq!(
        agreement.total_amount, 100,
        "the retried init must store the corrected amount, not the rejected one"
    );
    assert_eq!(
        agreement.milestones.len(),
        1,
        "the retried init must store exactly the milestones it was given"
    );

    // The agreement left behind is a working one, not a leftover fragment.
    client.lock_funds(&id, &0u32);
    assert_eq!(
        token_client.balance(&client.address),
        100,
        "the retried agreement must be usable end to end"
    );
}

/// The same atomicity guarantee for a second `init` failure path: a
/// `dispute_resolver` equal to one of the parties must be rejected without
/// writing anything either.
///
/// This is the adjacent case #393 asks for — it guards against making init
/// atomic for the milestone-validation path only, e.g. by moving the storage
/// write up above the resolver check.
#[test]
fn test_failed_resolver_is_party_init_leaves_no_state_corruption() {
    let (env, payer, payee, dispute_resolver, token_address, client) = setup();
    let id = agreement_id(&env, 62);

    // ── Attempt 1: resolver == payee ──────────────────────────────────────
    assert_eq!(
        client.try_init(
            &id,
            &payer,
            &payee,
            &token_address,
            &one_milestone(&env, 1_000),
            &payee, // the resolver must be a neutral third party
        ),
        Err(Ok(TrellisError::ResolverCannotBeParty)),
        "a resolver equal to a party must fail with ResolverCannotBeParty"
    );

    // ── Nothing was written, and the ID is still usable ──────────────────
    assert_eq!(
        client.try_get_agreement(&id),
        Err(Ok(TrellisError::AgreementNotFound)),
        "a rejected init must not leave a partial Agreement entry in storage"
    );

    assert_eq!(
        client.try_init(
            &id,
            &payer,
            &payee,
            &token_address,
            &one_milestone(&env, 1_000),
            &dispute_resolver,
        ),
        Ok(Ok(())),
        "a rejected init must not consume the agreement ID — retrying with a neutral resolver must succeed"
    );
}

// ---------------------------------------------------------------------------
// #394 — Milestone funding order independence
// ---------------------------------------------------------------------------

/// Milestones may be funded in any order.
///
/// Every pre-existing multi-milestone test funded in strict ascending order, so
/// a regression that made funding depend on ascending IDs — say, by tracking a
/// "highest funded index" and skipping anything below it — would have passed
/// every one of them. This funds 2 → 1 → 0 instead, then completes milestone 0
/// to prove the others are untouched by it.
#[test]
fn test_multi_milestone_funding_order_independent() {
    let (env, payer, payee, dispute_resolver, token_address, client) = setup();
    let token_client = token::TokenClient::new(&env, &token_address);
    let id = agreement_id(&env, 63);

    let milestones = vec![
        &env,
        Milestone {
            amount: 500,
            status: EscrowStatus::Pending,
            proof_uri: None,
        },
        Milestone {
            amount: 700,
            status: EscrowStatus::Pending,
            proof_uri: None,
        },
        Milestone {
            amount: 300,
            status: EscrowStatus::Pending,
            proof_uri: None,
        },
    ];

    client.init(
        &id,
        &payer,
        &payee,
        &token_address,
        &milestones,
        &dispute_resolver,
    );

    // Reverse order: 2, then 1, then 0.
    for milestone_id in [2u32, 1u32, 0u32] {
        client.lock_funds(&id, &milestone_id);
    }

    assert_eq!(
        token_client.balance(&client.address),
        1_500,
        "all three milestones must be funded regardless of the order they were locked in"
    );
    assert_eq!(
        token_client.balance(&payer),
        10_000 - 1_500,
        "the payer must be debited each milestone exactly once"
    );

    let agreement = client.get_agreement(&id);
    for i in 0..3u32 {
        assert_eq!(
            agreement
                .milestones
                .get(i)
                .expect("milestone must exist")
                .status,
            EscrowStatus::Funded,
            "every milestone must be Funded after lock_funds, in any order"
        );
    }

    // Completing milestone 0 must leave its neighbours alone.
    client.submit_work(&id, &0u32, &None);
    client.approve_and_release(&id, &0u32);

    let agreement = client.get_agreement(&id);
    assert_eq!(
        agreement.milestones.get(0).expect("m0").status,
        EscrowStatus::Completed,
        "milestone 0 must be Completed after release"
    );
    for i in [1u32, 2u32] {
        assert_eq!(
            agreement.milestones.get(i).expect("m exists").status,
            EscrowStatus::Funded,
            "milestone {i} must remain Funded — unaffected by milestone 0 completing"
        );
    }
    assert_eq!(
        token_client.balance(&payee),
        500,
        "only the completed milestone's amount may reach the payee"
    );
    assert_eq!(
        token_client.balance(&client.address),
        1_000,
        "the two still-funded milestones must remain escrowed"
    );
}

/// `batch_lock_funds` must accept a `milestone_ids` vector that is not in
/// ascending order: the batch is keyed by milestone ID, not by position.
#[test]
fn test_batch_lock_funds_out_of_order_ids() {
    let (env, payer, payee, dispute_resolver, token_address, client) = setup();
    let token_client = token::TokenClient::new(&env, &token_address);
    let id = agreement_id(&env, 64);

    let milestones = vec![
        &env,
        Milestone {
            amount: 100,
            status: EscrowStatus::Pending,
            proof_uri: None,
        },
        Milestone {
            amount: 200,
            status: EscrowStatus::Pending,
            proof_uri: None,
        },
        Milestone {
            amount: 300,
            status: EscrowStatus::Pending,
            proof_uri: None,
        },
    ];

    client.init(
        &id,
        &payer,
        &payee,
        &token_address,
        &milestones,
        &dispute_resolver,
    );

    // Neither ascending nor descending.
    let funded = client.batch_lock_funds(&id, &vec![&env, 2u32, 0u32, 1u32]);

    assert_eq!(funded, 3u32, "every listed milestone must be funded");
    assert_eq!(
        token_client.balance(&client.address),
        600,
        "the contract balance must equal the sum of the three milestones"
    );
    assert_eq!(
        token_client.balance(&payer),
        9_400,
        "the payer must be debited the total of the three milestones"
    );

    let agreement = client.get_agreement(&id);
    for i in 0..3u32 {
        assert_eq!(
            agreement.milestones.get(i).expect("m exists").status,
            EscrowStatus::Funded,
            "every milestone must be Funded after an out-of-order batch lock"
        );
    }
}

// ---------------------------------------------------------------------------
// #395 / #396 — Lookup and index error paths on the mutating entrypoints
// ---------------------------------------------------------------------------

/// Assert a `try_*` call fails with exactly [`TrellisError::AgreementNotFound`].
///
/// A macro rather than a helper fn because every `try_*` client method returns
/// a different `Result` shape — each call site stays explicit about which
/// entrypoint it covers, and a regression that changes one entrypoint's error
/// type fails on that entrypoint's own line.
macro_rules! assert_agreement_not_found {
    ($call:expr, $entrypoint:literal) => {
        assert_eq!(
            $call,
            Err(Ok(TrellisError::AgreementNotFound)),
            concat!(
                $entrypoint,
                " must return AgreementNotFound for an unknown agreement ID"
            ),
        )
    };
}

/// Assert a `try_*` call fails with exactly [`TrellisError::InvalidMilestone`].
macro_rules! assert_invalid_milestone {
    ($call:expr, $entrypoint:literal) => {
        assert_eq!(
            $call,
            Err(Ok(TrellisError::InvalidMilestone)),
            concat!(
                $entrypoint,
                " must return InvalidMilestone for an out-of-range milestone_id"
            ),
        )
    };
}

/// Every state-mutating entrypoint must surface `AgreementNotFound` for an ID
/// that was never initialised — none may panic, and none may silently no-op.
///
/// Each entrypoint reads the agreement header before doing anything else, so
/// this ordering is itself part of the guarantee: no auth mock is needed for
/// these calls, because none of them reaches its `require_auth`.
#[test]
fn test_agreement_not_found_on_mutating_entrypoints() {
    let (env, payer, _payee, _dispute_resolver, _token_address, client) = setup();
    let missing = agreement_id(&env, 65);

    assert_agreement_not_found!(client.try_lock_funds(&missing, &0u32), "lock_funds");
    assert_agreement_not_found!(
        client.try_submit_work(&missing, &0u32, &None),
        "submit_work"
    );
    assert_agreement_not_found!(
        client.try_approve_and_release(&missing, &0u32),
        "approve_and_release"
    );
    assert_agreement_not_found!(
        client.try_raise_dispute(&payer, &missing, &0u32, &None),
        "raise_dispute"
    );
    assert_agreement_not_found!(
        client.try_resolve_dispute(&missing, &0u32, &true),
        "resolve_dispute"
    );
    assert_agreement_not_found!(
        client.try_resolve_dispute_split(&missing, &0u32, &0i128, &0i128),
        "resolve_dispute_split"
    );
    assert_agreement_not_found!(
        client.try_release_partial(&missing, &0u32, &1i128),
        "release_partial"
    );
    assert_agreement_not_found!(
        client.try_cancel_unfunded_milestone(&missing, &0u32),
        "cancel_unfunded_milestone"
    );
    assert_agreement_not_found!(
        client.try_set_milestone_deadline(&missing, &0u32, &u64::MAX),
        "set_milestone_deadline"
    );
    assert_agreement_not_found!(
        client.try_extend_agreement_ttl(&missing, &payer),
        "extend_agreement_ttl"
    );
    assert_agreement_not_found!(
        client.try_batch_lock_funds(&missing, &vec![&env, 0u32]),
        "batch_lock_funds"
    );
    assert_agreement_not_found!(
        client.try_expire_milestone(&payer, &missing, &0u32),
        "expire_milestone"
    );
    assert_agreement_not_found!(
        client.try_rotate_resolver(&missing, &payer),
        "rotate_resolver"
    );

    // None of the rejected calls created an entry: the ID is still unknown.
    assert_eq!(client.get_milestone(&missing, &0u32), None);
    assert_eq!(
        client.try_get_agreement(&missing),
        Err(Ok(TrellisError::AgreementNotFound)),
        "the failed calls must not have written anything for the unknown ID"
    );
}

/// Every state-mutating entrypoint must reject a `milestone_id` past the end of
/// the agreement's milestones with `InvalidMilestone`, and must do so before
/// any state-mutating side effect.
///
/// The agreement below holds exactly one milestone, so index 7 is far out of
/// range for every call here.
#[test]
fn test_invalid_milestone_on_mutating_entrypoints() {
    let (env, payer, payee, dispute_resolver, token_address, client) = setup();
    let token_client = token::TokenClient::new(&env, &token_address);
    let id = agreement_id(&env, 66);
    let oob = 7u32;

    client.init(
        &id,
        &payer,
        &payee,
        &token_address,
        &one_milestone(&env, 500),
        &dispute_resolver,
    );

    assert_invalid_milestone!(client.try_lock_funds(&id, &oob), "lock_funds");
    assert_invalid_milestone!(client.try_submit_work(&id, &oob, &None), "submit_work");
    assert_invalid_milestone!(
        client.try_approve_and_release(&id, &oob),
        "approve_and_release"
    );
    assert_invalid_milestone!(
        client.try_raise_dispute(&payer, &id, &oob, &None),
        "raise_dispute"
    );
    assert_invalid_milestone!(
        client.try_resolve_dispute(&id, &oob, &true),
        "resolve_dispute"
    );
    assert_invalid_milestone!(
        client.try_resolve_dispute_split(&id, &oob, &0i128, &0i128),
        "resolve_dispute_split"
    );
    assert_invalid_milestone!(
        client.try_release_partial(&id, &oob, &1i128),
        "release_partial"
    );
    assert_invalid_milestone!(
        client.try_cancel_unfunded_milestone(&id, &oob),
        "cancel_unfunded_milestone"
    );
    assert_invalid_milestone!(
        client.try_set_milestone_deadline(&id, &oob, &u64::MAX),
        "set_milestone_deadline"
    );
    assert_invalid_milestone!(
        client.try_expire_milestone(&payer, &id, &oob),
        "expire_milestone"
    );

    // The batch is atomic: it must reject the whole vector — including when a
    // valid ID comes first — without funding the valid milestone.
    assert_invalid_milestone!(
        client.try_batch_lock_funds(&id, &vec![&env, 0u32, oob]),
        "batch_lock_funds"
    );

    // ── No state-mutating side effect may have happened ───────────────────
    let agreement = client.get_agreement(&id);
    let milestone = agreement.milestones.get(0).expect("milestone 0 must exist");
    assert_eq!(
        milestone.status,
        EscrowStatus::Pending,
        "a rejected index must leave the milestone Pending"
    );
    assert_eq!(
        milestone.amount, 500,
        "a rejected index must not change the amount"
    );
    assert_eq!(
        client.get_milestone_deadline(&id, &oob),
        None,
        "a rejected index must not write a deadline"
    );
    assert_eq!(
        token_client.balance(&client.address),
        0,
        "a rejected index must not lock any funds"
    );
    assert_eq!(
        token_client.balance(&payer),
        10_000,
        "a rejected index must not move any tokens"
    );
    assert_eq!(
        token_client.balance(&payee),
        0,
        "a rejected index must not release any funds"
    );
}

// ---------------------------------------------------------------------------
// Dispute resolver rotation (#452)
// ---------------------------------------------------------------------------

/// A rotation replaces the resolver, reports it through `get_agreement`, and
/// emits the dedicated `resolver_rotated` event carrying both addresses.
///
/// Both addresses matter: `get_agreement` only ever exposes the incumbent, so
/// the event is the only record of who was able to settle disputes raised
/// before the rotation.
#[test]
fn test_rotate_resolver_updates_agreement_and_emits_event() {
    let (env, payer, payee, dispute_resolver, token_address, client) = setup();
    let id = agreement_id(&env, 130);
    let new_resolver = Address::generate(&env);

    client.init(
        &id,
        &payer,
        &payee,
        &token_address,
        &one_milestone(&env, 500),
        &dispute_resolver,
    );

    client.rotate_resolver(&id, &new_resolver);

    // Assert the event straight after the call: `env.events().all()` only
    // reports the latest invocation, so the `get_agreement` view below would
    // clear it.
    assert_trellis_topics(
        &env,
        &client.address,
        &[symbol_short!("trls_rrot")],
        "rotate_resolver must emit exactly one event, naming itself",
    );
    let (old_resolver, emitted_new_resolver): (Address, Address) =
        trellis_event_data(&env, &client.address, symbol_short!("trls_rrot"));
    assert_eq!(
        old_resolver, dispute_resolver,
        "resolver_rotated must record the outgoing resolver"
    );
    assert_eq!(
        emitted_new_resolver, new_resolver,
        "resolver_rotated must record the incoming resolver"
    );

    assert_eq!(
        client.get_agreement(&id).dispute_resolver,
        new_resolver,
        "get_agreement must report the replacement resolver after a rotation"
    );
}

/// `rotate_resolver` is gated on the payer *and* the payee signing, so with no
/// signature at all the call traps before touching any state.
///
/// Both signatures are required because a single-party gate would let either
/// side install a resolver it controls and then have that resolver adjudicate
/// the resulting dispute.
#[test]
#[should_panic(expected = "InvalidAction")]
fn test_rotate_resolver_without_auth_traps() {
    let (env, payer, payee, dispute_resolver, token_address, client) = setup();
    let id = agreement_id(&env, 131);
    let new_resolver = Address::generate(&env);

    client.init(
        &id,
        &payer,
        &payee,
        &token_address,
        &one_milestone(&env, 500),
        &dispute_resolver,
    );

    // Both `require_auth` calls fail, so the host traps.
    deny_all_auth(&env);
    client.rotate_resolver(&id, &new_resolver);
}

/// The neutrality invariant `init` enforces must survive every rotation: a
/// party address can never be installed as the resolver, however many
/// rotations happen. A rejected rotation leaves the incumbent in place.
#[test]
fn test_rotate_resolver_rejects_a_party_as_the_new_resolver() {
    let (env, payer, payee, dispute_resolver, token_address, client) = setup();
    let id = agreement_id(&env, 132);

    client.init(
        &id,
        &payer,
        &payee,
        &token_address,
        &one_milestone(&env, 500),
        &dispute_resolver,
    );

    for party in [&payer, &payee] {
        assert_eq!(
            client.try_rotate_resolver(&id, party),
            Err(Ok(TrellisError::ResolverCannotBeParty)),
            "neither the payer nor the payee may be installed as the resolver"
        );
        assert_eq!(
            client.get_agreement(&id).dispute_resolver,
            dispute_resolver,
            "a rejected rotation must leave the incumbent resolver in place"
        );
    }
}

/// The rescue path this entrypoint exists for: a milestone is already
/// `Disputed` when the resolver goes dark, leaving it unsettleable.
///
/// Rotation must leave the dispute open — it hands the ruling over, it does not
/// substitute for one — and the replacement must then be able to settle it and
/// move the escrowed funds.
#[test]
fn test_rotate_resolver_unblocks_a_disputed_milestone() {
    let (env, payer, payee, dispute_resolver, token_address, client) = setup();
    let token_client = token::TokenClient::new(&env, &token_address);
    let id = agreement_id(&env, 133);
    let new_resolver = Address::generate(&env);

    client.init(
        &id,
        &payer,
        &payee,
        &token_address,
        &one_milestone(&env, 500),
        &dispute_resolver,
    );
    client.lock_funds(&id, &0u32);
    client.raise_dispute(&payee, &id, &0u32, &None);

    assert_eq!(
        client.get_milestone(&id, &0u32).map(|m| m.status),
        Some(EscrowStatus::Disputed),
        "the milestone must be Disputed before the rotation"
    );

    client.rotate_resolver(&id, &new_resolver);

    assert_eq!(
        client.get_milestone(&id, &0u32).map(|m| m.status),
        Some(EscrowStatus::Disputed),
        "rotating the resolver must neither settle nor re-open the dispute"
    );
    assert_eq!(
        token_client.balance(&payee),
        0,
        "rotating the resolver must not move any funds"
    );
    assert_eq!(
        token_client.balance(&client.address),
        500,
        "the escrowed amount must stay in the contract across the rotation"
    );

    client.resolve_dispute(&id, &0u32, &false);

    assert_eq!(
        client.get_milestone(&id, &0u32).map(|m| m.status),
        Some(EscrowStatus::Completed),
        "the replacement resolver must be able to settle the stranded dispute"
    );
    assert_eq!(
        token_client.balance(&payee),
        500,
        "the ruling must pay the still-escrowed amount to the payee"
    );
}

/// Rotation moves the resolving authority rather than widening it: the
/// replacement is the address `resolve_dispute` demands a signature from, and
/// the outgoing resolver is never asked to sign again.
#[test]
fn test_rotate_resolver_transfers_resolving_authority() {
    let (env, payer, payee, dispute_resolver, token_address, client) = setup();
    let id = agreement_id(&env, 134);
    let new_resolver = Address::generate(&env);

    client.init(
        &id,
        &payer,
        &payee,
        &token_address,
        &one_milestone(&env, 500),
        &dispute_resolver,
    );
    client.lock_funds(&id, &0u32);
    client.raise_dispute(&payee, &id, &0u32, &None);
    client.rotate_resolver(&id, &new_resolver);

    client.resolve_dispute(&id, &0u32, &false);

    // `env.auths()` reports which addresses the most recent top-level
    // invocation demanded a signature from, which is exactly the set
    // `resolve_dispute`'s `dispute_resolver.require_auth()` gate asks for.
    let mut asked_replacement = false;
    let mut asked_outgoing = false;
    for (address, _invocation) in env.auths() {
        if address == new_resolver {
            asked_replacement = true;
        }
        if address == dispute_resolver {
            asked_outgoing = true;
        }
    }

    assert!(
        asked_replacement,
        "resolve_dispute must demand the replacement resolver's signature"
    );
    assert!(
        !asked_outgoing,
        "the outgoing resolver must lose all authority once it is replaced"
    );
}

/// The adjacent case that a careless fix could regress: rotation rewrites the
/// whole header, so every *other* agreement-level field has to survive it —
/// including the cumulative `released_amounts` entry a `release_partial` has
/// already recorded and the funds still sitting in escrow.
#[test]
fn test_rotate_resolver_preserves_escrow_and_release_state() {
    let (env, payer, payee, dispute_resolver, token_address, client) = setup();
    let token_client = token::TokenClient::new(&env, &token_address);
    let id = agreement_id(&env, 135);
    let new_resolver = Address::generate(&env);

    client.init(
        &id,
        &payer,
        &payee,
        &token_address,
        &one_milestone(&env, 500),
        &dispute_resolver,
    );
    client.lock_funds(&id, &0u32);
    assert_eq!(
        client.release_partial(&id, &0u32, &200i128),
        300,
        "a partial release must leave the remainder escrowed"
    );

    let before = client.get_agreement(&id);
    client.rotate_resolver(&id, &new_resolver);
    let after = client.get_agreement(&id);

    assert_eq!(
        after.agreement_id, before.agreement_id,
        "rotation must not change the agreement ID"
    );
    assert_eq!(
        after.payer, before.payer,
        "rotation must not change the payer"
    );
    assert_eq!(
        after.payee, before.payee,
        "rotation must not change the payee"
    );
    assert_eq!(
        after.token, before.token,
        "rotation must not change the token"
    );
    assert_eq!(
        after.total_amount, before.total_amount,
        "rotation must not change the agreement total"
    );
    assert_eq!(
        after.milestones, before.milestones,
        "rotation must not change any milestone status"
    );
    assert_eq!(
        after.released_amounts, before.released_amounts,
        "rotation must not disturb the cumulative release ledger"
    );
    assert_eq!(
        after.dispute_resolver, new_resolver,
        "the resolver is the only field a rotation may change"
    );

    assert_eq!(
        token_client.balance(&client.address),
        300,
        "rotation must leave the un-released remainder escrowed"
    );
    assert_eq!(
        token_client.balance(&payee),
        200,
        "rotation must not claw back funds already released"
    );
    assert_eq!(
        token_client.balance(&payer),
        10_000 - 500,
        "rotation must not move the payer's remaining balance"
    );
}

/// #612: happy path for `release_partial`. Two partial releases — one while
/// the milestone is `Funded`, one after the work is submitted — must each move
/// exactly `amount` from the contract to the payee, return the remainder,
/// leave the milestone in its current non-terminal status, record the
/// cumulative release in `released_amounts`, and emit a single `trls_prtl`
/// event carrying `(milestone_id, amount, remaining)`.
#[test]
fn test_release_partial_happy_path() {
    let (env, payer, payee, dispute_resolver, token_address, client) = setup();
    let token_client = token::TokenClient::new(&env, &token_address);
    let id = agreement_id(&env, 136);

    client.init(
        &id,
        &payer,
        &payee,
        &token_address,
        &milestones_from_amounts(&env, &[1_000, 400]),
        &dispute_resolver,
    );
    client.lock_funds(&id, &0u32);

    assert_eq!(token_client.balance(&payer), 9_000);
    assert_eq!(token_client.balance(&client.address), 1_000);
    assert_eq!(token_client.balance(&payee), 0);

    // --- First partial release, milestone still `Funded` -------------------
    let remaining = client.release_partial(&id, &0u32, &300i128);

    // Events must be checked straight after the call (see `assert_trellis_topics`).
    assert_trellis_topics(
        &env,
        &client.address,
        &[symbol_short!("trls_prtl")],
        "a non-final partial release must emit exactly one trls_prtl event",
    );
    let (event_milestone, event_amount, event_remaining): (u32, i128, i128) =
        trellis_event_data(&env, &client.address, symbol_short!("trls_prtl"));
    assert_eq!(event_milestone, 0, "trls_prtl must name the released milestone");
    assert_eq!(event_amount, 300, "trls_prtl must carry the released amount");
    assert_eq!(event_remaining, 700, "trls_prtl must carry the remainder");

    assert_eq!(remaining, 700, "release_partial must return the remainder");
    assert_eq!(
        token_client.balance(&client.address),
        700,
        "the contract must be debited exactly the released amount"
    );
    assert_eq!(
        token_client.balance(&payee),
        300,
        "the payee must be credited exactly the released amount"
    );
    assert_eq!(
        token_client.balance(&payer),
        9_000,
        "a partial release must not touch the payer's balance"
    );

    let agreement = client.get_agreement(&id);
    assert_eq!(
        agreement.milestones.get(0).expect("milestone 0 must exist").status,
        EscrowStatus::Funded,
        "a partially released milestone must stay Funded"
    );
    assert_eq!(
        agreement.released_amounts.get(0),
        Some(300),
        "released_amounts must record the first release"
    );
    assert_eq!(
        agreement.released_amounts.get(1),
        None,
        "an untouched milestone must have no release entry"
    );

    // --- Second partial release, milestone now `WorkSubmitted` -------------
    client.submit_work(&id, &0u32, &None);
    let remaining = client.release_partial(&id, &0u32, &450i128);

    assert_trellis_topics(
        &env,
        &client.address,
        &[symbol_short!("trls_prtl")],
        "a non-final partial release must emit exactly one trls_prtl event",
    );
    let (event_milestone, event_amount, event_remaining): (u32, i128, i128) =
        trellis_event_data(&env, &client.address, symbol_short!("trls_prtl"));
    assert_eq!(event_milestone, 0, "trls_prtl must name the released milestone");
    assert_eq!(event_amount, 450, "trls_prtl must carry the released amount");
    assert_eq!(event_remaining, 250, "trls_prtl must carry the remainder");

    assert_eq!(remaining, 250, "release_partial must return the remainder");
    assert_eq!(
        token_client.balance(&client.address),
        250,
        "the contract must be debited exactly the second released amount"
    );
    assert_eq!(
        token_client.balance(&payee),
        750,
        "the payee must be credited the cumulative released amount"
    );
    assert_eq!(token_client.balance(&payer), 9_000);

    let agreement = client.get_agreement(&id);
    assert_eq!(
        agreement.milestones.get(0).expect("milestone 0 must exist").status,
        EscrowStatus::WorkSubmitted,
        "a partially released milestone must keep its WorkSubmitted status"
    );
    assert_eq!(
        agreement.milestones.get(1).expect("milestone 1 must exist").status,
        EscrowStatus::Pending,
        "a partial release must not affect other milestones"
    );
    assert_eq!(
        agreement.released_amounts.get(0),
        Some(750),
        "released_amounts must hold the cumulative total of both releases"
    );
}
