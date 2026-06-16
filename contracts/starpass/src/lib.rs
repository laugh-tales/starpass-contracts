#![no_std]

use soroban_sdk::{contract, contractimpl, contracttype, token, Address, Env, String, Symbol, Vec};

// ============================================================
// Data Types
// ============================================================

/// Membership tier defined by a creator
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Tier {
    pub tier_id: u32,
    pub creator: Address,
    pub name: String,
    pub price: i128,     // price in stroops (USDC base units)
    pub duration: u64,   // duration in seconds
    pub max_supply: u32, // 0 = unlimited
    pub minted: u32,
    pub active: bool,
}

/// An access pass owned by a fan
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Pass {
    pub pass_id: u64,
    pub tier_id: u32,
    pub creator: Address,
    pub owner: Address,
    pub token: Address, // USDC token contract address
    pub purchased_at: u64,
    pub expires_at: u64,
    pub active: bool,
}

/// Creator profile registered on-chain
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Creator {
    pub address: Address,
    pub registered_at: u64,
    pub total_earned: i128,
    pub pass_count: u64,
}

/// Storage keys
#[contracttype]
pub enum DataKey {
    Admin,
    Token,          // USDC token address
    ProtocolFeeBps, // basis points e.g. 250 = 2.5%
    Creator(Address),
    Tier(u32), // tier_id -> Tier
    TierCount,
    Pass(u64), // pass_id -> Pass
    PassCount,
    CreatorBalance(Address), // unclaimed earnings per creator
    FanPasses(Address),      // fan address -> Vec<u64> pass IDs
    CreatorTiers(Address),   // creator address -> Vec<u32> tier IDs
}

// ============================================================
// Contract
// ============================================================

#[contract]
pub struct StarPassContract;

#[contractimpl]
impl StarPassContract {
    // --------------------------------------------------------
    // Admin / Initialization
    // --------------------------------------------------------

    /// Initialize the contract with admin, USDC token, and protocol fee
    pub fn initialize(env: Env, admin: Address, token: Address, fee_bps: u32) {
        admin.require_auth();
        assert!(fee_bps <= 1000, "Fee cannot exceed 10%");

        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::Token, &token);
        env.storage()
            .instance()
            .set(&DataKey::ProtocolFeeBps, &fee_bps);
        env.storage().instance().set(&DataKey::TierCount, &0u32);
        env.storage().instance().set(&DataKey::PassCount, &0u64);

        env.events()
            .publish((Symbol::new(&env, "initialized"),), (admin, token, fee_bps));
    }

    /// Update protocol fee (admin only)
    pub fn set_fee(env: Env, fee_bps: u32) {
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .expect("Not initialized");
        admin.require_auth();
        assert!(fee_bps <= 1000, "Fee cannot exceed 10%");
        env.storage()
            .instance()
            .set(&DataKey::ProtocolFeeBps, &fee_bps);
    }

    /// Withdraw accumulated protocol fees (admin only)
    pub fn withdraw_fees(env: Env, recipient: Address, amount: i128) {
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .expect("Not initialized");
        admin.require_auth();
        assert!(amount > 0, "Amount must be greater than zero");

        let token: Address = env
            .storage()
            .instance()
            .get(&DataKey::Token)
            .expect("Token not set");
        let token_client = token::Client::new(&env, &token);
        token_client.transfer(&env.current_contract_address(), &recipient, &amount);

        env.events()
            .publish((Symbol::new(&env, "fees_withdrawn"),), (recipient, amount));
    }

    // --------------------------------------------------------
    // Creator Registration
    // --------------------------------------------------------

    /// Register as a creator
    pub fn register_creator(env: Env, creator: Address) {
        creator.require_auth();
        assert!(
            !env.storage()
                .persistent()
                .has(&DataKey::Creator(creator.clone())),
            "Creator already registered"
        );

        let now = env.ledger().timestamp();
        let profile = Creator {
            address: creator.clone(),
            registered_at: now,
            total_earned: 0,
            pass_count: 0,
        };

        env.storage()
            .persistent()
            .set(&DataKey::Creator(creator.clone()), &profile);
        env.storage()
            .persistent()
            .set(&DataKey::CreatorBalance(creator.clone()), &0i128);
        env.storage().persistent().set(
            &DataKey::CreatorTiers(creator.clone()),
            &Vec::<u32>::new(&env),
        );

        env.events()
            .publish((Symbol::new(&env, "creator_registered"),), (creator, now));
    }

    // --------------------------------------------------------
    // Tier Management
    // --------------------------------------------------------

    /// Create a new membership tier
    pub fn create_tier(
        env: Env,
        creator: Address,
        name: String,
        price: i128,
        duration: u64,
        max_supply: u32,
    ) -> u32 {
        creator.require_auth();
        assert!(
            env.storage()
                .persistent()
                .has(&DataKey::Creator(creator.clone())),
            "Must register as creator first"
        );
        assert!(price > 0, "Price must be greater than zero");
        assert!(duration > 0, "Duration must be greater than zero");
        assert!(!name.is_empty(), "Name cannot be empty");

        let count: u32 = env
            .storage()
            .instance()
            .get(&DataKey::TierCount)
            .unwrap_or(0);
        let tier_id = count + 1;

        let tier = Tier {
            tier_id,
            creator: creator.clone(),
            name,
            price,
            duration,
            max_supply,
            minted: 0,
            active: true,
        };

        env.storage()
            .persistent()
            .set(&DataKey::Tier(tier_id), &tier);
        env.storage().instance().set(&DataKey::TierCount, &tier_id);

        // Add tier to creator's tier list
        let mut tiers: Vec<u32> = env
            .storage()
            .persistent()
            .get(&DataKey::CreatorTiers(creator.clone()))
            .unwrap_or(Vec::new(&env));
        tiers.push_back(tier_id);
        env.storage()
            .persistent()
            .set(&DataKey::CreatorTiers(creator.clone()), &tiers);

        env.events().publish(
            (Symbol::new(&env, "tier_created"),),
            (tier_id, creator, price, duration),
        );

        tier_id
    }

    /// Deactivate a tier (creator only)
    pub fn deactivate_tier(env: Env, creator: Address, tier_id: u32) {
        creator.require_auth();
        let mut tier: Tier = env
            .storage()
            .persistent()
            .get(&DataKey::Tier(tier_id))
            .expect("Tier not found");
        assert!(tier.creator == creator, "Not the tier creator");
        assert!(tier.active, "Tier already inactive");

        tier.active = false;
        env.storage()
            .persistent()
            .set(&DataKey::Tier(tier_id), &tier);

        env.events()
            .publish((Symbol::new(&env, "tier_deactivated"),), (tier_id, creator));
    }

    /// Update tier price (creator only, only affects future purchases)
    pub fn update_tier_price(env: Env, creator: Address, tier_id: u32, new_price: i128) {
        creator.require_auth();
        assert!(new_price > 0, "Price must be greater than zero");

        let mut tier: Tier = env
            .storage()
            .persistent()
            .get(&DataKey::Tier(tier_id))
            .expect("Tier not found");
        assert!(tier.creator == creator, "Not the tier creator");
        assert!(tier.active, "Tier is not active");

        let old_price = tier.price;
        tier.price = new_price;
        env.storage()
            .persistent()
            .set(&DataKey::Tier(tier_id), &tier);

        env.events().publish(
            (Symbol::new(&env, "tier_price_updated"),),
            (tier_id, old_price, new_price),
        );
    }

    // --------------------------------------------------------
    // Pass Minting (Fan purchases)
    // --------------------------------------------------------

    /// Mint a pass for a fan — fan pays USDC, creator receives funds minus protocol fee
    /// Mints a new access pass for a fan after collecting USDC payment.
    /// Splits payment between the creator and protocol fee.
    /// Returns the new pass ID.
    pub fn mint_pass(env: Env, fan: Address, tier_id: u32) -> u64 {
        fan.require_auth();

        let mut tier: Tier = env
            .storage()
            .persistent()
            .get(&DataKey::Tier(tier_id))
            .expect("Tier not found");

        assert!(tier.active, "Tier is not active");
        assert!(
            tier.max_supply == 0 || tier.minted < tier.max_supply,
            "Tier is sold out"
        );

        let token: Address = env
            .storage()
            .instance()
            .get(&DataKey::Token)
            .expect("Token not set");
        let fee_bps: u32 = env
            .storage()
            .instance()
            .get(&DataKey::ProtocolFeeBps)
            .unwrap_or(0);

        // Calculate fee split
        let protocol_fee = (tier.price * fee_bps as i128) / 10_000;
        let creator_amount = tier.price - protocol_fee;

        // Transfer full price from fan to contract
        let token_client = token::Client::new(&env, &token);
        token_client.transfer(&fan, &env.current_contract_address(), &tier.price);

        // Credit creator's balance
        let current_balance: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::CreatorBalance(tier.creator.clone()))
            .unwrap_or(0);
        env.storage().persistent().set(
            &DataKey::CreatorBalance(tier.creator.clone()),
            &(current_balance + creator_amount),
        );

        // Update creator profile
        let mut creator_profile: Creator = env
            .storage()
            .persistent()
            .get(&DataKey::Creator(tier.creator.clone()))
            .expect("Creator not found");
        creator_profile.total_earned += creator_amount;
        creator_profile.pass_count += 1;
        env.storage()
            .persistent()
            .set(&DataKey::Creator(tier.creator.clone()), &creator_profile);

        // Mint the pass
        let pass_count: u64 = env
            .storage()
            .instance()
            .get(&DataKey::PassCount)
            .unwrap_or(0);
        let pass_id = pass_count + 1;
        let now = env.ledger().timestamp();

        let pass = Pass {
            pass_id,
            tier_id,
            creator: tier.creator.clone(),
            owner: fan.clone(),
            token: token.clone(),
            purchased_at: now,
            expires_at: now + tier.duration,
            active: true,
        };

        env.storage()
            .persistent()
            .set(&DataKey::Pass(pass_id), &pass);
        env.storage().instance().set(&DataKey::PassCount, &pass_id);

        // Update tier minted count
        tier.minted += 1;
        env.storage()
            .persistent()
            .set(&DataKey::Tier(tier_id), &tier);

        // Add pass to fan's pass list
        let mut fan_passes: Vec<u64> = env
            .storage()
            .persistent()
            .get(&DataKey::FanPasses(fan.clone()))
            .unwrap_or(Vec::new(&env));
        fan_passes.push_back(pass_id);
        env.storage()
            .persistent()
            .set(&DataKey::FanPasses(fan.clone()), &fan_passes);

        env.events().publish(
            (Symbol::new(&env, "pass_minted"),),
            (pass_id, tier_id, fan, now + tier.duration),
        );

        pass_id
    }

    // --------------------------------------------------------
    // Creator Withdrawals
    // --------------------------------------------------------

    /// Creator withdraws their earned balance
    pub fn withdraw(env: Env, creator: Address) {
        creator.require_auth();

        let balance: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::CreatorBalance(creator.clone()))
            .unwrap_or(0);

        assert!(balance > 0, "No balance to withdraw");

        let token: Address = env
            .storage()
            .instance()
            .get(&DataKey::Token)
            .expect("Token not set");
        let token_client = token::Client::new(&env, &token);
        token_client.transfer(&env.current_contract_address(), &creator, &balance);

        // Reset balance
        env.storage()
            .persistent()
            .set(&DataKey::CreatorBalance(creator.clone()), &0i128);

        env.events()
            .publish((Symbol::new(&env, "creator_withdrew"),), (creator, balance));
    }

    // --------------------------------------------------------
    // Read / Query Functions
    // --------------------------------------------------------

    /// Check if a fan has a valid (non-expired) pass for a tier
    pub fn has_valid_pass(env: Env, fan: Address, tier_id: u32) -> bool {
        let fan_passes: Vec<u64> = match env.storage().persistent().get(&DataKey::FanPasses(fan)) {
            Some(p) => p,
            None => return false,
        };
        let now = env.ledger().timestamp();

        for pass_id in fan_passes.iter() {
            let pass: Pass = match env.storage().persistent().get(&DataKey::Pass(pass_id)) {
                Some(p) => p,
                None => continue,
            };
            if pass.tier_id == tier_id && pass.active && pass.expires_at > now {
                return true;
            }
        }

        false
    }

    /// Get pass details
    pub fn get_pass(env: Env, pass_id: u64) -> Pass {
        env.storage()
            .persistent()
            .get(&DataKey::Pass(pass_id))
            .expect("Pass not found")
    }

    /// Get tier details
    pub fn get_tier(env: Env, tier_id: u32) -> Tier {
        env.storage()
            .persistent()
            .get(&DataKey::Tier(tier_id))
            .expect("Tier not found")
    }

    /// Get creator profile
    pub fn get_creator(env: Env, creator: Address) -> Creator {
        env.storage()
            .persistent()
            .get(&DataKey::Creator(creator))
            .expect("Creator not found")
    }

    /// Get creator's pending balance
    pub fn get_creator_balance(env: Env, creator: Address) -> i128 {
        env.storage()
            .persistent()
            .get(&DataKey::CreatorBalance(creator))
            .unwrap_or(0)
    }

    /// Get total pass count
    pub fn get_pass_count(env: Env) -> u64 {
        env.storage()
            .instance()
            .get(&DataKey::PassCount)
            .unwrap_or(0)
    }

    /// Get total tier count
    pub fn get_tier_count(env: Env) -> u32 {
        env.storage()
            .instance()
            .get(&DataKey::TierCount)
            .unwrap_or(0)
    }

    /// Get all pass IDs owned by a fan
    pub fn get_fan_passes(env: Env, fan: Address) -> Vec<u64> {
        env.storage()
            .persistent()
            .get(&DataKey::FanPasses(fan))
            .unwrap_or(Vec::new(&env))
    }

    /// Get all tier IDs created by a creator
    pub fn get_creator_tiers(env: Env, creator: Address) -> Vec<u32> {
        env.storage()
            .persistent()
            .get(&DataKey::CreatorTiers(creator))
            .unwrap_or(Vec::new(&env))
    }
}

// ============================================================
// Tests
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;
    use soroban_sdk::{
        testutils::{Address as _, Ledger},
        token::StellarAssetClient,
        Address, Env, String,
    };

    fn setup_env() -> (Env, Address, Address, Address, Address, Address) {
        let env = Env::default();
        env.mock_all_auths();

        let admin = Address::generate(&env);
        let creator = Address::generate(&env);
        let fan = Address::generate(&env);

        let token_admin = Address::generate(&env);
        let token = env
            .register_stellar_asset_contract_v2(token_admin.clone())
            .address();

        StellarAssetClient::new(&env, &token).mint(&fan, &10_000_000);

        let contract_id = env.register_contract(None, StarPassContract);
        let client = StarPassContractClient::new(&env, &contract_id);
        client.initialize(&admin, &token, &250u32);

        (env, contract_id, admin, creator, fan, token)
    }

    #[test]
    fn test_initialize() {
        let env = Env::default();
        env.mock_all_auths();
        let admin = Address::generate(&env);
        let token_admin = Address::generate(&env);
        let token = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();

        let contract_id = env.register_contract(None, StarPassContract);
        let client = StarPassContractClient::new(&env, &contract_id);
        client.initialize(&admin, &token, &250u32);

        assert_eq!(client.get_pass_count(), 0);
        assert_eq!(client.get_tier_count(), 0);
    }

    #[test]
    fn test_register_creator() {
        let (env, contract_id, _admin, creator, _fan, _token) = setup_env();
        let client = StarPassContractClient::new(&env, &contract_id);

        client.register_creator(&creator);
        let profile = client.get_creator(&creator);

        assert_eq!(profile.address, creator);
        assert_eq!(profile.total_earned, 0);
        assert_eq!(profile.pass_count, 0);
    }

    #[test]
    fn test_create_tier() {
        let (env, contract_id, _admin, creator, _fan, _token) = setup_env();
        let client = StarPassContractClient::new(&env, &contract_id);
        client.register_creator(&creator);

        let tier_id = client.create_tier(
            &creator,
            &String::from_str(&env, "Gold"),
            &1_000_000i128,
            &2_592_000u64,
            &0u32,
        );

        assert_eq!(tier_id, 1);
        let tier = client.get_tier(&tier_id);
        assert_eq!(tier.price, 1_000_000);
        assert_eq!(tier.active, true);
        assert_eq!(tier.minted, 0);
    }

    #[test]
    fn test_mint_pass() {
        let (env, contract_id, _admin, creator, fan, _token) = setup_env();
        let client = StarPassContractClient::new(&env, &contract_id);
        client.register_creator(&creator);

        let tier_id = client.create_tier(
            &creator,
            &String::from_str(&env, "Silver"),
            &1_000_000i128,
            &2_592_000u64,
            &0u32,
        );

        let pass_id = client.mint_pass(&fan, &tier_id);
        assert_eq!(pass_id, 1);

        let pass = client.get_pass(&pass_id);
        assert_eq!(pass.owner, fan);
        assert_eq!(pass.tier_id, tier_id);
        assert_eq!(pass.active, true);
    }

    #[test]
    fn test_has_valid_pass() {
        let (env, contract_id, _admin, creator, fan, _token) = setup_env();
        env.ledger().set_timestamp(1000);
        let client = StarPassContractClient::new(&env, &contract_id);
        client.register_creator(&creator);

        let tier_id = client.create_tier(
            &creator,
            &String::from_str(&env, "Bronze"),
            &1_000_000i128,
            &2_592_000u64,
            &0u32,
        );

        assert_eq!(client.has_valid_pass(&fan, &tier_id), false);
        client.mint_pass(&fan, &tier_id);
        assert_eq!(client.has_valid_pass(&fan, &tier_id), true);
    }

    #[test]
    fn test_fee_split() {
        let (env, contract_id, _admin, creator, fan, _token) = setup_env();
        let client = StarPassContractClient::new(&env, &contract_id);
        client.register_creator(&creator);

        let tier_id = client.create_tier(
            &creator,
            &String::from_str(&env, "Gold"),
            &1_000_000i128,
            &2_592_000u64,
            &0u32,
        );

        client.mint_pass(&fan, &tier_id);
        let creator_balance = client.get_creator_balance(&creator);
        assert_eq!(creator_balance, 975_000);
    }

    #[test]
    fn test_creator_withdraw() {
        let (env, contract_id, _admin, creator, fan, _token) = setup_env();
        let client = StarPassContractClient::new(&env, &contract_id);
        client.register_creator(&creator);

        let tier_id = client.create_tier(
            &creator,
            &String::from_str(&env, "Gold"),
            &1_000_000i128,
            &2_592_000u64,
            &0u32,
        );

        client.mint_pass(&fan, &tier_id);
        assert_eq!(client.get_creator_balance(&creator), 975_000);

        client.withdraw(&creator);
        assert_eq!(client.get_creator_balance(&creator), 0);
    }

    #[test]
    fn test_max_supply_enforced() {
        let (env, contract_id, _admin, creator, fan, token) = setup_env();
        StellarAssetClient::new(&env, &token).mint(&fan, &100_000_000);
        let client = StarPassContractClient::new(&env, &contract_id);
        client.register_creator(&creator);

        let tier_id = client.create_tier(
            &creator,
            &String::from_str(&env, "Limited"),
            &1_000_000i128,
            &2_592_000u64,
            &1u32,
        );

        client.mint_pass(&fan, &tier_id);
        let result = client.try_mint_pass(&fan, &tier_id);
        assert!(result.is_err());
    }

    #[test]
    fn test_deactivate_tier() {
        let (env, contract_id, _admin, creator, fan, _token) = setup_env();
        let client = StarPassContractClient::new(&env, &contract_id);
        client.register_creator(&creator);

        let tier_id = client.create_tier(
            &creator,
            &String::from_str(&env, "Bronze"),
            &1_000_000i128,
            &2_592_000u64,
            &0u32,
        );

        client.deactivate_tier(&creator, &tier_id);
        let tier = client.get_tier(&tier_id);
        assert_eq!(tier.active, false);

        let result = client.try_mint_pass(&fan, &tier_id);
        assert!(result.is_err());
    }

    #[test]
    #[should_panic(expected = "Price must be greater than zero")]
    fn test_zero_price_rejected() {
        let (env, contract_id, _admin, creator, _fan, _token) = setup_env();
        let client = StarPassContractClient::new(&env, &contract_id);
        client.register_creator(&creator);

        client.create_tier(
            &creator,
            &String::from_str(&env, "Free"),
            &0i128,
            &2_592_000u64,
            &0u32,
        );
    }

    #[test]
    fn test_update_tier_price() {
        let (env, contract_id, _admin, creator, _fan, _token) = setup_env();
        let client = StarPassContractClient::new(&env, &contract_id);
        client.register_creator(&creator);

        let tier_id = client.create_tier(
            &creator,
            &String::from_str(&env, "Silver"),
            &1_000_000i128,
            &2_592_000u64,
            &0u32,
        );

        client.update_tier_price(&creator, &tier_id, &2_000_000i128);
        let tier = client.get_tier(&tier_id);
        assert_eq!(tier.price, 2_000_000);
    }

    #[test]
    fn test_expired_pass_returns_false() {
        let (env, contract_id, _admin, creator, fan, _token) = setup_env();
        let client = StarPassContractClient::new(&env, &contract_id);
        client.register_creator(&creator);

        // duration: 86_400 seconds (1 day)
        let duration = 86_400u64;
        let start = 1_000_000u64;

        let tier_id = client.create_tier(
            &creator,
            &String::from_str(&env, "Daily"),
            &1_000_000i128,
            &duration,
            &0u32,
        );

        // Mint pass at `start`; expires_at = start + duration
        env.ledger().set_timestamp(start);
        client.mint_pass(&fan, &tier_id);

        // Before expiry: pass should be valid
        assert_eq!(client.has_valid_pass(&fan, &tier_id), true);

        // One second past expiry: pass must be invalid
        env.ledger().set_timestamp(start + duration + 1);
        assert_eq!(client.has_valid_pass(&fan, &tier_id), false);
    }

    #[test]
    fn test_get_fan_passes() {
        let (env, contract_id, _admin, creator, fan, token) = setup_env();
        StellarAssetClient::new(&env, &token).mint(&fan, &100_000_000);
        let client = StarPassContractClient::new(&env, &contract_id);
        client.register_creator(&creator);

        let tier1 = client.create_tier(
            &creator,
            &String::from_str(&env, "Bronze"),
            &1_000_000i128,
            &2_592_000u64,
            &0u32,
        );
        let tier2 = client.create_tier(
            &creator,
            &String::from_str(&env, "Gold"),
            &2_000_000i128,
            &2_592_000u64,
            &0u32,
        );

        client.mint_pass(&fan, &tier1);
        client.mint_pass(&fan, &tier2);

        let passes = client.get_fan_passes(&fan);
        assert_eq!(passes.len(), 2);
    }
}
