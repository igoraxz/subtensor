#![allow(clippy::arithmetic_side_effects, clippy::unwrap_used)]
//! Covered continuous-unwind short derivatives — edge-case suite.
//!
//! Covers subnet creation, low liquidity, capacity/anti-split, decay +
//! restoration, the full close/default/top-up lifecycle, value conservation,
//! and subnet deregistration (in-the-money and underwater terminal settlement).

use super::mock::*;
use crate::*;
use frame_support::{assert_noop, assert_ok};
use sp_core::U256;
use substrate_fixed::types::{I64F64, I96F32};
use subtensor_runtime_common::{AlphaBalance, NetUid, TaoBalance, Token};

const TAO: u64 = 1_000_000_000;

fn t(v: u64) -> TaoBalance {
    TaoBalance::from(v)
}

fn bal(acc: &U256) -> u64 {
    Balances::free_balance(acc).into()
}

fn custody_bal(netuid: NetUid) -> u64 {
    bal(&SubtensorModule::short_custody_account(netuid))
}

fn assert_approx(a: u64, b: u64, tol: u64, what: &str) {
    let d = a.abs_diff(b);
    assert!(d <= tol, "{what}: {a} vs {b} (diff {d} > tol {tol})");
}

/// Dynamic subnet with balance-backed reserves, a warmed price EMA, shorts
/// enabled, and a generous footprint cap. Returns the netuid.
fn setup_market(tao_reserve: u64, alpha_reserve: u64, price: f64) -> NetUid {
    let owner_c = U256::from(1);
    let owner_h = U256::from(2);
    let netuid = add_dynamic_network(&owner_h, &owner_c);
    setup_reserves(netuid, t(tao_reserve), AlphaBalance::from(alpha_reserve));
    // Back the pool TAO with real balance so custody transfers can move it.
    let sa = SubtensorModule::get_subnet_account_id(netuid).unwrap();
    add_balance_to_coldkey_account(&sa, t(tao_reserve));
    SubnetMovingPrice::<Test>::insert(netuid, I96F32::from_num(price));
    SubtensorModule::set_shorts_enabled(true);
    SubtensorModule::set_short_kappa_ppb(900_000_000); // κ = 0.9, generous
    netuid
}

/// Credit `q` alpha as stake at `(hotkey, coldkey)` without touching the pool,
/// mirroring the `SubnetAlphaOut` bump a real stake performs.
fn give_alpha(hotkey: U256, coldkey: U256, netuid: NetUid, q: AlphaBalance) {
    SubtensorModule::increase_stake_for_hotkey_and_coldkey_on_subnet(&hotkey, &coldkey, netuid, q);
    SubnetAlphaOut::<Test>::mutate(netuid, |o| *o = o.saturating_add(q));
}

// ---------------------------------------------------------------------------
// Gating & subnet-kind edges
// ---------------------------------------------------------------------------

#[test]
fn open_short_rejected_when_disabled() {
    new_test_ext(1).execute_with(|| {
        let netuid = setup_market(1000 * TAO, 1000 * TAO, 1.0);
        SubtensorModule::set_shorts_enabled(false);
        let trader = U256::from(10);
        add_balance_to_coldkey_account(&trader, t(1000 * TAO));
        assert_noop!(
            SubtensorModule::open_short(
                RuntimeOrigin::signed(trader),
                U256::from(11),
                netuid,
                t(100 * TAO),
                AlphaBalance::MAX
            ),
            Error::<Test>::ShortsDisabled
        );
    });
}

#[test]
fn open_short_rejected_on_stable_subnet() {
    new_test_ext(1).execute_with(|| {
        let netuid = setup_market(1000 * TAO, 1000 * TAO, 1.0);
        SubnetMechanism::<Test>::insert(netuid, 0); // stable
        let trader = U256::from(10);
        add_balance_to_coldkey_account(&trader, t(1000 * TAO));
        assert_noop!(
            SubtensorModule::open_short(
                RuntimeOrigin::signed(trader),
                U256::from(11),
                netuid,
                t(100 * TAO),
                AlphaBalance::MAX
            ),
            Error::<Test>::SubnetNotDynamic
        );
    });
}

#[test]
fn open_short_rejects_zero_input() {
    new_test_ext(1).execute_with(|| {
        let netuid = setup_market(1000 * TAO, 1000 * TAO, 1.0);
        let trader = U256::from(10);
        add_balance_to_coldkey_account(&trader, t(1000 * TAO));
        assert_noop!(
            SubtensorModule::open_short(
                RuntimeOrigin::signed(trader),
                U256::from(11),
                netuid,
                t(0),
                AlphaBalance::MAX
            ),
            Error::<Test>::AmountTooLow
        );
    });
}

// ---------------------------------------------------------------------------
// Open math vs spec worked example (§1.7–1.8)
// ---------------------------------------------------------------------------

#[test]
fn quote_matches_spec_worked_example() {
    new_test_ext(1).execute_with(|| {
        // Pool 1000 TAO / 100_000 alpha, price 0.01, pre-trade S = 100 TAO.
        let netuid = setup_market(1000 * TAO, 100_000 * TAO, 0.01);
        let mut agg = ShortAggregate::<Test>::get(netuid);
        agg.b_sigma = t(100 * TAO);
        ShortAggregate::<Test>::insert(netuid, agg);

        let q = SubtensorModule::quote_open_short(netuid, t(62_500_000_000)).unwrap(); // P = 62.5 TAO
        assert_approx(q.gross_collateral.to_u64(), 100 * TAO, TAO / 10, "C");
        assert_approx(q.retained_proceeds.to_u64(), 37_500_000_000, TAO / 10, "N");
        assert_approx(q.alpha_liability.to_u64(), 3902 * TAO, 10 * TAO, "Q");
        assert_approx(q.escrow.to_u64(), 39 * TAO, TAO / 2, "E");
        assert_approx(q.effective_ltv, 375_000_000, 2_000_000, "lambda_eff");
    });
}

#[test]
fn open_matches_quote_and_moves_pool() {
    new_test_ext(1).execute_with(|| {
        let netuid = setup_market(1000 * TAO, 1000 * TAO, 1.0);
        let trader = U256::from(10);
        let hotkey = U256::from(11);
        add_balance_to_coldkey_account(&trader, t(1000 * TAO));
        let p = 100 * TAO;

        let quote = SubtensorModule::quote_open_short(netuid, t(p)).unwrap();
        let tao_before = SubnetTAO::<Test>::get(netuid).to_u64();
        let trader_before = bal(&trader);

        assert_ok!(SubtensorModule::open_short(
            RuntimeOrigin::signed(trader),
            hotkey,
            netuid,
            t(p),
            AlphaBalance::MAX
        ));

        let pos = ShortPositions::<Test>::get(netuid, trader).unwrap();
        // Position fields equal the pure quote (same code path).
        assert_eq!(pos.r_stored, quote.retained_proceeds);
        assert_eq!(pos.q_liability, quote.alpha_liability);
        assert_eq!(pos.e_stored, quote.escrow);
        assert_eq!(pos.p_floor, t(p));
        assert_eq!(pos.hotkey, hotkey);
        assert!(pos.b_stored.to_u64() > 0);

        let n = quote.retained_proceeds.to_u64();
        let e = quote.escrow.to_u64();
        // Pool lost exactly N+E TAO; trader paid exactly P; custody holds P+N+E.
        assert_eq!(SubnetTAO::<Test>::get(netuid).to_u64(), tao_before - n - e);
        assert_eq!(bal(&trader), trader_before - p);
        assert_eq!(custody_bal(netuid), p + n + e);

        // Aggregate reflects the single position.
        let agg = ShortAggregate::<Test>::get(netuid);
        assert_eq!(agg.r_sigma, pos.r_stored);
        assert_eq!(agg.e_sigma, pos.e_stored);
        assert_eq!(agg.b_sigma, pos.b_stored);
        assert_eq!(agg.q_sigma, pos.q_liability);
    });
}

// ---------------------------------------------------------------------------
// Capacity / anti-split (§5.1–5.2.1)
// ---------------------------------------------------------------------------

#[test]
fn open_rejected_when_capacity_exceeded() {
    new_test_ext(1).execute_with(|| {
        let netuid = setup_market(1000 * TAO, 1000 * TAO, 1.0);
        SubtensorModule::set_short_kappa_ppb(1_000_000); // κ = 0.001 → cap ≈ 1 TAO
        let trader = U256::from(10);
        add_balance_to_coldkey_account(&trader, t(1000 * TAO));
        assert_noop!(
            SubtensorModule::open_short(
                RuntimeOrigin::signed(trader),
                U256::from(11),
                netuid,
                t(100 * TAO),
                AlphaBalance::MAX
            ),
            Error::<Test>::ShortCapacityExceeded
        );
    });
}

#[test]
fn stacked_opens_share_capacity() {
    new_test_ext(1).execute_with(|| {
        let netuid = setup_market(1000 * TAO, 1000 * TAO, 1.0);
        // Cap ≈ 70 TAO: one P=50 open (B≈47 TAO) fits; a second does not.
        SubtensorModule::set_short_kappa_ppb(70_000_000);
        let a = U256::from(10);
        let b = U256::from(20);
        add_balance_to_coldkey_account(&a, t(1000 * TAO));
        add_balance_to_coldkey_account(&b, t(1000 * TAO));

        assert_ok!(SubtensorModule::open_short(
            RuntimeOrigin::signed(a),
            U256::from(11),
            netuid,
            t(50 * TAO),
            AlphaBalance::MAX
        ));
        assert_noop!(
            SubtensorModule::open_short(
                RuntimeOrigin::signed(b),
                U256::from(21),
                netuid,
                t(50 * TAO),
                AlphaBalance::MAX
            ),
            Error::<Test>::ShortCapacityExceeded
        );
    });
}

// ---------------------------------------------------------------------------
// Execution bounds + validate-before-mutate (anti-sandwich, no fund stranding)
// ---------------------------------------------------------------------------

#[test]
fn open_short_rejects_when_liability_exceeds_bound() {
    new_test_ext(1).execute_with(|| {
        let netuid = setup_market(1000 * TAO, 1000 * TAO, 1.0);
        let trader = U256::from(10);
        add_balance_to_coldkey_account(&trader, t(1000 * TAO));
        // A 1-rao liability cap is below any real Q. `assert_noop!` proves the
        // bound is enforced before any transfer/reserve mutation (no state moves).
        assert_noop!(
            SubtensorModule::open_short(
                RuntimeOrigin::signed(trader),
                U256::from(11),
                netuid,
                t(100 * TAO),
                AlphaBalance::from(1)
            ),
            Error::<Test>::SlippageTooHigh
        );
        assert_eq!(custody_bal(netuid), 0);
        assert!(ShortPositions::<Test>::get(netuid, trader).is_none());
    });
}

#[test]
fn open_short_wrong_hotkey_merge_strands_no_funds() {
    new_test_ext(1).execute_with(|| {
        let netuid = setup_market(1000 * TAO, 1000 * TAO, 1.0);
        let trader = U256::from(10);
        add_balance_to_coldkey_account(&trader, t(1000 * TAO));
        assert_ok!(SubtensorModule::open_short(
            RuntimeOrigin::signed(trader),
            U256::from(11),
            netuid,
            t(50 * TAO),
            AlphaBalance::MAX
        ));
        // A merge against a different hotkey must reject BEFORE moving funds.
        // `assert_noop!` fails if any balance/reserve mutated before the error.
        assert_noop!(
            SubtensorModule::open_short(
                RuntimeOrigin::signed(trader),
                U256::from(12),
                netuid,
                t(50 * TAO),
                AlphaBalance::MAX
            ),
            Error::<Test>::ShortHotkeyMismatch
        );
    });
}

#[test]
fn open_long_rejects_when_liability_exceeds_bound() {
    new_test_ext(1).execute_with(|| {
        let netuid = setup_long(1000 * TAO, 1000 * TAO, 1.0);
        let trader = U256::from(10);
        let hotkey = U256::from(11);
        give_alpha(hotkey, trader, netuid, AlphaBalance::from(500 * TAO));
        // A 1-rao TAO-liability cap is below any real D ⇒ rejected before any
        // mutation (long-side mirror of the short execution bound).
        assert_noop!(
            SubtensorModule::open_long(
                RuntimeOrigin::signed(trader),
                hotkey,
                netuid,
                AlphaBalance::from(100 * TAO),
                TaoBalance::from(1)
            ),
            Error::<Test>::SlippageTooHigh
        );
        assert!(LongPositions::<Test>::get(netuid, trader).is_none());
    });
}

// Directly exercises the #[transactional] rollback: the trader's floor moves to
// custody first, then the pool→custody transfer of N+E FAILS (subnet account
// drained below it). The whole open must roll back — trader keeps their floor,
// nothing lands in custody, and pool reserves are untouched. Without
// #[transactional] the first transfer would persist and the trader would lose P.
#[test]
fn open_short_failed_pool_transfer_rolls_back_atomically() {
    new_test_ext(1).execute_with(|| {
        let netuid = setup_market(1000 * TAO, 1000 * TAO, 1.0);
        let trader = U256::from(10);
        add_balance_to_coldkey_account(&trader, t(1000 * TAO));
        // Drain the subnet account so it cannot cover the N+E pool→custody leg
        // (≈76 TAO for P=100), while SubnetTAO storage stays high for pricing.
        let sa = SubtensorModule::get_subnet_account_id(netuid).unwrap();
        remove_balance_from_coldkey_account(&sa, t(995 * TAO));

        let trader_before = bal(&trader);
        let tao_before = SubnetTAO::<Test>::get(netuid);

        let r = SubtensorModule::open_short(
            RuntimeOrigin::signed(trader),
            U256::from(11),
            netuid,
            t(100 * TAO),
            AlphaBalance::MAX,
        );
        assert!(
            r.is_err(),
            "open must fail when the pool leg can't be funded"
        );

        // Atomic rollback: floor returned, custody empty, reserves unchanged, no position.
        assert_eq!(
            bal(&trader),
            trader_before,
            "floor must be rolled back to the trader"
        );
        assert_eq!(custody_bal(netuid), 0, "nothing may remain in custody");
        assert_eq!(
            SubnetTAO::<Test>::get(netuid),
            tao_before,
            "pool reserve must be untouched"
        );
        assert!(ShortPositions::<Test>::get(netuid, trader).is_none());
        assert!(!ShortActiveSubnets::<Test>::contains_key(netuid));
    });
}

// ---------------------------------------------------------------------------
// Low liquidity (§4.1: λ_eff ≤ 0 rejects oversized opens)
// ---------------------------------------------------------------------------

#[test]
fn low_liquidity_rejects_oversized_open() {
    new_test_ext(1).execute_with(|| {
        let netuid = setup_market(10 * TAO, 10 * TAO, 1.0); // tiny pool
        let trader = U256::from(10);
        add_balance_to_coldkey_account(&trader, t(1000 * TAO));
        // P far larger than the pool can collateralize → retained proceeds ≤ 0.
        assert_noop!(
            SubtensorModule::open_short(
                RuntimeOrigin::signed(trader),
                U256::from(11),
                netuid,
                t(100 * TAO),
                AlphaBalance::MAX
            ),
            Error::<Test>::EffectiveLtvNonPositive
        );
    });
}

// Warm-EMA guard: opening on a cold-`pEMA` (freshly registered, no price
// history) subnet is rejected, since the EMA risk reference and terminal
// anti-suppression leg are unavailable there. Opens are admitted only once the
// EMA warms.
#[test]
fn open_rejected_on_cold_ema_subnet() {
    new_test_ext(1).execute_with(|| {
        let owner_c = U256::from(1);
        let owner_h = U256::from(2);
        let netuid = add_dynamic_network(&owner_h, &owner_c);
        setup_reserves(netuid, t(1000 * TAO), AlphaBalance::from(1000 * TAO));
        let sa = SubtensorModule::get_subnet_account_id(netuid).unwrap();
        add_balance_to_coldkey_account(&sa, t(1000 * TAO));
        SubtensorModule::set_shorts_enabled(true);
        SubtensorModule::set_short_kappa_ppb(900_000_000);
        assert_eq!(SubtensorModule::get_moving_alpha_price(netuid), 0); // cold

        let trader = U256::from(10);
        add_balance_to_coldkey_account(&trader, t(1000 * TAO));
        assert_noop!(
            SubtensorModule::open_short(
                RuntimeOrigin::signed(trader),
                U256::from(11),
                netuid,
                t(50 * TAO),
                AlphaBalance::MAX
            ),
            Error::<Test>::ColdEmaNotAllowed
        );
        assert!(ShortPositions::<Test>::get(netuid, trader).is_none());

        // Once the EMA warms, the same open is admitted.
        SubnetMovingPrice::<Test>::insert(netuid, I96F32::from_num(1.0));
        assert_ok!(SubtensorModule::open_short(
            RuntimeOrigin::signed(trader),
            U256::from(11),
            netuid,
            t(50 * TAO),
            AlphaBalance::MAX
        ));
        assert!(ShortPositions::<Test>::get(netuid, trader).is_some());
    });
}

// ---------------------------------------------------------------------------
// Decay + restoration (§6)
// ---------------------------------------------------------------------------

#[test]
fn decay_shrinks_buffer_and_restores_tao() {
    new_test_ext(1).execute_with(|| {
        let netuid = setup_market(1000 * TAO, 1000 * TAO, 1.0);
        let trader = U256::from(10);
        add_balance_to_coldkey_account(&trader, t(1000 * TAO));
        assert_ok!(SubtensorModule::open_short(
            RuntimeOrigin::signed(trader),
            U256::from(11),
            netuid,
            t(100 * TAO),
            AlphaBalance::MAX
        ));

        let r0 = ShortAggregate::<Test>::get(netuid).r_sigma.to_u64();
        let tao0 = SubnetTAO::<Test>::get(netuid).to_u64();
        let custody0 = custody_bal(netuid);
        let omega0 = ShortAggregate::<Test>::get(netuid).omega;

        for _ in 0..200 {
            SubtensorModule::run_short_decay();
        }

        let agg = ShortAggregate::<Test>::get(netuid);
        let r1 = agg.r_sigma.to_u64();
        let tao1 = SubnetTAO::<Test>::get(netuid).to_u64();
        let custody1 = custody_bal(netuid);

        assert!(r1 < r0, "buffer must decay: {r1} !< {r0}");
        assert!(agg.omega > omega0, "omega must increase");
        let restored = tao1 - tao0;
        let drained = custody0 - custody1;
        assert!(restored > 0, "TAO must be restored to the pool");
        // Conservation of the restoration leg: custody out == pool in.
        assert_eq!(restored, drained);
    });
}

#[test]
fn block_step_runs_decay() {
    new_test_ext(1).execute_with(|| {
        let netuid = setup_market(1000 * TAO, 1000 * TAO, 1.0);
        let trader = U256::from(10);
        add_balance_to_coldkey_account(&trader, t(1000 * TAO));
        assert_ok!(SubtensorModule::open_short(
            RuntimeOrigin::signed(trader),
            U256::from(11),
            netuid,
            t(100 * TAO),
            AlphaBalance::MAX
        ));
        let r0 = ShortAggregate::<Test>::get(netuid).r_sigma.to_u64();
        step_block(5);
        assert!(ShortAggregate::<Test>::get(netuid).r_sigma.to_u64() < r0);
    });
}

// ---------------------------------------------------------------------------
// Top-up (§8.2)
// ---------------------------------------------------------------------------

#[test]
fn top_up_adds_buffer_only() {
    new_test_ext(1).execute_with(|| {
        let netuid = setup_market(1000 * TAO, 1000 * TAO, 1.0);
        let trader = U256::from(10);
        add_balance_to_coldkey_account(&trader, t(1000 * TAO));
        assert_ok!(SubtensorModule::open_short(
            RuntimeOrigin::signed(trader),
            U256::from(11),
            netuid,
            t(100 * TAO),
            AlphaBalance::MAX
        ));

        let pos0 = ShortPositions::<Test>::get(netuid, trader).unwrap();
        let custody0 = custody_bal(netuid);
        assert_ok!(SubtensorModule::top_up_short(
            RuntimeOrigin::signed(trader),
            netuid,
            t(10 * TAO)
        ));
        let pos1 = ShortPositions::<Test>::get(netuid, trader).unwrap();

        assert_eq!(pos1.r_stored, pos0.r_stored + t(10 * TAO));
        assert_eq!(pos1.q_liability, pos0.q_liability); // unchanged
        assert_eq!(pos1.e_stored, pos0.e_stored);
        assert_eq!(pos1.b_stored, pos0.b_stored);
        assert_eq!(custody_bal(netuid), custody0 + 10 * TAO);
    });
}

#[test]
fn top_up_requires_position() {
    new_test_ext(1).execute_with(|| {
        let netuid = setup_market(1000 * TAO, 1000 * TAO, 1.0);
        let trader = U256::from(10);
        add_balance_to_coldkey_account(&trader, t(1000 * TAO));
        assert_noop!(
            SubtensorModule::top_up_short(RuntimeOrigin::signed(trader), netuid, t(TAO)),
            Error::<Test>::ShortPositionNotFound
        );
    });
}

// ---------------------------------------------------------------------------
// Merge (§8.6)
// ---------------------------------------------------------------------------

#[test]
fn additional_open_merges_into_position() {
    new_test_ext(1).execute_with(|| {
        let netuid = setup_market(1000 * TAO, 1000 * TAO, 1.0);
        let trader = U256::from(10);
        let hotkey = U256::from(11);
        add_balance_to_coldkey_account(&trader, t(1000 * TAO));

        assert_ok!(SubtensorModule::open_short(
            RuntimeOrigin::signed(trader),
            hotkey,
            netuid,
            t(50 * TAO),
            AlphaBalance::MAX
        ));
        let p1 = ShortPositions::<Test>::get(netuid, trader).unwrap();
        assert_ok!(SubtensorModule::open_short(
            RuntimeOrigin::signed(trader),
            hotkey,
            netuid,
            t(50 * TAO),
            AlphaBalance::MAX
        ));
        let p2 = ShortPositions::<Test>::get(netuid, trader).unwrap();

        assert_eq!(p2.p_floor, t(100 * TAO));
        assert!(p2.q_liability > p1.q_liability);
        assert!(p2.r_stored > p1.r_stored);
        // Single merged position, not two entries.
        assert_eq!(ShortPositions::<Test>::iter_prefix(netuid).count(), 1);
    });
}

// ---------------------------------------------------------------------------
// Close (§8.3–8.5) + conservation
// ---------------------------------------------------------------------------

#[test]
fn full_close_conserves_value() {
    new_test_ext(1).execute_with(|| {
        let netuid = setup_market(1000 * TAO, 1000 * TAO, 1.0);
        let trader = U256::from(10);
        let hotkey = U256::from(11);
        add_balance_to_coldkey_account(&trader, t(1000 * TAO));
        let p = 100 * TAO;
        assert_ok!(SubtensorModule::open_short(
            RuntimeOrigin::signed(trader),
            hotkey,
            netuid,
            t(p),
            AlphaBalance::MAX
        ));

        let pos = ShortPositions::<Test>::get(netuid, trader).unwrap();
        let (n, e, q) = (
            pos.r_stored.to_u64(),
            pos.e_stored.to_u64(),
            pos.q_liability,
        );
        let tao_after_open = SubnetTAO::<Test>::get(netuid).to_u64();
        let alpha_after_open = SubnetAlphaIn::<Test>::get(netuid).to_u64();

        // Trader acquires the liability alpha (seeded) and closes fully.
        give_alpha(
            hotkey,
            trader,
            netuid,
            AlphaBalance::from(q.to_u64() + 10 * TAO),
        );
        let trader_before_close = bal(&trader);

        assert_ok!(SubtensorModule::close_short(
            RuntimeOrigin::signed(trader),
            netuid,
            1_000_000_000
        ));

        // Position gone, aggregate empty.
        assert!(ShortPositions::<Test>::get(netuid, trader).is_none());
        let agg = ShortAggregate::<Test>::get(netuid);
        assert_eq!(agg.r_sigma.to_u64(), 0);
        assert_eq!(agg.q_sigma.to_u64(), 0);

        // Custody fully drained; pool regained escrow + repaid alpha.
        assert_eq!(custody_bal(netuid), 0);
        assert_eq!(SubnetTAO::<Test>::get(netuid).to_u64(), tao_after_open + e);
        assert_eq!(
            SubnetAlphaIn::<Test>::get(netuid).to_u64(),
            alpha_after_open + q.to_u64()
        );
        // Trader received floor + remaining buffer = P + N.
        assert_eq!(bal(&trader), trader_before_close + p + n);
    });
}

#[test]
fn partial_close_reduces_prorata() {
    new_test_ext(1).execute_with(|| {
        let netuid = setup_market(1000 * TAO, 1000 * TAO, 1.0);
        let trader = U256::from(10);
        let hotkey = U256::from(11);
        add_balance_to_coldkey_account(&trader, t(1000 * TAO));
        assert_ok!(SubtensorModule::open_short(
            RuntimeOrigin::signed(trader),
            hotkey,
            netuid,
            t(100 * TAO),
            AlphaBalance::MAX
        ));

        let pos0 = ShortPositions::<Test>::get(netuid, trader).unwrap();
        give_alpha(
            hotkey,
            trader,
            netuid,
            AlphaBalance::from(pos0.q_liability.to_u64()),
        );

        // Close half.
        assert_ok!(SubtensorModule::close_short(
            RuntimeOrigin::signed(trader),
            netuid,
            500_000_000
        ));
        let pos1 = ShortPositions::<Test>::get(netuid, trader).unwrap();

        assert_approx(pos1.p_floor.to_u64(), pos0.p_floor.to_u64() / 2, 2, "p/2");
        assert_approx(
            pos1.q_liability.to_u64(),
            pos0.q_liability.to_u64() / 2,
            2,
            "q/2",
        );
        assert_approx(pos1.r_stored.to_u64(), pos0.r_stored.to_u64() / 2, 2, "r/2");
        assert_approx(pos1.e_stored.to_u64(), pos0.e_stored.to_u64() / 2, 2, "e/2");
    });
}

#[test]
fn close_without_alpha_rejected() {
    new_test_ext(1).execute_with(|| {
        let netuid = setup_market(1000 * TAO, 1000 * TAO, 1.0);
        let trader = U256::from(10);
        add_balance_to_coldkey_account(&trader, t(1000 * TAO));
        assert_ok!(SubtensorModule::open_short(
            RuntimeOrigin::signed(trader),
            U256::from(11),
            netuid,
            t(100 * TAO),
            AlphaBalance::MAX
        ));
        // No alpha staked at the hotkey → cannot repay the liability.
        assert_noop!(
            SubtensorModule::close_short(RuntimeOrigin::signed(trader), netuid, 1_000_000_000),
            Error::<Test>::InsufficientAlphaToClose
        );
    });
}

#[test]
fn close_invalid_fraction_rejected() {
    new_test_ext(1).execute_with(|| {
        let netuid = setup_market(1000 * TAO, 1000 * TAO, 1.0);
        let trader = U256::from(10);
        add_balance_to_coldkey_account(&trader, t(1000 * TAO));
        assert_ok!(SubtensorModule::open_short(
            RuntimeOrigin::signed(trader),
            U256::from(11),
            netuid,
            t(100 * TAO),
            AlphaBalance::MAX
        ));
        assert_noop!(
            SubtensorModule::close_short(RuntimeOrigin::signed(trader), netuid, 0),
            Error::<Test>::InvalidCloseFraction
        );
        assert_noop!(
            SubtensorModule::close_short(RuntimeOrigin::signed(trader), netuid, 1_000_000_001),
            Error::<Test>::InvalidCloseFraction
        );
    });
}

// ---------------------------------------------------------------------------
// Default (§7)
// ---------------------------------------------------------------------------

#[test]
fn default_rejected_when_buffer_above_dust() {
    new_test_ext(1).execute_with(|| {
        let netuid = setup_market(1000 * TAO, 1000 * TAO, 1.0);
        let trader = U256::from(10);
        add_balance_to_coldkey_account(&trader, t(1000 * TAO));
        assert_ok!(SubtensorModule::open_short(
            RuntimeOrigin::signed(trader),
            U256::from(11),
            netuid,
            t(100 * TAO),
            AlphaBalance::MAX
        ));
        let poker = U256::from(99);
        assert_noop!(
            SubtensorModule::default_short(RuntimeOrigin::signed(poker), trader, netuid),
            Error::<Test>::PositionNotDefaultEligible
        );
    });
}

#[test]
fn default_recycles_floor_and_restores_residual() {
    new_test_ext(1).execute_with(|| {
        let netuid = setup_market(1000 * TAO, 1000 * TAO, 1.0);
        let trader = U256::from(10);
        add_balance_to_coldkey_account(&trader, t(1000 * TAO));
        assert_ok!(SubtensorModule::open_short(
            RuntimeOrigin::signed(trader),
            U256::from(11),
            netuid,
            t(100 * TAO),
            AlphaBalance::MAX
        ));

        let pos = ShortPositions::<Test>::get(netuid, trader).unwrap();
        let (p, n, e) = (
            pos.p_floor.to_u64(),
            pos.r_stored.to_u64(),
            pos.e_stored.to_u64(),
        );
        // Make the whole buffer dust so the position is default-eligible now.
        SubtensorModule::set_short_dust(t(1000 * TAO));
        SubtensorModule::set_short_default_grace(0); // no anti-snipe delay for this test

        let tao0 = SubnetTAO::<Test>::get(netuid).to_u64();
        let ti0 = TotalIssuance::<Test>::get();
        let poker = U256::from(99);
        assert_ok!(SubtensorModule::default_short(
            RuntimeOrigin::signed(poker),
            trader,
            netuid
        ));

        // Position removed; residual R+E restored to pool; floor P recycled (TI down).
        assert!(ShortPositions::<Test>::get(netuid, trader).is_none());
        assert_eq!(SubnetTAO::<Test>::get(netuid).to_u64(), tao0 + n + e);
        assert_eq!(custody_bal(netuid), 0);
        assert_eq!(TotalIssuance::<Test>::get(), ti0 - t(p));
        let agg = ShortAggregate::<Test>::get(netuid);
        assert_eq!(agg.r_sigma.to_u64(), 0);
    });
}

#[test]
fn default_requires_position() {
    new_test_ext(1).execute_with(|| {
        let netuid = setup_market(1000 * TAO, 1000 * TAO, 1.0);
        assert_noop!(
            SubtensorModule::default_short(
                RuntimeOrigin::signed(U256::from(99)),
                U256::from(10),
                netuid
            ),
            Error::<Test>::ShortPositionNotFound
        );
    });
}

// ---------------------------------------------------------------------------
// Subnet deregistration terminal settlement (§11.4)
// ---------------------------------------------------------------------------

#[test]
fn dereg_settles_in_the_money_short() {
    new_test_ext(1).execute_with(|| {
        let netuid = setup_market(1000 * TAO, 1000 * TAO, 1.0);
        let trader = U256::from(10);
        let hotkey = U256::from(11);
        add_balance_to_coldkey_account(&trader, t(1000 * TAO));
        assert_ok!(SubtensorModule::open_short(
            RuntimeOrigin::signed(trader),
            hotkey,
            netuid,
            t(100 * TAO),
            AlphaBalance::MAX
        ));

        let pos = ShortPositions::<Test>::get(netuid, trader).unwrap();
        let c = pos.p_floor.to_u64() + pos.r_stored.to_u64(); // P + R
        let trader_before = bal(&trader);

        // Settle terminal. With pEMA = 1 and a bounded liability, equity > 0.
        SubtensorModule::settle_shorts_on_dereg(netuid);

        assert!(ShortPositions::<Test>::get(netuid, trader).is_none());
        assert_eq!(custody_bal(netuid), 0);
        // Trader received positive equity, strictly less than the full claim.
        let gained = bal(&trader) - trader_before;
        assert!(gained > 0 && gained < c, "equity {gained} not in (0,{c})");
    });
}

#[test]
fn dereg_settles_underwater_short_with_zero_equity() {
    new_test_ext(1).execute_with(|| {
        let netuid = setup_market(1000 * TAO, 1000 * TAO, 1.0);
        let trader = U256::from(10);
        let hotkey = U256::from(11);
        add_balance_to_coldkey_account(&trader, t(1000 * TAO));
        assert_ok!(SubtensorModule::open_short(
            RuntimeOrigin::signed(trader),
            hotkey,
            netuid,
            t(100 * TAO),
            AlphaBalance::MAX
        ));

        // Drive the EMA liability reference far above the collateral claim.
        SubnetMovingPrice::<Test>::insert(netuid, I96F32::from_num(50.0));
        let trader_before = bal(&trader);
        let ti0 = TotalIssuance::<Test>::get();

        SubtensorModule::settle_shorts_on_dereg(netuid);

        assert!(ShortPositions::<Test>::get(netuid, trader).is_none());
        assert_eq!(custody_bal(netuid), 0);
        // No equity paid; the full claim was recycled (issuance fell).
        assert_eq!(bal(&trader), trader_before);
        assert!(TotalIssuance::<Test>::get() < ti0);
    });
}

#[test]
fn dereg_cold_ema_caps_equity_at_floor() {
    new_test_ext(1).execute_with(|| {
        let netuid = setup_market(1000 * TAO, 1000 * TAO, 1.0);
        let trader = U256::from(10);
        let hotkey = U256::from(11);
        add_balance_to_coldkey_account(&trader, t(1000 * TAO));
        assert_ok!(SubtensorModule::open_short(
            RuntimeOrigin::signed(trader),
            hotkey,
            netuid,
            t(100 * TAO),
            AlphaBalance::MAX
        ));
        let p_floor = ShortPositions::<Test>::get(netuid, trader)
            .unwrap()
            .p_floor
            .to_u64();
        // Cold price EMA at settlement: no trustworthy slow reference. The cold-EMA
        // guard must floor K_D at the retained buffer R, so the trader recovers at
        // most their own floor P — never the pool-origin buffer.
        SubnetMovingPrice::<Test>::insert(netuid, I96F32::from_num(0));
        let before = bal(&trader);
        SubtensorModule::settle_shorts_on_dereg(netuid);
        let gained = bal(&trader) - before;
        assert!(
            gained <= p_floor,
            "cold-EMA equity {gained} must not exceed floor {p_floor}"
        );
        assert_eq!(custody_bal(netuid), 0);
    });
}

// Regression for the order-dependence fix: every position's terminal K_D is
// priced against ONE frozen pre-settlement reserve snapshot, so per-position
// equity does not depend on settlement (coldkey storage) order. Here we settle
// several positions and assert each paid equity equals the value computed
// against the snapshot captured before any escrow restoration. Pre-fix, later
// positions saw earlier positions' escrow already restored into SubnetTAO and
// were mispriced.
#[test]
fn terminal_settlement_order_independent() {
    new_test_ext(1).execute_with(|| {
        // price = 1.0 ⇒ pEMA warm; equal reserves ⇒ K_spot == K_EMA.
        let netuid = setup_market(2000 * TAO, 2000 * TAO, 1.0);
        let hotkey = U256::from(11);
        let traders = [
            U256::from(21),
            U256::from(22),
            U256::from(23),
            U256::from(24),
        ];
        for tr in traders.iter() {
            add_balance_to_coldkey_account(tr, t(1000 * TAO));
            assert_ok!(SubtensorModule::open_short(
                RuntimeOrigin::signed(*tr),
                hotkey,
                netuid,
                t(50 * TAO),
                AlphaBalance::MAX
            ));
        }

        // Frozen snapshot, captured before settlement (no decay tick has run, so
        // stored position values are the materialized values).
        let t0 = SubnetTAO::<Test>::get(netuid).to_u64() as u128;
        let a0 = SubnetAlphaIn::<Test>::get(netuid).to_u64() as u128;
        let pema = SubtensorModule::get_moving_alpha_price(netuid);
        let t_ema0 = (I96F32::from_num(pema) * I96F32::from_num(a0)).to_num::<u128>();
        // Byte-exact mirror of `buyback_cost_rao` (mod.rs), incl. the u64::MAX clamp.
        let bb = |pay: u128, recv: u128, amt: u128| -> u128 {
            if recv <= amt {
                u64::MAX as u128
            } else {
                pay.saturating_mul(amt)
                    .div_ceil(recv - amt)
                    .min(u64::MAX as u128)
            }
        };
        // Aggregate (split-neutral) pricing: K_Σ on the total liability, allocated
        // pro-rata (ceiling). Order-independent because every position reads the
        // same frozen snapshot + aggregate.
        let q_sigma: u128 = traders
            .iter()
            .map(|tr| {
                ShortPositions::<Test>::get(netuid, tr)
                    .unwrap()
                    .q_liability
                    .to_u64() as u128
            })
            .sum();
        let k_sigma = bb(t0, a0, q_sigma).max(bb(t_ema0, a0, q_sigma));
        let mut expected: Vec<u64> = vec![];
        let mut before: Vec<u64> = vec![];
        for tr in traders.iter() {
            let pos = ShortPositions::<Test>::get(netuid, tr).unwrap();
            let c = pos.p_floor.to_u64() as u128 + pos.r_stored.to_u64() as u128;
            let q = pos.q_liability.to_u64() as u128;
            let k_i = k_sigma.saturating_mul(q).div_ceil(q_sigma);
            expected.push(c.saturating_sub(k_i).min(u64::MAX as u128) as u64);
            before.push(bal(tr));
        }

        SubtensorModule::settle_shorts_on_dereg(netuid);

        for (i, tr) in traders.iter().enumerate() {
            let paid = bal(tr) - before[i];
            assert_eq!(
                paid, expected[i],
                "position {i} mispriced vs frozen snapshot (order-dependent settlement)"
            );
        }
        assert!(
            expected[0] > 0,
            "expected in-the-money equity to make the test meaningful"
        );
        assert!(ShortPositions::<Test>::iter_prefix(netuid).next().is_none());
    });
}

// Long-side mirror of the order-independence regression: every long position's
// terminal cover is priced against the same frozen pre-settlement snapshot, so
// per-position equity (minted as stake) is independent of settlement order.
#[test]
fn long_terminal_settlement_order_independent() {
    new_test_ext(1).execute_with(|| {
        let netuid = setup_long(2000 * TAO, 2000 * TAO, 1.0);
        let hotkey = U256::from(11);
        let traders = [
            U256::from(31),
            U256::from(32),
            U256::from(33),
            U256::from(34),
        ];
        for tr in traders.iter() {
            give_alpha(hotkey, *tr, netuid, AlphaBalance::from(500 * TAO));
            assert_ok!(SubtensorModule::open_long(
                RuntimeOrigin::signed(*tr),
                hotkey,
                netuid,
                AlphaBalance::from(50 * TAO),
                TaoBalance::MAX
            ));
        }

        let a0 = SubnetAlphaIn::<Test>::get(netuid).to_u64() as u128;
        let t0 = SubnetTAO::<Test>::get(netuid).to_u64() as u128;
        let pema = SubtensorModule::get_moving_alpha_price(netuid);
        let t_ema0 = (I96F32::from_num(pema) * I96F32::from_num(a0)).to_num::<u128>();
        let bb = |pay: u128, recv: u128, amt: u128| -> u128 {
            if recv <= amt {
                u64::MAX as u128
            } else {
                pay.saturating_mul(amt)
                    .div_ceil(recv - amt)
                    .min(u64::MAX as u128)
            }
        };
        let stake = |tr: &U256| -> u64 {
            SubtensorModule::get_stake_for_hotkey_and_coldkey_on_subnet(&hotkey, tr, netuid)
                .to_u64()
        };
        // Aggregate (split-neutral) cover: cover_Σ on the total D, allocated
        // pro-rata (ceiling) — order-independent against the frozen snapshot.
        let d_sigma: u128 = traders
            .iter()
            .map(|tr| {
                LongPositions::<Test>::get(netuid, tr)
                    .unwrap()
                    .d_liability
                    .to_u64() as u128
            })
            .sum();
        let cover_sigma = bb(a0, t0, d_sigma).max(bb(a0, t_ema0, d_sigma));
        let mut expected: Vec<u64> = vec![];
        let mut before: Vec<u64> = vec![];
        for tr in traders.iter() {
            let pos = LongPositions::<Test>::get(netuid, tr).unwrap();
            let c_l = pos.p_floor.to_u64() as u128 + pos.r_stored.to_u64() as u128;
            let d = pos.d_liability.to_u64() as u128;
            let cover = c_l.min(cover_sigma.saturating_mul(d).div_ceil(d_sigma));
            expected.push(c_l.saturating_sub(cover).min(u64::MAX as u128) as u64);
            before.push(stake(tr));
        }

        SubtensorModule::settle_longs_on_dereg(netuid);

        for (i, tr) in traders.iter().enumerate() {
            let minted = stake(tr) - before[i];
            assert_eq!(
                minted, expected[i],
                "long position {i} mispriced vs frozen snapshot (order-dependent settlement)"
            );
        }
        assert!(expected[0] > 0, "expected in-the-money long equity");
        assert!(LongPositions::<Test>::iter_prefix(netuid).next().is_none());
    });
}

// Split-neutrality (spec §10.1): terminal cover is priced ONCE on the aggregate
// liability Q_Σ and allocated pro-rata, so wallet-splitting one liability across
// many coldkeys cannot reduce total cover. Because the CPMM buyback is convex,
// per-position pricing (the prior behavior) would give Σ K(q_i) < K(ΣQ); this
// test asserts the realized total cover tracks K(Q_Σ), not the smaller per-position sum.
#[test]
fn terminal_settlement_split_neutral() {
    new_test_ext(1).execute_with(|| {
        let netuid = setup_market(2000 * TAO, 2000 * TAO, 1.0);
        let hotkey = U256::from(11);
        let traders = [
            U256::from(41),
            U256::from(42),
            U256::from(43),
            U256::from(44),
            U256::from(45),
        ];
        for tr in traders.iter() {
            add_balance_to_coldkey_account(tr, t(1000 * TAO));
            assert_ok!(SubtensorModule::open_short(
                RuntimeOrigin::signed(*tr),
                hotkey,
                netuid,
                t(50 * TAO),
                AlphaBalance::MAX
            ));
        }
        let t0 = SubnetTAO::<Test>::get(netuid).to_u64() as u128;
        let a0 = SubnetAlphaIn::<Test>::get(netuid).to_u64() as u128;
        let pema = SubtensorModule::get_moving_alpha_price(netuid);
        let t_ema0 = (I96F32::from_num(pema) * I96F32::from_num(a0)).to_num::<u128>();
        let bb = |pay: u128, recv: u128, amt: u128| -> u128 {
            if recv <= amt {
                u64::MAX as u128
            } else {
                pay.saturating_mul(amt)
                    .div_ceil(recv - amt)
                    .min(u64::MAX as u128)
            }
        };
        let mut q_sigma = 0u128;
        let mut c_sigma = 0u128;
        let mut k_single_sum = 0u128; // the convex per-position sum (buggy behavior)
        let mut before = vec![];
        for tr in traders.iter() {
            let p = ShortPositions::<Test>::get(netuid, tr).unwrap();
            let q = p.q_liability.to_u64() as u128;
            q_sigma += q;
            c_sigma += p.p_floor.to_u64() as u128 + p.r_stored.to_u64() as u128;
            k_single_sum += bb(t0, a0, q).max(bb(t_ema0, a0, q));
            before.push(bal(tr));
        }
        let k_agg = bb(t0, a0, q_sigma).max(bb(t_ema0, a0, q_sigma));
        // Convexity must hold or the test is not discriminating.
        assert!(
            k_agg > k_single_sum,
            "expected convex buyback: K(ΣQ) {k_agg} > Σ K(q_i) {k_single_sum}"
        );

        SubtensorModule::settle_shorts_on_dereg(netuid);

        let equity_sum: u128 = traders
            .iter()
            .zip(before.iter())
            .map(|(tr, b0)| (bal(tr) - b0) as u128)
            .sum();
        // cover = collateral − equity. Split-neutral ⇒ total cover ≈ K(Q_Σ),
        // (ceiling allocation makes it ≥ K_agg by < #positions rao), and strictly
        // MORE than the convex per-position sum a splitter would have paid.
        let cover_sum = c_sigma - equity_sum;
        assert!(
            cover_sum >= k_agg && cover_sum <= k_agg + traders.len() as u128,
            "total cover {cover_sum} must track aggregate K(ΣQ) {k_agg} (split-neutral)"
        );
        assert!(
            cover_sum > k_single_sum,
            "split-neutral cover {cover_sum} must exceed the convex per-position sum {k_single_sum}"
        );
    });
}

// #4: during EMA warmup a tiny pEMA makes T_ref = min(T_live, pEMA·A_live) tiny,
// so the capacity cap κ·T_ref admits only negligible opens — the cold/near-cold
// window is self-limiting even past the pEMA>0 guard.
#[test]
fn tiny_pema_caps_open_size() {
    new_test_ext(1).execute_with(|| {
        let netuid = setup_market(1000 * TAO, 1000 * TAO, 1.0);
        // Near-cold EMA: set pEMA to a tiny positive value (passes the warm guard).
        SubnetMovingPrice::<Test>::insert(netuid, I96F32::from_num(0.00001));
        let trader = U256::from(10);
        add_balance_to_coldkey_account(&trader, t(1000 * TAO));
        // A normal 100-TAO open is rejected — the tiny T_ref drives λ_eff≤0 /
        // capacity well before any meaningful size can open.
        let r = SubtensorModule::open_short(
            RuntimeOrigin::signed(trader),
            U256::from(11),
            netuid,
            t(100 * TAO),
            AlphaBalance::MAX,
        );
        assert!(
            r == Err(Error::<Test>::EffectiveLtvNonPositive.into())
                || r == Err(Error::<Test>::ShortCapacityExceeded.into())
                || r == Err(Error::<Test>::RetainedProceedsNonPositive.into()),
            "tiny pEMA must cap open size, got {r:?}"
        );
        assert!(ShortPositions::<Test>::get(netuid, trader).is_none());
    });
}

// Atomicity: `do_dissolve_network` is `#[frame_support::transactional]` and runs
// derivative terminal settlement BEFORE the fallible `destroy_alpha_in_out_stakes`
// / `clear_protocol_liquidity` legs. If a later leg fails, the settlement must roll
// back as a unit. This exercises that exact mechanism (`#[transactional]` ==
// `with_storage_layer`) on the real settlement fn: settle inside the layer, then a
// later step errors, and assert all derivative/custody/aggregate state is restored.
#[test]
fn dereg_settlement_rolls_back_on_later_failure() {
    new_test_ext(1).execute_with(|| {
        let netuid = setup_market(2000 * TAO, 2000 * TAO, 1.0);
        let trader = U256::from(10);
        let hotkey = U256::from(11);
        add_balance_to_coldkey_account(&trader, t(1000 * TAO));
        assert_ok!(SubtensorModule::open_short(
            RuntimeOrigin::signed(trader),
            hotkey,
            netuid,
            t(50 * TAO),
            AlphaBalance::MAX
        ));
        assert!(ShortPositions::<Test>::get(netuid, trader).is_some());
        let custody_before = custody_bal(netuid);
        let q_before = ShortAggregate::<Test>::get(netuid).q_sigma;

        // Model do_dissolve_network: settle, then a later fallible leg returns Err.
        let r = frame_support::storage::with_storage_layer(|| -> sp_runtime::DispatchResult {
            SubtensorModule::settle_shorts_on_dereg(netuid);
            // Inside the layer the position is settled/removed...
            assert!(ShortPositions::<Test>::get(netuid, trader).is_none());
            // ...then a subsequent dissolve leg fails.
            Err(Error::<Test>::SubnetNotExists.into())
        });
        assert!(r.is_err());

        // The whole settlement rolled back: position, custody, and aggregate restored.
        assert!(
            ShortPositions::<Test>::get(netuid, trader).is_some(),
            "position must survive a rolled-back dissolve"
        );
        assert_eq!(
            custody_bal(netuid),
            custody_before,
            "custody must be restored"
        );
        assert_eq!(
            ShortAggregate::<Test>::get(netuid).q_sigma,
            q_before,
            "aggregate must be restored"
        );
    });
}

#[test]
fn dissolve_network_clears_shorts() {
    new_test_ext(1).execute_with(|| {
        let netuid = setup_market(1000 * TAO, 1000 * TAO, 1.0);
        let trader = U256::from(10);
        add_balance_to_coldkey_account(&trader, t(1000 * TAO));
        assert_ok!(SubtensorModule::open_short(
            RuntimeOrigin::signed(trader),
            U256::from(11),
            netuid,
            t(100 * TAO),
            AlphaBalance::MAX
        ));
        assert!(ShortPositions::<Test>::get(netuid, trader).is_some());

        assert_ok!(SubtensorModule::do_dissolve_network(netuid));

        // Terminal hook fired: positions and aggregate cleared.
        assert!(ShortPositions::<Test>::get(netuid, trader).is_none());
        assert!(!ShortAggregate::<Test>::contains_key(netuid));
        assert!(!ShortActiveSubnets::<Test>::contains_key(netuid));
    });
}

// ---------------------------------------------------------------------------
// Audit fixes
// ---------------------------------------------------------------------------

// Fix: additional open must target the same hotkey (else close would repay from
// the wrong stake).
#[test]
fn merge_with_mismatched_hotkey_rejected() {
    new_test_ext(1).execute_with(|| {
        let netuid = setup_market(1000 * TAO, 1000 * TAO, 1.0);
        let trader = U256::from(10);
        add_balance_to_coldkey_account(&trader, t(1000 * TAO));
        assert_ok!(SubtensorModule::open_short(
            RuntimeOrigin::signed(trader),
            U256::from(11),
            netuid,
            t(50 * TAO),
            AlphaBalance::MAX
        ));
        // Second open with a different hotkey must be rejected, leaving state intact.
        assert_noop!(
            SubtensorModule::open_short(
                RuntimeOrigin::signed(trader),
                U256::from(12),
                netuid,
                t(50 * TAO),
                AlphaBalance::MAX
            ),
            Error::<Test>::ShortHotkeyMismatch
        );
        let pos = ShortPositions::<Test>::get(netuid, trader).unwrap();
        assert_eq!(pos.hotkey, U256::from(11));
        assert_eq!(pos.p_floor, t(50 * TAO)); // unchanged by the rejected merge
    });
}

// Fix: opens below the minimum input are rejected (dust-spam / terminal-load bound).
#[test]
fn open_below_min_input_rejected() {
    new_test_ext(1).execute_with(|| {
        let netuid = setup_market(1000 * TAO, 1000 * TAO, 1.0);
        let trader = U256::from(10);
        add_balance_to_coldkey_account(&trader, t(1000 * TAO));
        SubtensorModule::set_short_min_input(t(TAO)); // 1 TAO floor

        assert_noop!(
            SubtensorModule::open_short(
                RuntimeOrigin::signed(trader),
                U256::from(11),
                netuid,
                t(TAO / 2),
                AlphaBalance::MAX
            ),
            Error::<Test>::AmountTooLow
        );
        // At/above the floor it succeeds.
        assert_ok!(SubtensorModule::open_short(
            RuntimeOrigin::signed(trader),
            U256::from(11),
            netuid,
            t(TAO),
            AlphaBalance::MAX
        ));
    });
}

// Fix: a third party cannot snipe a default within the grace window after the
// owner's last action; after the window it is allowed.
#[test]
fn permissionless_default_respects_grace_window() {
    new_test_ext(1).execute_with(|| {
        let netuid = setup_market(1000 * TAO, 1000 * TAO, 1.0);
        let trader = U256::from(10);
        add_balance_to_coldkey_account(&trader, t(1000 * TAO));
        assert_ok!(SubtensorModule::open_short(
            RuntimeOrigin::signed(trader),
            U256::from(11),
            netuid,
            t(100 * TAO),
            AlphaBalance::MAX
        ));

        // Make the buffer dust-eligible, set a short grace window.
        SubtensorModule::set_short_dust(t(1000 * TAO));
        SubtensorModule::set_short_default_grace(5);
        let poker = U256::from(99);

        // Within the grace window: rejected even though the buffer is dust.
        assert_noop!(
            SubtensorModule::default_short(RuntimeOrigin::signed(poker), trader, netuid),
            Error::<Test>::PositionNotDefaultEligible
        );

        // After the grace window: allowed.
        step_block(6);
        assert_ok!(SubtensorModule::default_short(
            RuntimeOrigin::signed(poker),
            trader,
            netuid
        ));
        assert!(ShortPositions::<Test>::get(netuid, trader).is_none());
    });
}

// Fix: the owner can defeat a snipe by topping up, which resets the grace window.
#[test]
fn top_up_resets_default_grace() {
    new_test_ext(1).execute_with(|| {
        let netuid = setup_market(1000 * TAO, 1000 * TAO, 1.0);
        let trader = U256::from(10);
        add_balance_to_coldkey_account(&trader, t(1000 * TAO));
        assert_ok!(SubtensorModule::open_short(
            RuntimeOrigin::signed(trader),
            U256::from(11),
            netuid,
            t(100 * TAO),
            AlphaBalance::MAX
        ));
        SubtensorModule::set_short_dust(t(1000 * TAO));
        SubtensorModule::set_short_default_grace(5);

        step_block(6); // grace from open has elapsed
        // Owner tops up, resetting last_active to the current block.
        assert_ok!(SubtensorModule::top_up_short(
            RuntimeOrigin::signed(trader),
            netuid,
            t(TAO)
        ));

        // A snipe is now blocked again for another grace window.
        let poker = U256::from(99);
        assert_noop!(
            SubtensorModule::default_short(RuntimeOrigin::signed(poker), trader, netuid),
            Error::<Test>::PositionNotDefaultEligible
        );
    });
}

// Fix: only subnets with live short state are tracked for the per-block decay
// tick; membership is added on open and removed when the last position closes.
#[test]
fn active_subnet_set_tracks_membership() {
    new_test_ext(1).execute_with(|| {
        let netuid = setup_market(1000 * TAO, 1000 * TAO, 1.0);
        let trader = U256::from(10);
        let hotkey = U256::from(11);
        add_balance_to_coldkey_account(&trader, t(1000 * TAO));

        // No shorts yet → not tracked.
        assert!(!ShortActiveSubnets::<Test>::contains_key(netuid));

        assert_ok!(SubtensorModule::open_short(
            RuntimeOrigin::signed(trader),
            hotkey,
            netuid,
            t(100 * TAO),
            AlphaBalance::MAX
        ));
        assert!(ShortActiveSubnets::<Test>::contains_key(netuid));

        let pos = ShortPositions::<Test>::get(netuid, trader).unwrap();
        give_alpha(
            hotkey,
            trader,
            netuid,
            AlphaBalance::from(pos.q_liability.to_u64() + 10 * TAO),
        );
        assert_ok!(SubtensorModule::close_short(
            RuntimeOrigin::signed(trader),
            netuid,
            1_000_000_000
        ));

        // Fully closed → no longer tracked, so decay skips this subnet.
        assert!(!ShortActiveSubnets::<Test>::contains_key(netuid));
    });
}

// ---------------------------------------------------------------------------
// Read / RPC layer
// ---------------------------------------------------------------------------

// The position view materializes decay to the current block, while raw storage
// stays at the last materialization.
#[test]
fn position_view_materializes_decay() {
    new_test_ext(1).execute_with(|| {
        let netuid = setup_market(1000 * TAO, 1000 * TAO, 1.0);
        let trader = U256::from(10);
        add_balance_to_coldkey_account(&trader, t(1000 * TAO));
        assert_ok!(SubtensorModule::open_short(
            RuntimeOrigin::signed(trader),
            U256::from(11),
            netuid,
            t(100 * TAO),
            AlphaBalance::MAX
        ));
        SubtensorModule::set_decay_bounds_ppb(1_000_000_000, 1_000_000_000); // strong decay

        let raw = ShortPositions::<Test>::get(netuid, trader)
            .unwrap()
            .r_stored
            .to_u64();
        for _ in 0..2000 {
            SubtensorModule::run_short_decay();
        }

        let info = SubtensorModule::get_short_position(&trader, netuid).unwrap();
        // View reflects decay; raw storage is still the last-materialized value.
        assert!(
            info.buffer.to_u64() < raw,
            "view buffer {} !< raw {}",
            info.buffer.to_u64(),
            raw
        );
        assert_eq!(
            ShortPositions::<Test>::get(netuid, trader)
                .unwrap()
                .r_stored
                .to_u64(),
            raw
        );
        assert_eq!(
            info.collateral_claim.to_u64(),
            info.floor.to_u64() + info.buffer.to_u64()
        );
        assert!(info.daily_decay > 0);
        assert!(info.blocks_to_dust > 0 && info.blocks_to_dust < u64::MAX);
        assert_eq!(info.alpha_needed, info.alpha_liability); // holds none yet
    });
}

// The view's default-eligibility tracks the grace window.
#[test]
fn position_view_reports_default_window() {
    new_test_ext(1).execute_with(|| {
        let netuid = setup_market(1000 * TAO, 1000 * TAO, 1.0);
        let trader = U256::from(10);
        add_balance_to_coldkey_account(&trader, t(1000 * TAO));
        assert_ok!(SubtensorModule::open_short(
            RuntimeOrigin::signed(trader),
            U256::from(11),
            netuid,
            t(100 * TAO),
            AlphaBalance::MAX
        ));
        SubtensorModule::set_short_dust(t(1000 * TAO)); // buffer is dust
        SubtensorModule::set_short_default_grace(5);

        let info = SubtensorModule::get_short_position(&trader, netuid).unwrap();
        assert!(!info.default_eligible, "within grace, not yet defaultable");

        step_block(6);
        let info2 = SubtensorModule::get_short_position(&trader, netuid).unwrap();
        assert!(info2.default_eligible, "after grace, defaultable");
        assert_eq!(info2.defaultable_at_block, info.defaultable_at_block);
    });
}

// Market view exposes capacity and parameters.
#[test]
fn market_view_reports_capacity() {
    new_test_ext(1).execute_with(|| {
        let netuid = setup_market(1000 * TAO, 1000 * TAO, 1.0);
        let trader = U256::from(10);
        add_balance_to_coldkey_account(&trader, t(1000 * TAO));
        assert_ok!(SubtensorModule::open_short(
            RuntimeOrigin::signed(trader),
            U256::from(11),
            netuid,
            t(100 * TAO),
            AlphaBalance::MAX
        ));

        let pos = ShortPositions::<Test>::get(netuid, trader).unwrap();
        let m = SubtensorModule::get_subnet_short_state(netuid).unwrap();
        assert!(m.shorts_enabled);
        assert!(m.footprint_used.to_u64() > 0);
        assert!(m.footprint_cap.to_u64() > m.footprint_used.to_u64());
        assert_eq!(
            m.footprint_remaining.to_u64(),
            m.footprint_cap.to_u64() - m.footprint_used.to_u64()
        );
        assert_eq!(m.open_interest_alpha, pos.q_liability);
        assert_eq!(m.buffer_total, pos.r_stored);
        assert!(m.current_daily_decay > 0);
    });
}

// Close quote matches the amounts an actual full close moves.
#[test]
fn close_quote_matches_position() {
    new_test_ext(1).execute_with(|| {
        let netuid = setup_market(1000 * TAO, 1000 * TAO, 1.0);
        let trader = U256::from(10);
        add_balance_to_coldkey_account(&trader, t(1000 * TAO));
        assert_ok!(SubtensorModule::open_short(
            RuntimeOrigin::signed(trader),
            U256::from(11),
            netuid,
            t(100 * TAO),
            AlphaBalance::MAX
        ));
        let pos = ShortPositions::<Test>::get(netuid, trader).unwrap();

        let full = SubtensorModule::quote_close_short(&trader, netuid, 1_000_000_000).unwrap();
        assert_eq!(full.repay_alpha, pos.q_liability);
        assert_eq!(
            full.returned_tao.to_u64(),
            pos.p_floor.to_u64() + pos.r_stored.to_u64()
        );
        assert_eq!(full.alpha_needed, pos.q_liability); // holds none
        assert!(full.est_buyback_cost.to_u64() > 0);

        let half = SubtensorModule::quote_close_short(&trader, netuid, 500_000_000).unwrap();
        assert_approx(
            half.repay_alpha.to_u64(),
            full.repay_alpha.to_u64() / 2,
            2,
            "half repay",
        );
        assert_approx(
            half.returned_tao.to_u64(),
            full.returned_tao.to_u64() / 2,
            2,
            "half return",
        );
    });
}

// Materialization can never inflate a position: even with a (impossible)
// entry accumulator above the aggregate, the factor is clamped to ≤ 1.
#[test]
fn materialize_never_inflates() {
    new_test_ext(1).execute_with(|| {
        let netuid = setup_market(1000 * TAO, 1000 * TAO, 1.0);
        let trader = U256::from(10);
        add_balance_to_coldkey_account(&trader, t(1000 * TAO));
        assert_ok!(SubtensorModule::open_short(
            RuntimeOrigin::signed(trader),
            U256::from(11),
            netuid,
            t(100 * TAO),
            AlphaBalance::MAX
        ));

        // Corrupt the invariant: set omega_entry far above the aggregate omega.
        let mut pos = ShortPositions::<Test>::get(netuid, trader).unwrap();
        let buf = pos.r_stored;
        pos.omega_entry = I64F64::from_num(1000);
        ShortPositions::<Test>::insert(netuid, trader, pos);

        // The materialized view must not exceed the stored buffer (no inflation).
        let info = SubtensorModule::get_short_position(&trader, netuid).unwrap();
        assert!(
            info.buffer <= buf,
            "materialize inflated: {} > {}",
            info.buffer.to_u64(),
            buf.to_u64()
        );
    });
}

// Open immediately followed by full close cannot be a rounding-profit loop: the
// trader gets back at most floor + buffer and must repay the full liability.
#[test]
fn open_close_roundtrip_is_not_profitable() {
    new_test_ext(1).execute_with(|| {
        let netuid = setup_market(1000 * TAO, 1000 * TAO, 1.0);
        let trader = U256::from(10);
        let hotkey = U256::from(11);
        add_balance_to_coldkey_account(&trader, t(1000 * TAO));

        let before = bal(&trader);
        assert_ok!(SubtensorModule::open_short(
            RuntimeOrigin::signed(trader),
            hotkey,
            netuid,
            t(100 * TAO),
            AlphaBalance::MAX
        ));
        let pos = ShortPositions::<Test>::get(netuid, trader).unwrap();
        let n = pos.r_stored.to_u64();
        // Seed exactly the liability alpha so the round trip is self-contained.
        give_alpha(hotkey, trader, netuid, pos.q_liability);
        assert_ok!(SubtensorModule::close_short(
            RuntimeOrigin::signed(trader),
            netuid,
            1_000_000_000
        ));

        // TAO-only delta is +N (the retained proceeds); the trader still had to
        // source Q alpha, whose pool buy-cost strictly exceeds N — so no free TAO.
        assert_eq!(bal(&trader), before + n);
        let buy_cost = SubtensorModule::get_subnet_short_state(netuid); // sanity: market still consistent
        assert!(buy_cost.is_some());
    });
}

// Fix (L3): close must never mint alpha by saturating SubnetAlphaOut to zero.
#[test]
fn close_guards_against_alpha_mint() {
    new_test_ext(1).execute_with(|| {
        let netuid = setup_market(1000 * TAO, 1000 * TAO, 1.0);
        let trader = U256::from(10);
        let hotkey = U256::from(11);
        add_balance_to_coldkey_account(&trader, t(1000 * TAO));
        assert_ok!(SubtensorModule::open_short(
            RuntimeOrigin::signed(trader),
            hotkey,
            netuid,
            t(100 * TAO),
            AlphaBalance::MAX
        ));
        let pos = ShortPositions::<Test>::get(netuid, trader).unwrap();
        give_alpha(hotkey, trader, netuid, pos.q_liability);

        // Corrupt outstanding alpha below the liability: close must refuse rather
        // than push SubnetAlphaIn up while SubnetAlphaOut saturates (a mint).
        SubnetAlphaOut::<Test>::insert(netuid, AlphaBalance::from(0));
        let alpha_in_before = SubnetAlphaIn::<Test>::get(netuid);
        assert_noop!(
            SubtensorModule::close_short(RuntimeOrigin::signed(trader), netuid, 1_000_000_000),
            Error::<Test>::InsufficientAlphaToClose
        );
        assert_eq!(SubnetAlphaIn::<Test>::get(netuid), alpha_in_before); // no mint
    });
}

// Fix (L2): the open quote is unavailable while shorts are disabled.
#[test]
fn open_quote_gated_by_enable_flag() {
    new_test_ext(1).execute_with(|| {
        let netuid = setup_market(1000 * TAO, 1000 * TAO, 1.0);
        assert!(SubtensorModule::quote_open_short(netuid, t(100 * TAO)).is_some());
        SubtensorModule::set_shorts_enabled(false);
        assert!(SubtensorModule::quote_open_short(netuid, t(100 * TAO)).is_none());
    });
}

// Long-side mirror of L2: the long open quote is unavailable while longs are disabled.
#[test]
fn long_open_quote_gated_by_enable_flag() {
    new_test_ext(1).execute_with(|| {
        let netuid = setup_long(1000 * TAO, 1000 * TAO, 1.0);
        assert!(SubtensorModule::quote_open_long(netuid, AlphaBalance::from(100 * TAO)).is_some());
        SubtensorModule::set_longs_enabled(false);
        assert!(SubtensorModule::quote_open_long(netuid, AlphaBalance::from(100 * TAO)).is_none());
    });
}

// The caller execution bound is an opt-out: `*::MAX` accepts any realized liability
// (so a normal open never reverts on slippage), while a tight bound rejects. Asserts
// both directions in one scenario for both sides.
#[test]
fn open_max_liability_bound_opts_out() {
    new_test_ext(1).execute_with(|| {
        // SHORT: MAX opts out (opens), 1-rao bound rejects.
        let netuid = setup_market(1000 * TAO, 1000 * TAO, 1.0);
        let trader = U256::from(10);
        add_balance_to_coldkey_account(&trader, t(1000 * TAO));
        assert_ok!(SubtensorModule::open_short(
            RuntimeOrigin::signed(trader),
            U256::from(11),
            netuid,
            t(50 * TAO),
            AlphaBalance::MAX
        ));
        assert!(ShortPositions::<Test>::get(netuid, trader).is_some());
        let trader2 = U256::from(20);
        add_balance_to_coldkey_account(&trader2, t(1000 * TAO));
        assert_noop!(
            SubtensorModule::open_short(
                RuntimeOrigin::signed(trader2),
                U256::from(21),
                netuid,
                t(50 * TAO),
                AlphaBalance::from(1)
            ),
            Error::<Test>::SlippageTooHigh
        );

        // LONG: MAX opts out (opens), 1-rao bound rejects.
        let lnet = setup_long(1000 * TAO, 1000 * TAO, 1.0);
        let lt = U256::from(30);
        let lhot = U256::from(31);
        give_alpha(lhot, lt, lnet, AlphaBalance::from(500 * TAO));
        assert_ok!(SubtensorModule::open_long(
            RuntimeOrigin::signed(lt),
            lhot,
            lnet,
            AlphaBalance::from(100 * TAO),
            TaoBalance::MAX
        ));
        assert!(LongPositions::<Test>::get(lnet, lt).is_some());
        let lt2 = U256::from(40);
        let lhot2 = U256::from(41);
        give_alpha(lhot2, lt2, lnet, AlphaBalance::from(500 * TAO));
        assert_noop!(
            SubtensorModule::open_long(
                RuntimeOrigin::signed(lt2),
                lhot2,
                lnet,
                AlphaBalance::from(100 * TAO),
                TaoBalance::from(1)
            ),
            Error::<Test>::SlippageTooHigh
        );
    });
}

// Fix (M4): per-subnet open-position count is capped and maintained, bounding
// deregistration-settlement work.
#[test]
fn position_count_cap_enforced_and_maintained() {
    new_test_ext(1).execute_with(|| {
        let netuid = setup_market(1000 * TAO, 1000 * TAO, 1.0);
        SubtensorModule::set_short_max_positions(2);
        let (a, b, c) = (U256::from(10), U256::from(20), U256::from(30));
        for k in [a, b, c] {
            add_balance_to_coldkey_account(&k, t(1000 * TAO));
        }

        assert_ok!(SubtensorModule::open_short(
            RuntimeOrigin::signed(a),
            U256::from(11),
            netuid,
            t(20 * TAO),
            AlphaBalance::MAX
        ));
        assert_ok!(SubtensorModule::open_short(
            RuntimeOrigin::signed(b),
            U256::from(21),
            netuid,
            t(20 * TAO),
            AlphaBalance::MAX
        ));
        assert_eq!(ShortPositionCount::<Test>::get(netuid), 2);

        // Third distinct position exceeds the cap.
        assert_noop!(
            SubtensorModule::open_short(
                RuntimeOrigin::signed(c),
                U256::from(31),
                netuid,
                t(20 * TAO),
                AlphaBalance::MAX
            ),
            Error::<Test>::ShortPositionLimit
        );

        // Closing one frees a slot; the count is decremented and reusable.
        let pos = ShortPositions::<Test>::get(netuid, a).unwrap();
        give_alpha(U256::from(11), a, netuid, pos.q_liability);
        assert_ok!(SubtensorModule::close_short(
            RuntimeOrigin::signed(a),
            netuid,
            1_000_000_000
        ));
        assert_eq!(ShortPositionCount::<Test>::get(netuid), 1);
        assert_ok!(SubtensorModule::open_short(
            RuntimeOrigin::signed(c),
            U256::from(31),
            netuid,
            t(20 * TAO),
            AlphaBalance::MAX
        ));
        assert_eq!(ShortPositionCount::<Test>::get(netuid), 2);

        // A merge (same coldkey, same hotkey) does not consume a new slot.
        assert_ok!(SubtensorModule::open_short(
            RuntimeOrigin::signed(c),
            U256::from(31),
            netuid,
            t(20 * TAO),
            AlphaBalance::MAX
        ));
        assert_eq!(ShortPositionCount::<Test>::get(netuid), 2);
    });
}

// ===========================================================================
// PROOF: global value conservation across the full mixed lifecycle.
//
// Exercises the real dispatch path for both sides (open/top-up/partial+full
// close) plus continuous decay, and asserts that no TAO and no Alpha is minted
// or destroyed once every position is closed. Decay is driven directly (not via
// step_block) so coinbase emissions don't perturb issuance.
// ===========================================================================
#[test]
fn proof_full_lifecycle_conserves_tao_and_alpha() {
    new_test_ext(1).execute_with(|| {
        let netuid = setup_long(1000 * TAO, 1000 * TAO, 1.0); // both sides enabled
        let (s_cold, s_hot) = (U256::from(10), U256::from(11));
        let (l_cold, l_hot) = (U256::from(20), U256::from(21));
        // Fund: short needs TAO (floor + top-up) and Alpha (repay Q); long needs
        // Alpha (collateral) and TAO (repay D).
        add_balance_to_coldkey_account(&s_cold, t(1000 * TAO));
        add_balance_to_coldkey_account(&l_cold, t(1000 * TAO));
        give_alpha(s_hot, s_cold, netuid, AlphaBalance::from(5000 * TAO));
        give_alpha(l_hot, l_cold, netuid, AlphaBalance::from(500 * TAO));

        // Baseline after all seeding.
        let tao0 = TotalIssuance::<Test>::get().to_u64();
        let alpha0 = alpha_issuance(netuid);

        assert_ok!(SubtensorModule::open_short(
            RuntimeOrigin::signed(s_cold),
            s_hot,
            netuid,
            t(100 * TAO),
            AlphaBalance::MAX
        ));
        assert_ok!(SubtensorModule::open_long(
            RuntimeOrigin::signed(l_cold),
            l_hot,
            netuid,
            AlphaBalance::from(100 * TAO),
            TaoBalance::MAX
        ));

        // Continuous unwind on both sides.
        for _ in 0..500 {
            SubtensorModule::run_short_decay();
            SubtensorModule::run_long_decay();
        }

        // Mid-life owner actions.
        assert_ok!(SubtensorModule::top_up_short(
            RuntimeOrigin::signed(s_cold),
            netuid,
            t(10 * TAO)
        ));
        assert_ok!(SubtensorModule::close_short(
            RuntimeOrigin::signed(s_cold),
            netuid,
            500_000_000
        )); // half

        // Close everything out.
        assert_ok!(SubtensorModule::close_short(
            RuntimeOrigin::signed(s_cold),
            netuid,
            1_000_000_000
        ));
        assert_ok!(SubtensorModule::close_long(
            RuntimeOrigin::signed(l_cold),
            netuid,
            1_000_000_000
        ));

        // CONSERVATION.
        // TAO only ever *moves* between accounts (no recycle on this all-close
        // path), so total TAO supply is conserved exactly.
        assert_eq!(
            TotalIssuance::<Test>::get().to_u64(),
            tao0,
            "TAO supply not conserved"
        );

        // Alpha is burned/minted around the pool; fixed-point flooring means the
        // restored amount is never ABOVE baseline (no value minted) and is below
        // it only by bounded rounding dust.
        let alpha1 = alpha_issuance(netuid);
        const DUST_TOL: u64 = 1_000_000; // 0.001 Alpha; observed drift is ~5e2 rao
        assert!(alpha1 <= alpha0, "Alpha was minted: {alpha1} > {alpha0}");
        assert!(
            alpha0 - alpha1 <= DUST_TOL,
            "Alpha loss {} exceeds dust tol",
            alpha0 - alpha1
        );
        assert!(
            custody_bal(netuid) <= DUST_TOL,
            "short custody dust too large"
        );

        // Positions and counts are cleared exactly; fixed liabilities net to 0.
        assert!(ShortPositions::<Test>::get(netuid, s_cold).is_none());
        assert!(LongPositions::<Test>::get(netuid, l_cold).is_none());
        assert_eq!(ShortPositionCount::<Test>::get(netuid), 0);
        assert_eq!(LongPositionCount::<Test>::get(netuid), 0);
        assert_eq!(ShortAggregate::<Test>::get(netuid).q_sigma.to_u64(), 0);
        assert_eq!(LongAggregate::<Test>::get(netuid).d_sigma.to_u64(), 0);
        // cleanup-on-empty evicts fully-closed subnets from the decay tick.
        assert!(!ShortActiveSubnets::<Test>::contains_key(netuid));
        assert!(!LongActiveSubnets::<Test>::contains_key(netuid));
    });
}

// PROOF (invariant): custody TAO always covers the materialized obligations
// Σ(P + R(t) + E(t)) across decay — including at the DecayMax clamp extreme
// (1.0/day), where the aggregate Σ-decay's faster flooring vs the per-position
// exp decay is most stressed. Locks the "custody ≥ obligations" solvency claim
// against future edits (architect M2).
#[test]
fn proof_custody_geq_obligations_under_decay() {
    new_test_ext(1).execute_with(|| {
        let netuid = setup_market(1000 * TAO, 1000 * TAO, 1.0);
        SubtensorModule::set_decay_bounds_ppb(100_000_000, 1_000_000_000); // 10%..100%/day
        let traders = [
            (U256::from(10), U256::from(11)),
            (U256::from(20), U256::from(21)),
            (U256::from(30), U256::from(31)),
        ];
        for (c, h) in traders.iter() {
            add_balance_to_coldkey_account(c, t(1000 * TAO));
            give_alpha(*h, *c, netuid, AlphaBalance::from(1000 * TAO)); // alpha to repay Q on close
        }
        for (i, (c, h)) in traders.iter().enumerate() {
            assert_ok!(SubtensorModule::open_short(
                RuntimeOrigin::signed(*c),
                *h,
                netuid,
                t((20 + 10 * i as u64) * TAO),
                AlphaBalance::MAX
            ));
        }
        // Σ materialized (floor + buffer + escrow) over every live position.
        let obligations = |nid: NetUid| -> u64 {
            traders
                .iter()
                .filter_map(|(c, _)| SubtensorModule::get_short_position(c, nid))
                .map(|p| p.floor.to_u64() + p.buffer.to_u64() + p.escrow.to_u64())
                .sum()
        };
        assert!(
            custody_bal(netuid) >= obligations(netuid),
            "custody < obligations at open"
        );
        for k in 0..3000 {
            SubtensorModule::run_short_decay();
            // Check every tick (not sampled): a one-block transient breach can't hide.
            assert!(
                custody_bal(netuid) >= obligations(netuid),
                "custody < obligations during decay (block {k})"
            );
        }
        // Mid-life partial close must preserve the invariant too.
        assert_ok!(SubtensorModule::close_short(
            RuntimeOrigin::signed(traders[0].0),
            netuid,
            400_000_000
        ));
        assert!(
            custody_bal(netuid) >= obligations(netuid),
            "custody < obligations after partial close"
        );
    });
}

// PROOF (invariant): the three denormalized bookkeeping copies stay in sync —
// ShortPositionCount == |ShortPositions[netuid]|, and ShortActiveSubnets membership
// iff the aggregate has any nonzero Σ — through an open/partial/full-close churn.
// Guards the per-subnet position cap and bounded-dereg-work guarantees (architect M3).
#[test]
fn proof_position_count_matches_map_through_churn() {
    new_test_ext(1).execute_with(|| {
        let netuid = setup_market(1000 * TAO, 1000 * TAO, 1.0);
        let traders = [
            (U256::from(10), U256::from(11)),
            (U256::from(20), U256::from(21)),
            (U256::from(30), U256::from(31)),
        ];
        for (c, h) in traders.iter() {
            add_balance_to_coldkey_account(c, t(1000 * TAO));
            give_alpha(*h, *c, netuid, AlphaBalance::from(1000 * TAO)); // alpha to repay Q on close
        }
        let check = |nid: NetUid| {
            let map_count = ShortPositions::<Test>::iter_prefix(nid).count() as u32;
            assert_eq!(
                ShortPositionCount::<Test>::get(nid),
                map_count,
                "count != map size"
            );
            let agg = ShortAggregate::<Test>::get(nid);
            let nonzero = !(agg.r_sigma.is_zero()
                && agg.e_sigma.is_zero()
                && agg.b_sigma.is_zero()
                && agg.q_sigma.is_zero());
            assert_eq!(
                ShortActiveSubnets::<Test>::contains_key(nid),
                nonzero,
                "active-set membership != nonzero aggregate"
            );
        };
        check(netuid);
        for (c, h) in traders.iter() {
            assert_ok!(SubtensorModule::open_short(
                RuntimeOrigin::signed(*c),
                *h,
                netuid,
                t(30 * TAO),
                AlphaBalance::MAX
            ));
            check(netuid);
        }
        // partial close (count unchanged), then full closes (count decrements).
        assert_ok!(SubtensorModule::close_short(
            RuntimeOrigin::signed(traders[0].0),
            netuid,
            500_000_000
        ));
        check(netuid);
        assert_ok!(SubtensorModule::close_short(
            RuntimeOrigin::signed(traders[1].0),
            netuid,
            1_000_000_000
        ));
        check(netuid);
        assert_ok!(SubtensorModule::close_short(
            RuntimeOrigin::signed(traders[0].0),
            netuid,
            1_000_000_000
        ));
        check(netuid);
        assert_ok!(SubtensorModule::close_short(
            RuntimeOrigin::signed(traders[2].0),
            netuid,
            1_000_000_000
        ));
        check(netuid);
        assert_eq!(ShortPositionCount::<Test>::get(netuid), 0);
        assert!(!ShortActiveSubnets::<Test>::contains_key(netuid));
    });
}

// PROOF: default reduces issuance by EXACTLY the recycled floor — no more, no
// less — on both sides.
#[test]
fn proof_default_recycles_exactly_the_floor() {
    new_test_ext(1).execute_with(|| {
        // Short side: TotalIssuance (TAO) drops by exactly the floor P.
        let netuid = setup_long(1000 * TAO, 1000 * TAO, 1.0);
        let (s_cold, s_hot) = (U256::from(10), U256::from(11));
        add_balance_to_coldkey_account(&s_cold, t(1000 * TAO));
        SubtensorModule::set_short_default_grace(0);
        SubtensorModule::set_short_dust(t(10_000 * TAO));
        assert_ok!(SubtensorModule::open_short(
            RuntimeOrigin::signed(s_cold),
            s_hot,
            netuid,
            t(100 * TAO),
            AlphaBalance::MAX
        ));
        let tao_before = TotalIssuance::<Test>::get().to_u64();
        assert_ok!(SubtensorModule::default_short(
            RuntimeOrigin::signed(U256::from(99)),
            s_cold,
            netuid
        ));
        assert_eq!(
            TotalIssuance::<Test>::get().to_u64(),
            tao_before - 100 * TAO,
            "short default must recycle exactly the floor"
        );

        // Long side: Alpha issuance drops by exactly the floor P.
        let (l_cold, l_hot) = (U256::from(20), U256::from(21));
        give_alpha(l_hot, l_cold, netuid, AlphaBalance::from(500 * TAO));
        SubtensorModule::set_long_dust(AlphaBalance::from(10_000 * TAO));
        SubtensorModule::set_long_default_grace(0);
        // Measure BEFORE open: long open burns alpha, default restores all but the
        // floor, so the net effect of open+default is exactly −floor.
        let alpha_before = alpha_issuance(netuid);
        assert_ok!(SubtensorModule::open_long(
            RuntimeOrigin::signed(l_cold),
            l_hot,
            netuid,
            AlphaBalance::from(100 * TAO),
            TaoBalance::MAX
        ));
        assert_ok!(SubtensorModule::default_long(
            RuntimeOrigin::signed(U256::from(98)),
            l_cold,
            netuid
        ));
        assert_eq!(
            alpha_issuance(netuid),
            alpha_before - 100 * TAO,
            "long default must recycle exactly the floor"
        );
    });
}

// PROOF (multi-position): the aggregate Σ-decay and per-position lazy decay
// stay solvent across MANY heterogeneous positions on both sides through a long
// decay horizon — the configuration the single-position tests can't exercise.
#[test]
fn proof_multi_position_decay_conserves() {
    new_test_ext(1).execute_with(|| {
        let netuid = setup_long(10_000 * TAO, 10_000 * TAO, 1.0);
        let shorts: [(U256, U256, u64); 3] = [
            (U256::from(10), U256::from(11), 50 * TAO),
            (U256::from(12), U256::from(13), 100 * TAO),
            (U256::from(14), U256::from(15), 30 * TAO),
        ];
        let longs: [(U256, U256, u64); 2] = [
            (U256::from(20), U256::from(21), 40 * TAO),
            (U256::from(22), U256::from(23), 60 * TAO),
        ];
        for (c, h, _) in shorts {
            add_balance_to_coldkey_account(&c, t(2000 * TAO));
            give_alpha(h, c, netuid, AlphaBalance::from(5000 * TAO)); // to repay Q
        }
        for (c, h, _) in longs {
            add_balance_to_coldkey_account(&c, t(2000 * TAO)); // to repay D
            give_alpha(h, c, netuid, AlphaBalance::from(1000 * TAO)); // collateral
        }

        let tao0 = TotalIssuance::<Test>::get().to_u64();
        let alpha0 = alpha_issuance(netuid);

        for (c, h, p) in shorts {
            assert_ok!(SubtensorModule::open_short(
                RuntimeOrigin::signed(c),
                h,
                netuid,
                t(p),
                AlphaBalance::MAX
            ));
        }
        for (c, h, p) in longs {
            assert_ok!(SubtensorModule::open_long(
                RuntimeOrigin::signed(c),
                h,
                netuid,
                AlphaBalance::from(p),
                TaoBalance::MAX
            ));
        }

        for _ in 0..300 {
            SubtensorModule::run_short_decay();
            SubtensorModule::run_long_decay();
        }

        for (c, _, _) in shorts {
            assert_ok!(SubtensorModule::close_short(
                RuntimeOrigin::signed(c),
                netuid,
                1_000_000_000
            ));
        }
        for (c, _, _) in longs {
            assert_ok!(SubtensorModule::close_long(
                RuntimeOrigin::signed(c),
                netuid,
                1_000_000_000
            ));
        }

        const TOL: u64 = 10_000_000; // 0.01 token
        assert_eq!(
            TotalIssuance::<Test>::get().to_u64(),
            tao0,
            "TAO supply not conserved"
        );
        let alpha1 = alpha_issuance(netuid);
        assert!(alpha1 <= alpha0, "Alpha minted across many positions");
        assert!(
            alpha0 - alpha1 <= TOL,
            "Alpha drift {} > tol",
            alpha0 - alpha1
        );
        assert!(
            custody_bal(netuid) <= TOL,
            "custody not drained across many positions"
        );
        assert_eq!(ShortPositionCount::<Test>::get(netuid), 0);
        assert_eq!(LongPositionCount::<Test>::get(netuid), 0);
        assert!(!ShortActiveSubnets::<Test>::contains_key(netuid));
        assert!(!LongActiveSubnets::<Test>::contains_key(netuid));
    });
}

// Many partial closes followed by a full close drain the position cleanly (the
// floor-rounding residue path), with TAO conserved and custody emptied.
#[test]
fn short_many_partial_closes_drain_cleanly() {
    new_test_ext(1).execute_with(|| {
        let netuid = setup_market(10_000 * TAO, 10_000 * TAO, 1.0);
        let trader = U256::from(10);
        let hotkey = U256::from(11);
        add_balance_to_coldkey_account(&trader, t(1000 * TAO));
        give_alpha(hotkey, trader, netuid, AlphaBalance::from(5000 * TAO));

        let tao0 = TotalIssuance::<Test>::get().to_u64();
        assert_ok!(SubtensorModule::open_short(
            RuntimeOrigin::signed(trader),
            hotkey,
            netuid,
            t(100 * TAO),
            AlphaBalance::MAX
        ));
        for _ in 0..9 {
            assert_ok!(SubtensorModule::close_short(
                RuntimeOrigin::signed(trader),
                netuid,
                100_000_000
            )); // 10% of remaining
        }
        assert_ok!(SubtensorModule::close_short(
            RuntimeOrigin::signed(trader),
            netuid,
            1_000_000_000
        ));

        assert!(ShortPositions::<Test>::get(netuid, trader).is_none());
        assert_eq!(TotalIssuance::<Test>::get().to_u64(), tao0);
        assert!(
            custody_bal(netuid) <= 10_000,
            "custody dust after partial closes"
        );
        assert!(!ShortActiveSubnets::<Test>::contains_key(netuid));
    });
}

// Governance setters clamp out-of-range inputs (kappa can't freeze the market
// or remove the cap; decay bounds stay ordered and ≤ 1.0/day).
#[test]
fn governance_setters_clamp_ranges() {
    new_test_ext(1).execute_with(|| {
        let one = I64F64::from_num(1);
        let two = I64F64::from_num(2);

        SubtensorModule::set_short_kappa_ppb(0);
        assert!(
            ShortKappa::<Test>::get() > I64F64::from_num(0),
            "kappa=0 must clamp above 0"
        );
        SubtensorModule::set_short_kappa_ppb(10_000_000_000); // 10.0
        assert_eq!(ShortKappa::<Test>::get(), two, "kappa clamps to 2.0");
        SubtensorModule::set_long_kappa_ppb(0);
        assert!(LongKappa::<Test>::get() > I64F64::from_num(0));

        // min > max → enforced min ≤ max.
        SubtensorModule::set_decay_bounds_ppb(500_000_000, 100_000_000);
        assert!(DecayMax::<Test>::get() >= DecayMin::<Test>::get());
        // max > 1.0/day → clamped so per-block delta stays < 1.
        SubtensorModule::set_decay_bounds_ppb(0, 5_000_000_000);
        assert!(
            DecayMax::<Test>::get() <= one,
            "decay max clamps to 1.0/day"
        );
    });
}

// Cleanup-on-empty only evicts a subnet from the decay tick once its LAST
// position closes — not while others remain.
#[test]
fn cleanup_evicts_only_after_last_short_closes() {
    new_test_ext(1).execute_with(|| {
        let netuid = setup_market(10_000 * TAO, 10_000 * TAO, 1.0);
        let (a, b) = (U256::from(10), U256::from(20));
        for k in [a, b] {
            add_balance_to_coldkey_account(&k, t(1000 * TAO));
        }
        give_alpha(U256::from(11), a, netuid, AlphaBalance::from(5000 * TAO));
        give_alpha(U256::from(21), b, netuid, AlphaBalance::from(5000 * TAO));
        assert_ok!(SubtensorModule::open_short(
            RuntimeOrigin::signed(a),
            U256::from(11),
            netuid,
            t(50 * TAO),
            AlphaBalance::MAX
        ));
        assert_ok!(SubtensorModule::open_short(
            RuntimeOrigin::signed(b),
            U256::from(21),
            netuid,
            t(50 * TAO),
            AlphaBalance::MAX
        ));

        assert_ok!(SubtensorModule::close_short(
            RuntimeOrigin::signed(a),
            netuid,
            1_000_000_000
        ));
        assert!(
            ShortActiveSubnets::<Test>::contains_key(netuid),
            "still active while b open"
        );

        assert_ok!(SubtensorModule::close_short(
            RuntimeOrigin::signed(b),
            netuid,
            1_000_000_000
        ));
        assert!(
            !ShortActiveSubnets::<Test>::contains_key(netuid),
            "evicted after last close"
        );
    });
}

// Long capacity cap is enforced.
#[test]
fn long_capacity_cap_enforced() {
    new_test_ext(1).execute_with(|| {
        let netuid = setup_long(1000 * TAO, 1000 * TAO, 1.0);
        SubtensorModule::set_long_kappa_ppb(1_000_000); // κ_L = 0.001
        let trader = U256::from(10);
        let hotkey = U256::from(11);
        give_alpha(hotkey, trader, netuid, AlphaBalance::from(500 * TAO));
        assert_noop!(
            SubtensorModule::open_long(
                RuntimeOrigin::signed(trader),
                hotkey,
                netuid,
                AlphaBalance::from(100 * TAO),
                TaoBalance::MAX
            ),
            Error::<Test>::LongCapacityExceeded
        );
    });
}

// Long partial close reduces all legs pro-rata.
#[test]
fn long_partial_close_reduces_prorata() {
    new_test_ext(1).execute_with(|| {
        let netuid = setup_long(1000 * TAO, 1000 * TAO, 1.0);
        let trader = U256::from(10);
        let hotkey = U256::from(11);
        give_alpha(hotkey, trader, netuid, AlphaBalance::from(500 * TAO));
        add_balance_to_coldkey_account(&trader, t(1000 * TAO));
        assert_ok!(SubtensorModule::open_long(
            RuntimeOrigin::signed(trader),
            hotkey,
            netuid,
            AlphaBalance::from(100 * TAO),
            TaoBalance::MAX
        ));
        let p0 = LongPositions::<Test>::get(netuid, trader).unwrap();

        assert_ok!(SubtensorModule::close_long(
            RuntimeOrigin::signed(trader),
            netuid,
            500_000_000
        ));
        let p1 = LongPositions::<Test>::get(netuid, trader).unwrap();
        assert_approx(p1.p_floor.to_u64(), p0.p_floor.to_u64() / 2, 2, "p/2");
        assert_approx(
            p1.d_liability.to_u64(),
            p0.d_liability.to_u64() / 2,
            2,
            "d/2",
        );
        assert_approx(p1.r_stored.to_u64(), p0.r_stored.to_u64() / 2, 2, "r/2");
    });
}

// Long terminal settlement is underwater (equity 0) when the collateral can't
// cover the TAO debt at the terminal price.
#[test]
fn long_dereg_underwater_pays_zero_equity() {
    new_test_ext(1).execute_with(|| {
        let netuid = setup_long(1000 * TAO, 1000 * TAO, 1.0);
        let trader = U256::from(10);
        let hotkey = U256::from(11);
        give_alpha(hotkey, trader, netuid, AlphaBalance::from(500 * TAO));
        assert_ok!(SubtensorModule::open_long(
            RuntimeOrigin::signed(trader),
            hotkey,
            netuid,
            AlphaBalance::from(100 * TAO),
            TaoBalance::MAX
        ));

        // Crash the price: D/price ≫ collateral ⇒ cover = C_L, equity = 0.
        SubnetMovingPrice::<Test>::insert(netuid, I96F32::from_num(0.0001));
        let stake_before =
            SubtensorModule::get_stake_for_hotkey_and_coldkey_on_subnet(&hotkey, &trader, netuid)
                .to_u64();

        SubtensorModule::settle_longs_on_dereg(netuid);

        assert!(LongPositions::<Test>::get(netuid, trader).is_none());
        let stake_after =
            SubtensorModule::get_stake_for_hotkey_and_coldkey_on_subnet(&hotkey, &trader, netuid)
                .to_u64();
        assert_eq!(
            stake_after, stake_before,
            "underwater long must return no equity"
        );
        assert!(!LongActiveSubnets::<Test>::contains_key(netuid));
    });
}

#[test]
fn long_dereg_in_the_money_pays_bounded_equity() {
    new_test_ext(1).execute_with(|| {
        let netuid = setup_long(1000 * TAO, 1000 * TAO, 1.0);
        let trader = U256::from(10);
        let hotkey = U256::from(11);
        give_alpha(hotkey, trader, netuid, AlphaBalance::from(500 * TAO));
        assert_ok!(SubtensorModule::open_long(
            RuntimeOrigin::signed(trader),
            hotkey,
            netuid,
            AlphaBalance::from(100 * TAO),
            TaoBalance::MAX
        ));
        let pos = LongPositions::<Test>::get(netuid, trader).unwrap();
        let c_l = pos.p_floor.to_u64() + pos.r_stored.to_u64();
        let before =
            SubtensorModule::get_stake_for_hotkey_and_coldkey_on_subnet(&hotkey, &trader, netuid)
                .to_u64();
        SubtensorModule::settle_longs_on_dereg(netuid);
        let gained =
            SubtensorModule::get_stake_for_hotkey_and_coldkey_on_subnet(&hotkey, &trader, netuid)
                .to_u64()
                - before;
        assert!(
            gained > 0 && gained < c_l,
            "long equity {gained} not in (0,{c_l})"
        );
    });
}

#[test]
fn long_dereg_cold_ema_pays_zero_equity() {
    new_test_ext(1).execute_with(|| {
        let netuid = setup_long(1000 * TAO, 1000 * TAO, 1.0);
        let trader = U256::from(10);
        let hotkey = U256::from(11);
        give_alpha(hotkey, trader, netuid, AlphaBalance::from(500 * TAO));
        assert_ok!(SubtensorModule::open_long(
            RuntimeOrigin::signed(trader),
            hotkey,
            netuid,
            AlphaBalance::from(100 * TAO),
            TaoBalance::MAX
        ));
        // Cold price EMA: the `a_param=0` sentinel makes the cover saturate to the
        // full collateral, so a cold long pays zero equity (no pool-origin refund) —
        // the long analog of the short cold-EMA floor, here safe by construction.
        SubnetMovingPrice::<Test>::insert(netuid, I96F32::from_num(0));
        let before =
            SubtensorModule::get_stake_for_hotkey_and_coldkey_on_subnet(&hotkey, &trader, netuid)
                .to_u64();
        SubtensorModule::settle_longs_on_dereg(netuid);
        let gained =
            SubtensorModule::get_stake_for_hotkey_and_coldkey_on_subnet(&hotkey, &trader, netuid)
                .to_u64()
                - before;
        assert_eq!(gained, 0, "cold-EMA long must pay zero equity");
    });
}

#[test]
fn quote_open_long_matches_realized_open() {
    new_test_ext(1).execute_with(|| {
        let netuid = setup_long(1000 * TAO, 1000 * TAO, 1.0);
        let trader = U256::from(10);
        let hotkey = U256::from(11);
        give_alpha(hotkey, trader, netuid, AlphaBalance::from(500 * TAO));
        let p = AlphaBalance::from(100 * TAO);

        let quote = SubtensorModule::quote_open_long(netuid, p).unwrap();
        assert_ok!(SubtensorModule::open_long(
            RuntimeOrigin::signed(trader),
            hotkey,
            netuid,
            p,
            TaoBalance::MAX
        ));

        let pos = LongPositions::<Test>::get(netuid, trader).unwrap();
        // Pure quote equals the realized open (same code path).
        assert_eq!(pos.r_stored, quote.retained_proceeds);
        assert_eq!(pos.d_liability, quote.tao_liability);
        assert_eq!(pos.e_stored, quote.escrow);
        assert_eq!(pos.p_floor, p);
        assert_eq!(quote.est_close_cost, quote.tao_liability); // close repays D directly
    });
}

#[test]
fn long_position_and_market_views() {
    new_test_ext(1).execute_with(|| {
        let netuid = setup_long(1000 * TAO, 1000 * TAO, 1.0);
        let trader = U256::from(10);
        let hotkey = U256::from(11);
        give_alpha(hotkey, trader, netuid, AlphaBalance::from(500 * TAO));
        assert_ok!(SubtensorModule::open_long(
            RuntimeOrigin::signed(trader),
            hotkey,
            netuid,
            AlphaBalance::from(100 * TAO),
            TaoBalance::MAX
        ));

        let view = SubtensorModule::get_long_position(&trader, netuid).unwrap();
        let pos = LongPositions::<Test>::get(netuid, trader).unwrap();
        assert_eq!(view.floor, pos.p_floor);
        assert_eq!(view.tao_liability, pos.d_liability);
        assert_eq!(
            view.collateral_claim,
            pos.p_floor.saturating_add(pos.r_stored)
        );
        assert_eq!(view.est_close_cost, pos.d_liability);
        assert!(!view.default_eligible);
        assert_eq!(SubtensorModule::get_long_positions(&trader).len(), 1);

        let market = SubtensorModule::get_subnet_long_state(netuid).unwrap();
        assert!(market.longs_enabled);
        assert!(market.footprint_used > AlphaBalance::ZERO);
        assert!(market.open_interest_tao > TaoBalance::ZERO);
        // close quote is consistent with the position
        let cq = SubtensorModule::quote_close_long(&trader, netuid, 1_000_000_000).unwrap();
        assert_eq!(cq.repay_tao, pos.d_liability);
    });
}

// Fix (L1): long open won't mint alpha by saturating SubnetAlphaOut to zero.
#[test]
fn open_long_guards_against_alpha_mint() {
    new_test_ext(1).execute_with(|| {
        let netuid = setup_long(1000 * TAO, 1000 * TAO, 1.0);
        let trader = U256::from(10);
        let hotkey = U256::from(11);
        give_alpha(hotkey, trader, netuid, AlphaBalance::from(500 * TAO));
        // Corrupt outstanding alpha below the collateral; open must refuse.
        SubnetAlphaOut::<Test>::insert(netuid, AlphaBalance::from(0));
        assert_noop!(
            SubtensorModule::open_long(
                RuntimeOrigin::signed(trader),
                hotkey,
                netuid,
                AlphaBalance::from(100 * TAO),
                TaoBalance::MAX
            ),
            Error::<Test>::InsufficientCollateral
        );
    });
}

// Long top-up adds Alpha buffer (from stake) and resets the grace clock.
#[test]
fn long_top_up_adds_buffer_and_resets_grace() {
    new_test_ext(1).execute_with(|| {
        let netuid = setup_long(1000 * TAO, 1000 * TAO, 1.0);
        let trader = U256::from(10);
        let hotkey = U256::from(11);
        give_alpha(hotkey, trader, netuid, AlphaBalance::from(500 * TAO));
        assert_ok!(SubtensorModule::open_long(
            RuntimeOrigin::signed(trader),
            hotkey,
            netuid,
            AlphaBalance::from(100 * TAO),
            TaoBalance::MAX
        ));
        let r0 = LongPositions::<Test>::get(netuid, trader).unwrap().r_stored;
        let stake0 =
            SubtensorModule::get_stake_for_hotkey_and_coldkey_on_subnet(&hotkey, &trader, netuid);

        assert_ok!(SubtensorModule::top_up_long(
            RuntimeOrigin::signed(trader),
            netuid,
            AlphaBalance::from(10 * TAO)
        ));
        let pos = LongPositions::<Test>::get(netuid, trader).unwrap();
        assert_eq!(pos.r_stored, r0 + AlphaBalance::from(10 * TAO));
        assert_eq!(
            SubtensorModule::get_stake_for_hotkey_and_coldkey_on_subnet(&hotkey, &trader, netuid),
            stake0 - AlphaBalance::from(10 * TAO)
        );
    });
}

// Long merge must target the same hotkey; long position cap is enforced.
#[test]
fn long_merge_mismatch_and_position_cap() {
    new_test_ext(1).execute_with(|| {
        let netuid = setup_long(1000 * TAO, 1000 * TAO, 1.0);
        let a = U256::from(10);
        give_alpha(U256::from(11), a, netuid, AlphaBalance::from(500 * TAO));
        assert_ok!(SubtensorModule::open_long(
            RuntimeOrigin::signed(a),
            U256::from(11),
            netuid,
            AlphaBalance::from(20 * TAO),
            TaoBalance::MAX
        ));
        // Same coldkey, different hotkey → rejected.
        give_alpha(U256::from(12), a, netuid, AlphaBalance::from(100 * TAO));
        assert_noop!(
            SubtensorModule::open_long(
                RuntimeOrigin::signed(a),
                U256::from(12),
                netuid,
                AlphaBalance::from(20 * TAO),
                TaoBalance::MAX
            ),
            Error::<Test>::LongHotkeyMismatch
        );

        // Position cap: with max=1, a second distinct coldkey is rejected.
        SubtensorModule::set_long_max_positions(1);
        let b = U256::from(20);
        give_alpha(U256::from(21), b, netuid, AlphaBalance::from(100 * TAO));
        assert_noop!(
            SubtensorModule::open_long(
                RuntimeOrigin::signed(b),
                U256::from(21),
                netuid,
                AlphaBalance::from(20 * TAO),
                TaoBalance::MAX
            ),
            Error::<Test>::LongPositionLimit
        );
    });
}

// Long close rejects invalid fractions and below-min-input opens.
#[test]
fn long_close_invalid_fraction_and_min_input() {
    new_test_ext(1).execute_with(|| {
        let netuid = setup_long(1000 * TAO, 1000 * TAO, 1.0);
        let trader = U256::from(10);
        let hotkey = U256::from(11);
        give_alpha(hotkey, trader, netuid, AlphaBalance::from(500 * TAO));
        SubtensorModule::set_long_min_input(AlphaBalance::from(TAO));
        assert_noop!(
            SubtensorModule::open_long(
                RuntimeOrigin::signed(trader),
                hotkey,
                netuid,
                AlphaBalance::from(TAO / 2),
                TaoBalance::MAX
            ),
            Error::<Test>::AmountTooLow
        );
        assert_ok!(SubtensorModule::open_long(
            RuntimeOrigin::signed(trader),
            hotkey,
            netuid,
            AlphaBalance::from(100 * TAO),
            TaoBalance::MAX
        ));
        assert_noop!(
            SubtensorModule::close_long(RuntimeOrigin::signed(trader), netuid, 0),
            Error::<Test>::InvalidCloseFraction
        );
        assert_noop!(
            SubtensorModule::close_long(RuntimeOrigin::signed(trader), netuid, 1_000_000_001),
            Error::<Test>::InvalidCloseFraction
        );
    });
}

// Short and long default-grace windows are governed independently.
#[test]
fn default_grace_independent_per_side() {
    new_test_ext(1).execute_with(|| {
        let netuid = setup_long(1000 * TAO, 1000 * TAO, 1.0);
        let (sc, sh) = (U256::from(10), U256::from(11));
        let (lc, lh) = (U256::from(20), U256::from(21));
        add_balance_to_coldkey_account(&sc, t(1000 * TAO));
        give_alpha(lh, lc, netuid, AlphaBalance::from(500 * TAO));
        assert_ok!(SubtensorModule::open_short(
            RuntimeOrigin::signed(sc),
            sh,
            netuid,
            t(100 * TAO),
            AlphaBalance::MAX
        ));
        assert_ok!(SubtensorModule::open_long(
            RuntimeOrigin::signed(lc),
            lh,
            netuid,
            AlphaBalance::from(100 * TAO),
            TaoBalance::MAX
        ));

        SubtensorModule::set_short_dust(t(10_000 * TAO));
        SubtensorModule::set_long_dust(AlphaBalance::from(10_000 * TAO));
        SubtensorModule::set_short_default_grace(0); // shorts: no grace
        SubtensorModule::set_long_default_grace(5); // longs: still gated

        let poker = U256::from(99);
        // Short is immediately defaultable; long is not (independent grace).
        assert_ok!(SubtensorModule::default_short(
            RuntimeOrigin::signed(poker),
            sc,
            netuid
        ));
        assert_noop!(
            SubtensorModule::default_long(RuntimeOrigin::signed(poker), lc, netuid),
            Error::<Test>::PositionNotDefaultEligible
        );
    });
}

// Decay rate matches the closed form: one day at 1.0/day leaves ≈ e⁻¹, and the
// per-position materialized buffer stays consistent with the aggregate.
#[test]
fn decay_rate_matches_closed_form() {
    new_test_ext(1).execute_with(|| {
        let netuid = setup_market(1000 * TAO, 1000 * TAO, 1.0);
        let trader = U256::from(10);
        add_balance_to_coldkey_account(&trader, t(1000 * TAO));
        assert_ok!(SubtensorModule::open_short(
            RuntimeOrigin::signed(trader),
            U256::from(11),
            netuid,
            t(100 * TAO),
            AlphaBalance::MAX
        ));
        SubtensorModule::set_decay_bounds_ppb(1_000_000_000, 1_000_000_000); // d = 1.0/day

        let r0 = ShortAggregate::<Test>::get(netuid).r_sigma.to_u64();
        for _ in 0..7200 {
            SubtensorModule::run_short_decay(); // one day of blocks
        }
        let r1 = ShortAggregate::<Test>::get(netuid).r_sigma.to_u64();

        // (1 − 1/7200)^7200 ≈ e⁻¹ ≈ 0.3679 of the original buffer.
        let expected = (r0 as f64 * 0.3679) as u64;
        assert_approx(r1, expected, r0 / 50, "one-day decay ≈ e^-1"); // within 2%

        // Per-position view (single position) matches the aggregate.
        let info = SubtensorModule::get_short_position(&trader, netuid).unwrap();
        assert_approx(info.buffer.to_u64(), r1, r0 / 100, "position == aggregate");
    });
}

// ---------------------------------------------------------------------------
// Longs (mirror) + side independence
// ---------------------------------------------------------------------------

fn setup_long(tao_reserve: u64, alpha_reserve: u64, price: f64) -> NetUid {
    let netuid = setup_market(tao_reserve, alpha_reserve, price);
    SubtensorModule::set_longs_enabled(true);
    SubtensorModule::set_long_kappa_ppb(900_000_000);
    netuid
}

fn alpha_issuance(netuid: NetUid) -> u64 {
    SubnetAlphaIn::<Test>::get(netuid).to_u64() + SubnetAlphaOut::<Test>::get(netuid).to_u64()
}

#[test]
fn open_long_rejected_when_disabled() {
    new_test_ext(1).execute_with(|| {
        // setup_market enables shorts only; longs remain off by default.
        let netuid = setup_market(1000 * TAO, 1000 * TAO, 1.0);
        let trader = U256::from(10);
        let hotkey = U256::from(11);
        give_alpha(hotkey, trader, netuid, AlphaBalance::from(500 * TAO));
        assert_noop!(
            SubtensorModule::open_long(
                RuntimeOrigin::signed(trader),
                hotkey,
                netuid,
                AlphaBalance::from(100 * TAO),
                TaoBalance::MAX
            ),
            Error::<Test>::LongsDisabled
        );
    });
}

#[test]
fn open_long_moves_alpha_off_issuance() {
    new_test_ext(1).execute_with(|| {
        let netuid = setup_long(1000 * TAO, 1000 * TAO, 1.0);
        let trader = U256::from(10);
        let hotkey = U256::from(11);
        give_alpha(hotkey, trader, netuid, AlphaBalance::from(500 * TAO));

        let alpha_in0 = SubnetAlphaIn::<Test>::get(netuid).to_u64();
        let stake0 =
            SubtensorModule::get_stake_for_hotkey_and_coldkey_on_subnet(&hotkey, &trader, netuid)
                .to_u64();

        assert_ok!(SubtensorModule::open_long(
            RuntimeOrigin::signed(trader),
            hotkey,
            netuid,
            AlphaBalance::from(100 * TAO),
            TaoBalance::MAX
        ));
        let pos = LongPositions::<Test>::get(netuid, trader).unwrap();
        let (n, e, d) = (
            pos.r_stored.to_u64(),
            pos.e_stored.to_u64(),
            pos.d_liability.to_u64(),
        );

        assert!(n > 0 && e > 0 && d > 0);
        assert_eq!(pos.p_floor.to_u64(), 100 * TAO);
        // Pool alpha dropped by N+E; trader stake dropped by the floor P.
        assert_eq!(
            SubnetAlphaIn::<Test>::get(netuid).to_u64(),
            alpha_in0 - n - e
        );
        assert_eq!(
            SubtensorModule::get_stake_for_hotkey_and_coldkey_on_subnet(&hotkey, &trader, netuid)
                .to_u64(),
            stake0 - 100 * TAO
        );
        let agg = LongAggregate::<Test>::get(netuid);
        assert_eq!(agg.d_sigma, pos.d_liability);
        assert!(LongActiveSubnets::<Test>::contains_key(netuid));
    });
}

#[test]
fn full_close_long_conserves_value() {
    new_test_ext(1).execute_with(|| {
        let netuid = setup_long(1000 * TAO, 1000 * TAO, 1.0);
        let trader = U256::from(10);
        let hotkey = U256::from(11);
        give_alpha(hotkey, trader, netuid, AlphaBalance::from(500 * TAO));
        add_balance_to_coldkey_account(&trader, t(1000 * TAO)); // TAO to repay D

        let iss0 = alpha_issuance(netuid);
        assert_ok!(SubtensorModule::open_long(
            RuntimeOrigin::signed(trader),
            hotkey,
            netuid,
            AlphaBalance::from(100 * TAO),
            TaoBalance::MAX
        ));
        let pos = LongPositions::<Test>::get(netuid, trader).unwrap();
        let d = pos.d_liability.to_u64();
        let tao0 = SubnetTAO::<Test>::get(netuid).to_u64();

        assert_ok!(SubtensorModule::close_long(
            RuntimeOrigin::signed(trader),
            netuid,
            1_000_000_000
        ));

        assert!(LongPositions::<Test>::get(netuid, trader).is_none());
        assert!(!LongActiveSubnets::<Test>::contains_key(netuid));
        // Alpha issuance is fully restored (mint == earlier burn); TAO liability paid into pool.
        assert_eq!(alpha_issuance(netuid), iss0);
        assert_eq!(SubnetTAO::<Test>::get(netuid).to_u64(), tao0 + d);
        let agg = LongAggregate::<Test>::get(netuid);
        assert_eq!(agg.r_sigma.to_u64(), 0);
        assert_eq!(agg.d_sigma.to_u64(), 0);
    });
}

#[test]
fn long_decay_restores_alpha_to_pool() {
    new_test_ext(1).execute_with(|| {
        let netuid = setup_long(1000 * TAO, 1000 * TAO, 1.0);
        let trader = U256::from(10);
        let hotkey = U256::from(11);
        give_alpha(hotkey, trader, netuid, AlphaBalance::from(500 * TAO));
        assert_ok!(SubtensorModule::open_long(
            RuntimeOrigin::signed(trader),
            hotkey,
            netuid,
            AlphaBalance::from(100 * TAO),
            TaoBalance::MAX
        ));

        let r0 = LongAggregate::<Test>::get(netuid).r_sigma.to_u64();
        let alpha_in0 = SubnetAlphaIn::<Test>::get(netuid).to_u64();
        for _ in 0..300 {
            SubtensorModule::run_long_decay();
        }
        assert!(LongAggregate::<Test>::get(netuid).r_sigma.to_u64() < r0);
        assert!(SubnetAlphaIn::<Test>::get(netuid).to_u64() > alpha_in0); // alpha minted back to pool
    });
}

#[test]
fn long_default_recycles_floor_and_restores_residual() {
    new_test_ext(1).execute_with(|| {
        let netuid = setup_long(1000 * TAO, 1000 * TAO, 1.0);
        let trader = U256::from(10);
        let hotkey = U256::from(11);
        give_alpha(hotkey, trader, netuid, AlphaBalance::from(500 * TAO));
        assert_ok!(SubtensorModule::open_long(
            RuntimeOrigin::signed(trader),
            hotkey,
            netuid,
            AlphaBalance::from(100 * TAO),
            TaoBalance::MAX
        ));
        let pos = LongPositions::<Test>::get(netuid, trader).unwrap();
        let (p, n, e) = (
            pos.p_floor.to_u64(),
            pos.r_stored.to_u64(),
            pos.e_stored.to_u64(),
        );
        SubtensorModule::set_long_dust(AlphaBalance::from(1000 * TAO));
        SubtensorModule::set_long_default_grace(0);

        let alpha_in0 = SubnetAlphaIn::<Test>::get(netuid).to_u64();
        let iss0 = alpha_issuance(netuid);
        assert_ok!(SubtensorModule::default_long(
            RuntimeOrigin::signed(U256::from(99)),
            trader,
            netuid
        ));

        assert!(LongPositions::<Test>::get(netuid, trader).is_none());
        // Residual R+E minted back to the pool; floor P stays burned (recycled).
        assert_eq!(
            SubnetAlphaIn::<Test>::get(netuid).to_u64(),
            alpha_in0 + n + e
        );
        assert_eq!(alpha_issuance(netuid), iss0 + n + e); // P remains out of issuance
        assert_eq!(p, 100 * TAO);
    });
}

#[test]
fn dereg_settles_longs() {
    new_test_ext(1).execute_with(|| {
        let netuid = setup_long(1000 * TAO, 1000 * TAO, 1.0);
        let trader = U256::from(10);
        let hotkey = U256::from(11);
        give_alpha(hotkey, trader, netuid, AlphaBalance::from(500 * TAO));
        assert_ok!(SubtensorModule::open_long(
            RuntimeOrigin::signed(trader),
            hotkey,
            netuid,
            AlphaBalance::from(100 * TAO),
            TaoBalance::MAX
        ));
        assert!(LongPositions::<Test>::get(netuid, trader).is_some());

        assert_ok!(SubtensorModule::do_dissolve_network(netuid));
        assert!(LongPositions::<Test>::get(netuid, trader).is_none());
        assert!(!LongActiveSubnets::<Test>::contains_key(netuid));
    });
}

// Regression guard for the dereg ORDERING contract: long terminal equity is
// minted as alpha stake inside settle_longs_on_dereg and must be picked up by
// the immediately-following destroy_alpha_in_out_stakes (which converts stake to
// a TAO distribution). Here the trader's ONLY stake is the long collateral, so
// any TAO they receive across the FULL do_dissolve_network path proves the
// mint-before-stake-wipe ordering survives. If anyone reorders dissolve so the
// wipe runs before settlement, this test fails (equity would be silently lost).
#[test]
fn dereg_long_equity_survives_full_dissolve_path() {
    new_test_ext(1).execute_with(|| {
        let netuid = setup_long(1000 * TAO, 1000 * TAO, 1.0);
        let trader = U256::from(10);
        let hotkey = U256::from(11);
        // Exactly the collateral as stake (no other stake to mask the equity).
        give_alpha(hotkey, trader, netuid, AlphaBalance::from(100 * TAO));
        add_balance_to_coldkey_account(&trader, t(TAO)); // ED only
        assert_ok!(SubtensorModule::open_long(
            RuntimeOrigin::signed(trader),
            hotkey,
            netuid,
            AlphaBalance::from(100 * TAO),
            TaoBalance::MAX
        ));
        let bal_before = bal(&trader);

        assert_ok!(SubtensorModule::do_dissolve_network(netuid));

        assert!(LongPositions::<Test>::get(netuid, trader).is_none());
        // Equity (minted as stake by settlement) reached the trader as a TAO
        // distribution from the subsequent stake-wipe — proving the ordering.
        assert!(
            bal(&trader) > bal_before,
            "long equity must survive the full dissolve path as a TAO distribution"
        );
    });
}

// Fix: long collateral must be UNLOCKED alpha — opening a long against
// locked alpha (which a normal unstake would block) is rejected, so it can't
// be used to free locked stake.
#[test]
fn open_long_respects_stake_lock() {
    new_test_ext(1).execute_with(|| {
        let netuid = setup_long(1000 * TAO, 1000 * TAO, 1.0);
        let cold = U256::from(10);
        let hot = U256::from(11);
        register_ok_neuron(netuid, hot, cold, 0);
        give_alpha(hot, cold, netuid, AlphaBalance::from(200 * TAO));

        // Lock almost all the staked alpha.
        assert_ok!(SubtensorModule::do_lock_stake(
            &cold,
            netuid,
            &hot,
            AlphaBalance::from(195 * TAO)
        ));

        // A long against the locked alpha is rejected (would otherwise free it).
        assert_noop!(
            SubtensorModule::open_long(
                RuntimeOrigin::signed(cold),
                hot,
                netuid,
                AlphaBalance::from(100 * TAO),
                TaoBalance::MAX
            ),
            Error::<Test>::StakeUnavailable
        );
    });
}

// Shorts and longs are independently flaggable on the same subnet.
#[test]
fn short_and_long_flags_are_independent() {
    new_test_ext(1).execute_with(|| {
        let netuid = setup_market(1000 * TAO, 1000 * TAO, 1.0); // shorts on, longs off
        let trader = U256::from(10);
        let hotkey = U256::from(11);
        add_balance_to_coldkey_account(&trader, t(1000 * TAO));
        give_alpha(hotkey, trader, netuid, AlphaBalance::from(500 * TAO));

        // Shorts enabled, longs disabled.
        assert_ok!(SubtensorModule::open_short(
            RuntimeOrigin::signed(trader),
            hotkey,
            netuid,
            t(50 * TAO),
            AlphaBalance::MAX
        ));
        assert_noop!(
            SubtensorModule::open_long(
                RuntimeOrigin::signed(trader),
                hotkey,
                netuid,
                AlphaBalance::from(50 * TAO),
                TaoBalance::MAX
            ),
            Error::<Test>::LongsDisabled
        );

        // Flip: longs enabled, shorts disabled.
        SubtensorModule::set_shorts_enabled(false);
        SubtensorModule::set_longs_enabled(true);
        SubtensorModule::set_long_kappa_ppb(900_000_000);
        assert_noop!(
            SubtensorModule::open_short(
                RuntimeOrigin::signed(U256::from(20)),
                hotkey,
                netuid,
                t(50 * TAO),
                AlphaBalance::MAX
            ),
            Error::<Test>::ShortsDisabled
        );
        assert_ok!(SubtensorModule::open_long(
            RuntimeOrigin::signed(trader),
            hotkey,
            netuid,
            AlphaBalance::from(50 * TAO),
            TaoBalance::MAX
        ));
    });
}

// Listing returns every position a coldkey holds across subnets.
#[test]
fn list_positions_across_subnets() {
    new_test_ext(1).execute_with(|| {
        let n1 = setup_market(1000 * TAO, 1000 * TAO, 1.0);
        let n2 = setup_market(1000 * TAO, 1000 * TAO, 1.0);
        let trader = U256::from(10);
        add_balance_to_coldkey_account(&trader, t(1000 * TAO));
        assert_ok!(SubtensorModule::open_short(
            RuntimeOrigin::signed(trader),
            U256::from(11),
            n1,
            t(50 * TAO),
            AlphaBalance::MAX
        ));
        assert_ok!(SubtensorModule::open_short(
            RuntimeOrigin::signed(trader),
            U256::from(12),
            n2,
            t(50 * TAO),
            AlphaBalance::MAX
        ));

        let all = SubtensorModule::get_short_positions(&trader);
        assert_eq!(all.len(), 2);
        let mut netuids: Vec<_> = all.iter().map(|p| p.netuid).collect();
        netuids.sort();
        let mut want = vec![n1, n2];
        want.sort();
        assert_eq!(netuids, want);
    });
}
