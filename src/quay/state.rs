//! Account loading and per-slot state refresh for [`QuayVenue`].

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

        // `account.owner` is the on-chain program id, so the venue works
        // against any deploy of the program.
        let program_id = account.owner;
        let base_mint = Pubkey::new_from_array(strategy.base_mint);
        let quote_mint = Pubkey::new_from_array(strategy.quote_mint);
        let strategy_owner = Pubkey::new_from_array(strategy.owner);
        let quotes_key = Pubkey::new_from_array(strategy.quotes_account);

        let (mm_key, _) = pda::market_maker_pda(&program_id, &strategy_owner);
        let (global_config_key, _) = pda::global_config_pda(&program_id);
        let (vault_base_key, _) = pda::vault_pda(&program_id, &mm_key, &base_mint);
        let (vault_quote_key, _) = pda::vault_pda(&program_id, &mm_key, &quote_mint);
        let ext_keys: Vec<Pubkey> = strategy
            .ext_account_keys(&account.data)
            .map_err(|e| {
                TradingVenueError::DeserializationFailed(format!("ext binding: {e}").into())
            })?
            .iter()
            .map(|k| Pubkey::new_from_array(*k))
            .collect();

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
            ext_keys,
            // Fetched on the first update; the venue quotes only once
            // `ext_data` covers the binding.
            ext_data: Vec::new(),
            tokens: Vec::new(),
            routing_flags: strategy.routing_flags,
            // Halt bytes start at 1 so `initialized()` stays false until the
            // first `update_state` decodes the real on-chain flags.
            cfg_swap_halted: 1,
            cfg_protocol_halted: 1,
            strategy_frozen: 1,
            strategy_frozen_admin: 1,
            mm_frozen: 1,
            mm_frozen_admin: 1,
            mm_halted_admin: 1,
            price_probe_base: strategy.price_probe_base,
            price_probe_quote: strategy.price_probe_quote,
            current_slot: 0,
            current_unix_sec: 0,
        })
    }
}

impl QuayVenue {
    /// The keyset `update_state` fetches each slot, in the order
    /// `refresh_state` decodes them. Backs
    /// `TradingVenue::get_required_pubkeys_for_update`.
    pub(super) fn required_update_pubkeys(&self) -> Result<Vec<Pubkey>, TradingVenueError> {
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
        ]
        .into_iter()
        .chain(self.ext_keys.iter().copied())
        .collect())
    }

    /// Re-fetch the full account set and refresh cached blobs, halt/freeze
    /// bytes, token metadata, and clock. Backs `TradingVenue::update_state`.
    ///
    /// The refresh is atomic over `self`: every slot decodes into locals and
    /// the venue is only written once all nine have resolved. A failure
    /// part-way through must leave the previous state — including the
    /// construction-time warmup halts — untouched, otherwise a failed first
    /// update could publish real (clear) halt bytes while the tokens or the
    /// clock were still defaults, and `initialized()` would report true
    /// against half-decoded state.
    pub(super) async fn refresh_state(
        &mut self,
        cache: &dyn AccountsCache,
    ) -> Result<(), TradingVenueError> {
        let needed = self.required_update_pubkeys()?;
        let accounts = cache.get_accounts(&needed).await?;
        if accounts.len() != needed.len() {
            return Err(TradingVenueError::FailedToFetchMultipleAccountData);
        }

        // Slot 0 — Strategy: frozen flags, routing flags, probe sizes.
        let strategy_account = accounts[0]
            .as_ref()
            .ok_or_else(|| TradingVenueError::NoAccountFound(self.strategy_key.into()))?;
        let strategy = *StrategyHeader::try_from_account(&strategy_account.data).map_err(|e| {
            TradingVenueError::DeserializationFailed(format!("StrategyHeader: {e}").into())
        })?;

        // Slot 1 — MarketMaker: asset table + admin halts.
        let mm_account = accounts[1]
            .as_ref()
            .ok_or_else(|| TradingVenueError::NoAccountFound(self.mm_key.into()))?;
        let mm = *MarketMakerHeader::try_from_account(&mm_account.data).map_err(|e| {
            TradingVenueError::DeserializationFailed(format!("MarketMakerHeader: {e}").into())
        })?;

        // Slot 2 — Quotes.
        let quotes_account = accounts[2]
            .as_ref()
            .ok_or_else(|| TradingVenueError::NoAccountFound(self.quotes_key.into()))?;

        // Slot 3 — GlobalConfig: protocol-wide halts.
        let cfg_account = accounts[3]
            .as_ref()
            .ok_or_else(|| TradingVenueError::NoAccountFound(self.global_config_key.into()))?;
        let cfg = *GlobalConfig::try_from_account(&cfg_account.data).map_err(|e| {
            TradingVenueError::DeserializationFailed(format!("GlobalConfig: {e}").into())
        })?;

        // Slots 4 + 5 — mints. `TokenInfo::new` handles both SPL Token and
        // Token-2022 layouts. Epoch 0 is fine here: it only selects the
        // epoch-indexed Token-2022 fee, which routing doesn't use.
        let base_mint_account = accounts[4]
            .as_ref()
            .ok_or_else(|| TradingVenueError::NoAccountFound(self.base_mint.into()))?;
        let quote_mint_account = accounts[5]
            .as_ref()
            .ok_or_else(|| TradingVenueError::NoAccountFound(self.quote_mint.into()))?;
        let base_info = TokenInfo::new(&self.base_mint, base_mint_account, 0)?;
        let quote_info = TokenInfo::new(&self.quote_mint, quote_mint_account, 0)?;

        // Slots 6 + 7 — vaults. The VM doesn't price off vault balances, so
        // only their existence is checked; the swap ix still needs them.
        if accounts[6].is_none() {
            return Err(TradingVenueError::NoAccountFound(self.vault_base_key.into()));
        }
        if accounts[7].is_none() {
            return Err(TradingVenueError::NoAccountFound(self.vault_quote_key.into()));
        }

        // Slot 8 — Clock sysvar, the source for the slot/time the simulator
        // sees. Freshness budget is one batch, the same as everything else.
        let clock_account = accounts[8]
            .as_ref()
            .ok_or_else(|| TradingVenueError::NoAccountFound(SYSVAR_CLOCK_ID.into()))?;
        let (slot, unix_sec) = decode_clock(&clock_account.data).ok_or_else(|| {
            TradingVenueError::DeserializationFailed("Clock sysvar too short".into())
        })?;

        // Slots 9.. — the bound external accounts, fetched under the
        // PREVIOUS binding. If the fresh strategy data rebound the keys, or
        // any account is missing, publish an empty `ext_data` — the venue
        // goes dark for one cycle instead of quoting off mis-paired data.
        let fresh_ext_keys: Vec<Pubkey> = strategy
            .ext_account_keys(&strategy_account.data)
            .map_err(|e| {
                TradingVenueError::DeserializationFailed(format!("ext binding: {e}").into())
            })?
            .iter()
            .map(|k| Pubkey::new_from_array(*k))
            .collect();
        let mut ext_data = Vec::with_capacity(fresh_ext_keys.len());
        if fresh_ext_keys == self.ext_keys {
            for (i, key) in fresh_ext_keys.iter().enumerate() {
                match accounts.get(9 + i).and_then(|a| a.as_ref()) {
                    Some(acc) => ext_data.push(acc.data.clone()),
                    None => {
                        let _ = key;
                        ext_data.clear();
                        break;
                    }
                }
            }
            if ext_data.len() != fresh_ext_keys.len() {
                ext_data.clear();
            }
        }

        // Every slot resolved — commit the new snapshot.
        self.strategy_data = strategy_account.data.clone();
        self.ext_keys = fresh_ext_keys;
        self.ext_data = ext_data;
        self.strategy_frozen = strategy.frozen;
        self.strategy_frozen_admin = strategy.frozen_admin;
        self.routing_flags = strategy.routing_flags;
        self.price_probe_base = strategy.price_probe_base;
        self.price_probe_quote = strategy.price_probe_quote;
        self.mm_data = mm_account.data.clone();
        self.mm_frozen = mm.frozen;
        self.mm_frozen_admin = mm.frozen_admin;
        self.mm_halted_admin = mm.halted_admin;
        self.quotes_data = quotes_account.data.clone();
        self.global_config_data = cfg_account.data.clone();
        self.cfg_swap_halted = cfg.swap_halted;
        self.cfg_protocol_halted = cfg.protocol_halted;
        self.tokens = vec![base_info, quote_info];
        self.current_slot = slot;
        self.current_unix_sec = unix_sec;

        Ok(())
    }
}
