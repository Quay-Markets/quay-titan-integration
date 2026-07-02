//! Venue discovery — Titan's pool-creation parsing contract for Quay.

use crate::trading_venue::{
    protocol::PoolProtocol,
    venue_creation::{ParsedInstruction, PoolCreation},
};

use super::QUAY_PROGRAM_ID;

/// Discover Quay strategies from the `init_strategy` (`0x10`) instructions of
/// a confirmed transaction.
///
/// The strategy account is the "pool". `base_mint` / `quote_mint` sit at
/// positional accounts 5 and 6 of `init_strategy`
/// (`[strategy, mm, cfg, owner, system, base_mint, quote_mint, ...]`), so the
/// pair is read straight off the instruction with no account load. Malformed
/// instructions are skipped rather than erroring.
///
/// A freshly-created strategy is born frozen with `routing_flags == 0`, so it
/// is discovered here but not routed until its MM opts into Titan — the
/// `initialized()` gate handles that.
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
