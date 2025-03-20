/**
 * Raydium DEX Pool Implementation
 * 
 * This module implements the pool operations interface for Raydium DEX pools.
 * Raydium is a Solana-based automated market maker (AMM) that provides liquidity
 * to the Serum order book, enabling better price discovery and deeper liquidity.
 */

use std::collections::HashMap;
use std::fmt::Debug;
use serde::{Deserialize, Serialize};
use solana_sdk::account::Account;
use crate::serialize::token::{Token, WrappedPubkey, unpack_token_account};
use crate::serialize::pool::JSONFeeStructure;
use crate::pool::PoolOperations;

use anchor_client::solana_sdk::pubkey::Pubkey;
use anchor_client::Cluster;
use anchor_client::Program;

use solana_sdk::instruction::Instruction;

use tmp::accounts as tmp_accounts;
use tmp::instruction as tmp_ix;

use crate::pool_utils::base::CurveType;
use crate::utils::{str2pubkey, derive_token_address};
use crate::pool_utils::{
    raydium::{get_pool_quote_with_amounts},
    fees::Fees,
};
use crate::constants::*;

/// Represents a Raydium liquidity pool with its associated accounts and parameters
#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct RaydiumPool {
    /// Pool's program address
    pub address: WrappedPubkey,
    /// Pool's nonce for PDA derivation
    pub nonce: u64,
    /// Pool authority PDA
    pub authority: WrappedPubkey,
    /// LP token mint address
    pub lp_token_mint: WrappedPubkey,
    /// Account that collects fees
    pub fee_account: WrappedPubkey,
    /// List of token IDs in the pool
    pub token_ids: Vec<String>,
    /// Map of token data keyed by token ID
    pub tokens: HashMap<String, Token>,
    /// Fee structure for the pool
    pub fee_structure: JSONFeeStructure,
    /// Current token amounts in the pool (set at runtime)
    #[serde(default)]
    pub pool_amounts: HashMap<String, u128>,
}

/// Implementation of pool operations for Raydium DEX
impl PoolOperations for RaydiumPool {
    /// Creates swap instructions for executing a trade
    /// 
    /// # Arguments
    /// * `program` - The Anchor program instance
    /// * `owner` - The owner's public key
    /// * `mint_in` - Input token mint
    /// * `mint_out` - Output token mint
    /// 
    /// # Returns
    /// * Vector of instructions for executing the swap
    fn swap_ix(&self, 
        program: &Program,
        owner: &Pubkey,
        mint_in: &Pubkey, 
        mint_out: &Pubkey
    ) -> Vec<Instruction> {
        // Derive swap state PDA
        let (swap_state, _) = Pubkey::find_program_address(
            &[b"swap_state"], 
            &program.id()
        );

        // Derive user token accounts
        let user_src = derive_token_address(owner, mint_in);
        let user_dst = derive_token_address(owner, mint_out); 

        // Derive pool authority PDA
        let (authority_pda, _) = Pubkey::find_program_address(
            &[&self.address.to_bytes()],
            &RAYDIUM_PROGRAM_ID 
        );

        // Get pool token accounts
        let pool_src = self.mint_2_addr(mint_in);
        let pool_dst = self.mint_2_addr(mint_out);

        // Build swap instruction
        let swap_ix = program
            .request()
            .accounts(tmp_accounts::RaydiumSwap {
                token_swap: self.address.0, 
                authority: authority_pda,
                user_transfer_authority: *owner,
                user_src,
                pool_src,
                user_dst,
                pool_dst,
                lp_mint: self.lp_token_mint.0,
                fee_account: self.fee_account.0,
                token_program: *TOKEN_PROGRAM_ID,
                swap_program: *RAYDIUM_PROGRAM_ID,
                swap_state,
            })
            .args(tmp_ix::RaydiumSwap { })
            .instructions()
            .unwrap();

        swap_ix
    }

    /// Calculates the expected output amount for a given input amount
    /// 
    /// # Arguments
    /// * `scaled_amount_in` - Input amount scaled to proper decimals
    /// * `mint_in` - Input token mint
    /// * `mint_out` - Output token mint
    /// 
    /// # Returns
    /// * Expected output amount
    fn get_quote_with_amounts_scaled(
        &self, 
        scaled_amount_in: u128, 
        mint_in: &Pubkey,
        mint_out: &Pubkey,
    ) -> u128 {
        // Get current pool amounts
        let pool_src_amount = self.pool_amounts.get(&mint_in.to_string()).unwrap();
        let pool_dst_amount = self.pool_amounts.get(&mint_out.to_string()).unwrap();

        // Set up fee structure
        let trader_fee = &self.fee_structure.trader_fee;
        let fees = Fees {
            trade_fee_numerator: trader_fee.numerator,
            trade_fee_denominator: trader_fee.denominator,
            owner_trade_fee_numerator: 0,
            owner_trade_fee_denominator: 0,
            owner_withdraw_fee_numerator: 0,
            owner_withdraw_fee_denominator: 0,
            host_fee_numerator: 0,
            host_fee_denominator: 0,
        };

        // Calculate quote using Raydium's constant product formula
        get_pool_quote_with_amounts(
            scaled_amount_in,
            ctype,
            self.amp, 
            &fees, 
            *pool_src_amount, 
            *pool_dst_amount, 
            None,
        ).unwrap()
    }

    /// Returns a list of account public keys that need to be updated
    fn get_update_accounts(&self) -> Vec<Pubkey> {
        // Get pool vault accounts for all tokens
        self.get_mints()
            .iter()
            .map(|mint| self.mint_2_addr(mint))
            .collect()        
    }

    /// Checks if trading is possible between two tokens
    /// 
    /// # Arguments
    /// * `_mint_in` - Input token mint
    /// * `_mint_out` - Output token mint
    /// 
    /// # Returns
    /// * Boolean indicating if trading is possible
    fn can_trade(&self, 
        _mint_in: &Pubkey,
        _mint_out: &Pubkey
    ) -> bool {
        // Check if any pool has zero liquidity
        for amount in self.pool_amounts.values() {
            if *amount == 0 { return false; }
        }
        true
    }

    /// Updates the pool's token amounts with new account data
    /// 
    /// # Arguments
    /// * `accounts` - Vector of optional accounts
    /// * `_cluster` - The Solana cluster being used
    fn set_update_accounts(&mut self, accounts: Vec<Option<Account>>, _cluster: Cluster) { 
        // Get token IDs
        let ids: Vec<String> = self
            .get_mints()
            .iter()
            .map(|mint| mint.to_string())
            .collect();
        
        // Extract token amounts from account data
        let acc_data0 = &accounts[0].as_ref().unwrap().data;
        let acc_data1 = &accounts[1].as_ref().unwrap().data;

        let amount0 = unpack_token_account(acc_data0).amount as u128;
        let amount1 = unpack_token_account(acc_data1).amount as u128;

        // Update pool amounts
        self.pool_amounts.insert(ids[0].clone(), amount0);
        self.pool_amounts.insert(ids[1].clone(), amount1);
    }

    /// Returns the name of the DEX
    fn get_name(&self) -> String {
        "Raydium".to_string()
    }

    /// Returns the token account address for a given mint
    fn mint_2_addr(&self, mint: &Pubkey) -> Pubkey {
        let token = self.tokens.get(&mint.to_string()).unwrap();
        token.addr.0
    }

    /// Returns the decimal scale for a given mint
    fn mint_2_scale(&self, mint: &Pubkey) -> u64 {
        let token = self.tokens.get(&mint.to_string()).unwrap();
        token.scale
    }

    /// Returns a sorted vector of the pool's token mint addresses
    fn get_mints(&self) -> Vec<Pubkey> {
        let mut mints: Vec<Pubkey> = self.token_ids
            .iter()
            .map(|k| str2pubkey(k))
            .collect();
        // Sort for consistent ordering across pools
        mints.sort();
        mints
    }
}
