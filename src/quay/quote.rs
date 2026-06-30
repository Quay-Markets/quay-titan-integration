//! Pricing for [`QuayVenue`].
//!
//! `compute_quote` (the body of `TradingVenue::quote`) prices an `ExactIn`
//! swap by running the strategy's DSL curve through
//! `quay_sdk::simulate::simulate_swap_in` — bit-identical to the on-chain
//! `swap` — and reports the marginal price `f'(amount)` by finite difference.

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

        // MM's price-probe input for this side — used as both the spot probe
        // size and the marginal-price step. Sized by the MM (from token
        // prices/decimals) so one step moves enough output atoms that ±1-atom
        // rounding stays under Titan's 0.1 bps tolerance.
        let price_probe_in = if side == SIDE_SELL_BASE {
            self.price_probe_base
        } else {
            self.price_probe_quote
        };

        // Spot rate (Titan asks with amount == 0): probe once at the probe size.
        // `0.0` if it can't fill — we never shrink below the probe size.
        // `simulate_out` never returns `Some(0)`, so the divisor is positive.
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

        // Marginal price `f'(a)` ≈ the chord over one price-probe step: probe
        // `a + price_probe_in` (forward, or backward near the inventory bound).
        // The MM sizes the probe so this step moves enough output atoms to keep
        // ±1-atom rounding under Titan's 0.1 bps MVT tolerance, and it's small vs
        // pool depth so the chord tracks the true local rate. Strict `>` guards:
        // a zero diff means the slope is unmeasurable here, so fall back to the
        // average rate, never `price == 0`.
        let h = max(price_probe_in, 1);
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
