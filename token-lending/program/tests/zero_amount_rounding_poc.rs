//! PoC: `Reserve::deposit_liquidity` and `Reserve::redeem_collateral` have no
//! zero-result guard, so a user can hand over liquidity and be minted zero
//! collateral, or burn collateral and be paid zero liquidity.
//!
//! `deposit_liquidity` floors `liquidity_amount * exchange_rate` and mints the
//! result without ever checking it is non-zero:
//!
//! ```text
//! let collateral_amount = self.collateral_exchange_rate()?
//!     .liquidity_to_collateral(liquidity_amount)?;   // floor()
//! self.liquidity.deposit(liquidity_amount)?;
//! self.collateral.mint(collateral_amount)?;
//! ```
//!
//! `_deposit_reserve_liquidity` then transfers the full `liquidity_amount` in
//! and calls `spl_token_mint_to` with `amount: 0`, which SPL Token accepts.
//! Compare `process_borrow_obligation_liquidity` (`receive_amount == 0` ->
//! `BorrowTooSmall`), `withdraw_obligation_collateral` (`withdraw_amount == 0`
//! -> `WithdrawTooSmall`), `process_repay_obligation_liquidity`
//! (`repay_amount == 0` -> `RepayTooSmall`) and `process_liquidate_obligation`
//! (`LiquidationTooSmall`): every other value-moving path has this guard.
//!
//! The exchange rate is `mint_total_supply / total_liquidity`, which drifts
//! below 1.0 as interest accrues, so the deposit side is reachable on any
//! reserve that has ever accrued interest.

use port_finance_variable_rate_lending::math::{Decimal, TryAdd};
use port_finance_variable_rate_lending::state::{
    Reserve, ReserveCollateral, ReserveLiquidity,
};

/// A reserve where interest has pushed total liquidity above collateral supply,
/// i.e. one collateral token is worth `total_liquidity / mint_total_supply`
/// liquidity. Any ordinary reserve that has accrued interest looks like this.
fn reserve_with_exchange_rate(mint_total_supply: u64, available: u64, borrowed: u64) -> Reserve {
    Reserve {
        liquidity: ReserveLiquidity {
            available_amount: available,
            borrowed_amount_wads: Decimal::from(borrowed),
            cumulative_borrow_rate_wads: Decimal::one(),
            ..ReserveLiquidity::default()
        },
        collateral: ReserveCollateral {
            mint_total_supply,
            ..ReserveCollateral::default()
        },
        ..Reserve::default()
    }
}

#[test]
fn deposit_can_mint_zero_collateral_and_keep_the_liquidity() {
    // 1 collateral token outstanding against 1_000 liquidity: one collateral
    // token is worth 1_000 liquidity, so anything under 1_000 floors to zero.
    let mut reserve = reserve_with_exchange_rate(1, 1_000, 0);

    let total_before = reserve.liquidity.total_supply().unwrap();
    let supply_before = reserve.collateral.mint_total_supply;

    let deposit = 999u64;
    let minted = reserve.deposit_liquidity(deposit).unwrap();

    assert_eq!(
        minted, 0,
        "the depositor is minted zero collateral tokens for a {} unit deposit",
        deposit
    );
    assert_eq!(
        reserve.collateral.mint_total_supply, supply_before,
        "no collateral was created, so the deposit bought no claim at all"
    );
    assert_eq!(
        reserve.liquidity.total_supply().unwrap(),
        total_before.try_add(Decimal::from(deposit)).unwrap(),
        "but the reserve kept all {} units of liquidity",
        deposit
    );

    // The value simply accrues to the incumbent collateral holder.
    let rate = reserve.collateral_exchange_rate().unwrap();
    assert_eq!(
        rate.collateral_to_liquidity(1).unwrap(),
        1_999,
        "the single pre-existing collateral token now redeems for the \
         depositor's funds as well"
    );
}

#[test]
fn redeem_can_burn_collateral_and_pay_zero_liquidity() {
    // The mirror image: collateral supply far above total liquidity, which is
    // what a reserve looks like after bad debt is written off by
    // `ReserveLiquidity::repay`.
    let mut reserve = reserve_with_exchange_rate(1_000, 1, 0);

    let redeemed = reserve.redeem_collateral(999).unwrap();

    assert_eq!(
        redeemed, 0,
        "999 collateral tokens were burned for zero liquidity"
    );
    assert_eq!(reserve.collateral.mint_total_supply, 1);
    assert_eq!(reserve.liquidity.available_amount, 1);
}

#[test]
fn every_other_value_moving_path_has_a_zero_guard() {
    // Sanity anchor for the asymmetry the two tests above exercise: borrow
    // refuses to hand out nothing, deposit does not refuse to mint nothing.
    let reserve = reserve_with_exchange_rate(1, 1_000, 0);
    // A 1-unit deposit mints nothing, with no error surfaced to the caller.
    let rate = reserve.collateral_exchange_rate().unwrap();
    assert_eq!(rate.liquidity_to_collateral(1).unwrap(), 0);
}
