//! Pricing for [`QuayVenue`].
//!
//! `compute_quote` (the body of `TradingVenue::quote`) prices an `ExactIn`
//! swap by running the strategy's DSL curve through
//! `quay_sdk::simulate::simulate_swap_in` — bit-identical to the on-chain
//! `swap` — and reports the marginal price `f'(amount)` by finite difference.

use quay_sdk::consts::{MAX_USERSPACE_LEN, ROUTE_TITAN, SIDE_BUY_BASE, SIDE_SELL_BASE};
use quay_sdk::simulate::{simulate_swap_in, SwapSimulationInputs};

use crate::trading_venue::{error::TradingVenueError, QuoteRequest, QuoteResult, SwapType};

use super::QuayVenue;

impl QuayVenue {
    /// Price an `ExactIn` swap. Backs `TradingVenue::quote`; see that trait
    /// method for the pricing invariants Titan relies on.
    pub(super) fn compute_quote(
        &self,
        request: QuoteRequest,
    ) -> Result<QuoteResult, TradingVenueError> {
        // ExactOut: Quay's swap ix is exact-in only and the DSL is a forward
        // pricer (no inverse). Reject cleanly so Titan's router degrades
        // gracefully.
        if matches!(request.swap_type, SwapType::ExactOut) {
            return Err(TradingVenueError::ExactOutNotSupported);
        }

        // Map mints to Quay's `side` byte. 0 = sell base, 1 = buy base.
        let side = if request.input_mint == self.base_mint
            && request.output_mint == self.quote_mint
        {
            SIDE_SELL_BASE
        } else if request.input_mint == self.quote_mint && request.output_mint == self.base_mint {
            SIDE_BUY_BASE
        } else {
            return Err(TradingVenueError::InvalidMint(request.input_mint.into()));
        };

        // Mirror `initialized()`: Titan-routed AND state populated AND halts
        // clear. The simulator would also reject on the halt bytes
        // (`client/sdk/src/simulate.rs`), but failing here gives the router a
        // single canonical "not initialized" surface to skip.
        if self.routing_flags & ROUTE_TITAN == 0
            || !self.has_all_state()
            || !self.halts_clear()
        {
            return Err(TradingVenueError::NotInitialized(self.strategy_key.into()));
        }

        // `TokenInfo.decimals` is `i32` in the upstream template; SPL mints
        // always fit in `u8` (0..=18), so a narrowing cast is safe.
        let base_decimals = self
            .tokens
            .iter()
            .find(|t| t.pubkey == self.base_mint)
            .map(|t| t.decimals as u8)
            .ok_or_else(|| TradingVenueError::MissingState("base TokenInfo".into()))?;
        let quote_decimals = self
            .tokens
            .iter()
            .find(|t| t.pubkey == self.quote_mint)
            .map(|t| t.decimals as u8)
            .ok_or_else(|| TradingVenueError::MissingState("quote TokenInfo".into()))?;

        // `simulate_out(x)` = output atoms for an `x`-atom swap, priced into a
        // reused stack scratch buffer so `quote()` performs **no heap
        // allocation** for any curve. `MAX_USERSPACE_LEN` (16 KiB) is the
        // program's hard userspace cap, so the buffer fits any strategy. Clock
        // comes from the `Clock` sysvar fetched in `update_state` (or a
        // `with_clock` override).
        let mut scratch = [0u8; MAX_USERSPACE_LEN as usize];
        // `simulate_out(x)` = output atoms for an `x`-atom swap, or `None` when
        // the curve refuses that size (over inventory, or the side is rejected).
        // It returns `Option`, not `Result`, on purpose: the boundary search
        // probes refused sizes under `assert_no_alloc`, so the refusal path must
        // not allocate an error string. A genuine `Err` from the simulator is
        // indistinguishable from "refused" at this point (malformed state is
        // already rejected in `update_state`), and both mean "not quotable at
        // this size".
        let mut simulate_out = |amt: u64| -> Option<u64> {
            simulate_swap_in(
                SwapSimulationInputs {
                    strategy_data: &self.strategy_data,
                    market_maker_data: &self.mm_data,
                    quotes_data: &self.quotes_data,
                    global_config_data: &self.global_config_data,
                    current_slot: self.current_slot,
                    current_unix_sec: self.current_unix_sec,
                    side,
                    amount_in: amt,
                    min_amount_out: 0,
                    base_decimals,
                    quote_decimals,
                },
                &mut scratch,
            )
            .ok()
            .map(|s| s.out_to_taker)
        };

        // Spot-price probe = one whole unit of the IN token. Small vs typical
        // inventory but large enough that integer output doesn't round to 0.
        let in_decimals = if side == SIDE_SELL_BASE { base_decimals } else { quote_decimals };
        let probe = 10u64.checked_pow(u32::from(in_decimals)).unwrap_or(1_000_000);

        // Zero-amount: Titan requests zero-input quotes for the spot rate and we
        // must not panic/error. Report the spot price `f'(0) ≈ f(p)/p` for the
        // smallest whole-unit probe that fills: start at one input unit and, if
        // that overdraws a small maker's inventory, shrink until a swap
        // succeeds. `f` returns `None` (never `Some(0)`) on a zero/insufficient
        // output, so any `Some` is a positive fill. Only a curve that refuses
        // every size down to one atom yields `0.0`.
        if request.amount == 0 {
            let mut p = probe;
            let price = loop {
                match simulate_out(p) {
                    Some(out) => break out as f64 / p as f64,
                    None if p > 1 => p = (p / 16).max(1),
                    None => break 0.0,
                }
            };
            return Ok(QuoteResult {
                input_mint: request.input_mint,
                output_mint: request.output_mint,
                amount: 0,
                expected_output: 0,
                not_enough_liquidity: false,
                price,
            });
        }

        let a = request.amount;
        let Some(out) = simulate_out(a) else {
            // The curve refuses this size. Signal "not enough liquidity" (the
            // designed Titan flag) instead of an allocating error — the boundary
            // search relies on this and runs under `assert_no_alloc`.
            return Ok(QuoteResult {
                input_mint: request.input_mint,
                output_mint: request.output_mint,
                amount: a,
                expected_output: 0,
                not_enough_liquidity: true,
                price: 0.0,
            });
        };

        // Marginal price `f'(a) = d(output)/d(input)` by finite difference —
        // forward, falling back to a backward step near the inventory bound
        // (where `a + h` is refused). Titan routes on this and requires it to be
        // the genuine derivative of the output curve (positive, non-increasing,
        // consistent with `expected_output` via the mean-value theorem).
        //
        // Step sizing: outputs are integer atoms, so a finite difference over
        // `h` input atoms carries ±1 atom of quantization noise — a relative
        // error of `1/Δout` where `Δout ≈ h·f'`. Titan's mean-value-theorem
        // check compares adjacent quotes' prices with no absolute slack, so on a
        // near-linear curve (constant marginal) that noise must stay well under
        // its `1e-5` tolerance. We therefore size `h` to a fixed *output* target
        // (`PRICE_PROBE_OUT` atoms) rather than a fraction of `a`: `h ≈
        // PRICE_PROBE_OUT / f'`, estimating `f' ≈ out/a`. `2^20` atoms bounds the
        // quantization error near `1e-6`. The probe costs one extra `simulate`
        // call, so the quote path runs two sims.
        //
        // The guards are strict (`>`): a *zero* finite difference means the
        // output didn't move over `h` atoms — a flat region or a rate so small
        // the step rounds to 0 output. Either way the local slope is unmeasurable
        // here, so we fall through to the average rate rather than report
        // `price == 0` (which would break Titan's positivity invariant).
        const PRICE_PROBE_OUT: u128 = 1 << 20;
        let h = ((PRICE_PROBE_OUT * u128::from(a)) / u128::from(out.max(1)))
            .min(u128::from(u64::MAX)) as u64;
        let h = h.max(1);
        let price = match simulate_out(a.saturating_add(h)) {
            Some(up) if up > out => (up - out) as f64 / h as f64,
            _ => match simulate_out(a.saturating_sub(h)) {
                Some(down) if out > down => (out - down) as f64 / h as f64,
                _ => out as f64 / a as f64, // flat / sub-granular: average rate (always > 0)
            },
        };

        Ok(QuoteResult {
            input_mint: request.input_mint,
            output_mint: request.output_mint,
            amount: a,
            expected_output: out,
            // Quay's curves cannot short-fill — the simulator either prices the
            // full `amount_in` or returns an error.
            not_enough_liquidity: false,
            price,
        })
    }
}
