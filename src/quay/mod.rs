//! Titan `TradingVenue` adapter for the Quay program.
//!
//! Each `QuayVenue` is one Quay Strategy — one DSL-priced curve on one
//! `(base_mint, quote_mint)` pair. Titan's router holds one venue per active
//! strategy. Construction and the per-slot refresh live in `state`, pricing
//! in `quote`, swap-instruction building in `swap`, and pool discovery in
//! `creation`.

use async_trait::async_trait;
use solana_instruction::Instruction;
use solana_pubkey::Pubkey;

use quay_sdk::consts::ROUTE_TITAN;
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

/// Quay's mainnet program id.
///
/// `from_account` reads the live program id off `account.owner`, so this
/// constant is only for callers that need the id before loading a Strategy
/// (e.g. to filter a `getProgramAccounts` query).
pub const QUAY_PROGRAM_ID: Pubkey =
    solana_pubkey::pubkey!("QUayE6nexQWYNZAEqfN8FxoNwQDSu3CAzT2qq9J1ArG");

/// Titan's mainnet router program id — the top-level program a routed swap
/// executes under. Needed for accurate quoting.
pub const TITAN_ROUTER_ID: Pubkey =
    solana_pubkey::pubkey!("T1TANpTeScyeqVzzgNViGDNrkQ6qHz9KrSBS4aNXvGT");

/// Solana's `Clock` sysvar. Fetched in `update_state` so curves using
/// `LoadNowSlot` / `LoadNowUnixSec` see the same clock a real swap would.
/// It is a well-known account, so Titan's cache dedups it across venues.
const SYSVAR_CLOCK_ID: Pubkey = solana_sdk_ids::sysvar::clock::ID;

/// `Clock` sysvar layout: `slot` (u64 LE) at byte 0, `unix_timestamp`
/// (i64 LE) at byte 32.
const CLOCK_SLOT_OFFSET: usize = 0;
const CLOCK_UNIX_TS_OFFSET: usize = 32;
const CLOCK_MIN_LEN: usize = CLOCK_UNIX_TS_OFFSET + 8;

/// Decode `(slot, unix_timestamp)` from a `Clock` sysvar account.
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

/// A single Quay Strategy exposed as a Titan trading venue.
///
/// Built with `FromAccount::from_account` from the Strategy account, then
/// refreshed with `update_state` every slot. The account blobs are kept raw
/// because `quay_sdk::simulate::simulate_swap_in` reads them as opaque
/// `&[u8]`.
#[derive(Clone)]
pub struct QuayVenue {
    /// Read off `Strategy.account.owner`, so the venue also works against
    /// devnet/localnet deploys of the program.
    program_id: Pubkey,

    strategy_key: Pubkey,
    strategy_data: Vec<u8>,

    /// MarketMaker the strategy is anchored to (PDA of `strategy.owner`).
    mm_key: Pubkey,
    mm_data: Vec<u8>,

    /// The strategy's bound quotes account (`strategy.quotes_account`).
    quotes_key: Pubkey,
    quotes_data: Vec<u8>,

    global_config_key: Pubkey,
    global_config_data: Vec<u8>,

    base_mint: Pubkey,
    quote_mint: Pubkey,

    /// Vault PDAs. The pricing VM does not read vault balances; the keys are
    /// kept because the on-chain `swap` ix takes the vaults as positional
    /// accounts, so they belong in the update/lookup-table key sets.
    vault_base_key: Pubkey,
    vault_quote_key: Pubkey,

    /// `[base, quote]` token metadata. Populated by `update_state`; empty
    /// before the first call.
    tokens: Vec<TokenInfo>,

    /// `StrategyHeader.routing_flags`. The program stores the bitmask but
    /// enforces nothing; this venue surfaces the strategy only when
    /// `routing_flags & ROUTE_TITAN != 0`, so an MM opts in explicitly.
    routing_flags: u8,

    /// Halt / freeze bytes — the same set the on-chain swap handler checks.
    /// All must be 0 for `initialized()` to return true. Seeded to 1 at
    /// construction so the venue refuses to quote until the first
    /// `update_state` has decoded the real on-chain bytes.
    cfg_swap_halted: u8,
    cfg_protocol_halted: u8,
    strategy_frozen: u8,
    strategy_frozen_admin: u8,
    mm_frozen: u8,
    mm_frozen_admin: u8,
    mm_halted_admin: u8,

    /// Per-side price-probe input size, set by the MM on the strategy: the
    /// probe size for `amount == 0` spot quotes and the finite-difference
    /// step for the marginal price.
    price_probe_base: u64,
    price_probe_quote: u64,

    /// From the `Clock` sysvar, refreshed each `update_state`; zero until
    /// the first update.
    current_slot: u64,
    current_unix_sec: i64,
}

impl QuayVenue {
    /// Whether `update_state` has populated the dependent account blobs.
    fn has_all_state(&self) -> bool {
        !self.mm_data.is_empty()
            && !self.quotes_data.is_empty()
            && !self.global_config_data.is_empty()
    }

    /// True only when every halt / freeze byte the on-chain swap checks is
    /// clear. Used by `initialized()` and the gate at the top of `quote()`.
    fn halts_clear(&self) -> bool {
        self.cfg_swap_halted == 0
            && self.cfg_protocol_halted == 0
            && self.strategy_frozen == 0
            && self.strategy_frozen_admin == 0
            && self.mm_frozen == 0
            && self.mm_frozen_admin == 0
            && self.mm_halted_admin == 0
    }
}

#[async_trait]
impl TradingVenue for QuayVenue {
    fn initialized(&self) -> bool {
        // Routable once the MM opted the strategy into Titan, the first
        // update has landed, and the on-chain halt / freeze set is clear.
        self.routing_flags & ROUTE_TITAN != 0 && self.has_all_state() && self.halts_clear()
    }

    fn program_id(&self) -> Pubkey {
        self.program_id
    }

    fn program_dependencies(&self) -> Vec<Pubkey> {
        // Both token programs (the swap dispatches per mint at runtime) and
        // the system program (vault-rent CPIs in admin instructions).
        vec![
            pda::SPL_TOKEN_PROGRAM_ID,
            pda::TOKEN_2022_PROGRAM_ID,
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
