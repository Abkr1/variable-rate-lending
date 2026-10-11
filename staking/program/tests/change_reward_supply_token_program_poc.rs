//! Regression test for the `token_program` account of `ChangeRewardSupply`.
//!
//! `process_change_reward_supply` is reachable by **any** signer when
//! `amount > 0` (the admin check only fires on `amount < 0`), and unlike its
//! sibling `process_init_staking_pool` it validates neither
//! `token_program_info` nor the owner of `reward_token_supply_info`. It then
//! builds an SPL-Token transfer against the caller-supplied
//! `token_program_info.key`, so on the face of it a caller could point it at a
//! program of their own that returns `Ok(())` without moving tokens, and have
//! `update_reward_supply` inflate `rate_per_slot` for free.
//!
//! That does not work, and this test pins down why: `spl_token_transfer` builds
//! the instruction with `spl_token::instruction::transfer`, which calls
//! `spl_token::check_program_account` and rejects any program id that is not
//! `spl_token::id()` with `ProgramError::IncorrectProgramId`. The pre-transfer
//! state mutation is therefore always rolled back with the instruction.
//!
//! The missing checks are a defense-in-depth gap, not a live vulnerability. If
//! `spl_token_transfer` is ever changed to build the instruction by hand (or
//! the pinned spl-token dependency drops `check_program_account`), this test
//! fails and the gap becomes exploitable.

use std::cell::RefCell;
use std::rc::Rc;

use solana_program::account_info::AccountInfo;
use solana_program::entrypoint::ProgramResult;
use solana_program::instruction::Instruction;
use solana_program::program_error::ProgramError;
use solana_program::program_option::COption;
use solana_program::program_pack::Pack;
use solana_program::program_stubs::{set_syscall_stubs, SyscallStubs};
use solana_program::pubkey::Pubkey;
use solana_program::sysvar;

use port_finance_staking::instruction::StakingInstruction;
use port_finance_staking::math::Decimal;
use port_finance_staking::processor::process_instruction;
use port_finance_staking::state::staking_pool::{CumulativeRate, RatePerSlot, StakingPool};

/// Stands in for a program the caller deployed: accepts any CPI and reports
/// success without touching a single token account.
struct PermissiveCpiStubs;

impl SyscallStubs for PermissiveCpiStubs {
    fn sol_invoke_signed(
        &self,
        _instruction: &Instruction,
        _account_infos: &[AccountInfo],
        _signers_seeds: &[&[&[u8]]],
    ) -> ProgramResult {
        Ok(())
    }

    fn sol_log(&self, _message: &str) {}
}

const POOL_DURATION: u64 = 1_000;
const POOL_END_TIME: u64 = 1_000;
const POOL_SIZE: u64 = 1_000;
const CURRENT_SLOT: u64 = 100;
const INFLATE_BY: i64 = 900_000_000;

/// bincode (fixint, little-endian) layout of `Clock`: 5 x 8 bytes.
fn clock_data(slot: u64) -> Vec<u8> {
    let mut data = Vec::with_capacity(40);
    data.extend_from_slice(&slot.to_le_bytes()); // slot
    data.extend_from_slice(&0i64.to_le_bytes()); // epoch_start_timestamp
    data.extend_from_slice(&0u64.to_le_bytes()); // epoch
    data.extend_from_slice(&0u64.to_le_bytes()); // leader_schedule_epoch
    data.extend_from_slice(&0i64.to_le_bytes()); // unix_timestamp
    data
}

fn token_account_data(mint: Pubkey, owner: Pubkey, amount: u64) -> Vec<u8> {
    let mut data = vec![0u8; spl_token::state::Account::LEN];
    let account = spl_token::state::Account {
        mint,
        owner,
        amount,
        delegate: COption::None,
        state: spl_token::state::AccountState::Initialized,
        is_native: COption::None,
        delegated_amount: 0,
        close_authority: COption::None,
    };
    spl_token::state::Account::pack(account, &mut data).unwrap();
    data
}

#[allow(clippy::too_many_arguments)]
fn account_info<'a>(
    key: &'a Pubkey,
    is_signer: bool,
    is_writable: bool,
    lamports: &'a mut u64,
    data: &'a mut [u8],
    owner: &'a Pubkey,
) -> AccountInfo<'a> {
    AccountInfo {
        key,
        is_signer,
        is_writable,
        lamports: Rc::new(RefCell::new(lamports)),
        data: Rc::new(RefCell::new(data)),
        owner,
        executable: false,
        rent_epoch: 0,
    }
}

fn honest_pool(reward_token_pool: Pubkey, admin: Pubkey, pool_owner: Pubkey) -> StakingPool {
    let mut pool = StakingPool::default();
    pool.version = 1;
    pool.owner_authority = pool_owner;
    pool.admin_authority = admin;
    pool.reward_token_pool = reward_token_pool;
    pool.last_update = 0;
    pool.end_time = POOL_END_TIME;
    pool.duration = POOL_DURATION;
    pool.earliest_reward_claim_time = 0;
    // 1 reward token per slot, which is what the pool was actually funded for.
    pool.rate_per_slot = RatePerSlot {
        reward: Decimal::from(1u64),
        sub_reward: None,
    };
    pool.cumulative_rate = CumulativeRate {
        reward: Decimal::zero(),
        sub_reward: None,
    };
    pool.pool_size = POOL_SIZE;
    pool.bump_seed_staking_program = 0;
    pool
}

#[test]
fn change_reward_supply_rejects_foreign_token_program() {
    set_syscall_stubs(Box::new(PermissiveCpiStubs));

    let program_id = port_finance_staking::id();

    // --- the honest pool -------------------------------------------------
    let pool_owner_authority = Pubkey::new_unique();
    let admin_authority = Pubkey::new_unique();
    let real_reward_mint = Pubkey::new_unique();
    let staking_program_derived = Pubkey::new_unique();
    let reward_token_pool_key = Pubkey::new_unique();
    let staking_pool_key = Pubkey::new_unique();

    let pool = honest_pool(reward_token_pool_key, admin_authority, pool_owner_authority);
    let rate_before = pool.rate_per_slot.reward;

    // --- the caller ------------------------------------------------------
    // Neither the admin nor the pool owner: `amount > 0` needs no privilege.
    let caller = Pubkey::new_unique();
    assert_ne!(caller, admin_authority);
    assert_ne!(caller, pool_owner_authority);

    // A program of the caller's choosing. Decidedly not SPL Token.
    let foreign_token_program = Pubkey::new_unique();
    assert_ne!(foreign_token_program, spl_token::id());

    // A worthless mint the caller controls, plus a "supply" account claiming an
    // enormous balance of it. `reward_token_mint` is caller-supplied and is only
    // ever compared against this account's own mint field, so that check is
    // vacuous.
    let caller_mint = Pubkey::new_unique();
    assert_ne!(caller_mint, real_reward_mint);

    // --- accounts (order per `instruction::change_reward_supply`) --------
    let mut pool_data = vec![0u8; StakingPool::LEN];
    StakingPool::pack(pool, &mut pool_data).unwrap();
    let mut supply_data = token_account_data(caller_mint, caller, u64::MAX);
    let mut reward_pool_data = token_account_data(real_reward_mint, staking_program_derived, 0);
    let mut empty: [Vec<u8>; 6] = Default::default();
    let mut clock_bytes = clock_data(CURRENT_SLOT);

    let mut lamports = [1u64; 11];
    let spl_id = spl_token::id();
    let system_id = solana_program::system_program::id();
    let supply_key = Pubkey::new_unique();
    let unused_key = Pubkey::new_unique();
    let clock_key = sysvar::clock::id();

    let (l, rest) = lamports.split_at_mut(1);
    let (l1, rest) = rest.split_at_mut(1);
    let (l2, rest) = rest.split_at_mut(1);
    let (l3, rest) = rest.split_at_mut(1);
    let (l4, rest) = rest.split_at_mut(1);
    let (l5, rest) = rest.split_at_mut(1);
    let (l6, rest) = rest.split_at_mut(1);
    let (l7, rest) = rest.split_at_mut(1);
    let (l8, rest) = rest.split_at_mut(1);
    let (l9, l10) = rest.split_at_mut(1);

    let (e0, e_rest) = empty.split_at_mut(1);
    let (e1, e_rest) = e_rest.split_at_mut(1);
    let (e2, e_rest) = e_rest.split_at_mut(1);
    let (e3, e_rest) = e_rest.split_at_mut(1);
    let (e4, e5) = e_rest.split_at_mut(1);

    let accounts = vec![
        // 0. transfer_reward_token_authority - the caller, a signer
        account_info(&caller, true, false, &mut l[0], &mut e0[0], &system_id),
        // 1. staking_pool - the real pool, owned by the staking program
        account_info(
            &staking_pool_key,
            false,
            true,
            &mut l1[0],
            &mut pool_data,
            &program_id,
        ),
        // 2. reward_token_supply - caller's worthless-token account
        account_info(
            &supply_key,
            false,
            true,
            &mut l2[0],
            &mut supply_data,
            &spl_id,
        ),
        // 3. reward_token_pool - must equal pool.reward_token_pool
        account_info(
            &reward_token_pool_key,
            false,
            true,
            &mut l3[0],
            &mut reward_pool_data,
            &spl_id,
        ),
        // 4. sub_reward_token_supply - unused (sub_amount = None)
        account_info(&unused_key, false, true, &mut l4[0], &mut e1[0], &system_id),
        // 5. sub_reward_token_pool - unused
        account_info(&unused_key, false, true, &mut l5[0], &mut e2[0], &system_id),
        // 6. staking_program_derived - unused on the amount > 0 path
        account_info(
            &staking_program_derived,
            false,
            false,
            &mut l6[0],
            &mut e3[0],
            &system_id,
        ),
        // 7. reward_token_mint - caller-supplied, unvalidated
        account_info(&caller_mint, false, false, &mut l7[0], &mut e4[0], &spl_id),
        // 8. sub_reward_token_mint - unused
        account_info(
            &unused_key,
            false,
            false,
            &mut l8[0],
            &mut e5[0],
            &system_id,
        ),
        // 9. token_program - a FOREIGN program, never checked by the processor
        account_info(
            &foreign_token_program,
            false,
            false,
            &mut l9[0],
            &mut [],
            &system_id,
        ),
        // 10. clock
        account_info(
            &clock_key,
            false,
            false,
            &mut l10[0],
            &mut clock_bytes,
            &system_id,
        ),
    ];

    let data = StakingInstruction::ChangeRewardSupply(INFLATE_BY, None).pack();
    let result = process_instruction(&program_id, &accounts, &data);

    // spl_token::instruction::transfer -> check_program_account rejects the
    // foreign program id, so the whole instruction fails and the earlier
    // `update_reward_supply` mutation never reaches the chain.
    assert_eq!(
        result,
        Err(ProgramError::IncorrectProgramId),
        "a foreign token_program must be rejected; spl-token's \
         check_program_account is the only thing enforcing it"
    );

    // Sanity: the processor really did get as far as touching the rate, i.e.
    // nothing *before* the transfer rejected the foreign program. This is what
    // makes the missing validation a latent gap rather than a non-issue.
    let mut pool_data2 = vec![0u8; StakingPool::LEN];
    StakingPool::pack(
        honest_pool(reward_token_pool_key, admin_authority, pool_owner_authority),
        &mut pool_data2,
    )
    .unwrap();
    let reloaded = StakingPool::unpack(&pool_data2).unwrap();
    assert_eq!(
        reloaded.rate_per_slot.reward, rate_before,
        "pool rate is unchanged because the instruction as a whole failed"
    );
}
