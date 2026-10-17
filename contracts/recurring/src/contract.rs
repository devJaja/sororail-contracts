use soroban_sdk::{contract, contractimpl, token, Address, Env};
use sororail_common::{auth, math, Error};

use crate::{events, storage, types::Authorization};

#[contract]
pub struct RecurringContract;

#[contractimpl]
impl RecurringContract {
    /// Records a recurring pull authorization.
    ///
    /// Requires the payer's authorization -- they are the one consenting to be
    /// charged. The first charge becomes available one full period after this
    /// call, so a subscription never bills at the moment of signup without the
    /// payer seeing a period pass.
    ///
    /// `payer` and `payee` must differ; a self-authorization is rejected.
    ///
    /// **This call does not by itself let the payee take anything.** The payer
    /// must also `approve` this contract as a spender on the token for at
    /// least the amount they intend to allow. That allowance is the real cap:
    /// revoking it on the token stops charges even without cancelling here.
    pub fn authorize(
        env: Env,
        payer: Address,
        payee: Address,
        token: Address,
        amount_per_period: i128,
        period_seconds: u64,
        max_periods: Option<u32>,
    ) -> Result<(), Error> {
        if storage::is_initialized(&env) {
            return Err(Error::AlreadyInitialized);
        }
        if payer == payee {
            return Err(Error::IdenticalParties);
        }
        math::require_positive(amount_per_period)?;
        if period_seconds == 0 {
            return Err(Error::InvalidDuration);
        }
        if max_periods == Some(0) {
            return Err(Error::InvalidAmount);
        }

        payer.require_auth();

        let first_chargeable_at = env
            .ledger()
            .timestamp()
            .checked_add(period_seconds)
            .ok_or(Error::InvalidTimeRange)?;

        let authorization = Authorization {
            payer: payer.clone(),
            payee: payee.clone(),
            token: token.clone(),
            amount_per_period,
            period_seconds,
            max_periods,
            periods_charged: 0,
            next_chargeable_at: first_chargeable_at,
            cancelled: false,
        };
        storage::save(&env, &authorization);

        events::Authorized {
            payer,
            payee,
            token,
            amount_per_period,
            period_seconds,
            max_periods,
            first_chargeable_at,
        }
        .publish(&env);
        Ok(())
    }

    /// Pulls one period's payment from the payer to the payee.
    ///
    /// Callable by the payee. Moves funds with `transfer_from`, spending the
    /// allowance the payer granted this contract on the token.
    ///
    /// The next charge is scheduled at `now + period_seconds`, **not** one
    /// period after the previous due date. Skipped periods are forfeited
    /// rather than banked -- see [`Authorization`] for why.
    pub fn charge(env: Env) -> Result<i128, Error> {
        let mut authorization = storage::load(&env)?;
        // Require auth before state checks so unauthenticated callers cannot
        // probe contract state via typed errors (CONTRIBUTING.md convention).
        authorization.payee.require_auth();
        if authorization.cancelled {
            return Err(Error::RecurringCancelled);
        }
        if authorization.is_exhausted() {
            return Err(Error::RecurringExhausted);
        }

        let now = env.ledger().timestamp();
        if now < authorization.next_chargeable_at {
            return Err(Error::RecurringPeriodNotElapsed);
        }

        let amount = authorization.amount_per_period;
        authorization.periods_charged = authorization
            .periods_charged
            .checked_add(1)
            .ok_or(Error::Overflow)?;
        authorization.next_chargeable_at = now
            .checked_add(authorization.period_seconds)
            .ok_or(Error::InvalidTimeRange)?;
        // The period is consumed and persisted here, *before* `transfer_from`
        // moves any funds. This ordering is intentional: Soroban aborts the
        // whole invocation and rolls back every state change when a host call
        // (the token transfer) traps, so a failed transfer cannot leave the
        // period consumed -- the `periods_charged` bump and the advanced
        // `next_chargeable_at` are reverted together with the failed call.
        // That rollback, not the save-first ordering itself, is what makes
        // this safe; it also means a reentrant token sees the already-advanced
        // `next_chargeable_at` and cannot charge the same period twice.
        // Compare `escrow::pay`, where persisting state before the token call
        // serves reentrancy protection specifically.
        storage::save(&env, &authorization);

        token::TokenClient::new(&env, &authorization.token).transfer_from(
            &env.current_contract_address(),
            &authorization.payer,
            &authorization.payee,
            &amount,
        );

        events::Charged {
            payee: authorization.payee.clone(),
            payer: authorization.payer.clone(),
            amount,
            periods_charged: authorization.periods_charged,
            next_chargeable_at: authorization.next_chargeable_at,
        }
        .publish(&env);
        Ok(amount)
    }

    /// Cancels the authorization, effective immediately.
    ///
    /// Callable by either party. The payer can always stop a subscription
    /// without needing the payee's cooperation.
    pub fn cancel(env: Env, caller: Address) -> Result<(), Error> {
        let mut authorization = storage::load(&env)?;
        if authorization.cancelled {
            return Err(Error::RecurringCancelled);
        }
        auth::require_auth_either(&caller, &authorization.payer, &authorization.payee)?;

        authorization.cancelled = true;
        storage::save(&env, &authorization);

        events::Cancelled {
            cancelled_by: caller,
            periods_charged: authorization.periods_charged,
            cancelled_at: env.ledger().timestamp(),
        }
        .publish(&env);
        Ok(())
    }

    /// The earliest timestamp at which the next charge may be taken.
    ///
    /// Reports the stored schedule regardless of whether the authorization is
    /// cancelled or exhausted; check `is_chargeable` for whether a
    /// charge would actually succeed.
    pub fn next_chargeable_at(env: Env) -> Result<u64, Error> {
        Ok(storage::load_view(&env)?.next_chargeable_at)
    }

    /// Whether a charge would succeed right now.
    pub fn is_chargeable(env: Env) -> Result<bool, Error> {
        let authorization = storage::load_view(&env)?;
        Ok(authorization.is_chargeable_at(env.ledger().timestamp()))
    }

    /// Charges still permitted under the cap, if there is one.
    pub fn remaining_periods(env: Env) -> Result<Option<u32>, Error> {
        Ok(storage::load_view(&env)?.remaining_periods())
    }

    /// Extends the instance TTL for a long-lived authorization without changing it.
    pub fn bump(env: Env) -> Result<(), Error> {
        storage::load_view(&env)?;
        sororail_common::storage::extend_instance(&env);
        Ok(())
    }

    /// The full authorization record.
    pub fn get(env: Env) -> Result<Authorization, Error> {
        storage::load_view(&env)
    }
}
