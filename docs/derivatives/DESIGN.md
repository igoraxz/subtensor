# Covered continuous-unwind derivatives — subtensor design

Implementation design for the **Fixed-Liability Covered Continuous-Unwind Model v3.6.1**
(`shorting.pdf`) inside `pallet-subtensor`. This document maps the spec onto the existing
runtime, fixes the reserve-accounting model against the real AMM, and locks the storage,
extrinsic, hook, and runtime-API surface. The companion `IMPLEMENTATION_PLAN.md` has the
phased file-by-file plan and diff estimate.

Launch scope is **shorts-first**. The long side is now **fully implemented and wired**
(`open_long`/`close_long`/`default_long`, decay, dereg settlement, read/RPC layer) but stays
**flag-gated off** (`LongsEnabled=false`) until the long-side trading games pass; shorts enable
first (`ShortsEnabled`). Everything below reuses primitives that already exist.

**Price-reference caveat (load-bearing).** All risk references and the terminal `K_EMA` leg are
built on `SubnetMovingPrice` (`pEMA`), which upstream updates as `EMA(min(spot, 1.0))` with a
~30-day half-life and is `0` at cold start. Two consequences the safety arguments depend on:
(1) `pEMA` is **capped at ~1.0 TAO/alpha** — for any subnet whose true price exceeds 1.0 the EMA
leg saturates, so the conservative-reference and anti-suppression guarantees hold **only while
price ≤ ~1.0** (true for every mainnet subnet today; max observed ≈0.018). (2) The slow half-life
is what makes the terminal anti-attack margin work and is therefore a **governance-tuned
invariant** — see §3.4. If upstream ever redefines the moving-price clamp or half-life, the
derivative risk math must be re-validated.

**Warm-EMA open guard.** `do_open_short`/`do_open_long` reject (`ColdEmaNotAllowed`) when
`pEMA == 0` (freshly registered subnet, no price history), since there the EMA risk reference
falls back to the live reserve and the terminal `K_EMA` anti-suppression leg is unavailable.
Positions can only be opened once the EMA warms; a position opened warm that later goes cold is
still bounded at settlement by the cold-EMA `K_D ≥ R` floor (short) / `u64::MAX` cover sentinel
(long).

**Spec upgrades required (intended deviations from v3.6.1 text).** The implementation deliberately
diverges from three literal spec formulas. In every case the divergence is toward a *more
conservative* realization against the live AMM that never under-charges an attacker. These are
**intended** — the code is the source of truth and the spec text should be **upgraded** to match;
they are not bugs:

1. **Terminal `K_EMA` is a slippage-aware CPMM buyback, not the scalar `Q·pEMA`** (spec §11.4,
   §15.5, Appendix A.6). A scalar understates the TAO cost of repurchasing a large `Q` from a
   finite pool. The implementation prices `K_EMA` as the CPMM buyback `⌈t·q/(a−q)⌉` (u128,
   ceiling-rounded) against the EMA-implied reserve `T_EMA = pEMA·A_live`, with a cold-EMA floor
   `K_D ≥ R`. Consequence: the §15.5 worked example (`K_EMA = 66` for `Q = 3900`) **no longer
   reproduces** for large `Q` — the realized CPMM cost is strictly higher. This strengthens the
   anti-extraction margin (§3.4) and must be folded into the spec's settlement formula and example.
2. **Restoration zap is a one-sided reserve credit, not the min-swap-plus-balanced-add** (spec
   §6.5/§6.6). Net CPMM effect is equivalent for a single full-range position; on a fee/weighted
   pool the two forms differ and the reconciliation is gated on the trading-games suite (§14.5).
3. **Close/terminal settlement zap is a one-sided pair of increments, not the balanced settlement
   zap** (spec §8.5). Same rationale and gate as (2).

Action: upgrade the v3.6.1 spec text (settlement formula §11.4/A.6, the §15.5 example, and the zap
definitions §6.6/§8.5) so the authoritative document matches the conservative implementation.

**Fee-pool divergence (consequence of the one-sided reserve ops).** Because open/close/restore/
terminal zaps are one-sided reserve mutations rather than fee-charging swap-engine calls, on a
**fee-charging** pool the derivative's realized close-cost, break-even, and terminal economics do
**not** include the pool swap fee. This is acceptable for the launch design (the math is priced and
realized one-sidedly), but it means quoted break-even ≠ the cost of an equivalent fee-paying spot
swap. If a future variant routes derivative legs through the fee-adjusted swap engine, break-even /
terminal quotes must be re-derived. Tracked as a pre-mainnet decision alongside the κ ramp.

**Close is in-kind only (UX note).** `close_short` requires the trader to already hold/stake the
Alpha liability `Q` on the position hotkey (`SubnetAlphaOut ≥ ρQ`), and `close_long` requires the
TAO liability `D`. There is **no auto-buy close path** in the launch design: a trader without the
liability asset must acquire/stake it first (the protocol liability `Q`/`D` is the same regardless,
but the incremental market close cost can be lower if the trader already holds the asset — spec §1.6).
This is intentional and safe; clients must surface "you need `Q` Alpha (`D` TAO) to close".

---

## 1. Reality check: what the spec assumes vs. what subtensor has

The spec is written against a **pure no-fee CPMM** (`x·y=k`). Subtensor's pool is different,
and that single fact drives most of the design decisions.

| Spec assumption | Subtensor reality | Consequence |
|---|---|---|
| Pure `x·y=k` | **Balancer-weighted** pool (`pallet_subtensor_swap`), weights in `SwapBalancer`, default 0.5/0.5 (CPMM-like only at init) | Use spec closed-forms **only for quoting/sizing**; realize every pool-touching leg through the live fee+weight-aware engine (`SwapHandler::sim_swap` / `swap`). The spec explicitly allows this (§4.4, §14.6). |
| User can remove/add liquidity | User LP is **deprecated** (`add_liquidity`/`remove_liquidity` → `Error::Deprecated`) | The "remove-and-sell-back" open and the restoration/settlement zaps are realized as **protocol reserve mutations**, not user LP ops. |
| Reserves `T`, `A` | `SubnetTAO` (TAO, the quote reserve), `SubnetAlphaIn` (alpha pool reserve), `SubnetAlphaOut` (staked alpha outside the pool) | Short open/restore are mostly `SubnetTAO` mutations; close settlement touches `SubnetAlphaIn`. |
| `pEMA` price reference | **Already exists**: `SubnetMovingPrice` (per-block halving EMA, TAO/alpha) | Reuse directly as the spec's `pEMA`. No new TWAP, no new price EMA. |
| `T_EMA`, `A_EMA` reserve EMAs | **Do not exist** | Derive `T_EMA` from `SubnetMovingPrice × SubnetAlphaIn` instead of storing a new per-block reserve EMA (see §4). |
| Recycle floor `P`, extinguish liability | **Already exists**: `recycle_tao(coldkey, amount)`, `recycle_subnet_alpha`/`burn_subnet_alpha` | Reuse for default and terminal settlement. |
| Per-block decay/unwind step | **Net-new** | One O(1)-per-subnet call added to `block_step()`. |
| Subnet deregistration hook | **Already exists**: `do_dissolve_network` (`coinbase/root.rs`) | Insert terminal derivative settlement before `destroy_alpha_in_out_stakes`. |
| Derivative flow-neutral for emissions | **Free by construction** | We mutate reserves directly and never call `record_tao_inflow/outflow`, so TaoFlow is untouched (spec §4.5). |

**Key takeaway:** the spec's `pEMA`, recycle, and dereg primitives already exist. The genuinely
new state is (a) the position store, (b) per-side aggregate + decay accumulator, (c) a per-block
decay step, (d) ~4 extrinsics, (e) one runtime-API quote. Risk reserve EMAs are *derived*, not stored.

---

## 2. Notation map (spec symbol → subtensor identifier)

| Spec | Meaning | Subtensor binding |
|---|---|---|
| `T` | live TAO reserve | `SubnetTAO::<T>::get(netuid)` |
| `A` | live alpha reserve | `SubnetAlphaIn::<T>::get(netuid)` |
| `T_ref` | conservative TAO ref `min(T_live, T_EMA)` | `min(SubnetTAO, pEMA·A_live)` — derived (§4) |
| `pEMA` | EMA price (TAO/alpha) | `Pallet::get_moving_alpha_price(netuid)` (`SubnetMovingPrice`) |
| `P` | user position input / floor | `ShortPosition.p_floor: TaoBalance` |
| `C` | gross collateral (open-time only) | computed, **not stored** |
| `N` | retained proceeds = `R0` | computed at open → `r_stored` |
| `R(t)` | retained buffer (decays) | `ShortPosition.r_stored` × decay factor |
| `Q` | fixed alpha liability | `ShortPosition.q_liability: AlphaBalance` |
| `E(t)` | linked TAO escrow (decays) | `ShortPosition.e_stored: TaoBalance` |
| `B` | utilization footprint `λC` (TAO) | `ShortPosition.b_stored: TaoBalance` |
| `S` | aggregate active footprint | `ShortAgg.b_sigma` |
| `Ω_S` | short decay accumulator | `ShortAgg.omega: U64F64` |
| `Ω_entry` | per-position accumulator snapshot | `ShortPosition.omega_entry: U64F64` |
| `λ`, `λ_eff` | base / effective LTV | governance param `ShortBaseLtv`; `λ_eff` computed |
| `κ_S` | short footprint cap factor | governance param `ShortKappa` |
| `d_min`,`d_max` | decay bounds | `DecayMin`, `DecayMax` |
| `R_dust` | dust threshold | `ShortDust` |
| `K_D(Q)` | terminal liability value | computed at dereg: `max(K_spot,last, K_EMA)`, both slippage-aware CPMM buybacks (`K_spot` on live reserves, `K_EMA` on `T_EMA=pEMA·A_live`); floored at retained buffer `R` when `pEMA==0` (cold-EMA guard) |

---

## 3. Reserve-accounting model (the load-bearing part)

All pool impact is expressed as mutations to `SubnetTAO` / `SubnetAlphaIn`, executed through the
existing helpers so weights and fees stay consistent:

- `increase_provided_tao_reserve` / `decrease_provided_tao_reserve`
- `increase_provided_alpha_reserve` / `decrease_provided_alpha_reserve`
- `T::SwapInterface::sim_swap` / `swap` with `GetAlphaForTao<T>` / `GetTaoForAlpha<T>` for any
  internal swap leg (fee + weight aware).

### 3.1 Open short — net pool effect

The spec's remove-and-sell-back (§4.3) on a pure CPMM nets to: **alpha reserve unchanged, TAO
reserve drops by `N + E`**, leaving the trader owing `Q = ϕA` alpha. We realize that directly:

```
TAO removed from pool = N + E = ϕ(2-ϕ)·T      // = T - (1-ϕ)²T on pure CPMM
SubnetTAO            -= (N + E)                 // the downward price impact
held by protocol      = E (escrow) + N (becomes buffer R0)
position liability     = Q = ϕ·A (alpha debt, virtual; alpha reserve untouched at open)
```

`ϕ`, `N`, `Q`, `E` are first quoted from the spec closed-forms (Appendix A.1), then the realized
TAO leg is taken from a fee-adjusted engine quote so the booked `N`/`E` match what the pool
actually moved. The trader supplies `P = C − N` TAO, held against the floor and recycle-on-default.

### 3.2 Continuous restoration (per block) — net pool effect

For a short the decayed amount `dU = dR + dE` is TAO-side. The spec zap (swap min portion to
alpha, re-add balanced) nets, on a CPMM, to **alpha unchanged, TAO `+= dU`, price drifts up** —
exactly reversing the open impact over time:

```
restoration_zap(netuid, dU)  ≡  increase_provided_tao_reserve(netuid, dU)
```

No weight change is needed (we *want* the upward drift), so this is a single reserve increment.
This conserves TAO: the `N + E` removed at open is returned over the position's life. (If
simulation later shows the weighted pool needs the explicit min-swap, swap `z = √(T(T+U)) − T`
via the engine then add the remainder — spec §6.6 — behind the same `restoration_zap` fn.)

### 3.3 Close (partial fraction ρ, full = ρ=1) — net pool effect

Trader repays `ρQ` alpha; protocol pairs it with the escrow slice `ρE` via the settlement zap
(§8.5). Net pool effect: `SubnetAlphaIn += ρQ`, `SubnetTAO += ρ·E_remaining_share`, balanced
through an engine min-swap. Trader receives `ρ(P + R)` back. Position `P, Q, R, E, B` reduced
pro-rata; aggregates updated.

### 3.4 Default (R ≤ R_dust) and terminal dereg

- **Default:** restore residual `R + E` (restoration zap), `recycle_tao(coldkey, P)` for the floor,
  extinguish `Q` (no alpha moves — it was virtual), drop position from aggregates.
- **Dereg terminal:** value liability at `K_D(Q) = max(K_spot,last(Q), K_EMA(Q))`, where both legs
  are slippage-aware CPMM buybacks (`⌈t·q/(a−q)⌉` in u128 with ceiling rounding) — `K_spot` on live
  reserves, `K_EMA` on the EMA-implied reserve `T_EMA = pEMA·A_live`. A scalar `Q·pEMA` is **not**
  used (it understates the cost of a large `Q`). When `pEMA==0` (cold subnet, no slow reference) the
  short floors `K_D ≥ R` so equity ≤ floor `P` (no pool-origin buffer is refunded); the long is
  naturally safe (the cold leg hits the un-buyable sentinel → cover = collateral). equity =
  `max(0, (P+R) − K_D)` paid to trader; `min(P+R, K_D)` recycled outside terminal distribution; `Q`
  extinguished. Hooked into `do_dissolve_network` before `destroy_alpha_in_out_stakes`.
  **Governance invariant — the price EMA must be SLOW (short-dereg / whale-extortion defense).**
  The terminal `K_EMA` leg is the *only* thing that keeps a short's dereg payout bounded when an
  attacker — or a whale extorting a subnet — deliberately drives the subnet toward deregistration.
  Because `K_D = max(K_spot, K_EMA)`, a *fast* EMA would track the attacker's crashed spot downward,
  collapse `K_D`, and hand the attacker a cheap terminal buyback (a free short-to-dereg extraction).
  The `SubnetMovingPrice` half-life must therefore be **slow relative to the realistic time to force
  a dereg**, so that over the whole suppression window:

      Σ carry paid (decay on R+E, every block)   ≥   bounded terminal equity max(0, (P+R) − K_D)

  holds with margin. Mechanism: while spot is suppressed, a slow `K_EMA` stays near the *pre-attack*
  price, so `K_D` stays high and terminal equity stays ≈ 0 (verified on-chain — a pre-dereg spot
  crash paid equity = 0), while the attacker keeps paying utilization carry on `R + E` every block
  for the entire window. Tune the EMA half-life together with `κ` (which bounds how far one short can
  move price — short impact *saturates* near `1 − √(1 − δ)`, so a single position cannot crash spot
  to zero; see §A.3) so that a short-driven deregistration is **never net-profitable**. A short /
  fast half-life **breaks this guarantee and must not be set**; if upstream shortens the moving-price
  half-life, the short-dereg margin must be re-validated before `κ` is ramped.

### 3.5 Conservation invariant (must be a test)

Over any position lifecycle, total TAO returned to `SubnetTAO` via restoration + close-settlement +
default-restore, plus recycled floor/liability-cover, **equals** the `N + E` removed at open plus the
`P` the trader posted, minus equity paid out. This invariant is the acceptance gate for the
reserve math and is the first item in the spec's trading-games suite (§14.5).

> **Primary implementation risk:** reconciling the spec's CPMM closed-forms with the Balancer
> weights. Mitigation: quote/size from closed-forms, realize from the engine, gate launch on the
> conservation + capacity simulations the spec already mandates (§14.5). `κ_S` starts tiny.

---

## 4. Risk reference reserves without new EMA storage

The spec wants `T_ref = min(T_live, T_EMA)` to stop a same-block reserve pump from improving open
terms (§3.1–3.2). Subtensor has no reserve EMA, but it has an EMA *price*. Since
`price = (w_base/w_quote)·(T/A)`, we reconstruct:

```
T_EMA  ≈  pEMA · A_live          (pEMA already folds the weight ratio at EMA time)
T_ref  =  min(SubnetTAO, T_EMA)
```

This reuses `SubnetMovingPrice` and adds **zero** per-block EMA maintenance. `A_live` is still
manipulable, but with `κ_S` starting conservative and the footprint cap `S + B ≤ κ_S·T_ref`, the
launch exposure is bounded; a dedicated stored reserve-EMA can be added later if the trading games
show it is needed. Decay utilization uses the same `T_ref` (spec §3.3), so flash trades cannot grief
carry either.

---

## 5. Storage layout

New module `pallets/subtensor/src/derivatives/`. Storage declared inline in `lib.rs` (the repo's
convention — there is no storage macro file). `#[pallet::without_storage_info]` is already set, so
`MaxEncodedLen` is not required.

### 5.1 Position struct

One **merged** short position per `(coldkey, netuid)` — additional same-side opens merge after
materialization (spec §8.6), which keeps the store sparse and avoids a position-id index.

```rust
#[freeze_struct("<hash>")]
#[derive(Encode, Decode, DecodeWithMemTracking, TypeInfo, Clone, PartialEq, Eq, Debug)]
pub struct ShortPosition {
    pub p_floor: TaoBalance,     // non-decaying floor (spec P)
    pub q_liability: AlphaBalance,// fixed alpha debt (spec Q)
    pub r_stored: TaoBalance,    // buffer at last materialization (spec R)
    pub e_stored: TaoBalance,    // escrow at last materialization (spec E)
    pub b_stored: TaoBalance,    // footprint at last materialization (spec B)
    pub omega_entry: U64F64,     // Ω_S snapshot at last materialization
    pub opened_at: u64,          // block, for UX/telemetry only
}
```

```rust
// --- DMAP (netuid, coldkey) -> ShortPosition
#[pallet::storage]
pub type ShortPositions<T: Config> = StorageDoubleMap<
    _, Identity, NetUid, Blake2_128Concat, T::AccountId, ShortPosition, OptionQuery>;
```

### 5.2 Per-subnet aggregate + decay accumulator

```rust
#[freeze_struct("<hash>")]
#[derive(Encode, Decode, DecodeWithMemTracking, TypeInfo, Clone, PartialEq, Eq, Debug, Default)]
pub struct ShortAgg {
    pub r_sigma: TaoBalance,   // Σ current R
    pub e_sigma: TaoBalance,   // Σ current E
    pub b_sigma: TaoBalance,   // Σ current B  == active footprint S
    pub q_sigma: AlphaBalance, // Σ fixed liability (open interest)
    pub omega: U64F64,         // Ω_S cumulative decay accumulator
}

#[pallet::storage]
pub type ShortAggregate<T: Config> =
    StorageMap<_, Identity, NetUid, ShortAgg, ValueQuery, DefaultShortAgg<T>>;
```

Materialization (spec §6.3): `f = exp(-(Ω - Ω_entry))`, multiply `r,e,b` by `f`, snapshot `Ω_entry = Ω`.
Aggregate tick is O(1) per subnet: `R,E,B *= g`, `Ω += -ln g` (spec §6.4).

### 5.3 Governance parameters (global defaults; per-subnet override optional later)

Stored as `StorageValue` with `#[pallet::type_value]` defaults; setters in `utils/misc.rs`; exposed
via `pallet-admin-utils` sudo/owner extrinsics (existing pattern).

| Storage | Type | Default | Spec |
|---|---|---|---|
| `ShortsEnabled` | `bool` | `false` (flip on after games) | §14.1 |
| `LongsEnabled` | `bool` | `false` | §9.3 |
| `ShortBaseLtv` | `U64F64` | `0.50` | §14.1 |
| `ShortKappa` | `U64F64` | small, conservative | §5.1 |
| `DecayMin` | `U64F64` | `0.001`/day | §6.2 |
| `DecayMax` | `U64F64` | `0.015`/day | §6.2 |
| `ShortDust` | `TaoBalance` | `1 TAO` | §7.2 |

No migration is required: new maps default cleanly; only `ShortsEnabled` flips via governance.

---

## 6. Per-block decay step

Add `Self::run_derivatives_decay()` to `block_step()` **after** `run_coinbase(...)` and **before**
`update_moving_prices()` (so decay sees post-emission reserves but feeds the same block's price EMA).
For each subnet with `ShortAggregate.b_sigma > 0`:

```
u      = min(1, b_sigma / (ShortKappa · T_ref))           // EMA-smoothed via T_ref
d_day  = DecayMin + (DecayMax - DecayMin)·u²
g      = (1 - d_day)^(1 block / blocks_per_day)           // const per-block factor
dR = r_sigma·(1-g);  dE = e_sigma·(1-g);  dB = b_sigma·(1-g)
r_sigma,e_sigma,b_sigma *= g;  omega += -ln g
restoration_zap(netuid, dR + dE)                          // SubnetTAO += dR+dE
```

O(1) per active subnet, no per-position iteration. `(1-d_day)^(1/blocks_per_day)` is computed with
the existing `substrate_fixed` helpers; `blocks_per_day ≈ 7200`.

**Defaults are lazy.** Because the tick never visits individual positions, a position that has decayed
below `R_dust` is settled (a) on its owner's next interaction (materialize → if dust, default), or
(b) by a permissionless `default_short(coldkey, netuid)` poke. This keeps the block hook O(1) and
matches the spec's MEV-insensitive, time-based default (§7.1, §7.4).

---

## 7. Extrinsics (shorts launch)

Thin dispatch wrappers in `macros/dispatches.rs` → `do_*` in `derivatives/`. Next free
`call_index` is **139**.

| call_index | Extrinsic | Delegates to | Notes |
|---|---|---|---|
| 139 | `open_short(netuid, hotkey, position_input: TaoBalance, max_alpha_liability: AlphaBalance)` | `do_open_short` | gated by `ShortsEnabled`; solves `C,N,ϕ,Q,E`; rejects `SlippageTooHigh` if live-derived `Q > max_alpha_liability` (caller-signed bound, `MAX` opts out); capacity + domain checks; merges into existing position |
| 140 | `top_up_short(netuid, amount: TaoBalance)` | `do_top_up_short` | adds to `R` only (spec §8.2); fresh decaying capital |
| 141 | `close_short(netuid, fraction_ppb: u64)` | `do_close_short` | `ρ = fraction_ppb/1e9`; partial (`ρ<1`) and full (`ρ=1`); repays `ρQ`, returns `ρ(P+R)` (close is deterministic given the materialized position — no execution bound needed) |
| 142 | `default_short(coldkey, netuid)` | `do_default_short` | permissionless; only valid when materialized `R ≤ R_dust` |

`hotkey` is carried so the position is associated with a `(hotkey, coldkey, netuid)` identity
consistent with the rest of staking, even though the merged position is keyed `(netuid, coldkey)`.
Long extrinsics are **not** added at launch (gated by spec §9; adding them later is symmetric).

Weights: start with inline `DbWeight::get().reads_writes(r, w)` placeholders (an accepted in-repo
pattern), benchmark before mainnet.

---

## 8. Events & errors

**Events** (`macros/events.rs`): `ShortOpened { netuid, coldkey, p, n, q, e, phi }`,
`ShortToppedUp`, `ShortClosed { netuid, coldkey, fraction, repaid_q, returned }`,
`ShortDefaulted`, `ShortTerminalSettled { netuid, coldkey, equity, liability_cover }`.

**Errors** (`macros/errors.rs`): `ShortsDisabled`, `ShortPositionNotFound`,
`EffectiveLtvNonPositive` (`λ_eff ≤ 0`), `RetainedProceedsNonPositive` (`N ≤ 0`),
`ShortCapacityExceeded` (`S + B > κ_S·T_ref`), `ReserveDomainExceeded` (`4N > T_live`),
`PositionNotDefaultEligible`, `SubnetNotDynamic` (mechanism ≠ 1 / root).

---

## 9. Runtime API (read-only quote)

Extend `runtime-api/src/lib.rs` + `rpc_info/` + `impl_runtime_apis!` (runtime/src/lib.rs).

```rust
fn quote_open_short(netuid: NetUid, position_input: TaoBalance) -> ShortOpenQuote;
fn get_short_position(coldkey: AccountId32, netuid: NetUid) -> Option<ShortPositionInfo>;
```

`ShortOpenQuote` carries the spec's pre-open trader view (§1.2): `c, n, q, e, phi, lambda_eff,
daily_decay, min/max_time_to_dust, est_close_cost (via sim_swap GetAlphaForTao for Q),
breakeven_close_price`. Pure reads + `sim_swap`; no state change. JSON-RPC wrapper is optional.

---

## 10. Invariants enforced (spec §17)

1. Shorts-first: `open_short` rejects unless `ShortsEnabled`; longs gated.
2. Covered: `P + N = C` at open.
3. No liquid proceeds: `N` is never paid out; it becomes `R0`.
4. Fixed liability: `Q` changes only on close / default / dereg.
5. Continuous unwind: `R,E,B` decay with one `g`; restored via `restoration_zap`.
6. No price-based liquidation: default iff `R ≤ R_dust`.
7. Limited recourse: residual `Q` extinguished at default/dereg.
8. Footprint cap: `S + B ≤ κ_S·T_ref` (also bounds same-block stacked opens via progressive `S`).
9. Flow neutrality: no `record_tao_*` calls on any derivative leg.
10. Dereg awareness: terminal alpha base read from subnet mode (legacy vs new, per `destroy_alpha_in_out_stakes` rules).
11. Terminal short settlement: `K_D(Q) = max(K_spot,last, K_EMA)` (both slippage-aware CPMM buybacks; cold-EMA floor `K_D ≥ R`).
12. Escrow bound: `E/R = 1/(1−ϕ)` stays bounded by `κ_S`-implied `ϕ_cap`, so dust default is MEV-trivial.

---

## 11. Explicit deferrals (faithful to spec)

- **Longs**: code-symmetric but flag-gated off (`LongsEnabled=false`). Long open mirrors with
  alpha/TAO swapped, `D=ϕT`, ADR-adjusted LTV (§9.2). Not in the launch diff.
- **Derivative TaoFlow** (`χ_S`): off; flow-neutral (§4.5). Not wired.
- **Stored reserve EMA / TWAP**: replaced by derived `T_ref` from `pEMA` (§4). TWAP is an optional
  later guard only (§3.4, §11.4).
- **Per-open `ϕ_max`**: not a control; only the `4N ≤ T_live` domain bound is enforced (§5.2).
- **Per-subnet param overrides**: launch uses globals; per-netuid maps can be added later without
  touching call sites.
