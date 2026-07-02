//! Pricing for [`QuayVenue`].
//!
//! `compute_quote` prices an `ExactIn` swap by running the strategy's DSL
//! curve through `quay_sdk::simulate::simulate_swap_in` — the same math as
//! the on-chain `swap` — and reports the marginal price `f'(amount)` by
//! finite difference.

use std::cmp::max;

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
        // Quay's swap is exact-in only and the DSL is a forward pricer with
        // no inverse.
        if matches!(request.swap_type, SwapType::ExactOut) {
            return Err(TradingVenueError::ExactOutNotSupported);
        }

        // Map mints to Quay's `side` byte: 0 = sell base, 1 = buy base.
        let side = if request.input_mint == self.base_mint
            && request.output_mint == self.quote_mint
        {
            SIDE_SELL_BASE
        } else if request.input_mint == self.quote_mint && request.output_mint == self.base_mint {
            SIDE_BUY_BASE
        } else {
            return Err(TradingVenueError::InvalidMint(request.input_mint.into()));
        };

        // Same gate as `initialized()`, so the router sees one canonical
        // "not initialized" error to skip.
        if self.routing_flags & ROUTE_TITAN == 0
            || !self.has_all_state()
            || !self.halts_clear()
        {
            return Err(TradingVenueError::NotInitialized(self.strategy_key.into()));
        }

        // SPL mint decimals always fit in `u8`; `TokenInfo.decimals` is
        // `i32` only because the upstream template says so.
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

        // The VM prices into a stack scratch buffer, so the quote path does
        // no heap allocation. `MAX_USERSPACE_LEN` is the program's hard
        // userspace cap, so the buffer fits any strategy.
        let mut scratch = [0u8; MAX_USERSPACE_LEN as usize];
        // `simulate_out(x)` = output atoms for an `x`-atom swap, or `None`
        // when the curve refuses that size. `Option` rather than `Result` on
        // purpose: the boundary search probes refused sizes under
        // `assert_no_alloc`, so the refusal path must not allocate an error
        // string, and at this point a simulator error and a refusal both
        // mean "not quotable at this size".
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

        // MM-set probe input for this side, used as both the spot-probe size
        // and the marginal-price step. The MM sizes it so one step moves
        // enough output atoms that ±1-atom rounding stays inside Titan's
        // mean-value-theorem tolerance.
        let price_probe_in = if side == SIDE_SELL_BASE {
            self.price_probe_base
        } else {
            self.price_probe_quote
        };

        // Spot rate (Titan asks with `amount == 0`): probe once at the probe
        // size, 0.0 if the curve can't fill it. `simulate_out` never returns
        // `Some(0)`, so the divisor is positive.
        if request.amount == 0 {
            let price = match simulate_out(price_probe_in) {
                Some(out) => out as f64 / price_probe_in as f64,
                None => 0.0,
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
            // Refused size: report "not enough liquidity" instead of an
            // allocating error — the boundary search runs this path under
            // `assert_no_alloc`.
            return Ok(QuoteResult {
                input_mint: request.input_mint,
                output_mint: request.output_mint,
                amount: a,
                expected_output: 0,
                not_enough_liquidity: true,
                price: 0.0,
            });
        };

        // Marginal price `f'(a)`: chord over one probe step, forward first,
        // backward near the inventory bound. A zero diff means the slope is
        // unmeasurable at this granularity, so fall back to the average rate
        // — never `price == 0` on a valid quote.
        let h = max(price_probe_in, 1);
        let price = match simulate_out(a.saturating_add(h)) {
            Some(up) if up > out => (up - out) as f64 / h as f64,
            _ => match simulate_out(a.saturating_sub(h)) {
                Some(down) if out > down => (out - down) as f64 / h as f64,
                _ => out as f64 / a as f64,
            },
        };

        Ok(QuoteResult {
            input_mint: request.input_mint,
            output_mint: request.output_mint,
            amount: a,
            expected_output: out,
            // Quay's curves cannot short-fill: the simulator either prices
            // the full `amount_in` or refuses.
            not_enough_liquidity: false,
            price,
        })
    }
}
