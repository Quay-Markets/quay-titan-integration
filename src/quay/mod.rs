//! Titan `TradingVenue` adapter for the Quay program.
//!
//! Implements the `trading_venue` contract (`src/trading_venue/mod.rs`) against
//! Quay's on-chain DSL-priced market-maker strategies.
//!
//! Each `QuayVenue` instance represents **one Strategy** — one pricing curve
//! on one `(base_mint, quote_mint)` pair. Titan's router holds many of these
//! (one per active strategy) and aggregates quote distribution across them.
//!
//! # Module layout
//!
//! The adapter is split by concern; the [`TradingVenue`] impl in this file is a
//! thin dispatcher delegating the heavy methods to inherent methods grouped by
//! responsibility:
//! - [`state`] — `FromAccount` construction + per-slot `refresh_state`
//!   (`update_state`) + `required_update_pubkeys`.
//! - [`quote`] — `compute_quote` (`quote`): DSL pricing + marginal price.
//! - [`swap`] — `build_swap_ix` (`generate_swap_instruction`),
//!   `swap_account_metas`, and the `AddressLookupTableTrait` impl.
//! - [`creation`] — `parse_pool_creations` venue discovery.
//!
//! # Account-tracking model
//!
//! `get_required_pubkeys_for_update` returns nine pubkeys:
//! 1. the **Strategy** itself (bytecode + userspace + frozen flags + fee bps),
//! 2. the **MarketMaker** the strategy is anchored to (asset table + halt flags),
//! 3. the strategy's bound **Quotes** account,
//! 4. **GlobalConfig** (halt flags),
//! 5. + 6. the base and quote **mints** (decimals + Token-2022 detection),
//! 7. + 8. the base and quote **vault** token accounts (swap-ix accounts only — the VM no longer prices off vault balances),
//! 9. the **`Clock` sysvar** — `update_state` decodes `slot` + `unix_timestamp`
//!    here and threads them into `simulate_swap_in` so curves using
//!    `LoadNowSlot` / `LoadNowUnixSec` / `LoadQuotesTimestampSec` see the
//!    same numbers a real swap would. The sysvar is a well-known account
//!    so Titan's cache dedups it across all Quay venues — one fetch per
//!    slot, not per venue.
//!
//! Heavy decoding lives in `update_state`; `quote` only re-runs the VM.
//! Mirrors the Jupiter sibling crate's "decode in update, simulate in quote"
//! split.
//!
//! # Program id
//!
//! [`QUAY_PROGRAM_ID`] holds Quay's mainnet program id
//! (`QUayE6nexQWYNZAEqfN8FxoNwQDSu3CAzT2qq9J1ArG`). Routers constructing
//! `QuayVenue` via `FromAccount::from_account` read the actual program id
//! directly off `account.owner`, so the constant only matters for callers
//! that need `program_id()` *before* loading the Strategy (e.g. to filter
//! `getProgramAccounts` queries to Quay strategies).

use async_trait::async_trait;
use solana_instruction::Instruction;
use solana_pubkey::Pubkey;

use quay_sdk::consts::{ROUTE_TITAN, SWAP_LOADED_ACCOUNTS_DATA_SIZE_LIMIT};
use quay_sdk::pda;

use crate::account_caching::AccountsCache;
use crate::trading_venue::{
    error::TradingVenueError, protocol::PoolProtocol, token_info::TokenInfo, QuoteRequest,
    QuoteResult, TradingVenue,
};

mod creation;
mod quote;
mod state;
mod swap;

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test code: panic on assertion failure is the desired behavior"
)]
mod tests;

pub use creation::parse_pool_creations;

// ──────────────────────────────────────────────────────────────────────────
// Public constants
// ──────────────────────────────────────────────────────────────────────────

/// Quay's mainnet program id.
///
/// Sibling adapters (e.g. `quay-aggregator-jupiter`) re-export the same
/// constant. Routers constructing `QuayVenue` via
/// `FromAccount::from_account` see the program id directly off
/// `account.owner`; this constant exists for callers that need to
/// reference the program id without first loading a Strategy account
/// (e.g. building a `getProgramAccounts` filter).
pub const QUAY_PROGRAM_ID: Pubkey = solana_pubkey::pubkey!("QUayE6nexQWYNZAEqfN8FxoNwQDSu3CAzT2qq9J1ArG");

/// Recommended `set_loaded_accounts_data_size_limit` value for every swap
/// tx Titan composes against Quay. Mirrors
/// `aggregators/jupiter/src/lib.rs::QuayAmm::RECOMMENDED_LOADED_ACCOUNTS_DATA_SIZE_LIMIT`.
pub const RECOMMENDED_LOADED_ACCOUNTS_DATA_SIZE_LIMIT: u32 = SWAP_LOADED_ACCOUNTS_DATA_SIZE_LIMIT;

/// Account count the on-chain `swap` ix expects: eleven program-read
/// positional accounts (cfg + strategy + mm + quotes + 2 vaults + 2 taker
/// ATAs + taker + 2 mints), the Instructions sysvar at index 11, and one
/// trailing token-program slot the program skips but the runtime must
/// load for the transfer CPIs. A mixed token-program market appends a
/// second program — see the SDK's `push_token_programs` dedup.
pub const SWAP_ACCOUNTS_LEN: usize = 13;

/// Solana's `Clock` sysvar address. Fetched by `update_state` so curves see
/// the live slot + unix-time off the same `AccountsCache` batch Titan uses
/// for everything else — no upstream-trait change needed. Re-exported from
/// `solana-sdk-ids` so we don't carry a hard-coded pubkey literal.
pub const SYSVAR_CLOCK_ID: Pubkey = solana_sdk_ids::sysvar::clock::ID;

/// `Clock` sysvar layout: `slot` (u64 LE, 0..8) + `epoch_start_timestamp`
/// (i64 LE, 8..16) + `epoch` (u64 LE, 16..24) + `leader_schedule_epoch`
/// (u64 LE, 24..32) + `unix_timestamp` (i64 LE, 32..40). We only read slot
/// and unix_timestamp.
const CLOCK_SLOT_OFFSET: usize = 0;
const CLOCK_UNIX_TS_OFFSET: usize = 32;
const CLOCK_MIN_LEN: usize = CLOCK_UNIX_TS_OFFSET + 8;

/// Decode `(slot, unix_timestamp)` from a `Clock` sysvar account. Returns
/// `None` if the blob is shorter than the layout demands — `update_state`
/// surfaces that as a `DeserializationFailed`.
fn decode_clock(data: &[u8]) -> Option<(u64, i64)> {
    if data.len() < CLOCK_MIN_LEN {
        return None;
    }
    let mut slot_buf = [0u8; 8];
    slot_buf.copy_from_slice(&data[CLOCK_SLOT_OFFSET..CLOCK_SLOT_OFFSET + 8]);
    let mut ts_buf = [0u8; 8];
    ts_buf.copy_from_slice(&data[CLOCK_UNIX_TS_OFFSET..CLOCK_UNIX_TS_OFFSET + 8]);
    Some((u64::from_le_bytes(slot_buf), i64::from_le_bytes(ts_buf)))
}

// ──────────────────────────────────────────────────────────────────────────
// QuayVenue
// ──────────────────────────────────────────────────────────────────────────

/// A single Quay Strategy exposed as a Titan trading venue.
///
/// Constructed via `FromAccount::from_account` (passing the Strategy
/// `Account`) and then refreshed via `update_state` every slot. The four
/// raw account blobs (`strategy_data` / `mm_data` / `quotes_data` /
/// `global_config_data`) are kept in-struct because
/// `quay_sdk::simulate::simulate_swap_in` reads them as opaque `&[u8]`.
#[derive(Clone)]
pub struct QuayVenue {
    /// Quay program id. Pulled off `Strategy.account.owner` at construction
    /// time so the venue is robust against devnet/testnet deploys where
    /// the program lives under a different key than [`QUAY_PROGRAM_ID`].
    program_id: Pubkey,

    /// Strategy account pubkey. Returned by `market_id()`.
    strategy_key: Pubkey,
    strategy_data: Vec<u8>,

    /// MarketMaker the strategy is anchored to (PDA of `strategy.owner`).
    mm_key: Pubkey,
    mm_data: Vec<u8>,

    /// Strategy's bound quotes account (`strategy.quotes_account`).
    quotes_key: Pubkey,
    quotes_data: Vec<u8>,

    /// `GlobalConfig` PDA — cached at construction.
    global_config_key: Pubkey,
    global_config_data: Vec<u8>,

    base_mint: Pubkey,
    quote_mint: Pubkey,

    /// Vault PDAs (base / quote sides). The pricing VM no longer reads vault
    /// balances (the `LoadVault*` opcodes were removed in the DSL-v1
    /// redesign), so no vault data is cached — these keys exist only because
    /// the on-chain `swap` ix still takes the vaults as positional accounts,
    /// so they stay in `get_required_pubkeys_for_update` for the router's
    /// account-set / lookup-table machinery.
    vault_base_key: Pubkey,
    vault_quote_key: Pubkey,

    /// `[base, quote]` `TokenInfo` (mints + decimals + Token-2022 program
    /// detection). Populated by `update_state`; empty before the first call.
    tokens: Vec<TokenInfo>,

    /// `StrategyHeader.routing_flags` — the per-venue aggregator bitmask. The
    /// program stores it but enforces nothing; each adapter surfaces a strategy
    /// only when **its own** bit is set. This venue routes only when
    /// `routing_flags & ROUTE_TITAN != 0`, so an MM opts a curve into Titan
    /// explicitly. Defaults to `0` (not routed) before the first decode.
    routing_flags: u8,

    /// Cached halt / freeze bytes — same set the on-chain `execute_swap`
    /// enforces, mirroring `aggregators/jupiter`'s [`QuayAmm`]. Every flag
    /// must read 0 for `initialized()` to return true. Bytes are sourced
    /// from `GlobalConfig` / `MarketMakerHeader` / `StrategyHeader` headers
    /// at the end of each `update_state`.
    ///
    /// Default to `1` (active-halt) before the first `update_state` so the
    /// router refuses to quote against half-decoded state during warmup —
    /// same convention Jupiter uses.
    cfg_swap_halted: u8,
    cfg_protocol_halted: u8,
    strategy_frozen: u8,
    strategy_frozen_admin: u8,
    mm_frozen: u8,
    mm_frozen_admin: u8,
    mm_halted_admin: u8,

    /// Per-side price-probe input size — the `amount_in == 0` spot-probe size
    /// and the marginal-price finite-difference step (base when selling base,
    /// quote when buying). MM-set at market creation, re-read each
    /// `update_state`. Sourced from `StrategyHeader::price_probe_{base,quote}`.
    price_probe_base: u64,
    price_probe_quote: u64,

    /// Wall clock the venue threads into `simulate_swap_in`. Production source
    /// is the `Clock` sysvar, fetched alongside the strategy / mm / vault
    /// blobs in `update_state` (see [`SYSVAR_CLOCK_ID`]). Callers running
    /// outside the Titan pipeline — replay tests, off-line backtests —
    /// can override post-update via [`QuayVenue::with_clock`] or
    /// [`QuayVenue::set_clock`]. Defaults to `0` so a `QuayVenue` queried
    /// before its first `update_state` quotes as if the clock were unset.
    current_slot: u64,
    current_unix_sec: i64,
}

impl QuayVenue {
    /// Did `update_state` populate the dependent account blobs? Mirrors the
    /// Jupiter adapter's "lazy" model where construction caches the Strategy
    /// bytes but defers loading the rest to the first update.
    fn has_all_state(&self) -> bool {
        !self.mm_data.is_empty()
            && !self.quotes_data.is_empty()
            && !self.global_config_data.is_empty()
    }

    /// Halt-gate mirror — `true` only when every flag the on-chain
    /// `execute_swap` checks is clear. Used by both `initialized()` and the
    /// short-circuit at the top of `quote()`.
    fn halts_clear(&self) -> bool {
        self.cfg_swap_halted == 0
            && self.cfg_protocol_halted == 0
            && self.strategy_frozen == 0
            && self.strategy_frozen_admin == 0
            && self.mm_frozen == 0
            && self.mm_frozen_admin == 0
            && self.mm_halted_admin == 0
    }

    /// Override the wall clock the venue feeds into `simulate_swap_in`.
    /// Production routers don't need this — `update_state` fetches the
    /// `Clock` sysvar through Titan's `AccountsCache` and updates both
    /// fields every slot. Useful for replay tests / off-line backtests
    /// that want to pin the venue to a historical clock domain.
    #[must_use]
    pub fn with_clock(mut self, current_slot: u64, current_unix_sec: i64) -> Self {
        self.current_slot = current_slot;
        self.current_unix_sec = current_unix_sec;
        self
    }

    /// In-place variant of [`Self::with_clock`]. Note that the next
    /// `update_state` call will overwrite both fields from the sysvar — use
    /// this only when the next caller is `quote()` and you intend to bypass
    /// the live clock.
    pub fn set_clock(&mut self, current_slot: u64, current_unix_sec: i64) {
        self.current_slot = current_slot;
        self.current_unix_sec = current_unix_sec;
    }
}

// ──────────────────────────────────────────────────────────────────────────
// TradingVenue — thin dispatcher; heavy methods live in the submodules above.
// ──────────────────────────────────────────────────────────────────────────

#[async_trait]
impl TradingVenue for QuayVenue {
    fn initialized(&self) -> bool {
        // Three gates Titan's route planner uses to skip the venue:
        //   1. the MM opted this strategy into Titan (`routing_flags & ROUTE_TITAN`),
        //   2. all account blobs populated (post-first-update),
        //   3. on-chain halt / freeze set clear (the same flags the swap
        //      handler checks — see `onchain/program/src/instructions/swap.rs`).
        // Stateful curves are routed too: `quote()` prices them allocation-free
        // on a stack buffer (see `quote`), and the on-chain `min_amount_out`
        // guard bounds any quote/fill drift to a reverted route, not a loss.
        self.routing_flags & ROUTE_TITAN != 0
            && self.has_all_state()
            && self.halts_clear()
    }

    fn program_id(&self) -> Pubkey {
        self.program_id
    }

    fn program_dependencies(&self) -> Vec<Pubkey> {
        // Declare both token programs so Titan's address-lookup-table
        // machinery sees them as venue prerequisites. The on-chain `swap`
        // dispatches the right one at runtime from each mint's `owner`. The
        // system program is included because the on-chain handlers CPI into
        // it for vault-rent payments during admin ixs.
        vec![
            pda::SPL_TOKEN_PROGRAM_ID,
            pda::TOKEN_2022_PROGRAM_ID,
            // System program (`11111111111111111111111111111111`).
            solana_sdk_ids::system_program::ID,
        ]
    }

    fn market_id(&self) -> Pubkey {
        self.strategy_key
    }

    fn get_token_info(&self) -> &[TokenInfo] {
        &self.tokens
    }

    fn protocol(&self) -> PoolProtocol {
        PoolProtocol::Quay
    }

    fn get_required_pubkeys_for_update(&self) -> Result<Vec<Pubkey>, TradingVenueError> {
        self.required_update_pubkeys()
    }

    async fn update_state(&mut self, cache: &dyn AccountsCache) -> Result<(), TradingVenueError> {
        self.refresh_state(cache).await
    }

    fn quote(&self, request: QuoteRequest) -> Result<QuoteResult, TradingVenueError> {
        self.compute_quote(request)
    }

    fn generate_swap_instruction(
        &self,
        request: QuoteRequest,
        user: Pubkey,
    ) -> Result<Instruction, TradingVenueError> {
        self.build_swap_ix(request, user)
    }
}
