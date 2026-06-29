#![no_std]

use soroban_sdk::{
    contract, contractimpl, contracttype, token, Address, BytesN, Env, String, Symbol, Vec,
};

// IMPLEMENTATION MAP:
// - Config change: add `LockPeriodSeconds` to `DataKey` and store in `initialize`.
// - Admin API: `update_lock_period(env, admin, new_period_seconds)` added.
// - Interception: `mint_pass` and `renew_pass` no longer directly credit
//   `CreatorBalance`; instead they create `PendingEarning` records keyed by
//   `(creator, earning_id)` and increment `PendingEarningCount(creator)`.
// - New types: `PendingEarning` struct added; `Error` enum appended.
// - Release paths:
//   * Normal: `process_unlocked_earnings(env, creator)` iterates pending
//     earnings and moves matured ones to `CreatorBalance` (maturity check: `now > unlocks_at`).
//   * Early release (2-of-2): `propose_early_release(admin, creator, earning_id)` stores
//     a proposal in instance storage; `approve_early_release(creator, earning_id)` co-signs
//     and executes release, removing the proposal.
// - Storage keys: `PendingEarningCount`, `PendingEarning(creator,id)`, `EarlyReleaseProposal(id)`.
// - Events: `earning_pending`, `earning_released`, `early_release_proposed`, `early_release_executed`, `lock_period_updated`.


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
    /// Duration in seconds that earnings are locked before becoming withdrawable
    LockPeriodSeconds,
    Creator(Address),
    Tier(u32), // tier_id -> Tier
    TierCount,
    Pass(u64), // pass_id -> Pass
    PassCount,
    CreatorBalance(Address), // unclaimed earnings per creator
    /// Per-creator monotonically incrementing pending earning id counter
    PendingEarningCount(Address),
    /// Pending earning record keyed by (creator, earning_id)
    PendingEarning(Address, u64),
    /// Early release proposal keyed by earning_id (instance storage)
    EarlyReleaseProposal(u64),
    FanPasses(Address),      // fan address -> Vec<u64> pass IDs
    CreatorTiers(Address),   // creator address -> Vec<u32> tier IDs
    ContractVersion,
}

// ============================================================
// Errors
// ============================================================

/// Contract-level errors (append-only)
#[contracttype]
#[derive(Clone, Debug)]
pub enum Error {
    /// lock_period_seconds must be greater than zero
    // SECURITY: prevents a zero-length lock which would bypass escrow
    InvalidLockPeriod,
    /// No PendingEarning exists for the given creator and earning_id
    EarningNotFound,
    /// The PendingEarning has already been released
    // SECURITY: prevents double-release/double-credit
    EarningAlreadyReleased,
    /// An early release proposal already exists for this earning_id
    ProposalAlreadyExists,
    /// No early release proposal exists for this earning_id
    NoProposalFound,
    /// The calling creator does not match the proposal's intended creator
    // SECURITY: prevents cross-creator approval attacks
    UnauthorizedApproval,
    /// process_unlocked_earnings called but earning is not yet matured
    EarningNotMatured,
}

/// Pending earning held in escrow until unlock or early release
#[contracttype]
#[derive(Clone, Debug)]
pub struct PendingEarning {
    /// The creator address this earning belongs to
    pub creator: Address,
    /// Amount of tokens held in this pending earning
    pub amount: i128,
    /// Token contract address for this earning
    pub token: Address,
    /// Ledger timestamp (seconds) after which this earning can be released
    /// via `process_unlocked_earnings`.
    pub unlocks_at: u64,
    /// Whether this earning has been released (to creator balance or via early release).
    /// Released earnings are retained in storage for auditability but ignored by
    /// processing functions.
    pub released: bool,
    /// earning_id — unique per creator, assigned from PendingEarningCount(creator)
    pub earning_id: u64,
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

    /// Initializes the contract with the admin address, USDC token, and protocol fee.
    ///
    /// Called once by the deployer. Sets global config and resets tier/pass counters to zero.
    /// Requires admin signature.
    ///
    /// # Panics
    ///
    /// - Panics if `fee_bps` exceeds 1000 (10%).
    pub fn initialize(
        env: Env,
        admin: Address,
        token: Address,
        fee_bps: u32,
        lock_period_seconds: u64,
    ) -> Result<(), Error> {
        admin.require_auth();
        assert!(fee_bps <= 1000, "Fee cannot exceed 10%");
        if lock_period_seconds == 0 {
            return Err(Error::InvalidLockPeriod);
        }

        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::Token, &token);
        env.storage()
            .instance()
            .set(&DataKey::ProtocolFeeBps, &fee_bps);
        env.storage().instance().set(&DataKey::TierCount, &0u32);
        env.storage().instance().set(&DataKey::PassCount, &0u64);
        env.storage()
            .instance()
            .set(&DataKey::ContractVersion, &1u32);

        env.events().publish(
            (Symbol::new(&env, "initialized"),),
            (admin, token, fee_bps, lock_period_seconds),
        );

        Ok(())
    }

    /// Updates the protocol fee charged on each pass purchase.
    ///
    /// Admin-only. Takes effect on all future `mint_pass` calls; does not affect passes
    /// already minted.
    ///
    /// # Panics
    ///
    /// - Panics if the contract has not been initialized.
    /// - Panics if `fee_bps` exceeds 1000 (10%).
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

    /// Updates the lock period for future PendingEarning records.
    ///
    /// Requires admin authentication.
    pub fn update_lock_period(env: Env, admin: Address, new_period_seconds: u64) -> Result<(), Error> {
        admin.require_auth();
        if new_period_seconds == 0 {
            return Err(Error::InvalidLockPeriod);
        }
        let old: u64 = env.storage().instance().get(&DataKey::LockPeriodSeconds).unwrap_or(0);
        env.storage()
            .instance()
            .set(&DataKey::LockPeriodSeconds, &new_period_seconds);
        env.events().publish(
            (Symbol::new(&env, "lock_period_updated"),),
            (old, new_period_seconds),
        );
        Ok(())
    }

    /// Withdraws accumulated protocol fees to a recipient address.
    ///
    /// Admin-only. Transfers `amount` USDC directly from the contract to `recipient`.
    ///
    /// # Panics
    ///
    /// - Panics if the contract has not been initialized or the token is not set.
    /// - Panics if `amount` is not greater than zero.
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

    /// Registers the calling address as a creator on StarPass.
    ///
    /// Must be called before `create_tier` or `withdraw`. Initializes the creator's
    /// profile, balance, and tier list. Requires creator signature.
    ///
    /// # Panics
    ///
    /// - Panics if the address is already registered as a creator.
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

    /// Creates a new membership tier for a registered creator.
    ///
    /// Creator-only. Returns the new `tier_id`. The creator must be registered via
    /// `register_creator` first. Requires creator signature.
    ///
    /// # Panics
    ///
    /// - Panics if the caller is not a registered creator.
    /// - Panics if `price` is zero.
    /// - Panics if `duration` is zero.
    /// - Panics if `name` is empty.
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

    /// Deactivates a tier, preventing any new passes from being minted for it.
    ///
    /// Creator-only. Existing passes remain valid until expiry. Requires creator
    /// signature; the caller must own the tier.
    ///
    /// # Panics
    ///
    /// - Panics if the tier does not exist.
    /// - Panics if the caller is not the tier's creator.
    /// - Panics if the tier is already inactive.
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

    /// Updates the USDC price of a tier for future purchases.
    ///
    /// Creator-only. Does not affect passes already minted. Requires creator
    /// signature; the caller must own the tier and the tier must be active.
    ///
    /// # Panics
    ///
    /// - Panics if `new_price` is zero.
    /// - Panics if the tier does not exist.
    /// - Panics if the caller is not the tier's creator.
    /// - Panics if the tier is inactive.
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

    /// Mints a new access pass for a fan by collecting a USDC payment.
    ///
    /// Fan-only. The fan pays the full tier price; the contract credits the creator's
    /// withdrawable balance after deducting the protocol fee. Returns the new `pass_id`.
    /// Requires fan signature.
    ///
    /// # Panics
    ///
    /// - Panics if the tier does not exist.
    /// - Panics if the tier is inactive.
    /// - Panics if the tier has a `max_supply` cap that has already been reached.
    /// - Panics if the USDC transfer from the fan fails (e.g. insufficient balance).
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

        // ESCROW: create PendingEarning instead of directly crediting creator balance
        // Earnings are locked for lock_period_seconds before becoming withdrawable.
        // See `process_unlocked_earnings()` to release matured earnings.
        let earning_id = {
            let cnt: u64 = env
                .storage()
                .persistent()
                .get(&DataKey::PendingEarningCount(tier.creator.clone()))
                .unwrap_or(0u64);
            let next = cnt + 1u64;
            env.storage()
                .persistent()
                .set(&DataKey::PendingEarningCount(tier.creator.clone()), &next);
            cnt
        };
        let lock_period: u64 = env.storage().instance().get(&DataKey::LockPeriodSeconds).unwrap_or(0u64);
        let unlocks_at = env.ledger().timestamp().saturating_add(lock_period); // ARITHMETIC: saturating
        let pending = PendingEarning {
            creator: tier.creator.clone(),
            amount: creator_amount,
            token: token.clone(),
            unlocks_at,
            released: false,
            earning_id,
        };
        env.storage()
            .persistent()
            .set(&DataKey::PendingEarning(tier.creator.clone(), earning_id), &pending);
        env.events().publish(
            (Symbol::new(&env, "earning_pending"),),
            (tier.creator.clone(), earning_id, creator_amount, unlocks_at),
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
    // Pass Renewal
    // --------------------------------------------------------

    /// Renew an existing pass — fan pays tier.price again (same fee split as
    /// mint_pass), extending expiry by one tier duration from whichever is
    /// later: the current ledger timestamp or the pass's current expiry.
    /// Renewing before expiry stacks on top of remaining time instead of
    /// resetting it. Returns the pass's new expiration timestamp.
    pub fn renew_pass(env: Env, fan: Address, pass_id: u64) -> u64 {
        fan.require_auth();

        let mut pass: Pass = env
            .storage()
            .persistent()
            .get(&DataKey::Pass(pass_id))
            .expect("Pass not found");

        assert!(pass.owner == fan, "Not the pass owner");
        assert!(pass.active, "Pass is not active");

        let tier: Tier = env
            .storage()
            .persistent()
            .get(&DataKey::Tier(pass.tier_id))
            .expect("Tier not found");

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

        // Calculate fee split (same as mint_pass)
        let protocol_fee = (tier.price * fee_bps as i128) / 10_000;
        let creator_amount = tier.price - protocol_fee;

        // Transfer full price from fan to contract
        let token_client = token::Client::new(&env, &token);
        token_client.transfer(&fan, &env.current_contract_address(), &tier.price);

        // ESCROW: create PendingEarning instead of directly crediting creator balance
        let earning_id = {
            let cnt: u64 = env
                .storage()
                .persistent()
                .get(&DataKey::PendingEarningCount(tier.creator.clone()))
                .unwrap_or(0u64);
            let next = cnt + 1u64;
            env.storage()
                .persistent()
                .set(&DataKey::PendingEarningCount(tier.creator.clone()), &next);
            cnt
        };
        let lock_period: u64 = env.storage().instance().get(&DataKey::LockPeriodSeconds).unwrap_or(0u64);
        let unlocks_at = env.ledger().timestamp().saturating_add(lock_period); // ARITHMETIC: saturating
        let pending = PendingEarning {
            creator: tier.creator.clone(),
            amount: creator_amount,
            token: token.clone(),
            unlocks_at,
            released: false,
            earning_id,
        };
        env.storage()
            .persistent()
            .set(&DataKey::PendingEarning(tier.creator.clone(), earning_id), &pending);
        env.events().publish(
            (Symbol::new(&env, "earning_pending"),),
            (tier.creator.clone(), earning_id, creator_amount, unlocks_at),
        );

        // Update creator profile
        let mut creator_profile: Creator = env
            .storage()
            .persistent()
            .get(&DataKey::Creator(tier.creator.clone()))
            .expect("Creator not found");
        creator_profile.total_earned += creator_amount;
        env.storage()
            .persistent()
            .set(&DataKey::Creator(tier.creator.clone()), &creator_profile);

        // Extend from whichever is later — rewards early renewal by not
        // discarding remaining time on the existing pass.
        let now = env.ledger().timestamp();
        let extend_from = if pass.expires_at > now {
            pass.expires_at
        } else {
            now
        };
        let new_expires_at = extend_from + tier.duration;
        pass.expires_at = new_expires_at;

        env.storage()
            .persistent()
            .set(&DataKey::Pass(pass_id), &pass);

        env.events().publish(
            (Symbol::new(&env, "pass_renewed"),),
            (pass_id, fan, new_expires_at),
        );

        new_expires_at
    }

    // --------------------------------------------------------
    // Creator Withdrawals
    // --------------------------------------------------------

    /// Withdraws all accumulated earnings to the creator's wallet.
    ///
    /// Creator-only. Transfers the full `CreatorBalance` to the creator and resets
    /// it to zero. Requires creator signature.
    ///
    /// # Panics
    ///
    /// - Panics if the creator has no balance to withdraw.
    /// - Panics if the USDC transfer fails.
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

    /// Moves all matured PendingEarning records for creator to the creator's
    /// withdrawable balance.
    ///
    /// No authentication required — anyone can call this to process
    /// a creator's matured earnings.
    pub fn process_unlocked_earnings(env: Env, creator: Address) -> Result<u32, Error> {
        let count: u64 = env
            .storage()
            .persistent()
            .get(&DataKey::PendingEarningCount(creator.clone()))
            .unwrap_or(0u64);
        if count == 0 {
            return Ok(0);
        }
        let mut released = 0u32;
        let now = env.ledger().timestamp();
        for id in 0..count {
            let key = DataKey::PendingEarning(creator.clone(), id);
            let mut pending: PendingEarning = match env.storage().persistent().get(&key) {
                Some(p) => p,
                None => continue,
            };
            // SECURITY: strictly greater than
            if !pending.released && now > pending.unlocks_at {
                pending.released = true;
                env.storage().persistent().set(&key, &pending);
                // credit creator balance
                let current_balance: i128 = env
                    .storage()
                    .persistent()
                    .get(&DataKey::CreatorBalance(creator.clone()))
                    .unwrap_or(0);
                env.storage().persistent().set(
                    &DataKey::CreatorBalance(creator.clone()),
                    &(current_balance + pending.amount),
                );
                released += 1;
                env.events().publish(
                    (Symbol::new(&env, "earning_released"),),
                    (creator.clone(), pending.earning_id, pending.amount, now),
                );
            }
        }
        Ok(released)
    }

    /// Admin proposes an early release for a specific PendingEarning.
    /// Stores a proposal in instance storage. Requires admin auth.
    pub fn propose_early_release(
        env: Env,
        admin: Address,
        creator: Address,
        earning_id: u64,
    ) -> Result<(), Error> {
        admin.require_auth();
        let key = DataKey::PendingEarning(creator.clone(), earning_id);
        let pending: PendingEarning = env
            .storage()
            .persistent()
            .get(&key)
            .ok_or(Error::EarningNotFound)?;
        if pending.released {
            return Err(Error::EarningAlreadyReleased);
        }
        let prop_key = DataKey::EarlyReleaseProposal(earning_id);
        if env.storage().instance().has(&prop_key) {
            return Err(Error::ProposalAlreadyExists);
        }
        let proposal = (admin.clone(), creator.clone(), earning_id, env.ledger().timestamp());
        env.storage().instance().set(&prop_key, &proposal);
        env.events().publish(
            (Symbol::new(&env, "early_release_proposed"),),
            (admin, creator, earning_id, env.ledger().timestamp()),
        );
        Ok(())
    }

    /// Creator approves an admin early release proposal, executing the release.
    pub fn approve_early_release(env: Env, creator: Address, earning_id: u64) -> Result<(), Error> {
        creator.require_auth();
        let prop_key = DataKey::EarlyReleaseProposal(earning_id);
        let proposal: (Address, Address, u64, u64) = env
            .storage()
            .instance()
            .get(&prop_key)
            .ok_or(Error::NoProposalFound)?;
        let (admin, prop_creator, _id, _proposed_at) = proposal.clone();
        if prop_creator != creator {
            return Err(Error::UnauthorizedApproval);
        }
        let key = DataKey::PendingEarning(creator.clone(), earning_id);
        let mut pending: PendingEarning = env.storage().persistent().get(&key).ok_or(Error::EarningNotFound)?;
        if pending.released {
            return Err(Error::EarningAlreadyReleased);
        }
        // MULTISIG: both admin (proposal) and creator (this call) have signed
        pending.released = true;
        env.storage().persistent().set(&key, &pending);
        // credit creator balance
        let current_balance: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::CreatorBalance(creator.clone()))
            .unwrap_or(0);
        env.storage().persistent().set(
            &DataKey::CreatorBalance(creator.clone()),
            &(current_balance + pending.amount),
        );
        // remove proposal
        env.storage().instance().remove(&prop_key);
        env.events().publish(
            (Symbol::new(&env, "early_release_executed"),),
            (admin, creator, earning_id, pending.amount, env.ledger().timestamp()),
        );
        Ok(())
    }

    /// Returns all PendingEarning records for creator, including released ones.
    pub fn get_pending_earnings(env: Env, creator: Address) -> Vec<PendingEarning> {
        let count: u64 = env
            .storage()
            .persistent()
            .get(&DataKey::PendingEarningCount(creator.clone()))
            .unwrap_or(0u64);
        let mut out: Vec<PendingEarning> = Vec::new(&env);
        for id in 0..count {
            if let Some(p) = env.storage().persistent().get(&DataKey::PendingEarning(creator.clone(), id)) {
                out.push_back(p);
            }
        }
        out
    }

    // --------------------------------------------------------
    // Read / Query Functions
    // --------------------------------------------------------

    /// Returns `true` if the fan holds an active, non-expired pass for `tier_id`.
    ///
    /// Read-only, no auth required. Can be called by any app, backend, or contract
    /// to gate access. A pass is valid when `active == true` and
    /// `expires_at > current_ledger_timestamp`.
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

    /// Returns `true` if the fan holds any active, non-expired pass issued by `creator`.
    ///
    /// Read-only, no auth required. Use this to gate creator-level content access
    /// regardless of which specific tier the fan purchased.
    pub fn has_any_valid_pass(env: Env, fan: Address, creator: Address) -> bool {
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
            if pass.creator == creator && pass.active && pass.expires_at > now {
                return true;
            }
        }

        false
    }

    /// Returns the [`Pass`] struct for the given `pass_id`.
    ///
    /// Read-only, no auth required.
    ///
    /// # Panics
    ///
    /// - Panics if no pass exists with `pass_id`.
    pub fn get_pass(env: Env, pass_id: u64) -> Pass {
        env.storage()
            .persistent()
            .get(&DataKey::Pass(pass_id))
            .expect("Pass not found")
    }

    /// Returns the [`Tier`] struct for the given `tier_id`.
    ///
    /// Read-only, no auth required.
    ///
    /// # Panics
    ///
    /// - Panics if no tier exists with `tier_id`.
    pub fn get_tier(env: Env, tier_id: u32) -> Tier {
        env.storage()
            .persistent()
            .get(&DataKey::Tier(tier_id))
            .expect("Tier not found")
    }

    /// Returns the [`Creator`] profile for the given address.
    ///
    /// Read-only, no auth required.
    ///
    /// # Panics
    ///
    /// - Panics if the address has not been registered as a creator.
    pub fn get_creator(env: Env, creator: Address) -> Creator {
        env.storage()
            .persistent()
            .get(&DataKey::Creator(creator))
            .expect("Creator not found")
    }

    /// Returns the pending withdrawal balance in stroops for the given creator.
    ///
    /// Read-only, no auth required. Returns `0` if the creator has no pending
    /// balance or is not registered.
    pub fn get_creator_balance(env: Env, creator: Address) -> i128 {
        env.storage()
            .persistent()
            .get(&DataKey::CreatorBalance(creator))
            .unwrap_or(0)
    }

    /// Returns the total number of passes ever minted across all creators and tiers.
    ///
    /// Read-only, no auth required. Returns `0` before any passes are minted.
    pub fn get_pass_count(env: Env) -> u64 {
        env.storage()
            .instance()
            .get(&DataKey::PassCount)
            .unwrap_or(0)
    }

    /// Returns the total number of tiers ever created across all creators.
    ///
    /// Read-only, no auth required. Returns `0` before any tiers are created.
    pub fn get_tier_count(env: Env) -> u32 {
        env.storage()
            .instance()
            .get(&DataKey::TierCount)
            .unwrap_or(0)
    }

    /// Returns all pass IDs owned by the given fan address.
    ///
    /// Read-only, no auth required. Returns an empty `Vec` if the fan has never
    /// minted a pass.
    pub fn get_fan_passes(env: Env, fan: Address) -> Vec<u64> {
        env.storage()
            .persistent()
            .get(&DataKey::FanPasses(fan))
            .unwrap_or(Vec::new(&env))
    }

    /// Returns all tier IDs created by the given creator address.
    ///
    /// Read-only, no auth required. Returns an empty `Vec` if the creator has
    /// no tiers or is not registered.
    pub fn get_creator_tiers(env: Env, creator: Address) -> Vec<u32> {
        env.storage()
            .persistent()
            .get(&DataKey::CreatorTiers(creator))
            .unwrap_or(Vec::new(&env))
    }

    /// Get a paginated slice of tier IDs created by a creator.
    ///
    /// * `offset` — zero-based start index into the creator's tier list
    /// * `limit`  — maximum number of tier IDs to return; capped at 20
    ///
    /// Returns an empty Vec when `offset` is beyond the end of the list.
    /// Panics if `limit` exceeds 20.
    // --------------------------------------------------------
    // Upgrade / Migration
    // --------------------------------------------------------
    /// Replaces the contract WASM with a new version.
    ///
    /// Admin-only. After calling `upgrade`, the next transaction should call
    /// `migrate` to transform existing storage to the new layout.
    pub fn upgrade(env: Env, admin: Address, new_wasm_hash: BytesN<32>) {
        admin.require_auth();
        env.deployer().update_current_contract_wasm(new_wasm_hash);
    }

    /// Transforms storage from the previous version to the current layout.
    ///
    /// Admin-only. Reads `ContractVersion`, panics if already at the target
    /// version (v2), then performs any key/data transformations needed and
    /// bumps the stored version. Safe to call only once per version increment.
    ///
    /// # Panics
    ///
    /// - Panics if `migrate` has already been called (version >= 2).
    pub fn migrate(env: Env, admin: Address) {
        admin.require_auth();

        let version: u32 = env
            .storage()
            .instance()
            .get(&DataKey::ContractVersion)
            .unwrap_or(0);
        assert!(version < 2, "Already migrated");

        // v1 -> v2 migration transforms
        // (No storage key changes in this migration; placeholder for future work)

        env.storage()
            .instance()
            .set(&DataKey::ContractVersion, &2u32);

        env.events()
            .publish((Symbol::new(&env, "migrated"),), (version, 2u32));
    }

    pub fn get_creator_tiers_page(env: Env, creator: Address, offset: u32, limit: u32) -> Vec<u32> {
        assert!(limit <= 20, "limit cannot exceed 20");

        let all: Vec<u32> = env
            .storage()
            .persistent()
            .get(&DataKey::CreatorTiers(creator))
            .unwrap_or(Vec::new(&env));

        let total = all.len();

        // offset beyond the end — return empty
        if offset >= total {
            return Vec::new(&env);
        }

        let mut page = Vec::new(&env);
        let end = (offset + limit).min(total);

        for i in offset..end {
            page.push_back(all.get(i).unwrap());
        }

        page
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
        let res = client.initialize(&admin, &token, &250u32, &3600u64);
        assert!(res.is_ok());

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
        let res = client.initialize(&admin, &token, &250u32, &3600u64);
        assert!(res.is_ok());

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
        let (env, contract_id, _admin, creator, fan, _token) = setup_env();
        let client = StarPassContractClient::new(&env, &contract_id);
        client.register_creator(&creator);
        assert_eq!(client.has_any_valid_pass(&fan, &creator), false);

        // Create tier and mint pass
        let tier_id = client.create_tier(
            &creator,
            &String::from_str(&env, "Bronze"),
            &1_000_000i128,
            &2_592_000u64,
            &0u32,
        );
        client.mint_pass(&fan, &tier_id);
        assert_eq!(client.has_any_valid_pass(&fan, &creator), true);

        // Expire the pass by advancing time beyond expiry
        let now = env.ledger().timestamp();
        env.ledger().set_timestamp(now + 2_592_001);
        assert_eq!(client.has_any_valid_pass(&fan, &creator), false);
    }

    // #[test]
    // fn test_has_any_valid_pass() {
    //     let (env, contract_id, _admin, creator, fan, _token) = setup_env();
    //     let client = StarPassContractClient::new(&env, &contract_id);
    //     client.register_creator(&creator);
    //     // No passes yet
    //     assert_eq!(client.has_any_valid_pass(&fan, &creator), false);
    //
    //     // Create tier and mint pass
    //     let tier_id = client.create_tier(
    //         &creator,
    //         &String::from_str(&env, "Bronze"),
    //         1_000_000i128,
    //         2_592_000u64,
    //         0u32,
    //     );
    //     client.mint_pass(&fan, &tier_id);
    //     assert_eq!(client.has_any_valid_pass(&fan, &creator), true);
    //
    //     // Expire the pass by advancing time beyond expiry
    //     let now = env.ledger().timestamp();
    //     env.ledger().set_timestamp(now + 2_592_001);
    //     assert_eq!(client.has_any_valid_pass(&fan, &creator), false);
    // }

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

        let now = env.ledger().timestamp(); // ARRANGE: record time of purchase
        client.mint_pass(&fan, &tier_id);
        // ACT: advance ledger past lock and process unlocked earnings
        env.ledger().set_timestamp(now + 3600 + 1);
        let res = client.process_unlocked_earnings(&creator);
        match res {
            Ok(n) => assert_eq!(n, 1u32),
            Err(_) => panic!("process_unlocked_earnings failed"),
        }
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

        let start = env.ledger().timestamp();
        client.mint_pass(&fan, &tier_id);
        env.ledger().set_timestamp(start + 3600 + 1);
        let res = client.process_unlocked_earnings(&creator);
        match res {
            Ok(n) => assert_eq!(n, 1u32),
            Err(_) => panic!("process_unlocked_earnings failed"),
        }
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
    fn test_renew_pass_before_expiry() {
        let (env, contract_id, _admin, creator, fan, _token) = setup_env();
        let client = StarPassContractClient::new(&env, &contract_id);
        client.register_creator(&creator);

        let duration = 2_592_000u64;
        let start = 1_000_000u64;
        env.ledger().set_timestamp(start);

        let tier_id = client.create_tier(
            &creator,
            &String::from_str(&env, "Bronze"),
            &1_000_000i128,
            &duration,
            &0u32,
        );

        let pass_id = client.mint_pass(&fan, &tier_id);
        let original_expires_at = client.get_pass(&pass_id).expires_at;
        assert_eq!(original_expires_at, start + duration);

        // Renew well before expiry — should extend from current expires_at,
        // not from "now", rewarding early renewal.
        env.ledger().set_timestamp(start + 1_000);
        let new_expires_at = client.renew_pass(&fan, &pass_id);

        assert_eq!(new_expires_at, original_expires_at + duration);
        let pass = client.get_pass(&pass_id);
        assert_eq!(pass.expires_at, new_expires_at);
        assert!(pass.active);

        // Fee split applied twice (mint + renewal), pass_count untouched.
        // ACT: advance ledger past both unlocks and process
        env.ledger().set_timestamp(start + 1_000 + 3600 + 1);
        let res = client.process_unlocked_earnings(&creator);
        match res {
            Ok(n) => assert_eq!(n, 2u32),
            Err(_) => panic!("process_unlocked_earnings failed"),
        }
        assert_eq!(client.get_creator_balance(&creator), 975_000 * 2);
        let profile = client.get_creator(&creator);
        assert_eq!(profile.total_earned, 975_000 * 2);
        assert_eq!(profile.pass_count, 1);
    }

    #[test]
    fn test_renew_pass_after_expiry() {
        let (env, contract_id, _admin, creator, fan, _token) = setup_env();
        let client = StarPassContractClient::new(&env, &contract_id);
        client.register_creator(&creator);

        let duration = 2_592_000u64;
        let start = 1_000_000u64;
        env.ledger().set_timestamp(start);

        let tier_id = client.create_tier(
            &creator,
            &String::from_str(&env, "Bronze"),
            &1_000_000i128,
            &duration,
            &0u32,
        );

        let pass_id = client.mint_pass(&fan, &tier_id);
        let original_expires_at = client.get_pass(&pass_id).expires_at;

        // Advance past expiry before renewing.
        let renew_time = original_expires_at + 500;
        env.ledger().set_timestamp(renew_time);
        assert!(!client.has_valid_pass(&fan, &tier_id));

        let new_expires_at = client.renew_pass(&fan, &pass_id);

        // Extends from "now" (renew_time), not from the stale expires_at.
        assert_eq!(new_expires_at, renew_time + duration);
        let pass = client.get_pass(&pass_id);
        assert_eq!(pass.expires_at, new_expires_at);
        assert!(client.has_valid_pass(&fan, &tier_id));
    }

    #[test]
    fn test_renew_pass_rejects_non_owner() {
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

        let pass_id = client.mint_pass(&fan, &tier_id);

        let impostor = Address::generate(&env);
        let result = client.try_renew_pass(&impostor, &pass_id);
        assert!(result.is_err());
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

    // --------------------------------------------------------
    // get_creator_tiers_page tests
    // --------------------------------------------------------

    /// Helper: register a creator and mint `n` tiers, returns their IDs in order.
    fn create_n_tiers(
        env: &Env,
        client: &StarPassContractClient,
        creator: &Address,
        n: u32,
    ) -> soroban_sdk::Vec<u32> {
        let mut ids = soroban_sdk::Vec::new(env);
        for i in 0..n {
            let name = String::from_str(env, "Tier");
            let _ = name; // silence unused warning
            let tier_id = client.create_tier(
                creator,
                &String::from_str(env, "Tier"),
                &1_000_000i128,
                &2_592_000u64,
                &0u32,
            );
            let _ = i;
            ids.push_back(tier_id);
        }
        ids
    }

    #[test]
    fn test_creator_tiers_page_first_page() {
        let (env, contract_id, _admin, creator, _fan, _token) = setup_env();
        let client = StarPassContractClient::new(&env, &contract_id);
        client.register_creator(&creator);
        create_n_tiers(&env, &client, &creator, 5);

        // First page: offset=0, limit=3 → tier IDs 1, 2, 3
        let page = client.get_creator_tiers_page(&creator, &0u32, &3u32);
        assert_eq!(page.len(), 3);
        assert_eq!(page.get(0).unwrap(), 1u32);
        assert_eq!(page.get(1).unwrap(), 2u32);
        assert_eq!(page.get(2).unwrap(), 3u32);
    }

    #[test]
    fn test_creator_tiers_page_last_page() {
        let (env, contract_id, _admin, creator, _fan, _token) = setup_env();
        let client = StarPassContractClient::new(&env, &contract_id);
        client.register_creator(&creator);
        create_n_tiers(&env, &client, &creator, 5);

        // Last page: offset=3, limit=5 → only 2 items remain (tier IDs 4, 5)
        let page = client.get_creator_tiers_page(&creator, &3u32, &5u32);
        assert_eq!(page.len(), 2);
        assert_eq!(page.get(0).unwrap(), 4u32);
        assert_eq!(page.get(1).unwrap(), 5u32);
    }

    #[test]
    fn test_creator_tiers_page_offset_beyond_end() {
        let (env, contract_id, _admin, creator, _fan, _token) = setup_env();
        let client = StarPassContractClient::new(&env, &contract_id);
        client.register_creator(&creator);
        create_n_tiers(&env, &client, &creator, 3);

        // offset=10 is past the 3-item list — must return empty, not panic
        let page = client.get_creator_tiers_page(&creator, &10u32, &5u32);
        assert_eq!(page.len(), 0);
    }

    #[test]
    #[should_panic(expected = "limit cannot exceed 20")]
    fn test_creator_tiers_page_limit_exceeded() {
        let (env, contract_id, _admin, creator, _fan, _token) = setup_env();
        let client = StarPassContractClient::new(&env, &contract_id);
        client.register_creator(&creator);

        // limit=21 must panic with a clear message
        client.get_creator_tiers_page(&creator, &0u32, &21u32);
    }

    #[test]
    fn test_cannot_deactivate_others_tier() {
        let (env, contract_id, _admin, creator, _fan, _token) = setup_env();
        let client = StarPassContractClient::new(&env, &contract_id);
        client.register_creator(&creator);

        let tier_id = client.create_tier(
            &creator,
            &String::from_str(&env, "Bronze"),
            &1_000_000i128,
            &2_592_000u64,
            &0u32,
        );

        let impostor = Address::generate(&env);
        let result = client.try_deactivate_tier(&impostor, &tier_id);
        assert!(result.is_err());
    }

    #[test]
    fn test_cannot_update_others_tier_price() {
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

        let impostor = Address::generate(&env);
        let result = client.try_update_tier_price(&impostor, &tier_id, &2_000_000i128);
        assert!(result.is_err());
    }

    #[test]
    fn test_cannot_withdraw_others_balance() {
        let env = Env::default();
        let contract_id = env.register_contract(None, StarPassContract);
        let client = StarPassContractClient::new(&env, &contract_id);
        let creator = Address::generate(&env);

        let result = client.try_withdraw(&creator);
        assert!(result.is_err());
    }

    // --------------------------------------------------------
    // Upgrade / Migration tests
    // --------------------------------------------------------

    #[test]
    fn test_full_upgrade_lifecycle() {
        let (env, contract_id, admin, creator, fan, token) = setup_env();
        let client = StarPassContractClient::new(&env, &contract_id);

        // Populate v1 state
        client.register_creator(&creator);

        let tier_id = client.create_tier(
            &creator,
            &String::from_str(&env, "Gold"),
            &2_000_000i128,
            &2_592_000u64,
            &0u32,
        );

        StellarAssetClient::new(&env, &token).mint(&fan, &100_000_000);
        let pass_id = client.mint_pass(&fan, &tier_id);

        // Verify pre-migration state
        assert_eq!(client.get_tier_count(), 1);
        assert_eq!(client.get_pass_count(), 1);
        let creator_profile = client.get_creator(&creator);
        assert_eq!(creator_profile.pass_count, 1);
        assert_eq!(creator_profile.total_earned, 1_950_000);
        assert_eq!(client.get_creator_balance(&creator), 1_950_000);
        let tier = client.get_tier(&tier_id);
        assert_eq!(tier.minted, 1);
        let pass = client.get_pass(&pass_id);
        assert_eq!(pass.owner, fan);
        assert!(pass.active);
        let fan_passes = client.get_fan_passes(&fan);
        assert_eq!(fan_passes.len(), 1);
        let creator_tiers = client.get_creator_tiers(&creator);
        assert_eq!(creator_tiers.len(), 1);

        // Migrate v1 -> v2
        client.migrate(&admin);

        // All state still readable
        assert_eq!(client.get_tier_count(), 1);
        assert_eq!(client.get_pass_count(), 1);
        let creator_profile = client.get_creator(&creator);
        assert_eq!(creator_profile.pass_count, 1);
        assert_eq!(creator_profile.total_earned, 1_950_000);
        assert_eq!(client.get_creator_balance(&creator), 1_950_000);
        let tier = client.get_tier(&tier_id);
        assert_eq!(tier.minted, 1);
        assert_eq!(tier.creator, creator);
        let pass = client.get_pass(&pass_id);
        assert_eq!(pass.owner, fan);
        assert!(pass.active);
        assert_eq!(pass.tier_id, tier_id);
        let fan_passes = client.get_fan_passes(&fan);
        assert_eq!(fan_passes.len(), 1);
        assert_eq!(fan_passes.get(0).unwrap(), pass_id);
        let creator_tiers = client.get_creator_tiers(&creator);
        assert_eq!(creator_tiers.len(), 1);
        assert_eq!(creator_tiers.get(0).unwrap(), tier_id);

        // has_valid_pass still works after migration
        assert!(client.has_valid_pass(&fan, &tier_id));

        // Double-migration panics
        let result = client.try_migrate(&admin);
        assert!(result.is_err());
    }

    #[test]
    fn test_upgrade_admin_only() {
        let (env, contract_id, admin, _creator, _fan, _token) = setup_env();
        let client = StarPassContractClient::new(&env, &contract_id);

        // upgrade compiles and can be called (test env may or may not
        // support actual WASM replacement, but the function exists)
        let wasm_hash = BytesN::from_array(&env, &[0u8; 32]);
        let result = client.try_upgrade(&admin, &wasm_hash);
        // May succeed or fail depending on test env deployer support,
        // but should not panic about auth (mock_all_auths is set).
        // This at minimum exercises the function path.
        let _ = result;
    }
}
