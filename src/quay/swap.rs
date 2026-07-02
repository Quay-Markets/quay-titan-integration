//! Swap-instruction construction and address-lookup-table keys for
//! [`QuayVenue`].

use async_trait::async_trait;
use solana_instruction::Instruction;
use solana_pubkey::Pubkey;
use spl_associated_token_account::get_associated_token_address_with_program_id;

use quay_sdk::consts::{SIDE_BUY_BASE, SIDE_SELL_BASE};
use quay_sdk::ix;

use crate::account_caching::AccountsCache;
use crate::trading_venue::{
    error::TradingVenueError, token_info::TokenInfo, AddressLookupTableTrait, QuoteRequest,
    SwapType, TradingVenue,
};

use super::QuayVenue;

impl QuayVenue {
    /// Look up the token-program owner for one of the strategy's mints.
    fn token_program_for(&self, mint: &Pubkey) -> Option<Pubkey> {
        self.tokens
            .iter()
            .find(|t| t.pubkey == *mint)
            .map(TokenInfo::get_token_program)
    }

    /// Build Quay's on-chain `swap` ix for `request`. Backs
    /// `TradingVenue::generate_swap_instruction`.
    pub(super) fn build_swap_ix(
        &self,
        request: QuoteRequest,
        user: Pubkey,
    ) -> Result<Instruction, TradingVenueError> {
        if matches!(request.swap_type, SwapType::ExactOut) {
            return Err(TradingVenueError::ExactOutNotSupported);
        }

        let side = if request.input_mint == self.base_mint
            && request.output_mint == self.quote_mint
        {
            SIDE_SELL_BASE
        } else if request.input_mint == self.quote_mint && request.output_mint == self.base_mint {
            SIDE_BUY_BASE
        } else {
            return Err(TradingVenueError::InvalidMint(request.input_mint.into()));
        };

        // Taker ATAs are derived per-mint: Token-2022 ATAs live under a
        // different program id than SPL Token ATAs.
        let base_program = self
            .token_program_for(&self.base_mint)
            .ok_or_else(|| TradingVenueError::MissingState("base TokenInfo".into()))?;
        let quote_program = self
            .token_program_for(&self.quote_mint)
            .ok_or_else(|| TradingVenueError::MissingState("quote TokenInfo".into()))?;

        let taker_ata_base =
            get_associated_token_address_with_program_id(&user, &self.base_mint, &base_program);
        let taker_ata_quote =
            get_associated_token_address_with_program_id(&user, &self.quote_mint, &quote_program);

        // `min_amount_out` is 0: Titan enforces slippage at the route level,
        // and Quay's swap only checks it when set non-zero.
        let ix = ix::swap(
            &self.program_id,
            &self.strategy_key,
            &self.mm_key,
            &self.quotes_key,
            &self.base_mint,
            &self.quote_mint,
            &user,
            &taker_ata_base,
            &taker_ata_quote,
            &base_program,
            &quote_program,
            request.amount,
            0,
            side,
        );
        Ok(ix)
    }
}

#[async_trait]
impl AddressLookupTableTrait for QuayVenue {
    /// The stable accounts every swap against this venue touches — everything
    /// except the taker and its two ATAs, which vary by user. All keys are
    /// known up front, so the cache is never needed.
    async fn get_lookup_table_keys(
        &self,
        _cache: Option<&dyn AccountsCache>,
    ) -> Result<Vec<Pubkey>, TradingVenueError> {
        let mut keys = vec![
            self.program_id,
            self.global_config_key,
            self.strategy_key,
            self.mm_key,
            self.quotes_key,
            self.vault_base_key,
            self.vault_quote_key,
            self.base_mint,
            self.quote_mint,
            ix::INSTRUCTIONS_SYSVAR_ID,
        ];
        keys.extend(self.program_dependencies());
        Ok(keys)
    }
}
