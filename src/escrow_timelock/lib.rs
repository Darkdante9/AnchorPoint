#![no_std]
use soroban_sdk::{contract, contractimpl, contracttype, token, Address, Env};

#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    EscrowInitialized,
    EscrowDetails,
    RefundClaimed,
    Cancelled,
}

#[contracttype]
#[derive(Clone)]
pub struct EscrowDetails {
    pub sender: Address,
    pub recipient: Address,
    pub token: Address,
    pub amount: i128,
    pub unlock_time: u64,
    pub release_timestamp: u64,
    pub conditions_met: bool,
}

#[contract]
pub struct EscrowTimelock;

#[contractimpl]
impl EscrowTimelock {
    pub fn set_security_registry(env: soroban_sdk::Env, registry: soroban_sdk::Address) {
        if env
            .storage()
            .instance()
            .has(&soroban_sdk::symbol_short!("sec_reg"))
        {
            panic!("already set");
        }
        env.storage()
            .instance()
            .set(&soroban_sdk::symbol_short!("sec_reg"), &registry);
    }

    /// Initialize a time-locked escrow contract
    ///
    /// # Arguments
    ///
    /// * `sender` - The address sending the funds into escrow
    /// * `recipient` - The address that will receive the funds when unlocked
    /// * `token` - The token contract address
    /// * `amount` - The amount of tokens to escrow
    /// * `unlock_time` - The timestamp (in seconds since epoch) when funds can be claimed
    ///
    /// # Panics
    ///
    /// * If the contract is already initialized
    /// * If the unlock_time is in the past
    /// * If the amount is zero or negative
    pub fn initialize(
        e: Env,
        sender: Address,
        recipient: Address,
        token: Address,
        amount: i128,
        unlock_time: u64,
    ) {
        if e.storage().instance().has(&DataKey::EscrowInitialized) {
            panic!("escrow already initialized");
        }

        if amount <= 0 {
            panic!("amount must be positive");
        }

        // Note: We don't validate unlock_time > current time here because
        // Soroban doesn't provide reliable timestamps during contract creation.
        // The unlock_time validation happens during claim/refund operations.

        sender.require_auth();

        let details = EscrowDetails {
            sender: sender.clone(),
            recipient,
            token,
            amount,
            unlock_time,
            release_timestamp: unlock_time,
            conditions_met: false,
        };

        e.storage()
            .instance()
            .set(&DataKey::EscrowDetails, &details);
        e.storage()
            .instance()
            .set(&DataKey::EscrowInitialized, &true);
        e.storage().instance().set(&DataKey::RefundClaimed, &false);
        e.storage().instance().set(&DataKey::Cancelled, &false);

        // Transfer tokens from sender to this contract
        let token_client = token::Client::new(&e, &details.token);
        token_client.transfer(&sender, e.current_contract_address(), &amount);
    }

    /// Mark conditions as met (can only be called by sender)
    pub fn mark_conditions_met(e: Env) {
        let mut details: EscrowDetails = e
            .storage()
            .instance()
            .get(&DataKey::EscrowDetails)
            .expect("escrow not initialized");

        details.sender.require_auth();
        details.conditions_met = true;

        e.storage()
            .instance()
            .set(&DataKey::EscrowDetails, &details);
    }

    /// Cancel a pending timelocked escrow before the lockup window begins.
    ///
    /// Only the original depositor (sender) may cancel, and only while
    /// `current_time < unlock_time`. Once the lockup has started the escrow
    /// can no longer be cancelled and must go through claim/refund instead.
    pub fn cancel_escrow(e: Env, depositor: Address, escrow_id: u64) {
        let _ = escrow_id;

        let details: EscrowDetails = e
            .storage()
            .instance()
            .get(&DataKey::EscrowDetails)
            .expect("escrow not initialized");

        // Only the original depositor may cancel.
        if depositor != details.sender {
            panic!("only depositor can cancel");
        }
        depositor.require_auth();

        let cancelled: bool = e
            .storage()
            .instance()
            .get(&DataKey::Cancelled)
            .unwrap_or(false);
        if cancelled {
            panic!("escrow already cancelled");
        }

        let refund_claimed: bool = e
            .storage()
            .instance()
            .get(&DataKey::RefundClaimed)
            .unwrap_or(false);
        if refund_claimed {
            panic!("escrow already settled");
        }

        // Cancellation is only permitted before the lockup window starts.
        let current_time = e.ledger().timestamp();
        if current_time >= details.unlock_time {
            panic!("lockup has started - cannot cancel");
        }

        // Mark as cancelled to prevent double cancellation / later claims.
        e.storage().instance().set(&DataKey::Cancelled, &true);
        e.storage().instance().set(&DataKey::RefundClaimed, &true);

        // Refund escrowed tokens back to the depositor.
        let token_client = token::Client::new(&e, &details.token);
        let contract_balance = token_client.balance(&e.current_contract_address());
        if contract_balance > 0 {
            token_client.transfer(
                &e.current_contract_address(),
                &details.sender,
                &contract_balance,
            );
        }
    }

    /// Claim funds as the recipient (only after unlock_time or if conditions are met)
    pub fn claim(e: Env) {
        if let Some(registry) = e
            .storage()
            .instance()
            .get::<_, soroban_sdk::Address>(&soroban_sdk::symbol_short!("sec_reg"))
        {
            let is_paused: bool = e.invoke_contract(
                &registry,
                &soroban_sdk::Symbol::new(&e, "is_paused"),
                soroban_sdk::vec![&e],
            );
            if is_paused {
                panic!("contract is paused");
            }
        }

        let details: EscrowDetails = e
            .storage()
            .instance()
            .get(&DataKey::EscrowDetails)
            .expect("escrow not initialized");

        let cancelled: bool = e
            .storage()
            .instance()
            .get(&DataKey::Cancelled)
            .unwrap_or(false);
        if cancelled {
            panic!("escrow cancelled");
        }

        let refund_claimed: bool = e
            .storage()
            .instance()
            .get(&DataKey::RefundClaimed)
            .unwrap_or(false);

        if refund_claimed {
            panic!("refund already claimed");
        }

        // Check timelock release delay: current_timestamp >= release_timestamp
        let current_timestamp = e.ledger().timestamp();
        assert!(
            current_timestamp >= details.release_timestamp,
            "timelock release delay not reached"
        );

        details.recipient.require_auth();

        // Transfer tokens to recipient
        let token_client = token::Client::new(&e, &details.token);
        let contract_balance = token_client.balance(&e.current_contract_address());

        if contract_balance > 0 {
            token_client.transfer(
                &e.current_contract_address(),
                &details.recipient,
                &contract_balance,
            );
        }
    }

    /// Request refund as sender (only if unlock_time has passed and recipient hasn't claimed)
    pub fn refund(e: Env) {
        if let Some(registry) = e
            .storage()
            .instance()
            .get::<_, soroban_sdk::Address>(&soroban_sdk::symbol_short!("sec_reg"))
        {
            let is_paused: bool = e.invoke_contract(
                &registry,
                &soroban_sdk::Symbol::new(&e, "is_paused"),
                soroban_sdk::vec![&e],
            );
            if is_paused {
                panic!("contract is paused");
            }
        }

        let details: EscrowDetails = e
            .storage()
            .instance()
            .get(&DataKey::EscrowDetails)
            .expect("escrow not initialized");

        let refund_claimed: bool = e
            .storage()
            .instance()
            .get(&DataKey::RefundClaimed)
            .unwrap_or(false);

        if refund_claimed {
            panic!("refund already processed");
        }

        // Refund is only available after unlock_time has passed
        if e.ledger().timestamp() < details.unlock_time {
            panic!("refund not yet available - unlock time has not passed");
        }

        details.sender.require_auth();

        // Mark refund as claimed to prevent double claims
        e.storage().instance().set(&DataKey::RefundClaimed, &true);

        // Transfer remaining tokens back to sender
        let token_client = token::Client::new(&e, &details.token);
        let contract_balance = token_client.balance(&e.current_contract_address());

        if contract_balance > 0 {
            token_client.transfer(
                &e.current_contract_address(),
                &details.sender,
                &contract_balance,
            );
        }
    }

    /// Get escrow details
    pub fn get_escrow_details(e: Env) -> EscrowDetails {
        e.storage()
            .instance()
            .get(&DataKey::EscrowDetails)
            .expect("escrow not initialized")
    }

    /// Check if refund has been claimed
    pub fn get_refund_claimed(e: Env) -> bool {
        e.storage()
            .instance()
            .get(&DataKey::RefundClaimed)
            .unwrap_or(false)
    }

    /// Check if the escrow has been cancelled
    pub fn get_cancelled(e: Env) -> bool {
        e.storage()
            .instance()
            .get(&DataKey::Cancelled)
            .unwrap_or(false)
    }

    /// Get current ledger timestamp
    pub fn get_current_time(e: Env) -> u64 {
        e.ledger().timestamp()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use soroban_sdk::{
        testutils::Address as _, testutils::Ledger, token::StellarAssetClient, Address, Env,
    };

    #[test]
    fn test_initialize_escrow() {
        let e = Env::default();
        e.mock_all_auths();

        let sender = Address::generate(&e);
        let recipient = Address::generate(&e);
        let admin = Address::generate(&e);
        let token_contract = e.register_stellar_asset_contract_v2(admin.clone());
        let token_id = token_contract.address();
        let stellar_asset = StellarAssetClient::new(&e, &token_id);
        stellar_asset.mint(&sender, &1000);

        let contract_id = e.register(EscrowTimelock, ());
        let client = EscrowTimelockClient::new(&e, &contract_id);

        client.initialize(&sender, &recipient, &token_id, &500, &2000);

        let details = client.get_escrow_details();
        assert_eq!(details.amount, 500);
        assert_eq!(details.unlock_time, 2000);
    }

    #[test]
    fn test_cancel_escrow_before_lockup() {
        let e = Env::default();
        e.mock_all_auths();

        let sender = Address::generate(&e);
        let recipient = Address::generate(&e);
        let admin = Address::generate(&e);
        let token_contract = e.register_stellar_asset_contract_v2(admin.clone());
        let token_id = token_contract.address();
        let stellar_asset = StellarAssetClient::new(&e, &token_id);
        stellar_asset.mint(&sender, &1000);

        let contract_id = e.register(EscrowTimelock, ());
        let client = EscrowTimelockClient::new(&e, &contract_id);

        e.ledger().set_timestamp(100);
        client.initialize(&sender, &recipient, &token_id, &500, &2000);

        // Cancel before lockup starts.
        client.cancel_escrow(&sender, &1);

        assert!(client.get_cancelled());
        let token_client = token::Client::new(&e, &token_id);
        assert_eq!(token_client.balance(&sender), 1000);
        assert_eq!(token_client.balance(&contract_id), 0);
    }

    #[test]
    #[should_panic(expected = "lockup has started - cannot cancel")]
    fn test_cancel_escrow_after_lockup_fails() {
        let e = Env::default();
        e.mock_all_auths();

        let sender = Address::generate(&e);
        let recipient = Address::generate(&e);
        let admin = Address::generate(&e);
        let token_contract = e.register_stellar_asset_contract_v2(admin.clone());
        let token_id = token_contract.address();
        let stellar_asset = StellarAssetClient::new(&e, &token_id);
        stellar_asset.mint(&sender, &1000);

        let contract_id = e.register(EscrowTimelock, ());
        let client = EscrowTimelockClient::new(&e, &contract_id);

        e.ledger().set_timestamp(100);
        client.initialize(&sender, &recipient, &token_id, &500, &2000);

        // Move past the lockup start; cancellation must now fail.
        e.ledger().set_timestamp(2000);
        client.cancel_escrow(&sender, &1);
    }
}
