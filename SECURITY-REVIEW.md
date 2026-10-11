# Port Variable Rate Lending — Security Review

**Scope:** `abkr1/variable-rate-lending` @ `bdc4ebf` (branch `claude/youthful-keller-ewj68h`, identical to `master`).
Both on-chain programs: `token-lending/program` (`Port7uDYB3wk6GJAw4KT1WpTeMtSu9bTcChBHkX2LfR`)
and `staking/program` (`stkarvwmSzv2BygN5e2LeTwimTczLWHCKPKGC2zVLiq`).

**Trust model used.** Trusted, therefore out of scope: the lending market owner
(`InitReserve`, `UpdateReserve`, `SetLendingMarketOwner`, `WithdrawFee`), the staking pool
`admin_authority` / `owner_authority`, and the Pyth / Switchboard oracles. Attacker: an
unprivileged user who can call `InitLendingMarket`, `InitObligation`, `Deposit*`, `Redeem*`,
`Borrow*`, `Repay*`, `Liquidate*`, `Refresh*`, `FlashLoan`, `CreateStakeAccount` and
`ClaimReward`. Target impact: direct theft, unauthorized mint, or protocol insolvency.

**Method.** First-hand read of all 9.8k lines of program source, with particular attention to
the fork-specific surface (fixed-price reserves, the `deposit_staking_pool` CPI integration,
`UpdateReserve`, `WithdrawFee`, `DepositReserveLiquidityAndObligationCollateral`, Switchboard
v2, and the whole staking program). Four PoC tests written and run; the repo's own BPF
harness (`solana-program-test` 1.10) segfaults on a modern toolchain, so the PoCs drive the
processor and state types directly.

---

## Bottom line

**No unprivileged drain of depositor principal in the lending program.** Every
PDA-authority outflow — `redeem_reserve_collateral`, `withdraw_obligation_collateral`,
`borrow_obligation_liquidity`, `liquidate_obligation`, `flash_loan` — is correctly gated, and
I was unable to construct a path that moves more value out than the gating math permits. The
accounting is conservative in the protocol's favour at every rounding boundary I checked.

The real exposure is in the **staking program and the lending↔staking integration**, which is
the fork's own code and visibly less hardened than the SPL-derived core:

1. **High — the staking emission schedule silently restarts every time a pool empties**,
   letting a late-arriving staker accrue against, and claim, reward tokens that belong to
   earlier stakers. Unprivileged, and it is a genuine drain of the reward token pool.
2. **High — an obligation's staked balance can be fragmented across stake accounts** so that
   no single stake account covers a full-size liquidation, forcing liquidators into one
   transaction per fragment. This converts recoverable positions into bad debt.

Both are confirmed by passing PoC tests committed alongside this review.

---

## Findings

### 1. High — unfunded emission restart drains the reward pool (`staking`)

`StakingPool::deposit` (`staking/program/src/state/staking_pool.rs:281`):

```rust
if self.pool_size == 0u64 {
    self.end_time = current_time.checked_add(self.duration)?;
    self.last_update = current_time;
} else {
    self.cumulative_rate = self.claim_reward_helper(current_time)?;
}
```

The branch exists to start the clock on the first-ever deposit, but it fires on **every**
deposit that finds the pool empty. It pushes `end_time` out by another full `duration` at the
unchanged `rate_per_slot`, with no new reward tokens transferred in. `StakingPool::init` sets
`rate_per_slot = supply / duration`, so each restart promises another whole `supply`.

Why this is a drain rather than only an accounting defect: after `end_time` passes,
`claim_reward_helper`'s `mark_time = min(current_time, self.end_time)` correctly caps accrual,
so a staker who joins late accrues **nothing**. The restart removes that cap. A pool that has
run its course still holds every reward its stakers accrued but never claimed (claiming is
lazy and `earliest_reward_claim_time` encourages batching). An attacker who deposits once the
pool is empty becomes the sole staker on a freshly restarted schedule, accrues the full
`rate_per_slot` per slot, and claims out of that leftover balance — which belongs to the
earlier stakers. `ClaimReward` is unprivileged and pays with the pool PDA's signature.

Precondition: `pool_size == 0`. For a reserve-linked pool `pool_size` is the total obligation
collateral deposited against that reserve, so an attacker cannot force it while others are
deposited — but it arrives naturally on a wound-down or unpopular reserve, which is exactly
when unclaimed rewards have piled up.

**PoC:** `staking/program/tests/emission_restart_poc.rs` — a pool funded for `SUPPLY`
promises `3 × SUPPLY` after two restarts, and `cumulative_rate` ends at 3× the funded total
per staked unit.

**Fix.** Separate "has the schedule started" from "is the pool currently empty". Add a
`started: bool` (or test `end_time == 0`, which is the uninitialized value) and only set
`end_time` on the first-ever deposit:

```rust
if self.end_time == 0 {
    self.end_time = current_time.checked_add(self.duration)?;
    self.last_update = current_time;
} else {
    if self.pool_size != 0 {
        self.cumulative_rate = self.claim_reward_helper(current_time)?;
    } else {
        // pool empty but schedule running: skip accrual (no divisor), do not
        // move end_time.
        self.last_update = min(current_time, self.end_time);
    }
}
```

Note the empty-but-running case must still advance `last_update`, otherwise the idle interval
would be paid to whoever deposits next.

### 2. High — stake-account fragmentation blocks full-size liquidation (`token-lending` ↔ `staking`)

`withdraw_from_staking_program` and `deposit_to_staking_program`
(`token-lending/program/src/processor.rs:1402`, `:1133`) make exactly one check on the stake
account they are handed:

```rust
let stake_account = StakeAccount::unpack(&stake_account_info.data.borrow())?;
if stake_account.owner != owner {
    return Err(LendingError::InvalidStakeAccount.into());
}
```

Nothing binds the stake account to the **obligation** whose collateral is moving. For an owner
holding several obligations and several stake accounts in the same pool, every deposit and
withdrawal can be attributed to any of their stake accounts. Only the aggregate is conserved
(`pool_size == Σ stake_account.deposited_amount`); the per-obligation attribution is not.

`process_liquidate_obligation` (`:1900`) then calls
`withdraw_from_staking_program(program_id, withdraw_amount, …, obligation_owner)` with the
single stake account the **liquidator** supplies, and `StakeAccount::withdraw` does
`deposited_amount.checked_sub(amount)` — so a `withdraw_amount` exceeding that one account's
balance returns `InvalidWithdrawAmountError` and **aborts the entire liquidation**.

An owner therefore shuffles their staked balance into many small stake accounts, after which
a liquidator can only seize one fragment per transaction. Liquidation is not permanently
frozen — the liquidator can shrink `liquidity_amount` until `withdraw_amount` fits the largest
fragment — but it becomes economically unattractive: the owner picks fragment sizes such that
`liquidation_bonus × seizable_value` falls below a liquidator's per-transaction cost, and the
position is left to rot as bad debt.

Economics, stated honestly: each fragment costs the attacker ~0.002 SOL of permanently locked
rent (`StakeAccount::LEN` ≈ 233 bytes, and the staking program has no close instruction),
against ~0.000005 SOL of transaction fee for the liquidator — roughly 400:1 against the
attacker per unit of friction. So this is a griefing/DoS vector that creates protocol
insolvency, not direct self-enrichment. It is still the most serious unprivileged issue in the
lending program, because liquidation is the only mechanism preventing bad debt.

**PoC:** `staking/program/tests/stake_account_fragmentation_poc.rs` — splits an obligation's
collateral across two same-owner stake accounts and shows a full-size liquidation reverting
against each, while a half-size one succeeds.

**Fix.** Bind the stake account to the obligation. The cleanest option is to store the stake
account pubkey on `ObligationCollateral` when the first staked deposit is made for that
reserve, and require it thereafter; a lighter-touch option is to derive the stake account as a
PDA of `(obligation, deposit_reserve)` so there is exactly one per obligation per reserve. In
either case `deposit_to_staking_program` / `withdraw_from_staking_program` should assert the
stake account matches, not merely that the owner does.

### 3. Medium — no zero-result guard on deposit/redeem rounding (`token-lending`)

`Reserve::deposit_liquidity` (`state/reserve.rs:66`) floors
`liquidity_amount × exchange_rate` and mints the result without checking it is non-zero;
`_deposit_reserve_liquidity` then transfers the full `liquidity_amount` in and calls
`spl_token_mint_to` with `amount: 0`, which SPL Token accepts. `redeem_collateral` is the
mirror image: it burns collateral and can pay out zero liquidity.

This is inconsistent with every other value-moving path in the program, all of which do have
the guard: `BorrowTooSmall`, `WithdrawTooSmall`, `RepayTooSmall`, `LiquidationTooSmall`.

Honest impact: the exchange rate is `mint_total_supply / total_liquidity`, and I verified an
attacker **cannot** engineer the skew needed to make this large. Redemption pays out strictly
proportionally whenever `mint_total_supply < total_liquidity`, and in the opposite regime
(`mint_total_supply > total_liquidity`, reachable after bad debt is written off) the free-burn
case only drives the ratio back towards 1.0 — it cannot push it the other way. So reaching
`total_liquidity / mint_total_supply = N` requires N-fold genuine interest accrual against a
tiny collateral supply: decades at realistic rates. The practical loss is dust (a 1-unit
deposit mints 0 on any reserve that has accrued interest), but it is silent and unbounded in
principle.

**PoC:** `token-lending/program/tests/zero_amount_rounding_poc.rs`.

**Fix.** Reject `collateral_amount == 0` in `_deposit_reserve_liquidity` and
`liquidity_amount == 0` in `_redeem_reserve_collateral`, mirroring the existing
`*TooSmall` errors.

### 4. Low — `ReserveLiquidity::repay` zeroes the reserve's total debt instead of erroring

`state/reserve.rs:435`:

```rust
// quick hack around the math overflow issue that happens here
// when attempting to repay the full borrowed value using `u64::MAX`
if settle_amount >= self.borrowed_amount_wads {
    self.borrowed_amount_wads = Decimal::zero();
} else {
    self.borrowed_amount_wads = self.borrowed_amount_wads.try_sub(settle_amount)?;
}
```

`settle_amount` is one *obligation's* settled debt; `self.borrowed_amount_wads` is the
*reserve's* total across all obligations. The clamp silently discards the difference. It is
reachable because the reserve compounds interest on the aggregate once per `RefreshReserve`
while each obligation compounds once per `RefreshObligation` via a cumulative-rate ratio —
both floor, and the reserve floors more often, so the reserve total can drift below the sum of
obligation debts. Zeroing it depresses `total_supply`, which raises the collateral exchange
rate against depositors.

Not practically exploitable: triggering it with another borrower present requires that
borrower's debt to be below the accumulated flooring drift (sub-femto-token scale). But the
clamp hides a real invariant violation that a future change could widen.

**Fix.** Track and subtract the drift explicitly, or clamp with a tolerance and emit a log
rather than silently zeroing an aggregate.

### 5. Low — `Reward::accumulate_reward` can silently discard accrued sub-rewards

`state/stake_account.rs:78`: if `self.sub_reward` is `Some(x)` and the incoming
`reward.sub_reward` is `None`, the `do_notation` block yields `None` and the accrued `x` is
dropped. Currently unreachable — once a pool has a sub-reward, `calculate_reward` always
returns `Some` — but it is one `add_sub_reward`-removal feature away from losing user rewards.

---

## Verified sound (and why)

- **Flash loan.** `source_liquidity_info` is pinned to `reserve.liquidity.supply_pubkey`; the
  post-callback check `actual_balance >= balance_before + origination_fee` reads the real token
  balance, so an unrepaid loan reverts. `invoke` (not `invoke_signed`) is used for the
  receiver, so the market-authority PDA signature is **not** extended to it. Forwarded extra
  accounts preserve `is_signer` faithfully but the PDA is never a transaction signer, so there
  is no privilege escalation. Setting `destination == source`, `destination ==
  fee_receiver`, or a self-chosen `host_fee_receiver` all net out to no gain. Solana's
  reentrancy ban blocks receiver → lending re-entry. `minimum_fee = 2` prevents the
  `owner_fee = origination_fee - host_fee` underflow.
- **Borrow/repay/withdraw/liquidate math.** Tokens leaving the reserve equal
  `floor(borrow_amount)` while recorded debt is the full `Decimal` — rounding favours the
  protocol. `max_withdraw_value = (allowed_borrow_value − borrowed_value) / withdraw_ltv` is
  the correct inverse of the health constraint. `calculate_liquidation` reduces algebraically
  to "liquidator pays seized-collateral-value / (1 + bonus)" in the `Greater` branch and
  "receives repaid-value × (1 + bonus)" in the `Less` branch. The dust branch
  (`borrowed_amount_wads < 2` base units) can write off more than it collects, but only at
  sub-2-unit scale.
- **Vault-reset / first-depositor inflation.** The `mint_total_supply == 0 ||
  total_liquidity == 0` branch of `ReserveCollateral::exchange_rate` resets the rate to 1.0,
  which would be a classic orphaned-liquidity drain. I traced every path that moves
  `available_amount` and `borrowed_amount_wads` and the state is unreachable: debt reduction
  always credits `available_amount` with a non-zero `repay_amount` (zero is rejected), and
  driving `available_amount` to 0 by redemption requires burning the entire collateral supply.
- **Unvalidated `token_program` accounts.** `process_change_reward_supply` validates neither
  `token_program_info` nor the owner of `reward_token_supply_info` — unlike its sibling
  `process_init_staking_pool` — and is reachable by any signer when `amount > 0`. It is **not**
  exploitable: `spl_token_transfer` builds the instruction with
  `spl_token::instruction::transfer`, which calls `spl_token::check_program_account` and
  rejects any program id other than `spl_token::id()`. The same accident protects
  `process_init_lending_market`, which never checks that `token_program_id` is SPL Token.
  Pinned as a regression test in
  `staking/program/tests/change_reward_supply_token_program_poc.rs`, which fails if
  `spl_token_transfer` is ever changed to hand-build the instruction.
- **Staking authority model.** `process_deposit` / `process_withdraw` require
  `owner_authority` or `admin_authority` (the lending market PDA), so users can only move
  stake through the lending program's obligation-ownership checks. `process_claim_reward`
  checks `pool_pubkey`, `stake_account.owner`, `reward_token_pool`, and the derived PDA.
  `StakeAccount` created against an uninitialized pool is inert: the zeroed authorities are
  `Pubkey::default()`, which cannot sign.
- **Arbitrary-program CPI via `staking_program_id`.** Only checked as
  `staking_pool_info.owner`, but `staking_pool_info` is pinned to
  `reserve.config.deposit_staking_pool`, so the chain of checks forces the real staking
  program. Even under an admin misconfiguration the callee receives no token accounts, so the
  forwarded PDA signature reaches nothing of value.
- **`ChangeRewardSupply` sign confusion.** The sub-reward transfer direction is chosen by
  `amount`, not `sub_amount`, and the admin gate only covers `amount < 0`. Mismatched signs
  cast a negative `i64` to a huge `u64`, which SPL Token rejects for insufficient funds, so the
  state mutation always reverts with it. Worth tidying; not exploitable.
- **Instruction and state deserialization.** `LendingInstruction::unpack` length-checks every
  field. `Obligation` packing fits in exactly 776 bytes with zero slack
  (`56 × 1 + 80 × 9`); 10 borrows would overflow `array_mut_ref!` and panic, prevented only by
  `process_borrow_obligation_liquidity` requiring a non-empty `deposits`. Correct today, one
  refactor from a panic — worth an assertion.

## Residual leads

- **`DepositReserveLiquidityAndObligationCollateral` clears the reserve's stale flag**
  (`processor.rs:2155`, `reserve.last_update.update_slot(clock.slot)`) without re-reading the
  oracle, purely so the chained `deposit_obligation_collateral` passes its freshness check.
  Safe today because `_deposit_reserve_liquidity` already requires a same-slot refresh and
  `STALE_AFTER_SLOTS_ELAPSED == 1` confines the effect to that slot. Worth replacing with a
  freshness-check parameter rather than forging the timestamp.
- **Switchboard v1 fast path** (`get_switchboard_price`, the
  `TYPE_AGGREGATOR_RESULT_PARSE_OPTIMIZED` branch) returns `feed_data.result.result` with **no
  staleness check at all**, and `FastRoundResultAccountData::deserialize(...).unwrap()` can
  panic. `get_pyth_price` never verifies `magic` / `ver`. All mitigated because the oracle
  account is pinned by admin config, so these are upgrade-robustness issues rather than live
  bugs — but the missing staleness check is a real divergence from the aggregator path next to
  it, which does check.
- **Two reserves sharing one `deposit_staking_pool`** is not prevented. `pool_size` counts
  collateral units 1:1 regardless of which reserve they came from, so if an admin ever pointed
  two reserves with very different collateral unit values at the same pool, the cheaper
  reserve's depositors would capture nearly all emissions. Admin misconfiguration, but cheap to
  assert against.
- **`process_create_stake_account` does not require `stake_account_owner_info` to be a
  signer**, so anyone can create a stake account owned by anyone. Harmless today (a
  zero-balance account), but it means stake accounts are not a reliable identity anchor — which
  is the same root cause as finding 2.
- **Not covered:** the BPF harness could not be run (`solana-program-test` 1.10 segfaults on
  rustc 1.97), so the end-to-end liquidation revert in finding 2 is established by code reading
  plus a state-level PoC rather than a full on-chain transaction. Reproducing it against
  Solana 1.10 / Rust ~1.60 would close that gap.

## Honest conclusion

The SPL-derived lending core is in good shape and I did not find an unprivileged way to drain
depositor principal from it. The fork's own additions are weaker, and the two High findings
above are both unprivileged, both confirmed by PoC, and both target the staking integration:
one steals reward tokens from other stakers, the other manufactures bad debt by making
liquidation uneconomic. Finding 2 in particular has a single clean root cause — stake accounts
are authenticated by owner but never bound to the obligation they account for — that also
explains several of the residual leads.

Two caveats on the Highs: finding 1 needs `pool_size` to reach zero, which an attacker cannot
force; finding 2 costs the attacker more in locked rent than it costs liquidators in fees, so
it is insolvency-by-griefing rather than profit. Neither is a push-button theft. Nothing here
was tested against mainnet.

### Build note

Verifying the PoCs on this machine required bumping `wasm-bindgen` 0.2.81 → 0.2.92 in
`Cargo.lock`, because the pinned version does not compile under rustc 1.97. That change is
**not** committed — it is an artifact of the host toolchain, unrelated to the findings. On the
repo's intended toolchain (Solana 1.8–1.10, rustc ~1.60) the committed lock file is correct and
the new tests compile as-is.
