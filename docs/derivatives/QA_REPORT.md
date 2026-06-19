# Derivatives (covered shorts/longs) — QA & Test Report

Scope: the pool-borrowing covered shorts/longs feature on the Alpha/TAO CPMM
(continuation of #2764). Feature is governance-gated **off** by default
(`ShortsEnabled`/`LongsEnabled = false`). This report records the QA performed on
the branch and an overall score.

**Overall QA & test score: 9/10.** All CI-grade gates green, comprehensive unit
coverage on both sides, four independent adversarial review rounds passed, and a
full on-chain lifecycle exercised on a live local chain. Weight benchmarks are now
implemented and wired (extrinsics + the O(N) decay/dereg hooks); the remaining
pre-mainnet items are operational gates, not code-correctness gaps: regenerating
the weight constants on CI reference hardware, and the adversarial trading-games
matrix (incl. the EMA-slowness safety margin) before any `κ` ramp or
`ShortsEnabled` flip.

---

## 1. Build (local laptop, aarch64 macOS)

| Target | Result |
|---|---|
| `cargo check -p pallet-subtensor` | ✅ clean |
| `cargo check -p node-subtensor-runtime` (native, `SKIP_WASM_BUILD=1`) | ✅ clean |
| **wasm runtime** (`cargo build -p node-subtensor-runtime`) | ✅ built (1m04s) |
| **full node** (`cargo build -p node-subtensor`) | ✅ 325 MB binary |
| **localnet release** (`--workspace --profile=release --features fast-runtime`) | ✅ 9m28s |

macOS note: the wasm build needs a WebAssembly-capable LLVM (Apple clang lacks the
target). Fix: `brew install llvm` and build with
`CC_wasm32v1_none=/opt/homebrew/opt/llvm/bin/clang
AR_wasm32v1_none=…/llvm-ar CFLAGS_wasm32v1_none="-DZSTD_DISABLE_ASM=1"`.

## 2. Static analysis / style gates

| Gate | Result |
|---|---|
| `cargo fmt --check` (all feature files) | ✅ clean |
| `cargo clippy -p pallet-subtensor --lib` | ✅ no warnings in feature code (only an unrelated `trie-db` dependency future-incompat note) |

## 3. Tests

- **Derivatives suite: 74 tests, 0 failed.**
- **Full pallet suite: `cargo test -p pallet-subtensor --lib` → 1258 passed / 0 failed / 9 ignored** (no regressions in adjacent staking / networks / weights / swap-hotkey / registration suites).

Coverage by area (both short and long sides unless noted):

- **Open**: quote↔open match; reject paths — disabled, stable-subnet, zero/min-input,
  capacity (`κ·ref`), low-liquidity (`λ_eff ≤ 0`), cold-EMA fresh subnet; merge +
  hotkey-mismatch; **execution-bound rejection** (`max_alpha_liability` /
  `max_tao_liability`, both sides); validate-before-mutate (`assert_noop!`
  strands-no-funds).
- **Atomicity**: `open_short_failed_pool_transfer_rolls_back_atomically` — forces the
  pool→custody leg to fail after the floor moved and asserts full `#[transactional]`
  rollback (would fail without the attribute).
- **Top-up / close**: partial + full close, alpha-mint guards, invalid fraction,
  many-partial drain, close quote consistency.
- **Default**: dust + grace eligibility, permissionless-default anti-snipe,
  per-side grace independence, recycle-exactly-the-floor proof.
- **Decay / dereg**: rate-vs-closed-form, restore, block-step, materialize-never-
  inflates; full terminal matrix (in-the-money / underwater / cold-EMA) both sides;
  dereg **full-`do_dissolve_network`-path** long-equity survival through the stake-wipe.
- **Views/RPC, governance clamps, capacity/anti-split, active-set tracking.**
- **Invariant proofs**: TAO+alpha conservation across the full mixed lifecycle;
  **custody ≥ Σ materialized(P+R+E) under decay at the `DecayMax` clamp extreme,
  checked every tick**; `ShortPositionCount == |ShortPositions[netuid]|` and
  active-set ⟺ nonzero Σ through churn.

## 4. Adversarial review (three independent lenses)

| Lens | Verdict |
|---|---|
| Security skeptic | **CONVINCED** — atomicity (`#[transactional]`) + decay-restore ordering re-derived; no overflow / conservation / desync residue |
| Architect skeptic | **CONVINCED** — operand-order footgun resolved, dereg ordering contract guarded by an end-to-end test, `pEMA` dependency documented, decay-hook bound noted; M2/M3 now property-tested |
| Exploiter | **DEFEATED** — no profitable extraction across self-short-to-dereg, sandwich, capacity-split, decay-drift mint, EMA manipulation, cross-side, RPC abuse |

Key hardening landed from review: caller-signed execution bounds; validate-before-mutate;
`#[transactional]` on all 8 money functions; terminal `K_D = max(K_spot, K_EMA)` as a
u128 ceiling CPMM buyback (fixes an `I64F64` rao² overflow) + short cold-EMA floor;
decay restore-then-commit ordering; cancellation-stable `solve_collateral`.

## 5. Precision review & on-chain CPMM audit

- **Precision vs house style**: the only rao² product (terminal buyback) uses u128
  ceiling math; all other `I64F64` use is price×rao / ratio×rao (rao-scale, no
  overflow). `solve_collateral` uses the cancellation-stable root. Matches the
  codebase's "no `I64F64` for rao² products" convention.
- **Live CPMM audit** (128 dynamic mainnet subnets): no cold/empty/tiny/over-1.0-price
  anomalies; spot↔EMA drift handled in the safe direction by the `min`/`max`
  reference design. Documented `pEMA` caveat (`min(spot,1.0)` clamp, ~30d half-life;
  guarantees hold for price ≤ ~1.0, true for all subnets today).

## 6. Live local-chain end-to-end (3-validator `fast-runtime` localnet)

- Chain produced blocks; runtime metadata contains all derivative extrinsics
  (incl. the `max_alpha_liability` bound param) and governance setters.
- **Governance**: `sudo_set_shorts_enabled`, `sudo_set_short_kappa`,
  `sudo_set_subtoken_enabled`, `sudo_set_longs_enabled` — all applied on-chain.
- **Full SHORT lifecycle**: `add_stake` (fund pool) → `open_short` (`ShortOpened`;
  SubnetTAO fell by exactly R+E — conservation) → `top_up_short` (R grew by the
  exact amount) → `close_short` full (`ShortClosed`; position cleared, escrow +
  repaid Q returned to pool).
- **Atomic rollback observed live**: an `open_short` against an unfunded pool failed
  on the pool leg and rolled back completely (no position, `SubnetTAO` Δ=0, only the
  tx fee).
- **LONG side**: enabled; `open_long` correctly rejected on the alpha-depleted pool
  via both guards (`EffectiveLtvNonPositive`, `AmountTooLow`) — extrinsic + domain
  checks live (a clean long open needs a pool with healthy alpha reserve).

## 7. Residual / out-of-scope (tracked, not code-correctness gaps)

- **Benchmarked weights** — *now implemented:* FRAME v2 benchmarks exist for all 8
  extrinsics plus the O(N) hooks (`run_short_decay`/`run_long_decay` with an
  active-subnet component over `[0,128]`, `settle_shorts_on_dereg`/`settle_longs_on_dereg`
  with a position component over `[0,1024]`); the 8 dispatches use `T::WeightInfo::*`,
  `on_initialize` charges the per-block decay at `TotalNetworks`, and the dissolve
  extrinsics charge terminal settlement at the *actual* per-subnet position count.
  The remaining step is regenerating the weight constants on CI reference hardware
  (`--extrinsic '*'`) before mainnet enablement — the harness/wiring is in place.
- A clean **successful long open on-chain** was subsequently demonstrated on a
  mainnet-seeded localnet (`open_long` P=1.0α → D liability at spot, full close
  clears); also covered by unit tests (`long_dereg_in_the_money_pays_bounded_equity`,
  conservation proofs).

### 7.1 Pre-enablement checklist (must clear before `ShortsEnabled`/`κ` ramp)

These are integration dependencies and operational invariants, not code-correctness
gaps. They must be re-verified at enablement time because they depend on upstream
state or governance configuration that can drift after merge.

1. **Adversarial trading-games matrix** on a mainnet-like replica — EMA half-life ×
   `κ` × pool depth × attacker capital × dereg distance × registration timing ×
   spot-buy defense. The short-to-dereg safety margin and the one-sided
   reserve-accounting approximation (intentional divergence from fee/weighted spot
   execution) are only valid once this passes.
2. **`pEMA` dependency is load-bearing.** The safety math assumes
   `SubnetMovingPrice = EMA(min(spot, 1.0))` with a slow half-life. If upstream
   changes the `min(·,1.0)` clamp or the half-life, the derivative anti-suppression
   math must be revalidated before enablement.
3. **High-price-subnet caveat.** Because `pEMA` is clamped around 1.0, the terminal
   anti-suppression guarantee is stated only for subnets priced below ~1.0 (true for
   all mainnet subnets today). Confirm this still holds at enablement.
4. **Decay weight vs subnet count.** Decay `WeightInfo` is benchmarked over `[0,128]`
   (= `DefaultSubnetLimit`). The hook conservatively charges `TotalNetworks` (≥ active
   derivative subnets), so it slightly over-charges block weight by design. If the
   subnet limit is ever raised above 128, regenerate the decay weights at the new
   ceiling (or clamp/paginate the hook) first.
5. **CI reference-hardware weight regen** (`--extrinsic '*'`) — wiring is in place.

### 7.2 Accepted tradeoffs (intentional, not blockers)

- **Terminal transfer failure = log + sweep, not abort.** A custody-invariant breach
  during dereg settlement logs loudly and the unpaid value stays in custody to be
  swept (recycled), rather than aborting the dereg. No value is created/lost and the
  emitted `equity` reflects only what was actually paid; the position is underpaid
  rather than the subnet bricked on a dust shortfall. Intentional.
