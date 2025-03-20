/**
 * Serum DEX Pool Implementation
 * 
 * This module implements the pool operations interface for Serum DEX markets.
 * Serum is a Solana-based decentralized exchange that uses an order book model
 * rather than an automated market maker (AMM) model. This implementation handles:
 * 1. Order book state management
 * 2. Price discovery through order matching
 * 3. Trade execution and settlement
 */

use core::panic;

use std::collections::HashMap;
use std::fmt::Debug;
use serde;
use serde::{Deserialize, Serialize};
use crate::pool::PoolOperations;
use crate::serialize::token::{WrappedPubkey};

use crate::utils::{derive_token_address}; 

use solana_sdk::pubkey::Pubkey;

use anchor_spl::dex::serum_dex::{
    critbit::{SlabView},
    matching::OrderBookState,
    state::Market,
};
use std::ops::DerefMut;


use anchor_client::{Program, Cluster};
use solana_sdk::instruction::Instruction;

use solana_sdk::clock::Epoch;
use solana_sdk::account::Account;
use solana_sdk::account_info::AccountInfo;
use crate::constants::*;
use crate::pool_utils::serum::*;

use anchor_spl::dex::serum_dex::{
    matching::Side,
};

use std::str::FromStr;
use tmp::accounts as tmp_accounts;
use tmp::instruction as tmp_instructions;

/// Represents a Serum market with its associated accounts and parameters
#[derive(Deserialize, Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct SerumPool {
    /// Market's program address
    pub own_address: WrappedPubkey,
    /// Base token mint address
    pub base_mint: WrappedPubkey,
    /// Quote token mint address
    pub quote_mint: WrappedPubkey,
    /// Base token decimal places
    pub base_scale: u64,
    /// Quote token decimal places
    pub quote_scale: u64,
    /// Base token vault address
    pub base_vault: WrappedPubkey,
    /// Quote token vault address
    pub quote_vault: WrappedPubkey,
    /// Request queue account address
    pub request_queue: WrappedPubkey,
    /// Event queue account address
    pub event_queue: WrappedPubkey,
    /// Bids account address
    pub bids: WrappedPubkey,
    /// Asks account address
    pub asks: WrappedPubkey,
    /// Vault signer PDA address
    pub vault_signer: WrappedPubkey,
    /// Taker fee percentage
    pub taker_fee_pct: f64,
    /// Market accounts (loaded at runtime)
    #[serde(skip)]
    pub accounts: Option<Vec<Option<Account>>>,
    /// Open orders map (loaded at runtime)
    #[serde(skip)]
    pub open_orders: Option<HashMap<String, String>>
}

/// Creates an AccountInfo structure from a Pubkey and Account
/// 
/// # Arguments
/// * `pk` - The public key of the account
/// * `account` - The account data
/// 
/// # Returns
/// * AccountInfo structure initialized with the provided data
fn account_info<'a>(pk: &'a Pubkey, account: &'a mut Account) -> AccountInfo<'a> {
    AccountInfo::new(
        pk, 
        false, 
        true, 
        &mut account.lamports, 
        &mut account.data, 
        &account.owner,
        false, 
        Epoch::default(),
    )
}

/// Structure to track order matching iteration state
struct Iteration { 
    /// Remaining input amount
    amount_in: u64, 
    /// Accumulated output amount
    amount_out: u64,
}

/// Process a bid order (quote -> base) against the order book
/// 
/// # Arguments
/// * `iteration` - Current iteration state
/// * `fee_tier` - Fee tier for the trader
/// * `ob` - Mutable reference to the order book state
/// 
/// # Returns
/// * Boolean indicating if order matching is complete
fn bid_iteration(
    iteration: &mut Iteration,
    fee_tier: &FeeTier, 
    ob: &mut OrderBookState,
) -> bool {
    let quote_lot_size = ob.market_state.pc_lot_size;
    let base_lot_size = ob.market_state.coin_lot_size;

    let start_amount_in = iteration.amount_in;
    let max_pc_qty = fee_tier.remove_taker_fee(iteration.amount_in) / quote_lot_size;
    let mut pc_qty_remaining = max_pc_qty; 

    // Match against asks until complete or no more matching orders
    let done = loop {
        let flag = match ob.asks.find_min() { // min = best ask 
            Some(_) => false, 
            None => true
        };
        if flag { break true; }
        let best_ask = ob.asks.find_min().unwrap(); 
        let best_offer_ref = ob.asks.get_mut(best_ask).unwrap().as_leaf_mut().unwrap();
     
        let trade_price = best_offer_ref.price();
        let offer_size = best_offer_ref.quantity();
        let trade_qty = offer_size
            .min(pc_qty_remaining / best_offer_ref.price().get());

        if trade_qty == 0 { // No more matching possible
            break true;
        }

        // Update amounts
        pc_qty_remaining -= trade_qty * trade_price.get();
        iteration.amount_out += trade_qty * base_lot_size; 

        // Update order book
        best_offer_ref.set_quantity(best_offer_ref.quantity() - trade_qty);

        if best_offer_ref.quantity() == 0 {
            let best_offer_id = best_offer_ref.order_id();
            ob.asks.remove_by_key(best_offer_id)
                .unwrap();
        }
        break false; 
    };

    // Calculate final amounts including fees
    let native_accum_fill_price = (max_pc_qty - pc_qty_remaining) * quote_lot_size;
    let native_taker_fee = fee_tier.taker_fee(native_accum_fill_price);
    let native_pc_qty_remaining =
        start_amount_in - native_accum_fill_price - native_taker_fee;
    iteration.amount_in = native_pc_qty_remaining; 

    done
}

/// Process an ask order (base -> quote) against the order book
/// 
/// # Arguments
/// * `iteration` - Current iteration state
/// * `fee_tier` - Fee tier for the trader
/// * `ob` - Mutable reference to the order book state
/// 
/// # Returns
/// * Boolean indicating if order matching is complete
fn ask_iteration(
    iteration: &mut Iteration,
    fee_tier: &FeeTier, 
    ob: &mut OrderBookState,
) -> bool {
    let pc_lot_size = ob.market_state.pc_lot_size;
    let coin_lot_size = ob.market_state.coin_lot_size;

    let max_qty = iteration.amount_in; 
    let mut unfilled_qty = max_qty / coin_lot_size;
    let mut accum_fill_price = 0;

    // Match against bids until complete or no more matching orders
    let done = loop {
        let best_bid = match ob.bids.find_max() { 
            Some(best_bid) => {
                best_bid
            }, 
            None => {
                break true; // No more bids
            }
        };
        let best_bid_ref = ob.bids.get_mut(best_bid).unwrap().as_leaf_mut().unwrap();
     
        let trade_price = best_bid_ref.price();
        let bid_size = best_bid_ref.quantity();
        let trade_qty = bid_size.min(unfilled_qty);

        if trade_qty == 0 { // No more matching possible
            break true;
        }

        // Update amounts
        best_bid_ref.set_quantity(best_bid_ref.quantity() - trade_qty);
        unfilled_qty -= trade_qty;
        accum_fill_price += trade_qty * trade_price.get();

        // Update order book
        if best_bid_ref.quantity() == 0 {
            let best_offer_id = best_bid_ref.order_id();
            ob.bids.remove_by_key(best_offer_id)
                .unwrap();
        }
        break false; 
    };

    // Calculate final amounts including fees
    let native_taker_pc_qty = accum_fill_price * pc_lot_size;
    let native_taker_fee = fee_tier.taker_fee(native_taker_pc_qty);
    let net_taker_pc_qty = native_taker_pc_qty - native_taker_fee;

    iteration.amount_out += net_taker_pc_qty;
    iteration.amount_in = unfilled_qty * coin_lot_size; 

    done
}

/// Implementation of pool operations for Serum DEX
impl PoolOperations for SerumPool {
    /// Returns the name of the DEX
    fn get_name(&self) -> String {
        "Serum".to_string()
    }

    /// Returns a list of account public keys that need to be updated
    fn get_update_accounts(&self) -> Vec<Pubkey> {
        vec![
            self.own_address.0, 
            self.bids.0, 
            self.asks.0,
        ]
    }

    /// Updates the market's accounts with new account data
    /// 
    /// # Arguments
    /// * `accounts` - Vector of optional accounts
    /// * `cluster` - The Solana cluster being used
    fn set_update_accounts(
        &mut self, 
        accounts: Vec<Option<Account>>,
        cluster: Cluster,
    ) {
        self.accounts = Some(accounts);
        
        // Load open orders from file based on cluster
        let oo_path = match cluster { 
            Cluster::Localnet => {
                "./serum_open_orders.json"
            }, 
            Cluster::Mainnet => {
                panic!("TODO"); // Not implemented for mainnet
                "./serum_open_orders.json"
            },
            _ => panic!("cluster {} not supported", cluster)
        };
        let oo_str = std::fs::read_to_string(oo_path).unwrap();
        let oo_book: HashMap<String, String> = serde_json::from_str(&oo_str).unwrap();
        self.open_orders = Some(oo_book); 
    }

    /// Returns the token account address for a given mint (Not implemented)
    fn mint_2_addr(&self, _mint: &Pubkey) -> Pubkey {
        panic!("Function not implemented")
    }

    /// Returns a sorted vector of the market's token mint addresses
    fn get_mints(&self) -> Vec<Pubkey> {
        let mut mints = vec![
            self.base_mint.0,
            self.quote_mint.0
        ];
        mints.sort();
        mints
    }

    /// Returns the decimal scale for a given mint
    fn mint_2_scale(&self, mint: &Pubkey) -> u64 {
        if *mint == self.base_mint.0 {
            self.base_scale
        } else if *mint == self.quote_mint.0 { 
            self.quote_scale
        } else { 
            panic!("Invalid mint provided")
        }
    }

    /// Calculates the expected output amount for a given input amount
    /// 
    /// # Arguments
    /// * `amount_in` - Input amount scaled to proper decimals
    /// * `mint_in` - Input token mint
    /// * `_mint_out` - Output token mint
    /// 
    /// # Returns
    /// * Expected output amount
    fn get_quote_with_amounts_scaled(
        &self, 
        amount_in: u128, 
        mint_in: &Pubkey,
        _mint_out: &Pubkey,
    ) -> u128 {
        // Get market accounts
        let market_acc = &self.accounts.as_ref().unwrap()[0].as_ref().unwrap();
        let bids_acc = &self.accounts.as_ref().unwrap()[1].as_ref().unwrap();
        let asks_acc = &self.accounts.as_ref().unwrap()[2].as_ref().unwrap();

        // Create account infos
        let mut market_acc = market_acc.clone();
        let mut bids_acc = bids_acc.clone();
        let mut asks_acc = asks_acc.clone();

        let market_acc_info = account_info(&self.own_address.0, &mut market_acc);
        let bids_acc_info = account_info(&self.bids.0, &mut bids_acc);
        let asks_acc_info = account_info(&self.asks.0, &mut asks_acc);

        // Load market state
        let market = Market::load(&market_acc_info, &SERUM_PROGRAM_ID).unwrap();
        let bids = market.load_bids_mut(&bids_acc_info).unwrap();
        let asks = market.load_asks_mut(&asks_acc_info).unwrap();

        // Set up order book state
        let mut orderbook = OrderBookState {
            bids: bids.deref_mut(),
            asks: asks.deref_mut(),
            market_state: market.deref_mut(),
        };

        // Set up iteration state
        let mut iteration = Iteration {
            amount_in: amount_in as u64,
            amount_out: 0,
        };

        // Process order based on direction
        let is_bid = *mint_in == self.quote_mint.0;
        let fee_tier = FeeTier::default();

        // Match orders until complete
        loop {
            let done = if is_bid {
                bid_iteration(&mut iteration, &fee_tier, &mut orderbook)
            } else {
                ask_iteration(&mut iteration, &fee_tier, &mut orderbook)
            };
            if done { break; }
        }

        iteration.amount_out as u128
    }

    /// Creates swap instructions for executing a trade
    /// 
    /// # Arguments
    /// * `program` - The Anchor program instance
    /// * `owner` - The owner's public key
    /// * `mint_in` - Input token mint
    /// * `_mint_out` - Output token mint
    /// 
    /// # Returns
    /// * Vector of instructions for executing the swap
    fn swap_ix(&self, 
        program: &Program,
        owner: &Pubkey,
        mint_in: &Pubkey, 
        _mint_out: &Pubkey
    ) -> Vec<Instruction> {
        // Determine trade direction
        let is_bid = *mint_in == self.quote_mint.0;
        let side = if is_bid { Side::Bid } else { Side::Ask };

        // Get open orders account
        let open_orders = self.open_orders
            .as_ref()
            .unwrap()
            .get(&owner.to_string())
            .unwrap();
        let open_orders = Pubkey::from_str(open_orders).unwrap();

        // Derive token accounts
        let user_src = derive_token_address(owner, mint_in);
        let user_dst = derive_token_address(
            owner, 
            if is_bid { &self.base_mint.0 } else { &self.quote_mint.0 }
        );

        // Build swap instruction
        let swap_ix = program
            .request()
            .accounts(tmp_accounts::SerumSwap {
                market: self.own_address.0,
                open_orders,
                request_queue: self.request_queue.0,
                event_queue: self.event_queue.0,
                bids: self.bids.0,
                asks: self.asks.0,
                base_vault: self.base_vault.0,
                quote_vault: self.quote_vault.0,
                vault_signer: self.vault_signer.0,
                user_src,
                user_dst,
                user_owner: *owner,
                token_program: *TOKEN_PROGRAM_ID,
                serum_program: *SERUM_PROGRAM_ID,
            })
            .args(tmp_instructions::SerumSwap { 
                side,
            })
            .instructions()
            .unwrap();

        swap_ix
    }

    /// Checks if trading is possible between two tokens
    /// 
    /// # Arguments
    /// * `mint_in` - Input token mint
    /// * `_mint_out` - Output token mint
    /// 
    /// # Returns
    /// * Boolean indicating if trading is possible
    fn can_trade(&self, 
        mint_in: &Pubkey,
        _mint_out: &Pubkey
    ) -> bool {
        // Get market accounts
        let market_acc = &self.accounts.as_ref().unwrap()[0].as_ref().unwrap();
        let bids_acc = &self.accounts.as_ref().unwrap()[1].as_ref().unwrap();
        let asks_acc = &self.accounts.as_ref().unwrap()[2].as_ref().unwrap();

        // Create account infos
        let mut market_acc = market_acc.clone();
        let mut bids_acc = bids_acc.clone();
        let mut asks_acc = asks_acc.clone();

        let market_acc_info = account_info(&self.own_address.0, &mut market_acc);
        let bids_acc_info = account_info(&self.bids.0, &mut bids_acc);
        let asks_acc_info = account_info(&self.asks.0, &mut asks_acc);

        // Load market state
        let market = Market::load(&market_acc_info, &SERUM_PROGRAM_ID).unwrap();
        let bids = market.load_bids_mut(&bids_acc_info).unwrap();
        let asks = market.load_asks_mut(&asks_acc_info).unwrap();

        // Check if there are matching orders
        let is_bid = *mint_in == self.quote_mint.0;
        if is_bid {
            asks.find_min().is_some()
        } else {
            bids.find_max().is_some()
        }
    }
}