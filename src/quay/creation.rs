//! Venue discovery — Titan's pool-creation parsing contract for Quay.

use crate::trading_venue::{
    protocol::PoolProtocol,
    venue_creation::{ParsedInstruction, PoolCreation},
};

use super::QUAY_PROGRAM_ID;

/// Discover Quay strategies from the `init_strategy` (`0x10`) instructions of a
/// confirmed transaction — Titan's pool-creation parsing contract.
///
/// The strategy account is the "pool"; `base_mint` / `quote_mint` are positional
/// accounts 5 and 6 on `init_strategy` (the program validates them against the
/// MarketMaker asset table), so the pair is read straight off the instruction
/// with no account load. Account order:
/// `[strategy, mm, cfg, owner, system, base_mint, quote_mint, (quotes)]`.
///
/// Each returned `pool` is then built into a venue via
/// [`QuayVenue::from_account`](super::QuayVenue) and gated by
/// [`TradingVenue::initialized`](crate::trading_venue::TradingVenue::initialized)
/// (`ROUTE_TITAN` + halt/freeze), so a freshly-created strategy — born frozen
/// with `routing_flags == 0` — is *discovered* but not *routed* until its MM
/// opts in. The parser is permissive: any short/malformed `init_strategy` is
/// skipped (the `get(..)` lookups return `None`).
///
/// Matches [`QUAY_PROGRAM_ID`] (Quay's mainnet deploy); a devnet/localnet
/// integration would parameterize the program id.
#[must_use]
pub fn parse_pool_creations(instructions: &[ParsedInstruction]) -> Vec<PoolCreation> {
    instructions
        .iter()
        .filter(|ix| {
            ix.program_id == QUAY_PROGRAM_ID
                && ix.data.first() == Some(&quay_sdk::consts::DISC_INIT_STRATEGY)
        })
        .filter_map(|ix| {
            Some(PoolCreation {
                protocol: PoolProtocol::Quay,
                pool: *ix.accounts.first()?,
                mints: vec![*ix.accounts.get(5)?, *ix.accounts.get(6)?],
            })
        })
        .collect()
}
