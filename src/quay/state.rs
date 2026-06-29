//! Account loading and per-slot state refresh for [`QuayVenue`].
//!
//! Construction (`FromAccount`) caches the Strategy bytes and derives the
//! dependent PDAs; `refresh_state` (the body of `TradingVenue::update_state`)
//! re-fetches the full account set every slot and decodes the halt/freeze
//! bytes, token metadata, and clock. `required_update_pubkeys` is the keyset
//! both depend on.

use solana_account::Account;
use solana_pubkey::Pubkey;

use quay_sdk::pda;
use quay_sdk::state::{GlobalConfig, MarketMakerHeader, StrategyHeader};

use crate::account_caching::AccountsCache;
use crate::trading_venue::{error::TradingVenueError, token_info::TokenInfo, FromAccount};

use super::{decode_clock, QuayVenue, SYSVAR_CLOCK_ID};

impl FromAccount for QuayVenue {
    fn from_account(pubkey: &Pubkey, account: &Account) -> Result<Self, TradingVenueError> {
        let strategy = StrategyHeader::try_from_account(&account.data).map_err(|e| {
            TradingVenueError::DeserializationFailed(format!("StrategyHeader: {e}").into())
        })?;

        // `account.owner` is the on-chain program id — read it directly
        // off the loaded Strategy so the venue works against any deploy
        // (mainnet, devnet, local validator). Same convention as the
        // sibling Jupiter adapter.
        let program_id = account.owner;
        let base_mint = Pubkey::new_from_array(strategy.base_mint);
        let quote_mint = Pubkey::new_from_array(strategy.quote_mint);
        let strategy_owner = Pubkey::new_from_array(strategy.owner);
        let quotes_key = Pubkey::new_from_array(strategy.quotes_account);

        let (mm_key, _) = pda::market_maker_pda(&program_id, &strategy_owner);
        let (global_config_key, _) = pda::global_config_pda(&program_id);
        let (vault_base_key, _) = pda::vault_pda(&program_id, &mm_key, &base_mint);
        let (vault_quote_key, _) = pda::vault_pda(&program_id, &mm_key, &quote_mint);

        Ok(Self {
            program_id,
            strategy_key: *pubkey,
            strategy_data: account.data.clone(),
            mm_key,
            mm_data: Vec::new(),
            quotes_key,
            quotes_data: Vec::new(),
            global_config_key,
            global_config_data: Vec::new(),
            base_mint,
            quote_mint,
            vault_base_key,
            vault_quote_key,
            tokens: Vec::new(),
            routing_flags: strategy.routing_flags,
            // Default to 1 (active-halt) so `initialized()` returns false
            // until the first `update_state` decodes real flag bytes.
            cfg_swap_halted: 1,
            cfg_protocol_halted: 1,
            // Strategy flags COULD be read off the bytes we already have, but
            // we warm up halted (1) like the MM / cfg defaults anyway. Seeding
            // their real values here would buy nothing: `initialized()` also
            // gates on `has_all_state()` (false until the first update), so the
            // venue can't route during warmup regardless, and `refresh_state`
            // re-decodes ALL these flags unconditionally on every call — there
            // is no change-detection, so an early-correct value is overwritten
            // on the first refresh. Keeping the symmetry keeps the gate
            // fail-closed and the block uniform.
            strategy_frozen: 1,
            strategy_frozen_admin: 1,
            mm_frozen: 1,
            mm_frozen_admin: 1,
            mm_halted_admin: 1,
            current_slot: 0,
            current_unix_sec: 0,
        })
    }
}

impl QuayVenue {
    /// The keyset `update_state` fetches each slot, in the order
    /// `refresh_state` decodes them. Backs `TradingVenue::get_required_pubkeys_for_update`.
    pub(super) fn required_update_pubkeys(&self) -> Result<Vec<Pubkey>, TradingVenueError> {
        // Trailing `SYSVAR_CLOCK_ID` is a well-known sysvar, so Titan's
        // batch fetcher dedups it across every Quay venue in the route
        // graph — one fetch per slot, not per venue.
        Ok(vec![
            self.strategy_key,
            self.mm_key,
            self.quotes_key,
            self.global_config_key,
            self.base_mint,
            self.quote_mint,
            self.vault_base_key,
            self.vault_quote_key,
            SYSVAR_CLOCK_ID,
        ])
    }

    /// Re-fetch the full account set and refresh cached blobs, halt/freeze
    /// bytes, token metadata, and clock. Backs `TradingVenue::update_state`.
    pub(super) async fn refresh_state(
        &mut self,
        cache: &dyn AccountsCache,
    ) -> Result<(), TradingVenueError> {
        let needed = self.required_update_pubkeys()?;
        let accounts = cache.get_accounts(&needed).await?;
        if accounts.len() != needed.len() {
            return Err(TradingVenueError::FailedToFetchMultipleAccountData);
        }

        // Slot 0 — Strategy. Re-decode so the frozen flags reflect the
        // latest state we'd see at swap time. Cache the two strategy halt
        // bytes so `initialized()` short-circuits without re-decoding.
        let strategy_account = accounts[0]
            .as_ref()
            .ok_or_else(|| TradingVenueError::NoAccountFound(self.strategy_key.into()))?;
        self.strategy_data = strategy_account.data.clone();
        let strategy = StrategyHeader::try_from_account(&self.strategy_data).map_err(|e| {
            TradingVenueError::DeserializationFailed(format!("StrategyHeader: {e}").into())
        })?;
        self.strategy_frozen = strategy.frozen;
        self.strategy_frozen_admin = strategy.frozen_admin;
        self.routing_flags = strategy.routing_flags;

        // Slot 1 — MarketMaker (asset table + admin halts).
        let mm_account = accounts[1]
            .as_ref()
            .ok_or_else(|| TradingVenueError::NoAccountFound(self.mm_key.into()))?;
        self.mm_data = mm_account.data.clone();
        let mm = MarketMakerHeader::try_from_account(&self.mm_data).map_err(|e| {
            TradingVenueError::DeserializationFailed(format!("MarketMakerHeader: {e}").into())
        })?;
        self.mm_frozen = mm.frozen;
        self.mm_frozen_admin = mm.frozen_admin;
        self.mm_halted_admin = mm.halted_admin;

        // Slot 2 — Quotes.
        let quotes_account = accounts[2]
            .as_ref()
            .ok_or_else(|| TradingVenueError::NoAccountFound(self.quotes_key.into()))?;
        self.quotes_data = quotes_account.data.clone();

        // Slot 3 — GlobalConfig. Cache the two cfg halt bytes alongside the
        // raw blob.
        let cfg_account = accounts[3]
            .as_ref()
            .ok_or_else(|| TradingVenueError::NoAccountFound(self.global_config_key.into()))?;
        self.global_config_data = cfg_account.data.clone();
        let cfg = GlobalConfig::try_from_account(&self.global_config_data).map_err(|e| {
            TradingVenueError::DeserializationFailed(format!("GlobalConfig: {e}").into())
        })?;
        self.cfg_swap_halted = cfg.swap_halted;
        self.cfg_protocol_halted = cfg.protocol_halted;

        // Slots 4 + 5 — base and quote mints. `TokenInfo::new` handles both
        // SPL Token and Token-2022 layouts (Token-2022 is a strict superset,
        // and `StateWithExtensions::unpack` accepts both). The epoch argument
        // is for epoch-indexed Token-2022 fees; we don't have a clock here
        // so we pass 0 — fine for routing, the on-chain swap picks up the
        // live value when it executes.
        let base_mint_account = accounts[4]
            .as_ref()
            .ok_or_else(|| TradingVenueError::NoAccountFound(self.base_mint.into()))?;
        let quote_mint_account = accounts[5]
            .as_ref()
            .ok_or_else(|| TradingVenueError::NoAccountFound(self.quote_mint.into()))?;
        let base_info = TokenInfo::new(&self.base_mint, base_mint_account, 0)?;
        let quote_info = TokenInfo::new(&self.quote_mint, quote_mint_account, 0)?;
        self.tokens = vec![base_info, quote_info];

        // Slots 6 + 7 — base and quote vaults. The VM no longer prices off
        // vault balances, so we don't cache their data; we only verify the
        // accounts exist, since the `swap` ix still needs them on-chain.
        if accounts[6].is_none() {
            return Err(TradingVenueError::NoAccountFound(self.vault_base_key.into()));
        }
        if accounts[7].is_none() {
            return Err(TradingVenueError::NoAccountFound(self.vault_quote_key.into()));
        }

        // Slot 8 — `Clock` sysvar. Replaces the `with_clock` / `set_clock`
        // path as the production source for `current_slot` /
        // `current_unix_sec`. Freshness budget is one batch (≈1 slot, the
        // same as every other account here). Callers that want a different
        // clock domain (replay tests, off-line backtests) can still
        // override post-update via `set_clock`.
        let clock_account = accounts[8]
            .as_ref()
            .ok_or_else(|| TradingVenueError::NoAccountFound(SYSVAR_CLOCK_ID.into()))?;
        let (slot, unix_sec) = decode_clock(&clock_account.data).ok_or_else(|| {
            TradingVenueError::DeserializationFailed("Clock sysvar too short".into())
        })?;
        self.current_slot = slot;
        self.current_unix_sec = unix_sec;

        Ok(())
    }
}
