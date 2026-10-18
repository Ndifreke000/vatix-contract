#![cfg(test)]

use soroban_sdk::{
    testutils::{Address as _, Ledger as _},
    token::{Client as TokenClient, StellarAssetClient},
    Address, Env, Vec,
};

use crate::{TreasuryContract, TreasuryContractClient, TreasuryError, ADDRESS_TIMELOCK_SECONDS};

// ── helpers ───────────────────────────────────────────────────────────────────

struct Setup {
    env: Env,
    admin: Address,
    market: Address,
    token: Address,
    treasury_id: Address,
    client: TreasuryContractClient<'static>,
}

fn setup() -> Setup {
    let env = Env::default();
    env.mock_all_auths();

    let admin = Address::generate(&env);
    let market = Address::generate(&env);

    let token_admin = Address::generate(&env);
    let token = env
        .register_stellar_asset_contract_v2(token_admin)
        .address();

    let treasury_id = env.register(TreasuryContract, ());
    let client: TreasuryContractClient<'static> =
        unsafe { core::mem::transmute(TreasuryContractClient::new(&env, &treasury_id)) };

    client.initialize(&admin, &market);

    Setup {
        env,
        admin,
        market,
        token,
        treasury_id,
        client,
    }
}

fn fund_treasury_via_fee(s: &Setup, amount: i128) {
    StellarAssetClient::new(&s.env, &s.token).mint(&s.treasury_id, &amount);
    s.client.collect_fee(&s.market, &s.token, &1u32, &amount);
}

// ── Issue #1019: initialize rejects a contract address as admin, and a second call ──

/// `initialize` must reject a deployed contract address as admin with
/// `InvalidAdmin`, before any state is written.
#[test]
fn initialize_rejects_contract_address_as_admin() {
    let env = Env::default();
    env.mock_all_auths();

    // Any deployed contract address works here — register a second,
    // independent treasury instance purely to obtain a contract Address.
    let contract_admin = env.register(TreasuryContract, ());
    let market = Address::generate(&env);

    let id = env.register(TreasuryContract, ());
    let client = TreasuryContractClient::new(&env, &id);

    let err = client
        .try_initialize(&contract_admin, &market)
        .unwrap_err()
        .unwrap();
    assert_eq!(err, TreasuryError::InvalidAdmin);

    // No admin was stored by the rejected call — the contract is still
    // unversioned/uninitialized.
    assert!(client.try_admin().unwrap().is_err());
}

/// `initialize` with a normal user `Address` succeeds.
#[test]
fn initialize_succeeds_with_user_admin() {
    let s = setup();
    assert_eq!(s.client.admin(), s.admin);
    assert!(s.client.is_authorized_market(&s.market));
}

/// A second `initialize` call — with any admin, including a different one —
/// must be rejected with `AlreadyInitialized`, and must not overwrite the
/// existing admin.
#[test]
fn initialize_rejects_second_call() {
    let s = setup();
    let other_admin = Address::generate(&s.env);
    let other_market = Address::generate(&s.env);

    let err = s
        .client
        .try_initialize(&other_admin, &other_market)
        .unwrap_err()
        .unwrap();
    assert_eq!(err, TreasuryError::AlreadyInitialized);

    // Original admin/market registration is untouched.
    assert_eq!(s.client.admin(), s.admin);
    assert!(s.client.is_authorized_market(&s.market));
    assert!(!s.client.is_authorized_market(&other_market));
}

// ── Issue #1022: address timelock blocks execute_market_contract/execute_stakeholders ──

/// `execute_market_contract` called before `effective_at` must return
/// `TimelockNotElapsed`; after the delay it succeeds and the new market
/// becomes authorized.
#[test]
fn execute_market_contract_respects_timelock() {
    let s = setup();
    let new_market = Address::generate(&s.env);

    s.client.propose_market_contract(&s.admin, &new_market);

    let err = s.client.try_execute_market_contract().unwrap_err().unwrap();
    assert_eq!(err, TreasuryError::TimelockNotElapsed);
    assert!(!s.client.is_authorized_market(&new_market));

    s.env.ledger().with_mut(|l| {
        l.timestamp += ADDRESS_TIMELOCK_SECONDS + 1;
    });

    let executed = s.client.execute_market_contract();
    assert_eq!(executed, new_market);
    assert!(s.client.is_authorized_market(&new_market));
    // The original market stays registered — execute appends, not replaces.
    assert!(s.client.is_authorized_market(&s.market));
}

/// `execute_stakeholders` called before `effective_at` must return
/// `TimelockNotElapsed`; after the delay it succeeds and the pending list
/// becomes the active one.
#[test]
fn execute_stakeholders_respects_timelock() {
    let s = setup();
    let holder = Address::generate(&s.env);
    let stakeholders: Vec<(Address, u32)> =
        soroban_sdk::vec![&s.env, (holder.clone(), 10_000u32)];

    s.client.propose_stakeholders(&s.admin, &stakeholders);

    let err = s.client.try_execute_stakeholders().unwrap_err().unwrap();
    assert_eq!(err, TreasuryError::TimelockNotElapsed);
    // The rejected call did not install the pending list as active.
    assert_eq!(s.client.get_stakeholders().len(), 0);

    s.env.ledger().with_mut(|l| {
        l.timestamp += ADDRESS_TIMELOCK_SECONDS + 1;
    });

    s.client.execute_stakeholders();
    let active = s.client.get_stakeholders();
    assert_eq!(active.len(), 1);
    assert_eq!(active.get(0).unwrap(), (holder, 10_000u32));
}

/// `execute_stakeholders` with nothing pending (fresh treasury) must return
/// `NoPendingStakeholderChange`.
#[test]
fn execute_stakeholders_with_no_pending_change_is_rejected() {
    let s = setup();
    let err = s.client.try_execute_stakeholders().unwrap_err().unwrap();
    assert_eq!(err, TreasuryError::NoPendingStakeholderChange);
}

// ── Issue #1018: distribute_fees / stakeholder share validation ──────────────

/// A stakeholder list whose shares sum to 9_999 (below `BPS_DENOMINATOR`) is
/// rejected at `propose_stakeholders` with `InvalidStakeholderWeights`, so it
/// can never become the active list `distribute_fees` would pay out against.
#[test]
fn propose_stakeholders_rejects_sum_below_10000() {
    let s = setup();
    let a = Address::generate(&s.env);
    let b = Address::generate(&s.env);
    let bad: Vec<(Address, u32)> =
        soroban_sdk::vec![&s.env, (a, 4_999u32), (b, 5_000u32)];

    let err = s
        .client
        .try_propose_stakeholders(&s.admin, &bad)
        .unwrap_err()
        .unwrap();
    assert_eq!(err, TreasuryError::InvalidStakeholderWeights);
    assert!(s.client.get_pending_stakeholders().is_none());
}

/// A stakeholder list whose shares sum to 10_001 (above `BPS_DENOMINATOR`) is
/// rejected at `propose_stakeholders` with `InvalidStakeholderWeights`.
#[test]
fn propose_stakeholders_rejects_sum_above_10000() {
    let s = setup();
    let a = Address::generate(&s.env);
    let b = Address::generate(&s.env);
    let bad: Vec<(Address, u32)> =
        soroban_sdk::vec![&s.env, (a, 5_001u32), (b, 5_000u32)];

    let err = s
        .client
        .try_propose_stakeholders(&s.admin, &bad)
        .unwrap_err()
        .unwrap();
    assert_eq!(err, TreasuryError::InvalidStakeholderWeights);
    assert!(s.client.get_pending_stakeholders().is_none());
}

/// An empty stakeholder list is rejected at `propose_stakeholders` with
/// `InvalidStakeholderWeights`.
#[test]
fn propose_stakeholders_rejects_empty_list() {
    let s = setup();
    let empty: Vec<(Address, u32)> = Vec::new(&s.env);

    let err = s
        .client
        .try_propose_stakeholders(&s.admin, &empty)
        .unwrap_err()
        .unwrap();
    assert_eq!(err, TreasuryError::InvalidStakeholderWeights);
}

/// `distribute_fees` on a freshly initialized treasury, before any
/// stakeholder list has ever been configured, returns
/// `NoStakeholdersConfigured`.
#[test]
fn distribute_fees_rejects_when_no_stakeholders_configured() {
    let s = setup();
    fund_treasury_via_fee(&s, 1_000_000i128);

    let err = s
        .client
        .try_distribute_fees(&s.admin, &s.token)
        .unwrap_err()
        .unwrap();
    assert_eq!(err, TreasuryError::NoStakeholdersConfigured);
}

/// A stakeholder list summing to exactly 10_000 is accepted, and once
/// executed `distribute_fees` succeeds and pays out proportionally.
#[test]
fn distribute_fees_succeeds_with_shares_summing_to_10000() {
    let s = setup();
    let a = Address::generate(&s.env);
    let b = Address::generate(&s.env);
    let good: Vec<(Address, u32)> =
        soroban_sdk::vec![&s.env, (a.clone(), 4_000u32), (b.clone(), 6_000u32)];

    s.client.propose_stakeholders(&s.admin, &good);
    s.env.ledger().with_mut(|l| {
        l.timestamp += ADDRESS_TIMELOCK_SECONDS + 1;
    });
    s.client.execute_stakeholders();

    fund_treasury_via_fee(&s, 1_000_000i128);
    s.client.distribute_fees(&s.admin, &s.token);

    assert_eq!(
        TokenClient::new(&s.env, &s.token).balance(&a),
        400_000i128
    );
    assert_eq!(
        TokenClient::new(&s.env, &s.token).balance(&b),
        600_000i128
    );
    assert_eq!(s.client.token_balance(&s.token), 0);
}
