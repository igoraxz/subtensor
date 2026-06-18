//! Fixed-liability covered continuous-unwind derivatives (spec v3.6.1).
//!
//! Both sides are implemented and independently gated (`ShortsEnabled` /
//! `LongsEnabled`, both default-off). Shorts live here; the long mirror is in
//! `long.rs`. The client/RPC read layer (`quote_*`, `get_*`) exists for both
//! sides (short views here, long views in `long.rs`).
//!
//! Custody model. Shorts park floor/buffer/escrow TAO in a dedicated per-subnet
//! custody account; longs have no custody account and instead track parked Alpha
//! via issuance accounting (burned at open, minted back on restore/close). Pool
//! reserves, `TotalStake`, and issuance move in lockstep and derivative legs
//! never write TaoFlow.
//!
//! Custody solvency invariant. `custody_balance(netuid)` (shorts) and the burned
//! Alpha (longs) equal `Σ materialized (P + R(t) + E(t))` **to within per-block
//! floor rounding**. The aggregate Σ-decay floors faster than the per-position
//! `exp` decay, so the drift is always in the safe direction (custody ≥
//! obligations); residual dust is reclaimed by the terminal sweep at dereg.

use super::*;
use frame_support::traits::tokens::{Fortitude, Precision, Preservation, fungible::Balanced};
use safe_math::FixedExt;
use sp_runtime::traits::AccountIdConversion;
use substrate_fixed::types::I64F64;
use subtensor_runtime_common::Token;

pub mod long;
pub mod types;
pub use types::*;

/// 12s blocks → 7200 per day. Decay rates are pro-rated per block.
const BLOCKS_PER_DAY: u64 = 7200;
/// Bisection tolerance for fixed-point square roots.
fn sqrt_eps() -> I64F64 {
    I64F64::from_num(0.000_000_001)
}

impl<T: Config> Pallet<T> {
    // ---- conversions ----------------------------------------------------

    fn tao_f(t: TaoBalance) -> I64F64 {
        I64F64::from_num(t.to_u64())
    }
    fn alpha_f(a: AlphaBalance) -> I64F64 {
        I64F64::from_num(a.to_u64())
    }
    fn to_tao(x: I64F64) -> TaoBalance {
        TaoBalance::from(x.max(I64F64::from_num(0)).saturating_to_num::<u64>())
    }
    fn to_alpha(x: I64F64) -> AlphaBalance {
        AlphaBalance::from(x.max(I64F64::from_num(0)).saturating_to_num::<u64>())
    }
    fn mul_tao(t: TaoBalance, f: I64F64) -> TaoBalance {
        Self::to_tao(Self::tao_f(t).saturating_mul(f))
    }
    fn mul_alpha(a: AlphaBalance, f: I64F64) -> AlphaBalance {
        Self::to_alpha(Self::alpha_f(a).saturating_mul(f))
    }

    // ---- accounts -------------------------------------------------------

    /// Per-subnet account holding parked derivative TAO (floor + buffer + escrow).
    /// Distinct from the subnet pool account so pool reserves are never polluted.
    pub fn short_custody_account(netuid: NetUid) -> T::AccountId {
        T::SubtensorPalletId::get().into_sub_account_truncating(("shrt", u16::from(netuid)))
    }

    /// Recycle TAO out of the protocol custody account (reduce issuance). Unlike
    /// `recycle_tao`, this does not preserve an existential deposit, so the
    /// custody account can be drained to zero.
    fn recycle_custody_tao(custody: &T::AccountId, amount: TaoBalance) {
        if amount.is_zero() {
            return;
        }
        // Never recycle (and never reduce issuance by) more than is actually
        // held: caps an `Exact` withdraw failure that would desync issuance.
        let amt = Self::get_coldkey_balance(custody).min(amount.into());
        TotalIssuance::<T>::mutate(|ti| *ti = ti.saturating_sub(amt));
        let _ = <T as Config>::Currency::withdraw(
            custody,
            amt,
            Precision::Exact,
            Preservation::Expendable,
            Fortitude::Force,
        );
    }

    // ---- references (spec §3, §4) --------------------------------------

    /// Conservative TAO reference `T_ref = min(T_live, T_EMA)`, with
    /// `T_EMA = pEMA · A_live` reconstructed from the existing price EMA.
    fn short_t_ref(netuid: NetUid) -> I64F64 {
        let t_live = Self::tao_f(SubnetTAO::<T>::get(netuid));
        let a_live = Self::alpha_f(SubnetAlphaIn::<T>::get(netuid));
        let pema = I64F64::from_num(Self::get_moving_alpha_price(netuid));
        // `pema` is the upstream `min(spot,1.0)`-clamped moving price, so `pema ≤ ~1`
        // and `pema·a_live ≤ a_live (≤ ~2e16 rao)` stays well inside I64F64 — no
        // saturation. (The guarantees here hold for price ≤ ~1.0; see DESIGN.md.)
        let t_ema = pema.saturating_mul(a_live);
        // A cold price EMA (`pema == 0`, e.g. a freshly created subnet) must not
        // lock the market; fall back to the live reserve until it warms up.
        if t_ema <= I64F64::from_num(0) {
            t_live
        } else {
            t_live.min(t_ema)
        }
    }

    /// Convex decay curve `d(u) = d_min + (d_max − d_min)·u²` (spec §6.2),
    /// shared by both sides (the rate is denomination-agnostic).
    fn decay_curve(u: I64F64) -> I64F64 {
        let dmin = DecayMin::<T>::get();
        let dmax = DecayMax::<T>::get();
        dmin.saturating_add(dmax.saturating_sub(dmin).saturating_mul(u).saturating_mul(u))
    }

    /// Utilization ratio `min(1, S / cap)`.
    fn utilization(s: I64F64, cap: I64F64) -> I64F64 {
        if cap > I64F64::from_num(0) {
            s.safe_div(cap).min(I64F64::from_num(1))
        } else {
            I64F64::from_num(0)
        }
    }

    /// Current short daily decay rate at the live short footprint.
    fn short_daily_decay(netuid: NetUid, b_sigma: TaoBalance) -> I64F64 {
        let cap = ShortKappa::<T>::get().saturating_mul(Self::short_t_ref(netuid));
        Self::decay_curve(Self::utilization(Self::tao_f(b_sigma), cap))
    }

    // ---- open-time math (spec §4.1–4.3, Appendix A.1) -------------------

    /// Solve gross collateral `C` and retained proceeds `N` from input `P`
    /// (spec §4.2). Side-agnostic: `ref_reserve` is `T_ref` for shorts / `A_ref`
    /// for longs, `lambda` the per-side base LTV. Returns `None` if `N ≤ 0`.
    fn solve_collateral(
        p: I64F64,
        ref_reserve: I64F64,
        s: I64F64,
        lambda: I64F64,
    ) -> Option<(I64F64, I64F64)> {
        let t_ref = ref_reserve;
        if t_ref <= I64F64::from_num(0) || lambda <= I64F64::from_num(0) {
            return None;
        }
        let one = I64F64::from_num(1);
        let two = I64F64::from_num(2);
        let four = I64F64::from_num(4);
        // a = λ²/T_ref ; b = 1 − λ + 2λS/T_ref
        let a = lambda.saturating_mul(lambda).safe_div(t_ref);
        let b = one
            .saturating_sub(lambda)
            .saturating_add(two.saturating_mul(lambda).saturating_mul(s).safe_div(t_ref));
        // Positive root of `a·C² + b·C − P = 0`. Use the cancellation-stable form
        //   C = 2P / (b + √(b² + 4aP))
        // rather than the algebraically-equal `(√(b²+4aP) − b) / 2a`: the latter
        // subtracts two nearly-equal positives when `4aP ≪ b²` (small `a` = large
        // pool / small position) and then divides by the tiny `2a`, compounding the
        // catastrophic cancellation; the stable form sums two positives and never
        // divides by `a` (it also limits gracefully to `P/b` as `a → 0`). This
        // follows the codebase's preference for numerically-robust fixed-point math.
        let disc = b
            .saturating_mul(b)
            .saturating_add(four.saturating_mul(a).saturating_mul(p));
        let root = disc.checked_sqrt(sqrt_eps())?;
        let c = two.saturating_mul(p).safe_div(b.saturating_add(root));
        let n = c.saturating_sub(p);
        if n <= I64F64::from_num(0) || c <= I64F64::from_num(0) {
            return None;
        }
        Some((c, n))
    }

    /// Pool fraction `ϕ = (1 − √(1 − 4N/T))/2` (spec §4.3). Returns `None` if the
    /// remove-and-sell-back domain `4N ≤ T` fails.
    fn solve_phi(n: I64F64, t_live: I64F64) -> Option<I64F64> {
        if t_live <= I64F64::from_num(0) {
            return None;
        }
        let one = I64F64::from_num(1);
        let four = I64F64::from_num(4);
        let frac = four.saturating_mul(n).safe_div(t_live);
        if frac > one {
            return None;
        }
        let root = one.saturating_sub(frac).checked_sqrt(sqrt_eps())?;
        Some(one.saturating_sub(root).safe_div(I64F64::from_num(2)))
    }

    /// Keep the active-short-subnet set in sync with the aggregate: a subnet is
    /// tracked iff it still has any live short state. The per-block decay tick
    /// iterates only this set instead of every subnet.
    fn sync_active_short(netuid: NetUid, agg: &ShortAgg) {
        if agg.r_sigma.is_zero()
            && agg.e_sigma.is_zero()
            && agg.b_sigma.is_zero()
            && agg.q_sigma.is_zero()
        {
            ShortActiveSubnets::<T>::remove(netuid);
        } else {
            ShortActiveSubnets::<T>::insert(netuid, ());
        }
    }

    /// `−ln(1 − δ) = δ + δ²/2 + δ³/3 + …` for the small per-block decay `δ`.
    ///
    /// Computed directly from the series rather than `checked_ln(1 − δ)`, which
    /// is imprecise (and can return the wrong sign) for arguments just below 1.
    /// This keeps the aggregate factor `g = 1 − δ` and the per-position factor
    /// `exp(−ΔΩ) = Π g` consistent to within per-block floor rounding (the
    /// 3-term series and `checked_exp`'s 7-term series are both truncations).
    fn neg_ln_one_minus(delta: I64F64) -> I64F64 {
        let d2 = delta.saturating_mul(delta);
        let d3 = d2.saturating_mul(delta);
        delta
            .saturating_add(d2.saturating_mul(I64F64::from_num(0.5)))
            .saturating_add(d3.saturating_mul(I64F64::from_num(1.0 / 3.0)))
    }

    /// When the last position on a subnet closes, drop the aggregate and the
    /// active-set entry so the per-block decay tick stops visiting it (otherwise
    /// floor-rounding dust in `r_sigma` keeps the subnet "active" forever). Any
    /// residual custody dust is reclaimed by the terminal sweep at dereg.
    fn cleanup_short_if_empty(netuid: NetUid) {
        if ShortPositionCount::<T>::get(netuid) == 0 {
            ShortAggregate::<T>::remove(netuid);
            ShortActiveSubnets::<T>::remove(netuid);
        }
    }

    /// Materialize a position to the current accumulator: `f = exp(−(Ω − Ω_entry))`.
    fn materialize_short(pos: &mut ShortPosition<T::AccountId>, omega_now: I64F64) {
        // `Ω` only ever grows, so `arg ≤ 0` and `f ≤ 1` (decay never inflates).
        // The `unwrap_or(0)` below is correct, not a silent failure: a large
        // negative `arg` legitimately decays the buffer toward 0. Clamp `arg ≤ 0`
        // defensively so an (impossible) positive `arg` can't yield `f > 1`.
        let arg = pos
            .omega_entry
            .saturating_sub(omega_now)
            .min(I64F64::from_num(0));
        let f = arg.checked_exp().unwrap_or_else(|| I64F64::from_num(0));
        pos.r_stored = Self::mul_tao(pos.r_stored, f);
        pos.e_stored = Self::mul_tao(pos.e_stored, f);
        pos.b_stored = Self::mul_tao(pos.b_stored, f);
        pos.omega_entry = omega_now;
    }

    // ---- user operations (spec §8) -------------------------------------

    /// Open (or merge into) a covered short (spec §8.1, §8.6).
    #[frame_support::transactional]
    pub fn do_open_short(
        origin: OriginFor<T>,
        hotkey: T::AccountId,
        netuid: NetUid,
        position_input: TaoBalance,
        max_alpha_liability: AlphaBalance,
    ) -> DispatchResult {
        let coldkey = ensure_signed(origin)?;
        ensure!(ShortsEnabled::<T>::get(), Error::<T>::ShortsDisabled);
        ensure!(Self::if_subnet_exist(netuid), Error::<T>::SubnetNotExists);
        ensure!(
            SubnetMechanism::<T>::get(netuid) == 1,
            Error::<T>::SubnetNotDynamic
        );
        ensure!(
            position_input >= ShortMinInput::<T>::get(),
            Error::<T>::AmountTooLow
        );

        let mut agg = ShortAggregate::<T>::get(netuid);
        let t_ref = Self::short_t_ref(netuid);
        let p = Self::tao_f(position_input);

        let (c, n) = Self::solve_collateral(p, t_ref, Self::tao_f(agg.b_sigma), ShortBaseLtv::<T>::get())
            .ok_or(Error::<T>::EffectiveLtvNonPositive)?;
        let b = ShortBaseLtv::<T>::get().saturating_mul(c);

        // Capacity: S + B ≤ κ_S · T_ref (also bounds same-block stacked opens).
        ensure!(
            Self::tao_f(agg.b_sigma).saturating_add(b) <= ShortKappa::<T>::get().saturating_mul(t_ref),
            Error::<T>::ShortCapacityExceeded
        );

        let t_live = Self::tao_f(SubnetTAO::<T>::get(netuid));
        let a_live = Self::alpha_f(SubnetAlphaIn::<T>::get(netuid));
        let phi = Self::solve_phi(n, t_live).ok_or(Error::<T>::ReserveDomainExceeded)?;

        let n_tao = Self::to_tao(n);
        let e_tao = Self::to_tao(phi.saturating_mul(t_live));
        let b_tao = Self::to_tao(b);
        let q_alpha = Self::to_alpha(phi.saturating_mul(a_live));
        ensure!(!n_tao.is_zero(), Error::<T>::RetainedProceedsNonPositive);

        // Caller-signed execution bound (anti-sandwich): the alpha liability
        // derived from live reserves at inclusion must not exceed the maximum the
        // trader accepted. `AlphaBalance::MAX` opts out of the bound.
        ensure!(q_alpha <= max_alpha_liability, Error::<T>::SlippageTooHigh);

        // Validate-before-mutate: all fallible eligibility checks that do not
        // depend on the realized legs run BEFORE any funds move, so a rejected
        // open never strands custody TAO or desyncs pool/`TotalStake` accounting.
        match ShortPositions::<T>::get(netuid, &coldkey) {
            Some(existing) => {
                ensure!(existing.hotkey == hotkey, Error::<T>::ShortHotkeyMismatch)
            }
            None => ensure!(
                ShortPositionCount::<T>::get(netuid) < ShortMaxPositions::<T>::get(),
                Error::<T>::ShortPositionLimit
            ),
        }

        let custody = Self::short_custody_account(netuid);
        let subnet_account =
            Self::get_subnet_account_id(netuid).ok_or(Error::<T>::SubnetNotExists)?;

        // 1. Trader posts floor P into custody (fails early if underfunded).
        Self::transfer_tao(&coldkey, &custody, position_input.into())?;
        // 2. Remove N+E TAO from the pool into custody (the downward price impact).
        let removed = n_tao.saturating_add(e_tao);
        Self::transfer_tao(&subnet_account, &custody, removed.into())?;
        Self::decrease_provided_tao_reserve(netuid, removed);
        TotalStake::<T>::mutate(|t| *t = t.saturating_sub(removed));

        let block = Self::get_current_block_as_u64();
        let pos = match ShortPositions::<T>::get(netuid, &coldkey) {
            Some(mut existing) => {
                // Hotkey match was validated before any mutation above.
                Self::materialize_short(&mut existing, agg.omega);
                existing.p_floor = existing.p_floor.saturating_add(position_input);
                existing.q_liability = existing.q_liability.saturating_add(q_alpha);
                existing.r_stored = existing.r_stored.saturating_add(n_tao);
                existing.e_stored = existing.e_stored.saturating_add(e_tao);
                existing.b_stored = existing.b_stored.saturating_add(b_tao);
                existing.last_active = block;
                existing
            }
            None => {
                // Position limit was validated before any mutation above; bump
                // the per-subnet count so dereg settlement work stays bounded.
                let count = ShortPositionCount::<T>::get(netuid);
                ShortPositionCount::<T>::insert(netuid, count.saturating_add(1));
                ShortPosition {
                    hotkey,
                    p_floor: position_input,
                    q_liability: q_alpha,
                    r_stored: n_tao,
                    e_stored: e_tao,
                    b_stored: b_tao,
                    omega_entry: agg.omega,
                    last_active: block,
                }
            }
        };
        ShortPositions::<T>::insert(netuid, &coldkey, pos);

        agg.r_sigma = agg.r_sigma.saturating_add(n_tao);
        agg.e_sigma = agg.e_sigma.saturating_add(e_tao);
        agg.b_sigma = agg.b_sigma.saturating_add(b_tao);
        agg.q_sigma = agg.q_sigma.saturating_add(q_alpha);
        ShortAggregate::<T>::insert(netuid, agg);
        ShortActiveSubnets::<T>::insert(netuid, ());

        Self::deposit_event(Event::ShortOpened {
            coldkey,
            netuid,
            position_input,
            retained_proceeds: n_tao,
            alpha_liability: q_alpha,
            escrow: e_tao,
        });
        Ok(())
    }

    /// Top up the carry buffer `R` with fresh capital (spec §8.2).
    #[frame_support::transactional]
    pub fn do_top_up_short(
        origin: OriginFor<T>,
        netuid: NetUid,
        amount: TaoBalance,
    ) -> DispatchResult {
        let coldkey = ensure_signed(origin)?;
        ensure!(!amount.is_zero(), Error::<T>::AmountTooLow);
        let mut pos =
            ShortPositions::<T>::get(netuid, &coldkey).ok_or(Error::<T>::ShortPositionNotFound)?;
        let mut agg = ShortAggregate::<T>::get(netuid);
        Self::materialize_short(&mut pos, agg.omega);

        Self::transfer_tao(&coldkey, &Self::short_custody_account(netuid), amount.into())?;
        pos.r_stored = pos.r_stored.saturating_add(amount);
        pos.last_active = Self::get_current_block_as_u64();
        agg.r_sigma = agg.r_sigma.saturating_add(amount);

        ShortPositions::<T>::insert(netuid, &coldkey, pos);
        ShortAggregate::<T>::insert(netuid, agg);
        Self::deposit_event(Event::ShortToppedUp {
            coldkey,
            netuid,
            amount,
        });
        Ok(())
    }

    /// Partial (`fraction_ppb < 1e9`) or full (`= 1e9`) close (spec §8.3–8.5).
    #[frame_support::transactional]
    pub fn do_close_short(
        origin: OriginFor<T>,
        netuid: NetUid,
        fraction_ppb: u64,
    ) -> DispatchResult {
        let coldkey = ensure_signed(origin)?;
        ensure!(
            fraction_ppb > 0 && fraction_ppb <= 1_000_000_000,
            Error::<T>::InvalidCloseFraction
        );
        let rho = I64F64::from_num(fraction_ppb).safe_div(I64F64::from_num(1_000_000_000u64));

        let mut pos =
            ShortPositions::<T>::get(netuid, &coldkey).ok_or(Error::<T>::ShortPositionNotFound)?;
        let mut agg = ShortAggregate::<T>::get(netuid);
        Self::materialize_short(&mut pos, agg.omega);

        let q_close = Self::mul_alpha(pos.q_liability, rho);
        let r_close = Self::mul_tao(pos.r_stored, rho);
        let e_close = Self::mul_tao(pos.e_stored, rho);
        let p_close = Self::mul_tao(pos.p_floor, rho);
        let b_close = Self::mul_tao(pos.b_stored, rho);

        // Trader repays ρQ alpha from staked balance at the position hotkey.
        ensure!(
            Self::get_stake_for_hotkey_and_coldkey_on_subnet(&pos.hotkey, &coldkey, netuid)
                >= q_close,
            Error::<T>::InsufficientAlphaToClose
        );
        // Guard against minting alpha: the repaid `q_close` must come out of
        // outstanding stake, never saturate `SubnetAlphaOut` to zero.
        ensure!(
            SubnetAlphaOut::<T>::get(netuid) >= q_close,
            Error::<T>::InsufficientAlphaToClose
        );
        // The repayment alpha must be unlocked (respect stake locks like unstake).
        Self::ensure_available_to_unstake(&coldkey, netuid, q_close)?;
        Self::decrease_stake_for_hotkey_and_coldkey_on_subnet(&pos.hotkey, &coldkey, netuid, q_close);
        SubnetAlphaOut::<T>::mutate(netuid, |o| *o = o.saturating_sub(q_close));
        Self::increase_provided_alpha_reserve(netuid, q_close);

        let custody = Self::short_custody_account(netuid);
        let subnet_account =
            Self::get_subnet_account_id(netuid).ok_or(Error::<T>::SubnetNotExists)?;
        // Settle escrow ρE back to the pool, return ρ(P+R) to the trader.
        if !e_close.is_zero() {
            Self::transfer_tao(&custody, &subnet_account, e_close.into())?;
            Self::increase_provided_tao_reserve(netuid, e_close);
            TotalStake::<T>::mutate(|t| *t = t.saturating_add(e_close));
        }
        let returned = p_close.saturating_add(r_close);
        if !returned.is_zero() {
            Self::transfer_tao(&custody, &coldkey, returned.into())?;
        }

        pos.q_liability = pos.q_liability.saturating_sub(q_close);
        pos.r_stored = pos.r_stored.saturating_sub(r_close);
        pos.e_stored = pos.e_stored.saturating_sub(e_close);
        pos.p_floor = pos.p_floor.saturating_sub(p_close);
        pos.b_stored = pos.b_stored.saturating_sub(b_close);

        agg.q_sigma = agg.q_sigma.saturating_sub(q_close);
        agg.r_sigma = agg.r_sigma.saturating_sub(r_close);
        agg.e_sigma = agg.e_sigma.saturating_sub(e_close);
        agg.b_sigma = agg.b_sigma.saturating_sub(b_close);
        Self::sync_active_short(netuid, &agg);
        ShortAggregate::<T>::insert(netuid, agg);

        if fraction_ppb == 1_000_000_000 || pos.p_floor.is_zero() {
            ShortPositions::<T>::remove(netuid, &coldkey);
            ShortPositionCount::<T>::mutate(netuid, |c| *c = c.saturating_sub(1));
            Self::cleanup_short_if_empty(netuid);
        } else {
            ShortPositions::<T>::insert(netuid, &coldkey, pos);
        }
        Self::deposit_event(Event::ShortClosed {
            coldkey,
            netuid,
            fraction_ppb,
            repaid_alpha: q_close,
            returned,
        });
        Ok(())
    }

    /// Permissionless default once the buffer has decayed to dust (spec §7.4).
    #[frame_support::transactional]
    pub fn do_default_short(
        origin: OriginFor<T>,
        coldkey: T::AccountId,
        netuid: NetUid,
    ) -> DispatchResult {
        ensure_signed(origin)?;
        let mut pos =
            ShortPositions::<T>::get(netuid, &coldkey).ok_or(Error::<T>::ShortPositionNotFound)?;
        let mut agg = ShortAggregate::<T>::get(netuid);
        Self::materialize_short(&mut pos, agg.omega);
        ensure!(
            pos.r_stored <= ShortDust::<T>::get(),
            Error::<T>::PositionNotDefaultEligible
        );
        // Anti-snipe: a third party cannot default within the grace window after
        // the owner's last action, so the owner always has time to top up.
        ensure!(
            Self::get_current_block_as_u64()
                >= pos.last_active.saturating_add(ShortDefaultGrace::<T>::get()),
            Error::<T>::PositionNotDefaultEligible
        );

        let custody = Self::short_custody_account(netuid);
        let subnet_account =
            Self::get_subnet_account_id(netuid).ok_or(Error::<T>::SubnetNotExists)?;
        // Restore residual R+E to the pool; recycle the floor P; extinguish Q.
        let residual = pos.r_stored.saturating_add(pos.e_stored);
        if !residual.is_zero() {
            Self::transfer_tao(&custody, &subnet_account, residual.into())?;
            Self::increase_provided_tao_reserve(netuid, residual);
            TotalStake::<T>::mutate(|t| *t = t.saturating_add(residual));
        }
        Self::recycle_custody_tao(&custody, pos.p_floor);

        agg.r_sigma = agg.r_sigma.saturating_sub(pos.r_stored);
        agg.e_sigma = agg.e_sigma.saturating_sub(pos.e_stored);
        agg.b_sigma = agg.b_sigma.saturating_sub(pos.b_stored);
        agg.q_sigma = agg.q_sigma.saturating_sub(pos.q_liability);
        Self::sync_active_short(netuid, &agg);
        ShortAggregate::<T>::insert(netuid, agg);
        ShortPositions::<T>::remove(netuid, &coldkey);
        ShortPositionCount::<T>::mutate(netuid, |c| *c = c.saturating_sub(1));
        Self::cleanup_short_if_empty(netuid);

        Self::deposit_event(Event::ShortDefaulted { coldkey, netuid });
        Ok(())
    }

    // ---- per-block decay + restoration (spec §6.4–6.5, §12.4) ----------

    /// O(1)-per-subnet aggregate decay tick with one-sided TAO restoration zap.
    /// Iterates only subnets with live short state (`ShortActiveSubnets`), whose
    /// size is bounded by the total subnet count (governance-capped), so the
    /// per-block hook cost is O(active subnets) with O(1) work each — bounded, but
    /// currently unmetered; real weight benchmarking is a tracked pre-mainnet item.
    pub fn run_short_decay() {
        let active: Vec<NetUid> = ShortActiveSubnets::<T>::iter_keys().collect();
        for netuid in active {
            let mut agg = ShortAggregate::<T>::get(netuid);
            if agg.r_sigma.is_zero() && agg.e_sigma.is_zero() && agg.b_sigma.is_zero() {
                continue;
            }
            let d_day = Self::short_daily_decay(netuid, agg.b_sigma);
            let delta = d_day.safe_div(I64F64::from_num(BLOCKS_PER_DAY));
            if delta <= I64F64::from_num(0) {
                continue;
            }
            let dr = Self::mul_tao(agg.r_sigma, delta);
            let de = Self::mul_tao(agg.e_sigma, delta);
            let db = Self::mul_tao(agg.b_sigma, delta);
            let restore = dr.saturating_add(de);

            // Restoration zap FIRST, then commit the decay. The decayed R+E is moved
            // from custody into the pool; only if that transfer actually lands do we
            // advance Ω, shrink the aggregates, and credit reserves. If it fails
            // (e.g. a dust shortfall) we leave the aggregate AND Ω untouched and
            // retry next block — so the per-position `exp(−ΔΩ)` materialization can
            // never decay ahead of TAO that is still sitting in custody (the
            // custody ≥ obligations invariant holds even on a failed transfer, and
            // a short custody can never inflate `SubnetTAO` / `TotalStake`).
            if !restore.is_zero() {
                let subnet_account = match Self::get_subnet_account_id(netuid) {
                    Some(a) => a,
                    None => continue,
                };
                if Self::transfer_tao(
                    &Self::short_custody_account(netuid),
                    &subnet_account,
                    restore.into(),
                )
                .is_err()
                {
                    continue;
                }
                Self::increase_provided_tao_reserve(netuid, restore);
                TotalStake::<T>::mutate(|t| *t = t.saturating_add(restore));
            }

            agg.r_sigma = agg.r_sigma.saturating_sub(dr);
            agg.e_sigma = agg.e_sigma.saturating_sub(de);
            agg.b_sigma = agg.b_sigma.saturating_sub(db);
            // Ω ← Ω + (−ln(1−δ)), so a later exp(−ΔΩ) reproduces Π(1−δ) exactly.
            agg.omega = agg.omega.saturating_add(Self::neg_ln_one_minus(delta));
            ShortAggregate::<T>::insert(netuid, agg);
        }
    }

    // ---- terminal deregistration settlement (spec §11.4) ---------------

    /// Settle all shorts on a subnet at deregistration. Must run before the
    /// pool is drained so restored escrow joins the terminal distribution.
    pub fn settle_shorts_on_dereg(netuid: NetUid) {
        let agg = ShortAggregate::<T>::get(netuid);
        let pema = I64F64::from_num(Self::get_moving_alpha_price(netuid));
        let custody = Self::short_custody_account(netuid);
        let subnet_account = match Self::get_subnet_account_id(netuid) {
            Some(a) => a,
            None => return,
        };

        let positions: Vec<(T::AccountId, ShortPosition<T::AccountId>)> =
            ShortPositions::<T>::iter_prefix(netuid).collect();
        for (coldkey, mut pos) in positions {
            Self::materialize_short(&mut pos, agg.omega);

            // Escrow returns to the pool (joins terminal distribution). Credit
            // reserves only on a successful transfer.
            if !pos.e_stored.is_zero()
                && Self::transfer_tao(&custody, &subnet_account, pos.e_stored.into()).is_ok()
            {
                Self::increase_provided_tao_reserve(netuid, pos.e_stored);
                TotalStake::<T>::mutate(|t| *t = t.saturating_add(pos.e_stored));
            }

            // K_D(Q) = max(K_spot,last, K_EMA), both slippage-aware (spec §11.4, §13.6).
            //
            // K_spot uses live reserves; K_EMA prices the buyback against the
            // EMA-implied reserve `T_EMA = pEMA·A_live`. Two reasons the EMA leg is
            // a CPMM buyback (not the scalar `Q·pEMA`):
            //   1. A scalar price understates the true cost of acquiring a large Q
            //      (spec §13.6) — slippage must be charged so terminal extraction is
            //      bounded by what closing actually costs.
            //   2. An attacker who shorts a subnet to force its deregistration
            //      suppresses the *live* price, which would cheapen K_spot. Pricing
            //      the EMA leg off the slow `pEMA` keeps K_D high, so the carry paid
            //      while waiting for dereg is not refunded at settlement. Provided
            //      `pEMA` is slow enough (governance: SubnetMovingPrice half-life)
            //      and the max price lift is capped (κ), the attacker's carry +
            //      bounded equity recovery exceeds any forced-slot-acquisition gain.
            let c_rao = u128::from(pos.p_floor.to_u64())
                .saturating_add(u128::from(pos.r_stored.to_u64()));
            let q_rao = u128::from(pos.q_liability.to_u64());
            let a_rao = u128::from(SubnetAlphaIn::<T>::get(netuid).to_u64());
            let t_rao = u128::from(SubnetTAO::<T>::get(netuid).to_u64());
            // EMA-implied TAO reserve at the slow price: `T_EMA = pEMA · A_live`.
            let t_ema_rao = pema
                .saturating_mul(Self::alpha_f(SubnetAlphaIn::<T>::get(netuid)))
                .max(I64F64::from_num(0))
                .saturating_to_num::<u128>();
            let k_spot = u128::from(Self::buyback_cost_rao(t_rao, a_rao, q_rao));
            let k_ema = u128::from(Self::buyback_cost_rao(t_ema_rao, a_rao, q_rao));
            let mut k_d = k_spot.max(k_ema);

            // Cold-EMA guard. When `pEMA == 0` (fresh subnet, no trustworthy slow
            // price), the EMA leg is 0 and only the suppressible live leg governs —
            // which would let a trader who forced the dereg recover the pool-origin
            // retained buffer `R` as equity. Floor `K_D` at `R` so equity can never
            // exceed the trader's own floor `P` (`equity = C − K_D ≤ P`); the buffer
            // is recycled rather than refunded. A warm EMA prices a genuine in-the-
            // money close correctly, so legitimate profit is unaffected.
            if pema <= I64F64::from_num(0) {
                k_d = k_d.max(u128::from(pos.r_stored.to_u64()));
            }

            let equity = TaoBalance::from(c_rao.saturating_sub(k_d).min(u128::from(u64::MAX)) as u64);
            let cover = TaoBalance::from(c_rao.min(k_d).min(u128::from(u64::MAX)) as u64);
            // Pay equity; if the transfer fails the amount stays in custody and is
            // recycled by the terminal sweep below, so the emitted `equity` reflects
            // what was actually paid (never claims an unpaid amount).
            let paid = if !equity.is_zero()
                && Self::transfer_tao(&custody, &coldkey, equity.into()).is_ok()
            {
                equity
            } else {
                TaoBalance::from(0)
            };
            Self::recycle_custody_tao(&custody, cover);

            ShortPositions::<T>::remove(netuid, &coldkey);
            Self::deposit_event(Event::ShortTerminalSettled {
                coldkey,
                netuid,
                equity: paid,
                liability_cover: cover,
            });
        }
        // Sweep any residual custody dust (rounding drift) so no TAO is orphaned
        // in the per-subnet custody account after the subnet is gone.
        Self::recycle_custody_tao(&custody, TaoBalance::MAX);
        ShortAggregate::<T>::remove(netuid);
        ShortActiveSubnets::<T>::remove(netuid);
        ShortPositionCount::<T>::remove(netuid);
    }

    /// Slippage-aware CPMM cost — in the **pay** asset, rao — to acquire
    /// `recv_amount` of the **recv** asset from a pool with reserves
    /// `(pay_reserve, recv_reserve)`: the exact constant-product amount
    /// `⌈pay_reserve · recv_amount / (recv_reserve − recv_amount)⌉`.
    ///
    /// The CPMM is symmetric in its two assets, so the **caller selects the
    /// denomination by operand order** (the params are intentionally asset-neutral):
    ///   - a short buying `Q` alpha with TAO  → `(T_reserve, A_reserve, Q)` → TAO cost;
    ///   - a long  repaying `D` TAO with alpha → `(A_reserve, T_reserve, D)` → alpha cost.
    ///
    /// Computed in u128 so the product (each operand up to ~2e16 rao) cannot
    /// overflow, and **ceiling-rounded** so the terminal cover is never
    /// under-charged (bounding the equity an attacker can recover at a forced
    /// deregistration). Saturates to `u64::MAX` when un-buyable
    /// (`recv_amount ≥ recv_reserve`), giving `cover = C, equity = 0`.
    fn buyback_cost_rao(pay_reserve: u128, recv_reserve: u128, recv_amount: u128) -> u64 {
        if recv_reserve <= recv_amount {
            return u64::MAX;
        }
        let num = pay_reserve.saturating_mul(recv_amount);
        let den = recv_reserve.saturating_sub(recv_amount);
        num.div_ceil(den).min(u64::MAX as u128) as u64
    }

    /// Slippage-aware TAO cost (as I64F64) to buy `q` alpha on the live pool.
    fn short_spot_close_cost(netuid: NetUid, q: AlphaBalance) -> I64F64 {
        let cost = Self::buyback_cost_rao(
            u128::from(SubnetTAO::<T>::get(netuid).to_u64()),
            u128::from(SubnetAlphaIn::<T>::get(netuid).to_u64()),
            u128::from(q.to_u64()),
        );
        I64F64::from_num(cost)
    }

    // ---- governance setters (spec §14.6) -------------------------------

    pub fn set_shorts_enabled(enabled: bool) {
        ShortsEnabled::<T>::put(enabled);
    }
    pub fn set_longs_enabled(enabled: bool) {
        LongsEnabled::<T>::put(enabled);
    }
    /// `κ_S`, supplied scaled by 1e9. Clamped to `(0, 2.0]` so governance can't
    /// freeze the market (`κ=0`) or remove the capacity guard entirely.
    pub fn set_short_kappa_ppb(kappa_ppb: u64) {
        let k = kappa_ppb.clamp(1, 2_000_000_000);
        ShortKappa::<T>::put(I64F64::from_num(k).safe_div(I64F64::from_num(1_000_000_000u64)));
    }
    /// `λ`, supplied scaled by 1e9. Clamped to `(0, 1)` so the open quadratic
    /// stays well-formed.
    pub fn set_short_base_ltv_ppb(ltv_ppb: u64) {
        let ltv = ltv_ppb.clamp(1, 999_999_999);
        ShortBaseLtv::<T>::put(I64F64::from_num(ltv).safe_div(I64F64::from_num(1_000_000_000u64)));
    }
    /// `d_min`, `d_max`, supplied scaled by 1e9. Each is clamped to `[0, 1.0]`
    /// per day (so the per-block factor `g = 1 − d/blocks_per_day` stays in
    /// `(0, 1]`) and `d_min ≤ d_max` is enforced.
    pub fn set_decay_bounds_ppb(min_ppb: u64, max_ppb: u64) {
        let scale = I64F64::from_num(1_000_000_000u64);
        let lo = min_ppb.min(1_000_000_000);
        let hi = max_ppb.clamp(lo, 1_000_000_000);
        DecayMin::<T>::put(I64F64::from_num(lo).safe_div(scale));
        DecayMax::<T>::put(I64F64::from_num(hi).safe_div(scale));
    }
    pub fn set_short_dust(dust: TaoBalance) {
        ShortDust::<T>::put(dust);
    }
    pub fn set_short_default_grace(blocks: u64) {
        ShortDefaultGrace::<T>::put(blocks);
    }
    pub fn set_long_default_grace(blocks: u64) {
        LongDefaultGrace::<T>::put(blocks);
    }
    pub fn set_short_min_input(min_input: TaoBalance) {
        ShortMinInput::<T>::put(min_input);
    }
    pub fn set_short_max_positions(max: u32) {
        ShortMaxPositions::<T>::put(max);
    }

    // ---- read-only quote (spec §1.2) -----------------------------------

    /// Pure pre-open quote for a given input `P`. Returns `None` when shorts are
    /// disabled or the subnet is not a dynamic market.
    pub fn quote_open_short(netuid: NetUid, position_input: TaoBalance) -> Option<ShortOpenQuote> {
        if !ShortsEnabled::<T>::get() || SubnetMechanism::<T>::get(netuid) != 1 {
            return None;
        }
        let agg = ShortAggregate::<T>::get(netuid);
        let t_ref = Self::short_t_ref(netuid);
        let p = Self::tao_f(position_input);
        let (c, n) = Self::solve_collateral(p, t_ref, Self::tao_f(agg.b_sigma), ShortBaseLtv::<T>::get())?;
        let t_live = Self::tao_f(SubnetTAO::<T>::get(netuid));
        let a_live = Self::alpha_f(SubnetAlphaIn::<T>::get(netuid));
        let phi = Self::solve_phi(n, t_live)?;

        let q_alpha = Self::to_alpha(phi.saturating_mul(a_live));
        let scale = I64F64::from_num(1_000_000_000u64);
        let lambda_eff = n.safe_div(c).saturating_mul(scale).saturating_to_num::<u64>();
        let daily_decay = Self::short_daily_decay(netuid, agg.b_sigma)
            .saturating_mul(scale)
            .saturating_to_num::<u64>();
        Some(ShortOpenQuote {
            gross_collateral: Self::to_tao(c),
            retained_proceeds: Self::to_tao(n),
            alpha_liability: q_alpha,
            escrow: Self::to_tao(phi.saturating_mul(t_live)),
            effective_ltv: lambda_eff,
            daily_decay,
            est_close_cost: Self::to_tao(Self::short_spot_close_cost(netuid, q_alpha)),
        })
    }

    /// Estimated blocks until `r_current` decays to dust at the current rate.
    /// `u64::MAX` when decay is effectively zero.
    fn short_blocks_to_dust(netuid: NetUid, r_current: TaoBalance, b_sigma: TaoBalance) -> u64 {
        let dust = ShortDust::<T>::get();
        if r_current <= dust || dust.is_zero() {
            return if r_current <= dust { 0 } else { u64::MAX };
        }
        let delta = Self::short_daily_decay(netuid, b_sigma)
            .safe_div(I64F64::from_num(BLOCKS_PER_DAY));
        if delta <= I64F64::from_num(0) {
            return u64::MAX;
        }
        let neg_ln_g = Self::neg_ln_one_minus(delta);
        if neg_ln_g <= I64F64::from_num(0) {
            return u64::MAX;
        }
        let ratio = Self::tao_f(r_current).safe_div(Self::tao_f(dust));
        match ratio.checked_ln() {
            Some(ln_ratio) if ln_ratio > I64F64::from_num(0) => ln_ratio
                .safe_div(neg_ln_g)
                .saturating_to_num::<u64>(),
            _ => 0,
        }
    }

    /// Materialized, health-rich view of one position (decayed to the current block).
    pub fn get_short_position(
        coldkey: &T::AccountId,
        netuid: NetUid,
    ) -> Option<ShortPositionInfo<T::AccountId>> {
        let mut pos = ShortPositions::<T>::get(netuid, coldkey)?;
        let agg = ShortAggregate::<T>::get(netuid);
        Self::materialize_short(&mut pos, agg.omega);

        let scale = I64F64::from_num(1_000_000_000u64);
        let daily_decay = Self::short_daily_decay(netuid, agg.b_sigma)
            .saturating_mul(scale)
            .saturating_to_num::<u64>();
        let now = Self::get_current_block_as_u64();
        let defaultable_at_block = pos.last_active.saturating_add(ShortDefaultGrace::<T>::get());
        let default_eligible = pos.r_stored <= ShortDust::<T>::get() && now >= defaultable_at_block;
        let alpha_held =
            Self::get_stake_for_hotkey_and_coldkey_on_subnet(&pos.hotkey, coldkey, netuid);

        Some(ShortPositionInfo {
            netuid,
            hotkey: pos.hotkey.clone(),
            floor: pos.p_floor,
            alpha_liability: pos.q_liability,
            buffer: pos.r_stored,
            escrow: pos.e_stored,
            collateral_claim: pos.p_floor.saturating_add(pos.r_stored),
            daily_decay,
            blocks_to_dust: Self::short_blocks_to_dust(netuid, pos.r_stored, agg.b_sigma),
            default_eligible,
            defaultable_at_block,
            est_close_cost: Self::to_tao(Self::short_spot_close_cost(netuid, pos.q_liability)),
            alpha_held,
            alpha_needed: AlphaBalance::from(
                pos.q_liability.to_u64().saturating_sub(alpha_held.to_u64()),
            ),
        })
    }

    /// All of a coldkey's short positions across subnets.
    pub fn get_short_positions(coldkey: &T::AccountId) -> Vec<ShortPositionInfo<T::AccountId>> {
        Self::get_all_subnet_netuids()
            .into_iter()
            .filter_map(|netuid| Self::get_short_position(coldkey, netuid))
            .collect()
    }

    /// Per-subnet short market state for sizing and capacity decisions.
    pub fn get_subnet_short_state(netuid: NetUid) -> Option<ShortMarketInfo> {
        if !Self::if_subnet_exist(netuid) {
            return None;
        }
        let agg = ShortAggregate::<T>::get(netuid);
        let t_ref = Self::short_t_ref(netuid);
        let cap = ShortKappa::<T>::get().saturating_mul(t_ref);
        let used = Self::tao_f(agg.b_sigma);
        let scale = I64F64::from_num(1_000_000_000u64);
        let ppb = |x: I64F64| x.saturating_mul(scale).saturating_to_num::<u64>();

        Some(ShortMarketInfo {
            shorts_enabled: ShortsEnabled::<T>::get(),
            base_ltv: ppb(ShortBaseLtv::<T>::get()),
            kappa: ppb(ShortKappa::<T>::get()),
            decay_min: ppb(DecayMin::<T>::get()),
            decay_max: ppb(DecayMax::<T>::get()),
            current_daily_decay: ppb(Self::short_daily_decay(netuid, agg.b_sigma)),
            t_ref: Self::to_tao(t_ref),
            footprint_used: agg.b_sigma,
            footprint_cap: Self::to_tao(cap),
            footprint_remaining: Self::to_tao(cap.saturating_sub(used)),
            open_interest_alpha: agg.q_sigma,
            buffer_total: agg.r_sigma,
            escrow_total: agg.e_sigma,
            dust_threshold: ShortDust::<T>::get(),
            min_input: ShortMinInput::<T>::get(),
            default_grace: ShortDefaultGrace::<T>::get(),
        })
    }

    /// Pre-close quote for `fraction_ppb / 1e9` of a position.
    pub fn quote_close_short(
        coldkey: &T::AccountId,
        netuid: NetUid,
        fraction_ppb: u64,
    ) -> Option<CloseShortQuote> {
        if fraction_ppb == 0 || fraction_ppb > 1_000_000_000 {
            return None;
        }
        let mut pos = ShortPositions::<T>::get(netuid, coldkey)?;
        let agg = ShortAggregate::<T>::get(netuid);
        Self::materialize_short(&mut pos, agg.omega);
        let rho = I64F64::from_num(fraction_ppb).safe_div(I64F64::from_num(1_000_000_000u64));

        let repay_alpha = Self::mul_alpha(pos.q_liability, rho);
        let returned_tao =
            Self::mul_tao(pos.p_floor, rho).saturating_add(Self::mul_tao(pos.r_stored, rho));
        let escrow_settled = Self::mul_tao(pos.e_stored, rho);
        let alpha_held =
            Self::get_stake_for_hotkey_and_coldkey_on_subnet(&pos.hotkey, coldkey, netuid);

        Some(CloseShortQuote {
            repay_alpha,
            returned_tao,
            escrow_settled,
            est_buyback_cost: Self::to_tao(Self::short_spot_close_cost(netuid, repay_alpha)),
            alpha_held,
            alpha_needed: AlphaBalance::from(
                repay_alpha.to_u64().saturating_sub(alpha_held.to_u64()),
            ),
        })
    }
}
