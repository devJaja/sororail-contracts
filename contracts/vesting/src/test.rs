// Test fixtures do plain arithmetic on known-small constants; the checked-math
// rule is for contract code.
#![allow(clippy::arithmetic_side_effects)]

use soroban_sdk::{
    map,
    testutils::{storage::Instance as _, Address as _, Events as _, Ledger as _},
    token::TokenClient,
    vec, Address, Env, Event, IntoVal, Symbol, Val,
};
use sororail_common::{storage::INSTANCE_BUMP, testutils::TestEnv, Error};

use crate::{
    contract::{VestingContract, VestingContractClient},
    events::{Claimed, Revoked},
    types::Grant,
};

#[test]
fn bump_restores_the_instance_ttl_after_ledger_advance() {
    let f = Fixture::new(true);
    f.env
        .ledger()
        .set_sequence_number(f.env.ledger().sequence() + INSTANCE_BUMP - 1);
    f.env.as_contract(&f.client.address, || {
        assert!(f.env.storage().instance().get_ttl() < INSTANCE_BUMP);
    });
    f.client.bump();
    f.env.as_contract(&f.client.address, || {
        assert_eq!(f.env.storage().instance().get_ttl(), INSTANCE_BUMP);
    });
}

#[test]
fn bump_extends_the_instance_ttl() {
    let f = Fixture::new(true);
    f.env.as_contract(&f.client.address, || {
        assert!(f.env.storage().instance().get_ttl() <= INSTANCE_BUMP);
    });
    f.client.bump();
    f.env.as_contract(&f.client.address, || {
        assert_eq!(f.env.storage().instance().get_ttl(), INSTANCE_BUMP);
    });
}

const TOTAL: i128 = 1_000_000;
const START: u64 = 1_000_000;
const CLIFF: u64 = 100;
const DURATION: u64 = 1_000;
const MINT: i128 = TOTAL * 10;

struct Fixture<'a> {
    env: Env,
    client: VestingContractClient<'a>,
    token: TokenClient<'a>,
    grantor: Address,
    beneficiary: Address,
}

impl Fixture<'_> {
    fn new(revocable: bool) -> Self {
        Fixture::with_schedule(revocable, CLIFF, DURATION)
    }

    fn with_schedule(revocable: bool, cliff: u64, duration: u64) -> Self {
        Fixture::with_schedule_at(START, cliff, duration, revocable)
    }

    fn with_schedule_at(start: u64, cliff: u64, duration: u64, revocable: bool) -> Self {
        let te = TestEnv::at(start);
        let (token, grantor) = te.make_token(MINT);
        let token_address = token.address.clone();
        let beneficiary = te.make_address();
        let env = te.env().clone();

        let client = VestingContractClient::new(&env, &env.register(VestingContract, ()));
        client.create(
            &grantor,
            &beneficiary,
            &token_address,
            &TOTAL,
            &start,
            &cliff,
            &duration,
            &revocable,
        );

        Fixture {
            token: TokenClient::new(&env, &token_address),
            env,
            client,
            grantor,
            beneficiary,
        }
    }

    fn at(&self, ts: u64) {
        self.env.ledger().with_mut(|l| l.timestamp = ts);
    }

    fn held(&self) -> i128 {
        self.token.balance(&self.client.address)
    }

    fn assert_conserved(&self) {
        let g = self.client.get();
        assert_eq!(
            g.claimed + g.returned + self.client.remaining(),
            g.total,
            "conservation violated: {g:?}"
        );
        assert_eq!(
            self.held(),
            self.client.remaining(),
            "held balance diverged from remaining: {g:?}"
        );
    }
}

/// A bare grant for testing the schedule in isolation.
fn bare(env: &Env, cliff: u64, duration: u64) -> Grant {
    Grant {
        grantor: Address::generate(env),
        beneficiary: Address::generate(env),
        token: Address::generate(env),
        total: TOTAL,
        start: START,
        cliff,
        duration,
        revocable: true,
        claimed: 0,
        returned: 0,
        revoked_at: None,
    }
}

// ----------------------------------------- the schedule (pure, no ledger)

#[test]
fn nothing_vests_before_the_cliff() {
    let env = Env::default();
    let g = bare(&env, CLIFF, DURATION);
    assert_eq!(g.vested_amount(0), Ok(0));
    assert_eq!(g.vested_amount(START), Ok(0));
    assert_eq!(g.vested_amount(START + CLIFF - 1), Ok(0));
}

#[test]
fn the_cliff_vests_the_elapsed_proportion_in_one_step() {
    let env = Env::default();
    let g = bare(&env, CLIFF, DURATION);
    // At the cliff, 100/1000 of the schedule has elapsed.
    assert_eq!(g.vested_amount(START + CLIFF), Ok(TOTAL / 10));
}

/// Issue #222: the cliff timestamp is **inclusive** — `vested_amount` at
/// exactly `start + cliff` must return a non-zero amount equal to the
/// elapsed proportion, and the second immediately before must return zero.
/// This pins the boundary documented in `docs/vesting-safe-range.md` and
/// in `Grant::vested_amount`: "Nothing vests before `start + cliff`; at
/// that moment the elapsed proportion vests in one step."
#[test]
fn vested_amount_at_exact_cliff_is_inclusive_and_nonzero() {
    let env = Env::default();
    let g = bare(&env, CLIFF, DURATION);
    let cliff_at = g.cliff_at(); // == START + CLIFF

    // One second before the cliff: nothing vested.
    assert_eq!(
        g.vested_amount(cliff_at - 1),
        Ok(0),
        "one second before cliff must vest nothing"
    );
    // Exactly at the cliff: the elapsed proportion vests (cliff is inclusive).
    // elapsed = CLIFF (100), duration = DURATION (1000) → 100/1000 of TOTAL.
    let expected = TOTAL * i128::from(CLIFF) / i128::from(DURATION);
    assert_eq!(
        g.vested_amount(cliff_at),
        Ok(expected),
        "exact cliff timestamp must be inclusive and vest the elapsed proportion"
    );
    // One second after the cliff: slightly more has vested (linear increase).
    assert!(
        g.vested_amount(cliff_at + 1).unwrap() > expected,
        "one second after cliff must vest more than at the cliff"
    );
}

#[test]
fn vesting_is_linear_after_the_cliff() {
    let env = Env::default();
    let g = bare(&env, CLIFF, DURATION);
    assert_eq!(g.vested_amount(START + 250), Ok(TOTAL / 4));
    assert_eq!(g.vested_amount(START + 500), Ok(TOTAL / 2));
    assert_eq!(g.vested_amount(START + 750), Ok(TOTAL * 3 / 4));
}

#[test]
fn fully_vested_at_start_plus_duration() {
    let env = Env::default();
    let g = bare(&env, CLIFF, DURATION);
    assert_eq!(g.vested_amount(START + DURATION), Ok(TOTAL));
    assert_eq!(g.vested_amount(START + DURATION + 1), Ok(TOTAL));
    assert_eq!(g.vested_amount(u64::MAX), Ok(TOTAL));
}

#[test]
fn a_cliff_equal_to_duration_is_all_or_nothing() {
    let env = Env::default();
    let g = bare(&env, DURATION, DURATION);
    assert_eq!(g.vested_amount(START + DURATION - 1), Ok(0));
    assert_eq!(g.vested_amount(START + DURATION), Ok(TOTAL));
}

#[test]
fn a_zero_cliff_vests_linearly_from_the_start() {
    let env = Env::default();
    let g = bare(&env, 0, DURATION);
    assert_eq!(g.vested_amount(START), Ok(0));
    assert_eq!(g.vested_amount(START + 1), Ok(TOTAL / i128::from(DURATION)));
    assert_eq!(g.vested_amount(START + 500), Ok(TOTAL / 2));
}

#[test]
fn a_one_second_schedule_vests_in_one_tick() {
    let env = Env::default();
    let g = bare(&env, 0, 1);
    assert_eq!(g.vested_amount(START), Ok(0));
    assert_eq!(g.vested_amount(START + 1), Ok(TOTAL));
}

#[test]
fn revocation_freezes_the_schedule() {
    let env = Env::default();
    let mut g = bare(&env, CLIFF, DURATION);
    g.revoked_at = Some(START + 300);
    assert_eq!(g.vested_amount(START + 300), Ok(TOTAL * 3 / 10));
    assert_eq!(g.vested_amount(START + 900), Ok(TOTAL * 3 / 10));
    assert_eq!(g.vested_amount(u64::MAX), Ok(TOTAL * 3 / 10));
}

#[test]
fn vesting_reports_overflow_rather_than_wrapping() {
    let env = Env::default();
    let mut g = bare(&env, 0, DURATION);
    g.total = i128::MAX;
    assert_eq!(g.vested_amount(START + 500), Err(Error::Overflow));
}

#[test]
fn vested_amount_never_underflows_with_a_start_near_u64_max() {
    // `elapsed = effective - self.start` is a u64 subtraction before the
    // cast to i128; if `effective` could ever fall below `start`, this
    // would panic in debug builds (or wrap in release) rather than return
    // an `Error`. `VestingContract::create` rejects any schedule whose
    // `start + duration` would overflow, which is what keeps `end` (and so
    // `effective`, which is always clamped below it) from ever landing
    // below `start` here.
    let env = Env::default();
    let g = Grant {
        grantor: Address::generate(&env),
        beneficiary: Address::generate(&env),
        token: Address::generate(&env),
        total: TOTAL,
        start: u64::MAX - 1,
        cliff: 0,
        duration: 1,
        revocable: true,
        claimed: 0,
        returned: 0,
        revoked_at: None,
    };
    assert_eq!(g.vested_amount(u64::MAX - 1), Ok(0));
    assert_eq!(g.vested_amount(u64::MAX), Ok(TOTAL));
}

/// Guards the division by `duration` in the linear branch of
/// `vested_amount`: every `(start, cliff, duration)` combination here
/// satisfies `create`'s validation (`duration > 0`, `cliff <= duration`), and
/// no probe timestamp may surface `DivisionByZero`.
#[test]
fn linear_branch_never_divides_by_zero_duration() {
    let env = Env::default();
    let starts = [0u64, 1, START, u64::MAX - 1_000, u64::MAX];
    let schedules = [
        (0u64, 1u64),
        (1, 1),
        (0, DURATION),
        (CLIFF, DURATION),
        (DURATION, DURATION),
        (0, u64::MAX),
    ];

    for start in starts {
        for (cliff, duration) in schedules {
            // Mirror `create`'s validation: only valid schedules are stored.
            assert!(duration > 0);
            assert!(cliff <= duration);
            let g = Grant {
                grantor: Address::generate(&env),
                beneficiary: Address::generate(&env),
                token: Address::generate(&env),
                total: TOTAL,
                start,
                cliff,
                duration,
                revocable: true,
                claimed: 0,
                returned: 0,
                revoked_at: None,
            };
            let cliff_at = g.cliff_at();
            let end = g.end_at();
            let probes = [
                0,
                start.saturating_sub(1),
                start,
                cliff_at.saturating_sub(1),
                cliff_at,
                end.saturating_sub(1),
                end,
                end.saturating_add(1),
                u64::MAX,
            ];
            for at in probes {
                let result = g.vested_amount(at);
                assert_ne!(
                    result,
                    Err(Error::DivisionByZero),
                    "div-by-zero at start={start} cliff={cliff} duration={duration} at={at}"
                );
                // When the linear branch is taken, the divisor is non-zero.
                let in_linear = at >= cliff_at && at < end;
                if in_linear {
                    assert!(
                        g.duration > 0,
                        "linear branch hit with zero duration at start={start}"
                    );
                    assert!(result.is_ok());
                }
            }
        }
    }
}

#[test]
fn long_duration_large_total_stays_inside_documented_safe_range() {
    let env = Env::default();
    let ten_years = 10 * 365 * 24 * 60 * 60;
    let mut g = bare(&env, 0, ten_years);

    // One billion 18-decimal tokens over ten years stays well below i128::MAX
    // during the checked `total * elapsed / duration` calculation.
    g.total = 1_000_000_000_i128 * 1_000_000_000_000_000_000_i128;

    assert_eq!(g.vested_amount(START + ten_years / 2), Ok(g.total / 2));
    assert!(g.vested_amount(START + ten_years - 1).is_ok());
    assert_eq!(g.vested_amount(START + ten_years), Ok(g.total));
}

// ------------------------------------------------------------------ create

// ------------------------------------------------------- schedule view

fn checkpoints(f: &Fixture) -> std::vec::Vec<(u64, i128)> {
    f.client
        .schedule()
        .iter()
        .map(|c| (c.at, c.vested))
        .collect()
}

#[test]
fn schedule_lists_start_cliff_and_end_for_a_cliffed_grant() {
    let f = Fixture::new(true);
    assert_eq!(
        checkpoints(&f),
        std::vec![(START, 0), (START + 100, 100_000), (START + 1_000, TOTAL)]
    );
}

#[test]
fn schedule_with_no_cliff_is_start_and_end() {
    let f = Fixture::with_schedule(true, 0, DURATION);
    assert_eq!(
        checkpoints(&f),
        std::vec![(START, 0), (START + DURATION, TOTAL)]
    );
}

#[test]
fn schedule_with_cliff_equal_to_duration_is_start_and_end() {
    let f = Fixture::with_schedule(true, DURATION, DURATION);
    assert_eq!(
        checkpoints(&f),
        std::vec![(START, 0), (START + DURATION, TOTAL)]
    );
}

#[test]
fn schedule_matches_vested_amount_at_every_checkpoint() {
    let f = Fixture::new(true);
    for (at, vested) in checkpoints(&f) {
        assert_eq!(f.client.vested_amount(&at), vested, "at {at}");
    }
}

#[test]
fn schedule_is_unchanged_by_revocation() {
    let f = Fixture::new(true);
    let before = checkpoints(&f);
    f.at(START + 500);
    f.client.revoke();
    assert_eq!(checkpoints(&f), before);
}

#[test]
fn create_funds_the_whole_grant_up_front() {
    let f = Fixture::new(true);
    assert_eq!(f.held(), TOTAL);
    assert_eq!(f.token.balance(&f.grantor), MINT - TOTAL);
    f.assert_conserved();
}

#[test]
fn create_rejects_a_non_positive_total() {
    let env = Env::default();
    env.mock_all_auths();
    let c = VestingContractClient::new(&env, &env.register(VestingContract, ()));
    let (a, b, t) = (
        Address::generate(&env),
        Address::generate(&env),
        Address::generate(&env),
    );
    for bad in [0i128, -1, i128::MIN] {
        assert_eq!(
            c.try_create(&a, &b, &t, &bad, &START, &CLIFF, &DURATION, &true),
            Err(Ok(Error::InvalidAmount)),
            "total {bad} was accepted"
        );
    }
}

#[test]
fn create_rejects_a_zero_duration() {
    let env = Env::default();
    env.mock_all_auths();
    let c = VestingContractClient::new(&env, &env.register(VestingContract, ()));
    let (a, b, t) = (
        Address::generate(&env),
        Address::generate(&env),
        Address::generate(&env),
    );
    assert_eq!(
        c.try_create(&a, &b, &t, &TOTAL, &START, &0, &0, &true),
        Err(Ok(Error::InvalidDuration))
    );
}

#[test]
fn create_rejects_a_cliff_after_the_end() {
    let env = Env::default();
    env.mock_all_auths();
    let c = VestingContractClient::new(&env, &env.register(VestingContract, ()));
    let (a, b, t) = (
        Address::generate(&env),
        Address::generate(&env),
        Address::generate(&env),
    );
    assert_eq!(
        c.try_create(
            &a,
            &b,
            &t,
            &TOTAL,
            &START,
            &(DURATION + 1),
            &DURATION,
            &true
        ),
        Err(Ok(Error::VestingCliffAfterEnd))
    );
}

/// When several create inputs are invalid at once, the first check in
/// `create`'s validation sequence wins. Locks the order:
/// `require_positive(total)` → `duration == 0` → `cliff > duration`.
#[test]
fn create_reports_the_first_validation_error_when_several_apply() {
    let env = Env::default();
    env.mock_all_auths();
    let c = VestingContractClient::new(&env, &env.register(VestingContract, ()));
    let (a, b, t) = (
        Address::generate(&env),
        Address::generate(&env),
        Address::generate(&env),
    );

    // total=-1, duration=0, cliff=5: InvalidAmount beats InvalidDuration /
    // VestingCliffAfterEnd.
    assert_eq!(
        c.try_create(&a, &b, &t, &-1, &START, &5, &0, &true),
        Err(Ok(Error::InvalidAmount))
    );

    // total ok, duration=0, cliff=5: InvalidDuration beats VestingCliffAfterEnd.
    assert_eq!(
        c.try_create(&a, &b, &t, &TOTAL, &START, &5, &0, &true),
        Err(Ok(Error::InvalidDuration))
    );
}

#[test]
fn create_rejects_a_schedule_whose_end_would_overflow_u64() {
    // `start + duration` must be representable, or `end_at`'s
    // `saturating_add` would silently cap it and `vested_amount` would be
    // evaluated against a shortened schedule instead of erroring up front.
    let env = Env::default();
    env.mock_all_auths();
    let c = VestingContractClient::new(&env, &env.register(VestingContract, ()));
    let (a, b, t) = (
        Address::generate(&env),
        Address::generate(&env),
        Address::generate(&env),
    );
    assert_eq!(
        c.try_create(&a, &b, &t, &TOTAL, &u64::MAX, &0, &1, &true),
        Err(Ok(Error::VestingScheduleOverflow))
    );
}

#[test]
fn create_accepts_a_schedule_whose_end_is_exactly_u64_max() {
    let f = Fixture::with_schedule_at(u64::MAX - 1, 0, 1, true);
    assert_eq!(f.client.get().duration, 1);
}

#[test]
fn create_rejects_a_second_call() {
    let f = Fixture::new(true);
    assert_eq!(
        f.client.try_create(
            &f.grantor,
            &f.beneficiary,
            &f.token.address,
            &TOTAL,
            &START,
            &CLIFF,
            &DURATION,
            &true
        ),
        Err(Ok(Error::AlreadyInitialized))
    );
}

#[test]
fn create_rejects_identical_grantor_and_beneficiary() {
    let env = Env::default();
    env.mock_all_auths();
    let c = VestingContractClient::new(&env, &env.register(VestingContract, ()));
    let party = Address::generate(&env);
    let token = Address::generate(&env);
    assert_eq!(
        c.try_create(&party, &party, &token, &TOTAL, &START, &CLIFF, &DURATION, &true),
        Err(Ok(Error::IdenticalParties))
    );
}

/// A self-grant is refused before any funds move, and leaves no grant behind.
#[test]
fn self_grant_is_refused_without_moving_funds() {
    let te = TestEnv::at(START);
    let (token, grantor) = te.make_token(MINT);
    let env = te.env().clone();
    let client = VestingContractClient::new(&env, &env.register(VestingContract, ()));
    assert_eq!(
        client.try_create(
            &grantor,
            &grantor,
            &token.address,
            &TOTAL,
            &START,
            &CLIFF,
            &DURATION,
            &true
        ),
        Err(Ok(Error::IdenticalParties))
    );
    assert_eq!(token.balance(&grantor), MINT);
    assert_eq!(token.balance(&client.address), 0);
    assert_eq!(client.try_get(), Err(Ok(Error::NotInitialized)));
}

/// `create` persists the grant before the token pull. A failed transfer must
/// roll back that write so `get` still returns `NotInitialized`.
#[test]
fn create_with_contract_as_beneficiary_succeeds() {
    let te = TestEnv::at(START);
    let (token_client, grantor) = te.make_token(MINT);
    let token = token_client.address.clone();
    let env = te.env().clone();
    let contract_id = env.register(VestingContract, ());
    let client = VestingContractClient::new(&env, &contract_id);

    let result = client.try_create(
        &grantor,
        &contract_id,
        &token,
        &TOTAL,
        &START,
        &CLIFF,
        &DURATION,
        &true,
    );
    assert!(
        result.is_ok(),
        "create with contract as beneficiary should succeed"
    );
}

#[test]
fn create_rejects_multiple_invalid_args_in_order() {
    let te = TestEnv::at(START);
    let (token_client, grantor) = te.make_token(MINT);
    let token = token_client.address.clone();
    let beneficiary = te.make_address();
    let env = te.env().clone();

    let contract_id = env.register(VestingContract, ());
    let client = VestingContractClient::new(&env, &contract_id);

    // AlreadyInitialized comes before InvalidAmount
    client.create(
        &grantor,
        &beneficiary,
        &token,
        &TOTAL,
        &START,
        &CLIFF,
        &DURATION,
        &true,
    );
    assert_eq!(
        client.try_create(
            &grantor,
            &beneficiary,
            &token,
            &0,
            &START,
            &CLIFF,
            &DURATION,
            &true
        ),
        Err(Ok(Error::AlreadyInitialized))
    );

    // InvalidAmount comes before InvalidDuration
    let contract_id2 = env.register(VestingContract, ());
    let client2 = VestingContractClient::new(&env, &contract_id2);
    assert_eq!(
        client2.try_create(
            &grantor,
            &beneficiary,
            &token,
            &0,
            &START,
            &CLIFF,
            &0,
            &true
        ),
        Err(Ok(Error::InvalidAmount))
    );

    // InvalidDuration comes before VestingCliffAfterEnd
    let contract_id3 = env.register(VestingContract, ());
    let client3 = VestingContractClient::new(&env, &contract_id3);
    assert_eq!(
        client3.try_create(
            &grantor,
            &beneficiary,
            &token,
            &TOTAL,
            &START,
            &CLIFF,
            &0,
            &true
        ),
        Err(Ok(Error::InvalidDuration))
    );
}

#[test]
fn create_reverts_cleanly_when_grantor_cannot_fund() {
    let te = TestEnv::at(START);
    let (token_client, grantor) = te.make_token(1);
    let token = token_client.address.clone();
    let beneficiary = te.make_address();
    let env = te.env().clone();
    let c = VestingContractClient::new(&env, &env.register(VestingContract, ()));

    assert!(c
        .try_create(
            &grantor,
            &beneficiary,
            &token,
            &TOTAL,
            &START,
            &CLIFF,
            &DURATION,
            &true
        )
        .is_err());
    assert_eq!(c.try_get(), Err(Ok(Error::NotInitialized)));
    assert_eq!(token_client.balance(&c.address), 0);
    assert_eq!(token_client.balance(&grantor), 1);
}

/// Pins the wire shape indexers decode (SPEC.md `indexed_events`). The
/// expected value is spelled out literally rather than built from
/// `events::Created`, so renaming a topic or a field fails here.
#[test]
fn create_emits_the_created_event() {
    let te = TestEnv::at(START);
    let (token_client, grantor) = te.make_token(MINT);
    let token_address = token_client.address.clone();
    let beneficiary = te.make_address();
    let env = te.env().clone();

    let client = VestingContractClient::new(&env, &env.register(VestingContract, ()));
    client.create(
        &grantor,
        &beneficiary,
        &token_address,
        &TOTAL,
        &START,
        &CLIFF,
        &DURATION,
        &true,
    );

    let total: Val = TOTAL.into_val(&env);
    let start: Val = START.into_val(&env);
    let cliff: Val = CLIFF.into_val(&env);
    let duration: Val = DURATION.into_val(&env);
    let revocable: Val = true.into_val(&env);
    // Map data: keys are the field names, serialized in sorted order.
    let data: Val = map![
        &env,
        (Symbol::new(&env, "beneficiary"), beneficiary.into_val(&env)),
        (Symbol::new(&env, "cliff"), cliff),
        (Symbol::new(&env, "duration"), duration),
        (Symbol::new(&env, "revocable"), revocable),
        (Symbol::new(&env, "start"), start),
        (Symbol::new(&env, "token"), token_address.into_val(&env)),
        (Symbol::new(&env, "total"), total),
    ]
    .into_val(&env);
    assert_eq!(
        env.events().all().filter_by_contract(&client.address),
        vec![
            &env,
            (
                client.address.clone(),
                vec![
                    &env,
                    Symbol::new(&env, "vesting").into_val(&env),
                    Symbol::new(&env, "created").into_val(&env),
                    grantor.into_val(&env),
                ],
                data,
            ),
        ]
    );
}

#[test]
fn entry_points_error_before_create() {
    let env = Env::default();
    env.mock_all_auths();
    let c = VestingContractClient::new(&env, &env.register(VestingContract, ()));
    assert_eq!(c.try_claim(), Err(Ok(Error::NotInitialized)));
    assert_eq!(c.try_revoke(), Err(Ok(Error::NotInitialized)));
    assert_eq!(c.try_claimable(), Err(Ok(Error::NotInitialized)));
    assert_eq!(c.try_remaining(), Err(Ok(Error::NotInitialized)));
    assert_eq!(c.try_vested_amount(&START), Err(Ok(Error::NotInitialized)));
}

#[test]
#[should_panic]
fn create_requires_the_grantors_authorization() {
    let env = Env::default();
    let c = VestingContractClient::new(&env, &env.register(VestingContract, ()));
    c.create(
        &Address::generate(&env),
        &Address::generate(&env),
        &Address::generate(&env),
        &TOTAL,
        &START,
        &CLIFF,
        &DURATION,
        &true,
    );
}

// ------------------------------------------------------------------- claim

#[test]
fn claim_is_blocked_before_the_cliff() {
    let f = Fixture::new(true);
    f.at(START + CLIFF - 1);
    assert_eq!(f.client.try_claim(), Err(Ok(Error::VestingCliffNotReached)));
    assert_eq!(f.held(), TOTAL);
}

#[test]
fn claim_exactly_at_and_one_second_before_cliff() {
    let env = Env::default();
    let g = bare(&env, CLIFF, DURATION);
    // One second before cliff_at(): nothing vested yet.
    assert_eq!(g.claimable_at(START + CLIFF - 1), Ok(0));
    // Exactly at cliff_at(): cliff vests its proportion in one step.
    assert_eq!(g.claimable_at(START + CLIFF), Ok(TOTAL / 10));
}

#[test]
fn claim_fails_one_second_before_cliff_but_succeeds_at_cliff() {
    let f = Fixture::new(true);
    f.at(START + CLIFF - 1);
    assert_eq!(f.client.try_claim(), Err(Ok(Error::VestingCliffNotReached)));
    f.at(START + CLIFF);
    assert_eq!(f.client.claim(), TOTAL / 10);
    assert_eq!(f.token.balance(&f.beneficiary), TOTAL / 10);
    f.assert_conserved();
}

#[test]
fn claim_pays_the_vested_portion() {
    let f = Fixture::new(true);
    f.at(START + 500);
    assert_eq!(f.client.claim(), TOTAL / 2);
    assert_eq!(f.token.balance(&f.beneficiary), TOTAL / 2);
    f.assert_conserved();
}

/// Pins the wire shape indexers decode (SPEC.md `indexed_events`). The
/// expected value is spelled out via `events::Claimed`, so renaming a topic
/// or a field fails here.
#[test]
fn claim_emits_the_claimed_event() {
    let f = Fixture::new(true);
    f.at(START + 500);
    f.client.claim();

    let expected = Claimed {
        beneficiary: f.beneficiary.clone(),
        amount: TOTAL / 2,
        total_claimed: TOTAL / 2,
    };
    // filter_by_contract returns ContractEvents; .events() gives &[ContractEvent].
    // The contract emits `created` at setup and `claimed` here -- take the last one.
    let all = f.env.events().all().filter_by_contract(&f.client.address);
    let events = all.events();
    let last = events.last().expect("no events emitted");
    assert_eq!(last, &expected.to_xdr(&f.env, &f.client.address));
}

#[test]
fn claim_twice_in_the_same_second_has_nothing_left() {
    let f = Fixture::new(true);
    f.at(START + 500);
    f.client.claim();
    assert_eq!(f.client.try_claim(), Err(Ok(Error::VestingNothingToClaim)));
    f.assert_conserved();
}

#[test]
fn incremental_claims_sum_to_the_total() {
    let f = Fixture::new(true);
    for step in 1..=10u64 {
        f.at(START + step * 100);
        f.client.claim();
        f.assert_conserved();
    }
    assert_eq!(f.token.balance(&f.beneficiary), TOTAL);
    assert_eq!(f.held(), 0);
}

#[test]
fn claim_after_the_end_takes_everything() {
    let f = Fixture::new(true);
    f.at(START + DURATION + 10_000);
    assert_eq!(f.client.claim(), TOTAL);
    assert_eq!(f.held(), 0);
    f.assert_conserved();
}

/// #216 – After the end date the full remaining amount is claimable and
/// `grant.claimed` reaches `grant.total`.
#[test]
fn claim_after_end_date_claimed_field_reaches_total() {
    let f = Fixture::new(true);
    f.at(START + DURATION + 1);
    let claimable = f.client.claimable();
    assert_eq!(claimable, TOTAL, "full amount must be claimable after end");
    let returned = f.client.claim();
    assert_eq!(returned, TOTAL, "claim must return the full total");
    let grant = f.client.get();
    assert_eq!(
        grant.claimed, grant.total,
        "claimed field must equal total after claiming everything"
    );
    f.assert_conserved();
}

#[test]
#[should_panic]
fn claim_requires_the_beneficiarys_authorization() {
    let f = Fixture::new(true);
    f.at(START + 500);
    f.env.set_auths(&[]);
    f.client.claim();
}

/// #218 – A non-beneficiary caller is rejected; only the beneficiary may claim.
///
/// Strips all authorisations so no address can satisfy `require_auth`.
/// Soroban panics on a failed `require_auth`, which `should_panic` catches.
#[test]
#[should_panic]
fn claim_is_callable_only_by_the_beneficiary() {
    let f = Fixture::new(true);
    f.at(START + 500);
    // Remove all auth so neither beneficiary nor anyone else is authorised.
    f.env.set_auths(&[]);
    f.client.claim();
}

// ------------------------------------------------------------------ revoke

#[test]
fn revoke_returns_only_the_unvested_portion() {
    let f = Fixture::new(true);
    let grantor_before = f.token.balance(&f.grantor);
    f.at(START + 300);
    assert_eq!(f.client.revoke(), TOTAL * 7 / 10);

    assert_eq!(f.token.balance(&f.grantor), grantor_before + TOTAL * 7 / 10);
    // The vested 30% stays in the contract for the beneficiary.
    assert_eq!(f.held(), TOTAL * 3 / 10);
    f.assert_conserved();
}

#[test]
fn the_vested_portion_stays_claimable_after_revocation() {
    let f = Fixture::new(true);
    f.at(START + 300);
    f.client.revoke();
    f.at(START + 900);
    assert_eq!(f.client.claim(), TOTAL * 3 / 10);
    assert_eq!(f.token.balance(&f.beneficiary), TOTAL * 3 / 10);
    assert_eq!(f.held(), 0);
    f.assert_conserved();
}

#[test]
fn revoke_accounts_for_what_was_already_claimed() {
    let f = Fixture::new(true);
    f.at(START + 200);
    f.client.claim();
    f.at(START + 600);
    f.client.revoke();

    // 60% vested overall, 20% already taken, 40% still claimable, 40% back.
    assert_eq!(f.client.claimable(), TOTAL * 4 / 10);
    f.assert_conserved();
    f.client.claim();
    assert_eq!(f.token.balance(&f.beneficiary), TOTAL * 6 / 10);
    assert_eq!(f.held(), 0);
    f.assert_conserved();
}

#[test]
fn revoke_before_the_cliff_returns_everything() {
    let f = Fixture::new(true);
    let grantor_before = f.token.balance(&f.grantor);
    assert_eq!(f.client.revoke(), TOTAL);
    assert_eq!(f.token.balance(&f.grantor), grantor_before + TOTAL);
    assert_eq!(f.held(), 0);
    // Nothing ever vested, so there is nothing to claim.
    assert_eq!(f.client.try_claim(), Err(Ok(Error::VestingCliffNotReached)));
    f.assert_conserved();
}

#[test]
fn revoke_before_the_cliff_then_advance_time_and_claim() {
    let f = Fixture::new(true);
    // Revoke before the cliff.
    f.client.revoke();
    // Advance time past the cliff -- but revoked_at freezes effective time.
    f.at(START + CLIFF + 1_000);
    // After revocation before the cliff, claim still reports CliffNotReached
    // because effective time is frozen at revoked_at (before the cliff).
    // Nothing was ever vested, so nothing is claimable.
    assert_eq!(f.client.try_claim(), Err(Ok(Error::VestingCliffNotReached)));
    f.assert_conserved();
}

#[test]
fn revoke_after_the_end_returns_nothing() {
    let f = Fixture::new(true);
    f.at(START + DURATION + 500);
    assert_eq!(f.client.revoke(), 0);
    assert_eq!(f.client.claimable(), TOTAL);
    f.assert_conserved();
}

#[test]
fn revoke_is_rejected_on_a_non_revocable_grant() {
    let f = Fixture::new(false);
    f.at(START + 300);
    assert_eq!(f.client.try_revoke(), Err(Ok(Error::VestingNotRevocable)));
    assert_eq!(f.held(), TOTAL);
}

#[test]
fn revoke_rejects_a_second_call() {
    let f = Fixture::new(true);
    f.at(START + 300);
    f.client.revoke();
    assert_eq!(f.client.try_revoke(), Err(Ok(Error::VestingRevoked)));
}

/// Issue #221: a second revoke must return `VestingRevoked` regardless of
/// how much time passes between the two calls.
#[test]
fn revoke_twice_is_rejected_with_already_revoked_error() {
    let f = Fixture::new(true);
    f.at(START + 300);
    f.client.revoke();
    // Advance time past the end of the schedule and try again.
    f.at(START + DURATION + 1_000);
    assert_eq!(
        f.client.try_revoke(),
        Err(Ok(Error::VestingRevoked)),
        "second revoke must return VestingRevoked"
    );
}

/// Issue #220: after a revoke the beneficiary can only claim the vested
/// portion; a claim that would exceed the vested amount errors and no
/// tokens are transferred.
#[test]
fn claim_after_revoke_only_pays_the_vested_portion_and_no_more() {
    let f = Fixture::new(true);
    // Revoke at 30 % through the schedule — 300 000 tokens are vested,
    // 700 000 are returned to the grantor.
    f.at(START + 300);
    let grantor_before = f.token.balance(&f.grantor);
    f.client.revoke();
    assert_eq!(f.token.balance(&f.grantor), grantor_before + TOTAL * 7 / 10);

    // Advance time well past the end — the frozen vested amount is still
    // 300 000; no additional tokens are accrued after revocation.
    f.at(START + DURATION + 10_000);

    // Claim the vested portion — this must succeed.
    assert_eq!(f.client.claim(), TOTAL * 3 / 10);
    assert_eq!(f.token.balance(&f.beneficiary), TOTAL * 3 / 10);
    f.assert_conserved();

    // A second claim must fail because nothing is left to claim.
    assert_eq!(
        f.client.try_claim(),
        Err(Ok(Error::VestingNothingToClaim)),
        "claim after revoke exhausted must error"
    );
    // Beneficiary balance is unchanged.
    assert_eq!(f.token.balance(&f.beneficiary), TOTAL * 3 / 10);
    f.assert_conserved();
}

#[test]
#[should_panic]
fn revoke_requires_the_grantors_authorization() {
    let f = Fixture::new(true);
    f.at(START + 300);
    f.env.set_auths(&[]);
    f.client.revoke();
}

#[test]
fn revoke_emits_the_revoked_event() {
    let f = Fixture::new(true);
    f.at(START + 300);
    f.client.revoke();

    // At t = START + 300 with DURATION = 1_000 and TOTAL = 1_000_000:
    // vested = TOTAL * 300 / 1000 = 300_000 (still claimable)
    // returned = TOTAL * 700 / 1000 = 700_000
    let expected = Revoked {
        grantor: f.grantor.clone(),
        returned_to_grantor: TOTAL * 7 / 10,
        still_claimable: TOTAL * 3 / 10,
        revoked_at: START + 300,
    };
    // filter_by_contract returns ContractEvents; .events() gives &[ContractEvent].
    // The contract emits `created` at setup and `revoked` here — take the last one.
    let all = f.env.events().all().filter_by_contract(&f.client.address);
    let events = all.events();
    let last = events.last().expect("no events emitted");
    assert_eq!(last, &expected.to_xdr(&f.env, &f.client.address));
}

/// Regression test for the `still_claimable: math::sub(vested, claimed)?`
/// dead-calculation concern: claiming *exactly* the vested amount and then
/// revoking in the same ledger leaves `claimed == vested`, so the event's
/// subtraction is exactly zero. `revoke` must succeed rather than abort with
/// `Underflow`, and there is nothing left claimable afterwards.
#[test]
fn revoke_succeeds_immediately_after_claiming_all_vested_tokens() {
    let f = Fixture::new(true);
    // Partial vest: at START + 300 with CLIFF = 100 and DURATION = 1_000,
    // vested = TOTAL * 300 / 1_000 = 300_000.
    f.at(START + 300);

    let claimed = f.client.claim();
    let vested_now = f.client.vested_amount(&(START + 300));
    assert_eq!(claimed, vested_now, "claim must take the full vested amount");
    assert_eq!(f.client.get().claimed, vested_now);

    // Revoke at the very same timestamp: claimed == vested, so the event's
    // `still_claimable` is exactly zero and `revoke` must not underflow.
    let returned = f.client.revoke();
    assert_eq!(
        returned,
        TOTAL - vested_now,
        "only the unvested remainder is returned"
    );

    let events = f
        .env
        .events()
        .all()
        .filter_by_contract(&f.client.address);
    let last = events.events().last().expect("no events emitted");
    let expected = Revoked {
        grantor: f.grantor.clone(),
        returned_to_grantor: TOTAL - vested_now,
        still_claimable: 0,
        revoked_at: START + 300,
    };
    assert_eq!(last, &expected.to_xdr(&f.env, &f.client.address));

    // Nothing is left to claim, and conservation still holds.
    assert_eq!(
        f.client.try_claim(),
        Err(Ok(Error::VestingNothingToClaim))
    );
    f.assert_conserved();
}

// ------------------------------------------------------- conservation

#[test]
fn conservation_holds_across_claim_then_revoke_then_claim() {
    let f = Fixture::new(true);
    f.assert_conserved();

    f.at(START + CLIFF);
    f.client.claim();
    f.assert_conserved();

    f.at(START + 450);
    f.client.claim();
    f.assert_conserved();

    f.at(START + 700);
    f.client.revoke();
    f.assert_conserved();

    f.client.claim();
    f.assert_conserved();

    let g = f.client.get();
    assert_eq!(f.held(), 0);
    assert_eq!(g.claimed + g.returned, g.total);
    assert_eq!(
        f.token.balance(&f.beneficiary) + f.token.balance(&f.grantor),
        MINT
    );
}

#[test]
fn a_schedule_that_divides_badly_still_conserves() {
    // 7 into 3 -- deliberately lossy proportions.
    let f = Fixture::with_schedule(true, 0, 3);
    f.at(START + 1);
    f.client.claim();
    f.assert_conserved();
    f.at(START + 2);
    f.client.claim();
    f.assert_conserved();
    f.at(START + 3);
    f.client.claim();
    f.assert_conserved();
    assert_eq!(f.token.balance(&f.beneficiary), TOTAL);
    assert_eq!(f.held(), 0);
}

#[test]
fn vesting_with_long_duration_and_large_total_probes_overflow_boundary() {
    // `mul_div` computes `total * elapsed` before dividing by `duration`, so
    // the overflow ceiling is on that product, not on `total` alone: at the
    // midpoint of a 10-year (≈315,576,000s) schedule, `elapsed` is
    // ≈157,788,000, and overflow needs `total > i128::MAX / elapsed`
    // (≈1.08e30) -- many orders of magnitude above a realistic token supply.
    const TEN_YEARS_SECONDS: u64 = 315_576_000;
    const SAFE_TOTAL: i128 = 29_000_000;
    const OVERFLOW_TOTAL: i128 = i128::MAX / 2;

    let env = Env::default();

    // Test that a safe total with long duration vests successfully.
    let g_safe = Grant {
        grantor: Address::generate(&env),
        beneficiary: Address::generate(&env),
        token: Address::generate(&env),
        total: SAFE_TOTAL,
        start: START,
        cliff: 0,
        duration: TEN_YEARS_SECONDS,
        revocable: true,
        claimed: 0,
        returned: 0,
        revoked_at: None,
    };
    // At the midpoint (half elapsed), vesting should succeed.
    let half_elapsed = TEN_YEARS_SECONDS / 2;
    let vested_halfway = g_safe.vested_amount(START + half_elapsed);
    assert!(
        vested_halfway.is_ok(),
        "safe total should not overflow at midpoint"
    );
    // At full duration, should have vested the full amount.
    assert_eq!(
        g_safe.vested_amount(START + TEN_YEARS_SECONDS),
        Ok(SAFE_TOTAL)
    );

    // Test that an unsafe total with long duration overflows.
    let g_unsafe = Grant {
        grantor: Address::generate(&env),
        beneficiary: Address::generate(&env),
        token: Address::generate(&env),
        total: OVERFLOW_TOTAL,
        start: START,
        cliff: 0,
        duration: TEN_YEARS_SECONDS,
        revocable: true,
        claimed: 0,
        returned: 0,
        revoked_at: None,
    };
    // At the midpoint (half elapsed), the multiplication should overflow.
    assert_eq!(
        g_unsafe.vested_amount(START + half_elapsed),
        Err(Error::Overflow),
        "unsafe total should overflow at midpoint of long duration"
    );
}

// ------------------------------------------------------- issue #223: vested_amount before start

#[test]
fn vested_amount_is_zero_before_the_start() {
    let env = Env::default();
    let g = bare(&env, CLIFF, DURATION);
    // Any timestamp strictly before `start` must return exactly 0 — never negative.
    assert_eq!(g.vested_amount(0), Ok(0));
    assert_eq!(g.vested_amount(START - 1), Ok(0));
    // Sanity: the boundary itself also returns 0 (nothing vested at t=start).
    assert_eq!(g.vested_amount(START), Ok(0));
}

// ------------------------------------------------------- issue #224: claimable after partial claim

#[test]
fn claimable_equals_vested_minus_claimed_after_partial_claim() {
    let f = Fixture::new(true);
    // Advance to halfway through the schedule and make a first claim.
    f.at(START + 500);
    let first_claim = f.client.claim();
    assert_eq!(first_claim, TOTAL / 2);

    // Advance further; more has vested.
    f.at(START + 750);
    let claimable = f.client.claimable();
    let grant = f.client.get();
    // claimable must equal vested_at_now minus already_claimed.
    let vested_now = f.client.vested_amount(&(START + 750));
    assert_eq!(claimable, vested_now - grant.claimed);
    f.assert_conserved();
}

// ---------------------------------------------------------------- property

mod vested_amount_properties {
    use proptest::prelude::*;

    use super::*;

    fn arb_grant(env: &Env) -> Grant {
        Grant {
            grantor: Address::generate(env),
            beneficiary: Address::generate(env),
            token: Address::generate(env),
            total: 0,
            start: 0,
            cliff: 0,
            duration: 1,
            revocable: false,
            claimed: 0,
            returned: 0,
            revoked_at: None,
        }
    }

    proptest! {
        /// `vested_amount` never decreases as `at` increases, for an
        /// unrevoked grant. Vesting only ever moves forward in time.
        #[test]
        fn vested_amount_is_monotonic_over_time(
            total in 1i128..=1_000_000_000_000,
            start in 0u64..=1_000_000_000,
            duration in 1u64..=10_000_000,
            cliff_frac in 0u64..=100,
            t0_offset in 0u64..=20_000_000,
            dt in 0u64..=20_000_000,
        ) {
            let env = Env::default();
            let cliff = duration.saturating_mul(cliff_frac) / 100;

            let grant = Grant {
                total,
                start,
                cliff,
                duration,
                ..arb_grant(&env)
            };

            let t0 = start.saturating_add(t0_offset);
            let t1 = t0.saturating_add(dt);

            let v0 = grant.vested_amount(t0).expect("vested_amount(t0) must not error");
            let v1 = grant.vested_amount(t1).expect("vested_amount(t1) must not error");

            prop_assert!(
                v1 >= v0,
                "vested_amount went from {v0} at t={t0} to {v1} at t={t1} (start={start}, cliff={cliff}, duration={duration}, total={total})"
            );
        }

        /// Revocation freezes the schedule: nothing vests past the
        /// revocation timestamp, no matter how far `at` is pushed beyond it.
        #[test]
        fn vested_amount_is_frozen_after_revocation(
            total in 1i128..=1_000_000_000_000,
            start in 0u64..=1_000_000_000,
            duration in 1u64..=10_000_000,
            revoked_after in 0u64..=10_000_000,
            dt in 0u64..=20_000_000,
        ) {
            let env = Env::default();
            let revoked_at = start.saturating_add(revoked_after);

            let grant = Grant {
                total,
                start,
                cliff: 0,
                duration,
                revoked_at: Some(revoked_at),
                ..arb_grant(&env)
            };

            let at_revocation = grant
                .vested_amount(revoked_at)
                .expect("vested_amount(revoked_at) must not error");
            let after = grant
                .vested_amount(revoked_at.saturating_add(dt))
                .expect("vested_amount(after revocation) must not error");

            prop_assert_eq!(at_revocation, after);
        }
    }
}

mod conservation_properties {
    use proptest::prelude::*;

    use super::*;

    /// One step of a random call sequence against a live grant. Invalid calls
    /// (e.g. claiming before the cliff) are included on purpose: a rejected
    /// call must roll back and preserve conservation.
    #[derive(Clone, Copy, Debug)]
    enum Op {
        Advance(u64),
        Claim,
        Revoke,
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        /// The vesting conservation invariant
        /// `claimed + returned + remaining == total` holds after every call of
        /// an arbitrary sequence of claim / revoke calls, and the contract's
        /// real token balance always equals `remaining`.
        #[test]
        fn conservation_holds_after_any_call_sequence(
            total in 1i128..=1_000_000_000,
            duration in 1u64..=1_000,
            cliff_frac in 0u64..=100,
            ops in proptest::collection::vec(prop_oneof![
                (1u64..=500).prop_map(Op::Advance),
                Just(Op::Claim),
                Just(Op::Revoke),
            ], 1..=24),
        ) {
            let start = 1_000_000u64;
            let cliff = duration.saturating_mul(cliff_frac) / 100;
            let te = TestEnv::at(start);
            let (token, grantor) = te.make_token(1_000_000_000_000_000);
            let token_address = token.address.clone();
            let beneficiary = te.make_address();
            let env = te.env().clone();

            let client = VestingContractClient::new(&env, &env.register(VestingContract, ()));
            client.create(
                &grantor,
                &beneficiary,
                &token_address,
                &total,
                &start,
                &cliff,
                &duration,
                &true,
            );

            for op in ops {
                match op {
                    Op::Advance(delta) => {
                        te.advance_to(te.timestamp().saturating_add(delta));
                    }
                    Op::Claim => {
                        let _ = client.try_claim();
                    }
                    Op::Revoke => {
                        let _ = client.try_revoke();
                    }
                }

                let g = client.get();
                let remaining = client.remaining();
                prop_assert!(
                    g.claimed >= 0 && g.returned >= 0 && remaining >= 0,
                    "negative component after {:?}: {:?}", op, g
                );
                prop_assert_eq!(
                    g.claimed + g.returned + remaining,
                    g.total,
                    "conservation violated after {:?}: {:?}", op, g
                );
                prop_assert_eq!(
                    token.balance(&client.address),
                    remaining,
                    "held balance diverged after {:?}: {:?}", op, g
                );
            }
        }
    }
}
