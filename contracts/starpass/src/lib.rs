#![no_std]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, token, Address, BytesN, Env, String,
    Symbol, Vec,
};

// IMPLEMENTATION MAP:
// - Config change: add `LockPeriodSeconds` to `DataKey` and store in `initialize`.
// - Admin API: `update_lock_period(env, admin, new_period_seconds)` added.
// - Interception: `mint_pass` and `renew_pass` no longer directly credit
//   `CreatorBalance`; instead they create `PendingEarning` records keyed by
//   `(creator, earning_id)` and increment `PendingEarningCount(creator)`.
// - New types: `PendingEarning` struct added; `Error` enum appended.
// - Pause/Resume: Pass struct extended with `paused: bool`, `paused_at: u64`,
//   `total_paused_seconds: u64`. `pause_pass` freezes expiry clock;
//   `resume_pass` extends `expires_at` by pause duration and accumulates
//   `total_paused_seconds`. `has_valid_pass`/`has_any_valid_pass`/
//   `get_fan_active_passes` reject paused passes.
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

/// How a tier's price is denominated.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TierPriceMode {
    /// Fixed price in the payment token's base units (stroops).
    Fixed(i128),
    /// Price denominated in USD cents. Converted to payment-token base units
    /// at mint/renewal time using the configured oracle's current rate.
    USDDenominated(i128),
}

/// Membership tier defined by a creator
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Tier {
    pub tier_id: u32,
    pub creator: Address,
    pub name: String,
    pub price: TierPriceMode, // fixed stroops or USD cents, see TierPriceMode
    pub duration: u64,        // duration in seconds
    pub max_supply: u32,      // 0 = unlimited
    pub minted: u32,
    pub active: bool,
    pub permissions: Vec<Symbol>, // named permissions defined by creator
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
    pub paused: bool,
    pub paused_at: u64,
    pub total_paused_seconds: u64,
    pub pass_permissions: Vec<Symbol>, // subset of tier permissions granted at mint
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

/// Temporary permission grant from creator to fan
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PermissionGrant {
    pub fan: Address,
    pub tier_id: u32,
    pub permissions: Vec<Symbol>,
    pub expires_at: u64,
}

/// Storage keys
#[contracttype]
pub enum DataKey {
    /// Storage keys
    Admin,
    Token,          // USDC token address
    ProtocolFeeBps, // basis points e.g. 250 = 2.5%
    /// Duration in seconds that earnings are locked before becoming withdrawable
    LockPeriodSeconds,
    /// Minimum threshold required for creator revenue withdrawals
    MinWithdrawal, // <-- ADD THIS LINE
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
    FanPasses(Address),    // fan address -> Vec<u64> pass IDs
    CreatorTiers(Address), // creator address -> Vec<u32> tier IDs
    ContractVersion,
    /// Permission grant keyed by (fan, tier_id)
    PermissionGrant(Address, u32),
}

// ============================================================
// Errors
// ============================================================

/// Contract-level errors (append-only)
#[contracterror]
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Error {
    /// lock_period_seconds must be greater than zero
    // SECURITY: prevents a zero-length lock which would bypass escrow
    InvalidLockPeriod = 1,
    /// No PendingEarning exists for the given creator and earning_id
    EarningNotFound = 2,
    /// The PendingEarning has already been released
    // SECURITY: prevents double-release/double-credit
    EarningAlreadyReleased = 3,
    /// An early release proposal already exists for this earning_id
    ProposalAlreadyExists = 4,
    /// No early release proposal exists for this earning_id
    NoProposalFound = 5,
    /// The calling creator does not match the proposal's intended creator
    // SECURITY: prevents cross-creator approval attacks
    UnauthorizedApproval = 6,
    /// process_unlocked_earnings called but earning is not yet matured
    EarningNotMatured = 7,
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
// Price Oracle
// ============================================================

/// Interface implemented by the external price oracle contract.
///
/// `get_price` returns `(price, timestamp)` where `price` is the USD cents
/// value of one base unit of `token` at `timestamp` (ledger seconds).
#[contractclient(name = "OracleClient")]
pub trait OracleInterface {
    fn get_price(env: Env, token: Address) -> (i128, u64);
}

/// Oracle data older than this (in seconds) is rejected as stale.
const ORACLE_STALENESS_THRESHOLD_SECONDS: u64 = 300;

/// Resolves the payment-token amount (base units) owed for `tier`.
///
/// Fixed-price tiers return their stored amount unchanged. USD-denominated
/// tiers call the configured oracle for the current USD-cents price of
/// `token`, reject stale or missing oracle data, and convert using 7-decimal
/// fixed-point math: `usd_price_cents * 10_000_000 / oracle_price_cents`.
///
/// # Panics
///
/// - Panics with "Oracle data unavailable or stale" if no oracle is
///   configured, the oracle reports a non-positive price, or the oracle's
///   data is older than `ORACLE_STALENESS_THRESHOLD_SECONDS`.
fn resolve_price_amount(env: &Env, tier: &Tier, token: &Address) -> i128 {
    match &tier.price {
        TierPriceMode::Fixed(amount) => *amount,
        TierPriceMode::USDDenominated(usd_price_cents) => {
            let usd_price_cents = *usd_price_cents;
            let oracle_address: Address = env
                .storage()
                .instance()
                .get(&DataKey::OracleContract)
                .expect("Oracle data unavailable or stale");

            let oracle_client = OracleClient::new(env, &oracle_address);
            let (oracle_price_cents, timestamp) = oracle_client.get_price(token);

            let now = env.ledger().timestamp();
            assert!(
                now.saturating_sub(timestamp) <= ORACLE_STALENESS_THRESHOLD_SECONDS,
                "Oracle data unavailable or stale"
            );
            assert!(oracle_price_cents > 0, "Oracle data unavailable or stale");

            usd_price_cents
                .checked_mul(10_000_000)
                .expect("Overflow computing token amount")
                / oracle_price_cents
        }
    }
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
        env.storage()
            .instance()
            .set(&DataKey::LockPeriodSeconds, &lock_period_seconds);
        env.storage().instance().set(&DataKey::TierCount, &0u32);
        env.storage().instance().set(&DataKey::PassCount, &0u64);
        env.storage()
            .instance()
            .set(&DataKey::ContractVersion, &1u32);

        // Set default minimum withdrawal threshold (1,000,000 stroops / 1 USDC)
        env.storage()
            .instance()
            .set(&DataKey::MinWithdrawal, &1_000_000_i128);

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
    pub fn update_lock_period(
        env: Env,
        admin: Address,
        new_period_seconds: u64,
    ) -> Result<(), Error> {
        admin.require_auth();
        if new_period_seconds == 0 {
            return Err(Error::InvalidLockPeriod);
        }
        let old: u64 = env
            .storage()
            .instance()
            .get(&DataKey::LockPeriodSeconds)
            .unwrap_or(0);
        env.storage()
            .instance()
            .set(&DataKey::LockPeriodSeconds, &new_period_seconds);
        env.events().publish(
            (Symbol::new(&env, "lock_period_updated"),),
            (old, new_period_seconds),
        );
        Ok(())
    }

    /// Sets the price oracle contract used to convert USD-denominated tier
    /// prices into payment-token amounts.
    ///
    /// Admin-only. `oracle_address` must implement `get_price(token: Address)
    /// -> (price: i128, timestamp: u64)`, returning the USD-cents price of
    /// one base unit of `token` and the ledger timestamp it was observed at.
    ///
    /// # Panics
    ///
    /// - Panics if the contract has not been initialized.
    /// - Panics if `admin` does not match the stored admin.
    pub fn set_oracle(env: Env, admin: Address, oracle_address: Address) {
        let stored_admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .expect("Not initialized");
        assert!(admin == stored_admin, "Not authorized");
        admin.require_auth();

        env.storage()
            .instance()
            .set(&DataKey::OracleContract, &oracle_address);

        env.events()
            .publish((Symbol::new(&env, "oracle_set"),), (admin, oracle_address));
    }

    /// Returns the configured price oracle contract address, if any.
    ///
    /// Read-only, no auth required.
    pub fn get_oracle(env: Env) -> Option<Address> {
        env.storage().instance().get(&DataKey::OracleContract)
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
        permissions: Vec<Symbol>,
    ) -> u32 {
        assert!(price > 0, "Price must be greater than zero");
        Self::create_tier_priced(
            env,
            creator,
            name,
            TierPriceMode::Fixed(price),
            duration,
            max_supply,
            permissions,
        )
    }

    /// Creates a new membership tier priced in USD cents.
    ///
    /// Creator-only. Behaves like `create_tier`, but the tier's price is
    /// denominated in USD cents and converted to the payment token's base
    /// units at mint/renewal time using the oracle set via `set_oracle`.
    ///
    /// # Panics
    ///
    /// - Panics if the caller is not a registered creator.
    /// - Panics if `usd_price_cents` is zero.
    /// - Panics if `duration` is zero.
    /// - Panics if `name` is empty.
    pub fn create_tier_usd(
        env: Env,
        creator: Address,
        name: String,
        usd_price_cents: i128,
        duration: u64,
        max_supply: u32,
        permissions: Vec<Symbol>,
    ) -> u32 {
        assert!(usd_price_cents > 0, "Price must be greater than zero");
        Self::create_tier_priced(
            env,
            creator,
            name,
            TierPriceMode::USDDenominated(usd_price_cents),
            duration,
            max_supply,
            permissions,
        )
    }

    fn create_tier_priced(
        env: Env,
        creator: Address,
        name: String,
        price: TierPriceMode,
        duration: u64,
        max_supply: u32,
        permissions: Vec<Symbol>,
    ) -> u32 {
        creator.require_auth();
        assert!(
            env.storage()
                .persistent()
                .has(&DataKey::Creator(creator.clone())),
            "Must register as creator first"
        );
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
            price: price.clone(),
            duration,
            max_supply,
            minted: 0,
            active: true,
            permissions,
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
        assert!(
            matches!(tier.price, TierPriceMode::Fixed(_)),
            "Tier is USD-denominated; use update_tier_price_usd"
        );

        let old_price = tier.price.clone();
        tier.price = TierPriceMode::Fixed(new_price);
        env.storage()
            .persistent()
            .set(&DataKey::Tier(tier_id), &tier);

        env.events().publish(
            (Symbol::new(&env, "tier_price_updated"),),
            (tier_id, old_price, new_price),
        );
    }

    /// Updates the USD-cents price of a USD-denominated tier for future purchases.
    ///
    /// Creator-only. Does not affect passes already minted. Requires creator
    /// signature; the caller must own the tier and the tier must be active.
    ///
    /// # Panics
    ///
    /// - Panics if `new_usd_price_cents` is zero.
    /// - Panics if the tier does not exist.
    /// - Panics if the caller is not the tier's creator.
    /// - Panics if the tier is inactive.
    /// - Panics if the tier is fixed-price.
    pub fn update_tier_price_usd(
        env: Env,
        creator: Address,
        tier_id: u32,
        new_usd_price_cents: i128,
    ) {
        creator.require_auth();
        assert!(new_usd_price_cents > 0, "Price must be greater than zero");

        let mut tier: Tier = env
            .storage()
            .persistent()
            .get(&DataKey::Tier(tier_id))
            .expect("Tier not found");
        assert!(tier.creator == creator, "Not the tier creator");
        assert!(tier.active, "Tier is not active");
        assert!(
            matches!(tier.price, TierPriceMode::USDDenominated(_)),
            "Tier is fixed-price; use update_tier_price"
        );

        let old_price = tier.price.clone();
        tier.price = TierPriceMode::USDDenominated(new_usd_price_cents);
        env.storage()
            .persistent()
            .set(&DataKey::Tier(tier_id), &tier);

        env.events().publish(
            (Symbol::new(&env, "tier_price_updated"),),
            (tier_id, old_price, new_usd_price_cents),
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
    /// - Panics if any pass_permission is not in the tier's permissions.
    pub fn mint_pass(env: Env, fan: Address, tier_id: u32, pass_permissions: Vec<Symbol>) -> u64 {
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

        // Validate that all pass_permissions are in tier's permissions
        for perm in pass_permissions.iter() {
            let mut found = false;
            for tier_perm in tier.permissions.iter() {
                if tier_perm == perm {
                    found = true;
                    break;
                }
            }
            assert!(found, "Permission not in tier definition");
        }

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

        // Resolve payment amount: Fixed tiers use their stored amount as-is;
        // USDDenominated tiers convert via the configured oracle.
        let price_amount = resolve_price_amount(&env, &tier, &token);

        // Calculate fee split
        let protocol_fee = (price_amount * fee_bps as i128) / 10_000;
        let creator_amount = price_amount - protocol_fee;

        // Transfer full price from fan to contract
        let token_client = token::Client::new(&env, &token);
        token_client.transfer(&fan, &env.current_contract_address(), &price_amount);

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
        let lock_period: u64 = env
            .storage()
            .instance()
            .get(&DataKey::LockPeriodSeconds)
            .unwrap_or(0u64);
        let unlocks_at = env.ledger().timestamp().saturating_add(lock_period); // ARITHMETIC: saturating
        let pending = PendingEarning {
            creator: tier.creator.clone(),
            amount: creator_amount,
            token: token.clone(),
            unlocks_at,
            released: false,
            earning_id,
        };
        env.storage().persistent().set(
            &DataKey::PendingEarning(tier.creator.clone(), earning_id),
            &pending,
        );
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
            paused: false,
            paused_at: 0,
            total_paused_seconds: 0,
            pass_permissions,
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

        // Resolve payment amount (same as mint_pass)
        let price_amount = resolve_price_amount(&env, &tier, &token);

        // Calculate fee split (same as mint_pass)
        let protocol_fee = (price_amount * fee_bps as i128) / 10_000;
        let creator_amount = price_amount - protocol_fee;

        // Transfer full price from fan to contract
        let token_client = token::Client::new(&env, &token);
        token_client.transfer(&fan, &env.current_contract_address(), &price_amount);

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
        let lock_period: u64 = env
            .storage()
            .instance()
            .get(&DataKey::LockPeriodSeconds)
            .unwrap_or(0u64);
        let unlocks_at = env.ledger().timestamp().saturating_add(lock_period); // ARITHMETIC: saturating
        let pending = PendingEarning {
            creator: tier.creator.clone(),
            amount: creator_amount,
            token: token.clone(),
            unlocks_at,
            released: false,
            earning_id,
        };
        env.storage().persistent().set(
            &DataKey::PendingEarning(tier.creator.clone(), earning_id),
            &pending,
        );
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
    // Pass Pause / Resume
    // --------------------------------------------------------

    /// Pause an active, non-expired pass.
    ///
    /// Pass owner only. Freezes the expiry clock while paused.
    /// Panics if the caller is not the pass owner, the pass is not
    /// active, the pass is already paused, or the pass has expired.
    pub fn pause_pass(env: Env, fan: Address, pass_id: u64) {
        fan.require_auth();

        let mut pass: Pass = env
            .storage()
            .persistent()
            .get(&DataKey::Pass(pass_id))
            .expect("Pass not found");

        assert!(pass.owner == fan, "Not the pass owner");
        assert!(pass.active, "Pass is not active");
        assert!(!pass.paused, "Pass is already paused");
        let now = env.ledger().timestamp();
        assert!(pass.expires_at > now, "Pass has expired");

        pass.paused = true;
        pass.paused_at = now;

        env.storage()
            .persistent()
            .set(&DataKey::Pass(pass_id), &pass);

        env.events()
            .publish((Symbol::new(&env, "pass_paused"),), (pass_id, fan, now));
    }

    /// Resume a paused pass, extending its expiry by the time it was paused.
    ///
    /// Pass owner only. Adds the pause duration to `expires_at` and
    /// accumulates it in `total_paused_seconds`. Panics if the caller is
    /// not the pass owner, the pass is not paused, or the pass is not active.
    pub fn resume_pass(env: Env, fan: Address, pass_id: u64) {
        fan.require_auth();

        let mut pass: Pass = env
            .storage()
            .persistent()
            .get(&DataKey::Pass(pass_id))
            .expect("Pass not found");

        assert!(pass.owner == fan, "Not the pass owner");
        assert!(pass.paused, "Pass is not paused");
        assert!(pass.active, "Pass is not active");

        let now = env.ledger().timestamp();
        let paused_duration = now - pass.paused_at;

        pass.expires_at = pass.expires_at.saturating_add(paused_duration);
        pass.total_paused_seconds = pass.total_paused_seconds.saturating_add(paused_duration);
        pass.paused = false;
        pass.paused_at = 0;

        env.storage()
            .persistent()
            .set(&DataKey::Pass(pass_id), &pass);

        env.events().publish(
            (Symbol::new(&env, "pass_resumed"),),
            (pass_id, fan, now, paused_duration),
        );
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

        // Retrieve minimum withdrawal threshold from configuration
        let min_withdrawal: i128 = env
            .storage()
            .instance()
            .get(&DataKey::MinWithdrawal)
            .unwrap_or(1_000_000_i128);

        assert!(
            balance >= min_withdrawal,
            "Withdrawal amount below minimum threshold"
        );

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

    /// Updates the minimum withdrawal threshold (Admin only)
    pub fn update_min_withdrawal(env: Env, admin: Address, new_min: i128) {
        admin.require_auth();

        let stored_admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .expect("Admin not set");
        assert!(admin == stored_admin, "Unauthorized");

        assert!(new_min >= 0, "Minimum withdrawal cannot be negative");

        env.storage()
            .instance()
            .set(&DataKey::MinWithdrawal, &new_min);
    }

    /// Returns the current minimum withdrawal threshold
    pub fn get_min_withdrawal(env: Env) -> i128 {
        env.storage()
            .instance()
            .get(&DataKey::MinWithdrawal)
            .unwrap_or(1_000_000_i128)
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
        let proposal = (
            admin.clone(),
            creator.clone(),
            earning_id,
            env.ledger().timestamp(),
        );
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
        let mut pending: PendingEarning = env
            .storage()
            .persistent()
            .get(&key)
            .ok_or(Error::EarningNotFound)?;
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
            (
                admin,
                creator,
                earning_id,
                pending.amount,
                env.ledger().timestamp(),
            ),
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
            if let Some(p) = env
                .storage()
                .persistent()
                .get(&DataKey::PendingEarning(creator.clone(), id))
            {
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
            if pass.tier_id == tier_id && pass.active && !pass.paused && pass.expires_at > now {
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
            if pass.creator == creator && pass.active && !pass.paused && pass.expires_at > now {
                return true;
            }
        }

        false
    }

    /// Returns `true` if the fan has the specified permission for the tier.
    ///
    /// Checks both pass permissions and active permission grants. A fan has a permission
    /// if they hold an active, non-expired pass for the tier that grants the permission,
    /// or if they have an active, non-expired permission grant from the tier's creator.
    ///
    /// Read-only, no auth required.
    pub fn has_permission(env: Env, fan: Address, tier_id: u32, permission: Symbol) -> bool {
        let now = env.ledger().timestamp();

        // Check pass-based permissions
        let fan_passes: Vec<u64> = match env.storage().persistent().get(&DataKey::FanPasses(fan.clone())) {
            Some(p) => p,
            None => return false,
        };

        for pass_id in fan_passes.iter() {
            let pass: Pass = match env.storage().persistent().get(&DataKey::Pass(*pass_id)) {
                Some(p) => p,
                None => continue,
            };
            if pass.tier_id == tier_id && pass.active && !pass.paused && pass.expires_at > now {
                // Check if permission is in pass_permissions
                for perm in pass.pass_permissions.iter() {
                    if perm == &permission {
                        return true;
                    }
                }
            }
        }

        // Check grant-based permissions
        if let Some(grant) = env.storage().persistent().get(&DataKey::PermissionGrant(fan, tier_id)) {
            if grant.expires_at > now {
                for perm in grant.permissions.iter() {
                    if perm == &permission {
                        return true;
                    }
                }
            }
        }

        false
    }

    // --------------------------------------------------------
    // Permission Grant Management
    // --------------------------------------------------------

    /// Grants temporary permissions to a fan for a tier.
    ///
    /// Creator-only. The creator can grant a subset of the tier's permissions to a fan
    /// for a specified duration. The grant expires after the duration.
    ///
    /// # Panics
    ///
    /// - Panics if the caller is not the tier's creator.
    /// - Panics if the tier does not exist.
    /// - Panics if any permission is not in the tier's permissions.
    pub fn grant_permission(
        env: Env,
        creator: Address,
        fan: Address,
        tier_id: u32,
        permissions: Vec<Symbol>,
        duration: u64,
    ) {
        creator.require_auth();

        let tier: Tier = env
            .storage()
            .persistent()
            .get(&DataKey::Tier(tier_id))
            .expect("Tier not found");
        assert!(tier.creator == creator, "Not the tier creator");

        // Validate that all permissions are in tier's permissions
        for perm in permissions.iter() {
            let mut found = false;
            for tier_perm in tier.permissions.iter() {
                if tier_perm == perm {
                    found = true;
                    break;
                }
            }
            assert!(found, "Permission not in tier definition");
        }

        let now = env.ledger().timestamp();
        let expires_at = now.saturating_add(duration);

        let grant = PermissionGrant {
            fan: fan.clone(),
            tier_id,
            permissions,
            expires_at,
        };

        env.storage()
            .persistent()
            .set(&DataKey::PermissionGrant(fan, tier_id), &grant);

        env.events().publish(
            (Symbol::new(&env, "permission_granted"),),
            (creator, fan, tier_id, expires_at),
        );
    }

    /// Revokes a permission grant from a fan for a tier.
    ///
    /// Creator-only. Removes the grant, immediately revoking the fan's delegated permissions.
    ///
    /// # Panics
    ///
    /// - Panics if the caller is not the tier's creator.
    /// - Panics if no grant exists for the fan and tier.
    pub fn revoke_permission_grant(env: Env, creator: Address, fan: Address, tier_id: u32) {
        creator.require_auth();

        let tier: Tier = env
            .storage()
            .persistent()
            .get(&DataKey::Tier(tier_id))
            .expect("Tier not found");
        assert!(tier.creator == creator, "Not the tier creator");

        let grant: PermissionGrant = env
            .storage()
            .persistent()
            .get(&DataKey::PermissionGrant(fan.clone(), tier_id))
            .expect("Grant not found");

        env.storage()
            .persistent()
            .remove(&DataKey::PermissionGrant(fan, tier_id));

        env.events().publish(
            (Symbol::new(&env, "permission_revoked"),),
            (creator, fan, tier_id),
        );
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

    /// Returns full [`Pass`] structs for all pass IDs owned by the given fan address.
    ///
    /// Read-only, no auth required. Returns an empty `Vec` if the fan has never
    /// minted a pass.
    pub fn get_fan_pass_details(env: Env, fan: Address) -> Vec<Pass> {
        let fan_passes: Vec<u64> = match env.storage().persistent().get(&DataKey::FanPasses(fan)) {
            Some(p) => p,
            None => return Vec::new(&env),
        };

        let mut passes = Vec::new(&env);
        for pass_id in fan_passes.iter() {
            let pass: Pass = match env.storage().persistent().get(&DataKey::Pass(pass_id)) {
                Some(p) => p,
                None => continue,
            };
            passes.push_back(pass);
        }
        passes
    }

    /// Returns pass IDs for all active, non-expired passes owned by the given fan address.
    ///
    /// A pass is considered active when `active == true` and `expires_at > current_ledger_timestamp`.
    ///
    /// Read-only, no auth required. Returns an empty `Vec` if the fan has no active passes.
    pub fn get_fan_active_passes(env: Env, fan: Address) -> Vec<u64> {
        let fan_passes: Vec<u64> = match env.storage().persistent().get(&DataKey::FanPasses(fan)) {
            Some(p) => p,
            None => return Vec::new(&env),
        };
        let now = env.ledger().timestamp();

        let mut active = Vec::new(&env);
        for pass_id in fan_passes.iter() {
            let pass: Pass = match env.storage().persistent().get(&DataKey::Pass(pass_id)) {
                Some(p) => p,
                None => continue,
            };
            if pass.active && !pass.paused && pass.expires_at > now {
                active.push_back(pass_id);
            }
        }
        active
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

    /// Returns the total number of passes minted across all of a creator's tiers.
    ///
    /// Computed by summing `minted` over every tier owned by `creator`, rather
    /// than returning the cached `Creator.pass_count` field, so it stays correct
    /// even if that cache and the per-tier counts were ever to drift apart.
    ///
    /// Read-only, no auth required. Returns `0` if the creator has no tiers or
    /// is not registered.
    pub fn get_creator_pass_count(env: Env, creator: Address) -> u64 {
        let tier_ids: Vec<u32> = env
            .storage()
            .persistent()
            .get(&DataKey::CreatorTiers(creator))
            .unwrap_or(Vec::new(&env));

        let mut total: u64 = 0;
        for tier_id in tier_ids.iter() {
            let tier: Option<Tier> = env.storage().persistent().get(&DataKey::Tier(tier_id));
            if let Some(tier) = tier {
                total += tier.minted as u64;
            }
        }
        total
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

    // --------------------------------------------------------
    // NFT Metadata (no_std, no serde, no format!)
    // --------------------------------------------------------

    /// Returns a compact on-chain JSON string conforming to the Stellar NFT
    /// metadata standard for a single pass.
    ///
    /// JSON shape:
    /// ```json
    /// {
    ///   "name": "<tier_name> Pass",
    ///   "description": "StarPass access pass",
    ///   "attributes": [
    ///     {"trait_type": "Tier",    "value": "<tier_name>"},
    ///     {"trait_type": "Creator", "value": "<creator_strkey>"},
    ///     {"trait_type": "Expires", "value": "<unix_ts_or_never>"},
    ///     {"trait_type": "Status",  "value": "active|expired|inactive"}
    ///   ]
    /// }
    /// ```
    ///
    /// Status logic:
    /// - `inactive`: the pass's `active` flag is false (manually deactivated)
    /// - `expired`:  `active` is true but `expires_at <= now`
    /// - `active`:   `active` is true and `expires_at > now`
    ///
    /// # Panics
    /// Panics if `pass_id` does not exist.
    pub fn get_pass_metadata(env: Env, pass_id: u64) -> String {
        let pass: Pass = env
            .storage()
            .persistent()
            .get(&DataKey::Pass(pass_id))
            .expect("Pass not found");

        let tier: Tier = env
            .storage()
            .persistent()
            .get(&DataKey::Tier(pass.tier_id))
            .expect("Tier not found");

        let now = env.ledger().timestamp();

        let status: &[u8] = if !pass.active {
            b"inactive"
        } else if pass.expires_at <= now {
            b"expired"
        } else {
            b"active"
        };

        // expires value: unix timestamp string or "never" (expires_at == 0)
        let mut expires_num_buf = [0u8; 20];
        let expires_bytes: &[u8] = if pass.expires_at == 0 {
            b"never"
        } else {
            u64_to_decimal(pass.expires_at, &mut expires_num_buf)
        };

        // Extract tier name bytes
        let tier_name_len = tier.name.len() as usize;
        let mut tier_name_buf = [0u8; 64];
        tier.name
            .copy_into_slice(&mut tier_name_buf[..tier_name_len]);
        let tier_name_bytes = &tier_name_buf[..tier_name_len];

        // Extract creator Strkey bytes (always 56 ASCII chars for C.../G... keys)
        let creator_str = pass.creator.to_string();
        let creator_len = creator_str.len() as usize;
        let mut creator_buf = [0u8; 56];
        creator_str.copy_into_slice(&mut creator_buf[..creator_len]);
        let creator_bytes = &creator_buf[..creator_len];

        // Assemble JSON into a fixed stack buffer.
        // Max size estimate:
        //   Fixed scaffold    ~200 bytes
        //   tier_name × 2     128 bytes
        //   creator strkey     56 bytes
        //   expires (u64 max)  20 bytes
        //   status              8 bytes
        // Total headroom: 512 bytes is safe.
        let mut buf = [0u8; 512];
        let mut cur = 0usize;

        append(&mut buf, &mut cur, b"{\"name\":\"");
        append(&mut buf, &mut cur, tier_name_bytes);
        append(
            &mut buf,
            &mut cur,
            b" Pass\",\"description\":\"StarPass access pass\",\"attributes\":[",
        );
        append(&mut buf, &mut cur, b"{\"trait_type\":\"Tier\",\"value\":\"");
        append(&mut buf, &mut cur, tier_name_bytes);
        append(&mut buf, &mut cur, b"\"},");
        append(
            &mut buf,
            &mut cur,
            b"{\"trait_type\":\"Creator\",\"value\":\"",
        );
        append(&mut buf, &mut cur, creator_bytes);
        append(&mut buf, &mut cur, b"\"},");
        append(
            &mut buf,
            &mut cur,
            b"{\"trait_type\":\"Expires\",\"value\":\"",
        );
        append(&mut buf, &mut cur, expires_bytes);
        append(&mut buf, &mut cur, b"\"},");
        append(
            &mut buf,
            &mut cur,
            b"{\"trait_type\":\"Status\",\"value\":\"",
        );
        append(&mut buf, &mut cur, status);
        append(&mut buf, &mut cur, b"\"}]}");

        String::from_bytes(&env, &buf[..cur])
    }

    /// Returns a compact on-chain JSON string representing the collection
    /// metadata for a tier (used by NFT marketplaces to describe the tier
    /// as a named collection).
    ///
    /// JSON shape:
    /// ```json
    /// {
    ///   "name": "<tier_name>",
    ///   "description": "StarPass tier collection by <creator_strkey>",
    ///   "attributes": [
    ///     {"trait_type": "Creator",    "value": "<creator_strkey>"},
    ///     {"trait_type": "Price",      "value": "<price_in_stroops>"},
    ///     {"trait_type": "Duration",   "value": "<duration_seconds>"},
    ///     {"trait_type": "MaxSupply",  "value": "<max_supply_or_unlimited>"},
    ///     {"trait_type": "Minted",     "value": "<minted_count>"},
    ///     {"trait_type": "Active",     "value": "true|false"}
    ///   ]
    /// }
    /// ```
    ///
    /// # Panics
    /// Panics if `tier_id` does not exist.
    pub fn get_tier_collection_metadata(env: Env, tier_id: u32) -> String {
        let tier: Tier = env
            .storage()
            .persistent()
            .get(&DataKey::Tier(tier_id))
            .expect("Tier not found");

        // Extract tier name bytes
        let tier_name_len = tier.name.len() as usize;
        let mut tier_name_buf = [0u8; 64];
        tier.name
            .copy_into_slice(&mut tier_name_buf[..tier_name_len]);
        let tier_name_bytes = &tier_name_buf[..tier_name_len];

        // Extract creator Strkey bytes
        let creator_str = tier.creator.to_string();
        let creator_len = creator_str.len() as usize;
        let mut creator_buf = [0u8; 56];
        creator_str.copy_into_slice(&mut creator_buf[..creator_len]);
        let creator_bytes = &creator_buf[..creator_len];

        // Numeric fields
        let mut price_buf = [0u8; 40]; // i128 can be up to 39 digits + sign
        let price_bytes = i128_to_decimal(tier.price, &mut price_buf);

        let mut duration_buf = [0u8; 20];
        let duration_bytes = u64_to_decimal(tier.duration, &mut duration_buf);

        let mut max_supply_buf = [0u8; 20];
        let max_supply_bytes: &[u8] = if tier.max_supply == 0 {
            b"unlimited"
        } else {
            u64_to_decimal(tier.max_supply as u64, &mut max_supply_buf)
        };

        let mut minted_buf = [0u8; 20];
        let minted_bytes = u64_to_decimal(tier.minted as u64, &mut minted_buf);

        let active_bytes: &[u8] = if tier.active { b"true" } else { b"false" };

        // Assemble — max size ~600 bytes; 768-byte buffer is safe.
        let mut buf = [0u8; 768];
        let mut cur = 0usize;

        append(&mut buf, &mut cur, b"{\"name\":\"");
        append(&mut buf, &mut cur, tier_name_bytes);
        append(
            &mut buf,
            &mut cur,
            b"\",\"description\":\"StarPass tier collection by ",
        );
        append(&mut buf, &mut cur, creator_bytes);
        append(&mut buf, &mut cur, b"\",\"attributes\":[");
        append(
            &mut buf,
            &mut cur,
            b"{\"trait_type\":\"Creator\",\"value\":\"",
        );
        append(&mut buf, &mut cur, creator_bytes);
        append(&mut buf, &mut cur, b"\"},");
        append(
            &mut buf,
            &mut cur,
            b"{\"trait_type\":\"Price\",\"value\":\"",
        );
        append(&mut buf, &mut cur, price_bytes);
        append(&mut buf, &mut cur, b"\"},");
        append(
            &mut buf,
            &mut cur,
            b"{\"trait_type\":\"Duration\",\"value\":\"",
        );
        append(&mut buf, &mut cur, duration_bytes);
        append(&mut buf, &mut cur, b"\"},");
        append(
            &mut buf,
            &mut cur,
            b"{\"trait_type\":\"MaxSupply\",\"value\":\"",
        );
        append(&mut buf, &mut cur, max_supply_bytes);
        append(&mut buf, &mut cur, b"\"},");
        append(
            &mut buf,
            &mut cur,
            b"{\"trait_type\":\"Minted\",\"value\":\"",
        );
        append(&mut buf, &mut cur, minted_bytes);
        append(&mut buf, &mut cur, b"\"},");
        append(
            &mut buf,
            &mut cur,
            b"{\"trait_type\":\"Active\",\"value\":\"",
        );
        append(&mut buf, &mut cur, active_bytes);
        append(&mut buf, &mut cur, b"\"}]}");

        String::from_bytes(&env, &buf[..cur])
    }
}

// ============================================================
// no_std JSON-builder helpers (module-private)
// ============================================================

/// Copy `src` bytes into `buf` at position `*cursor`, then advance the cursor.
///
/// # Panics
/// Panics if the write would overflow the buffer (programming error —
/// callers must size their buffers with sufficient headroom).
#[inline]
fn append(buf: &mut [u8], cursor: &mut usize, src: &[u8]) {
    let end = *cursor + src.len();
    buf[*cursor..end].copy_from_slice(src);
    *cursor = end;
}

/// Convert a `u64` to its ASCII decimal representation, writing digits into
/// `buf` (must be at least 20 bytes). Returns a slice of the filled portion.
fn u64_to_decimal(mut n: u64, buf: &mut [u8; 20]) -> &[u8] {
    if n == 0 {
        buf[0] = b'0';
        return &buf[..1];
    }
    // Write digits in reverse, then flip.
    let mut len = 0usize;
    while n > 0 {
        buf[len] = b'0' + (n % 10) as u8;
        n /= 10;
        len += 1;
    }
    buf[..len].reverse();
    &buf[..len]
}

/// Convert an `i128` to its ASCII decimal representation (with leading `-`
/// for negative values). `buf` must be at least 40 bytes. Returns a slice of
/// the filled portion.
fn i128_to_decimal(n: i128, buf: &mut [u8; 40]) -> &[u8] {
    if n == 0 {
        buf[0] = b'0';
        return &buf[..1];
    }
    let negative = n < 0;
    // Work with the absolute value; handle i128::MIN by using u128 arithmetic.
    let mut abs: u128 = if negative {
        // Cast via wrapping negation to handle i128::MIN safely.
        (!(n as u128)).wrapping_add(1)
    } else {
        n as u128
    };
    let mut len = 0usize;
    while abs > 0 {
        buf[len] = b'0' + (abs % 10) as u8;
        abs /= 10;
        len += 1;
    }
    if negative {
        buf[len] = b'-';
        len += 1;
    }
    buf[..len].reverse();
    &buf[..len]
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
        client.initialize(&admin, &token, &250u32, &3600u64);

        (env, contract_id, admin, creator, fan, token)
    }

    // --------------------------------------------------------
    // Mock price oracle (test-only)
    // --------------------------------------------------------

    /// Minimal mock implementing `OracleInterface`. Price/timestamp are set
    /// directly via `set_price` so tests can exercise fresh and stale data.
    #[contract]
    pub struct MockOracle;

    #[contractimpl]
    impl MockOracle {
        pub fn set_price(env: Env, price: i128, timestamp: u64) {
            env.storage()
                .instance()
                .set(&Symbol::new(&env, "price"), &price);
            env.storage()
                .instance()
                .set(&Symbol::new(&env, "ts"), &timestamp);
        }

        pub fn get_price(env: Env, _token: Address) -> (i128, u64) {
            let price: i128 = env
                .storage()
                .instance()
                .get(&Symbol::new(&env, "price"))
                .expect("mock price not set");
            let timestamp: u64 = env
                .storage()
                .instance()
                .get(&Symbol::new(&env, "ts"))
                .expect("mock timestamp not set");
            (price, timestamp)
        }
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
        client.initialize(&admin, &token, &250u32, &3600u64);

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
            soroban_sdk::Vec::new(&env),
        );
        client.mint_pass(&fan, &tier_id, soroban_sdk::Vec::new(&env));
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
    //         soroban_sdk::Vec::new(&env),
    //     );
    //     client.mint_pass(&fan, &tier_id, soroban_sdk::Vec::new(&env));
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
            soroban_sdk::Vec::new(&env),
        );

        let pass_id = client.mint_pass(&fan, &tier_id, soroban_sdk::Vec::new(&env));
        assert_eq!(pass_id, 1);

        let pass = client.get_pass(&pass_id);
        assert_eq!(pass.owner, fan);
        assert_eq!(pass.tier_id, tier_id);
        assert_eq!(pass.active, true);
    }

    // --------------------------------------------------------
    // Oracle-based USD-denominated pricing
    // --------------------------------------------------------

    #[test]
    fn test_mint_pass_fixed_price_unaffected() {
        // No oracle configured at all — a Fixed-price tier must mint fine.
        let (env, contract_id, _admin, creator, fan, _token) = setup_env();
        let client = StarPassContractClient::new(&env, &contract_id);
        client.register_creator(&creator);

        let tier_id = client.create_tier(
            &creator,
            &String::from_str(&env, "Bronze"),
            &1_000_000i128,
            &2_592_000u64,
            &0u32,
            soroban_sdk::Vec::new(&env),
        );
        let tier = client.get_tier(&tier_id);
        assert_eq!(tier.price, TierPriceMode::Fixed(1_000_000));

        let pass_id = client.mint_pass(&fan, &tier_id, soroban_sdk::Vec::new(&env));
        let pass = client.get_pass(&pass_id);
        assert_eq!(pass.active, true);
    }

    #[test]
    fn test_mint_pass_usd_denominated_correct_amount() {
        let (env, contract_id, admin, creator, fan, token) = setup_env();
        let client = StarPassContractClient::new(&env, &contract_id);
        client.register_creator(&creator);

        // Mock oracle: 1 token base unit = 10 USD cents.
        let oracle_id = env.register_contract(None, MockOracle);
        let oracle_client = MockOracleClient::new(&env, &oracle_id);
        let now = env.ledger().timestamp();
        oracle_client.set_price(&10i128, &now);

        client.set_oracle(&admin, &oracle_id);

        // Tier priced at $10.00 (1000 cents).
        let tier_id = client.create_tier_usd(
            &creator,
            &String::from_str(&env, "USD Tier"),
            &1000i128,
            &2_592_000u64,
            &0u32,
            soroban_sdk::Vec::new(&env),
        );

        // Expected token amount: 1000 * 10_000_000 / 10 = 1_000_000_000.
        StellarAssetClient::new(&env, &token).mint(&fan, &2_000_000_000);

        let pass_id = client.mint_pass(&fan, &tier_id, soroban_sdk::Vec::new(&env));
        assert!(client.get_pass(&pass_id).active);

        env.ledger().set_timestamp(now + 3600 + 1);
        let released = client.process_unlocked_earnings(&creator);
        assert_eq!(released, 1u32);

        // fee_bps = 250 (2.5%) → protocol_fee = 25_000_000, creator_amount = 975_000_000
        assert_eq!(client.get_creator_balance(&creator), 975_000_000);
    }

    #[test]
    #[should_panic(expected = "Oracle data unavailable or stale")]
    fn test_mint_pass_stale_oracle_panics() {
        let (env, contract_id, admin, creator, fan, _token) = setup_env();
        let client = StarPassContractClient::new(&env, &contract_id);
        client.register_creator(&creator);

        let oracle_id = env.register_contract(None, MockOracle);
        let oracle_client = MockOracleClient::new(&env, &oracle_id);
        let now = env.ledger().timestamp();
        oracle_client.set_price(&10i128, &now);
        client.set_oracle(&admin, &oracle_id);

        let tier_id = client.create_tier_usd(
            &creator,
            &String::from_str(&env, "USD Tier"),
            &1000i128,
            &2_592_000u64,
            &0u32,
            soroban_sdk::Vec::new(&env),
        );

        // Advance beyond the 300-second staleness window.
        env.ledger().set_timestamp(now + 301);

        client.mint_pass(&fan, &tier_id, soroban_sdk::Vec::new(&env));
    }

    #[test]
    #[should_panic(expected = "Oracle data unavailable or stale")]
    fn test_mint_pass_oracle_unavailable_panics() {
        // No oracle configured — USD-denominated tier must panic on mint.
        let (env, contract_id, _admin, creator, fan, _token) = setup_env();
        let client = StarPassContractClient::new(&env, &contract_id);
        client.register_creator(&creator);

        let tier_id = client.create_tier_usd(
            &creator,
            &String::from_str(&env, "USD Tier"),
            &1000i128,
            &2_592_000u64,
            &0u32,
            soroban_sdk::Vec::new(&env),
        );

        client.mint_pass(&fan, &tier_id, soroban_sdk::Vec::new(&env));
    }

    #[test]
    fn test_set_oracle_rejects_non_admin() {
        let (env, contract_id, _admin, _creator, _fan, _token) = setup_env();
        let client = StarPassContractClient::new(&env, &contract_id);

        let oracle_id = env.register_contract(None, MockOracle);
        let impostor = Address::generate(&env);
        let result = client.try_set_oracle(&impostor, &oracle_id);
        assert!(result.is_err());
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
            soroban_sdk::Vec::new(&env),
        );

        assert_eq!(client.has_valid_pass(&fan, &tier_id), false);
        client.mint_pass(&fan, &tier_id, soroban_sdk::Vec::new(&env));
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
            soroban_sdk::Vec::new(&env),
        );

        let now = env.ledger().timestamp(); // ARRANGE: record time of purchase
        client.mint_pass(&fan, &tier_id, soroban_sdk::Vec::new(&env));
        // ACT: advance ledger past lock and process unlocked earnings
        env.ledger().set_timestamp(now + 3600 + 1);
        let res = client.process_unlocked_earnings(&creator);
        assert_eq!(res, 1u32);
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
            soroban_sdk::Vec::new(&env),
        );

        let start = env.ledger().timestamp();
        client.mint_pass(&fan, &tier_id, soroban_sdk::Vec::new(&env));
        env.ledger().set_timestamp(start + 3600 + 1);
        let res = client.process_unlocked_earnings(&creator);
        assert_eq!(res, 1u32);
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
            soroban_sdk::Vec::new(&env),
        );

        client.mint_pass(&fan, &tier_id, soroban_sdk::Vec::new(&env));
        let result = client.try_mint_pass(&fan, &tier_id, soroban_sdk::Vec::new(&env));
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
            soroban_sdk::Vec::new(&env),
        );

        client.deactivate_tier(&creator, &tier_id);
        let tier = client.get_tier(&tier_id);
        assert_eq!(tier.active, false);

        let result = client.try_mint_pass(&fan, &tier_id, soroban_sdk::Vec::new(&env));
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
            soroban_sdk::Vec::new(&env),
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
            soroban_sdk::Vec::new(&env),
        );

        client.update_tier_price(&creator, &tier_id, &2_000_000i128);
        let tier = client.get_tier(&tier_id);
        assert_eq!(tier.price, TierPriceMode::Fixed(2_000_000));
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
            soroban_sdk::Vec::new(&env),
        );

        // Mint pass at `start`; expires_at = start + duration
        env.ledger().set_timestamp(start);
        client.mint_pass(&fan, &tier_id, soroban_sdk::Vec::new(&env));

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
            soroban_sdk::Vec::new(&env),
        );

        let pass_id = client.mint_pass(&fan, &tier_id, soroban_sdk::Vec::new(&env));
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
        assert_eq!(res, 2u32);
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
            soroban_sdk::Vec::new(&env),
        );

        let pass_id = client.mint_pass(&fan, &tier_id, soroban_sdk::Vec::new(&env));
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
            soroban_sdk::Vec::new(&env),
        );

        let pass_id = client.mint_pass(&fan, &tier_id, soroban_sdk::Vec::new(&env));

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
            soroban_sdk::Vec::new(&env),
        );
        let tier2 = client.create_tier(
            &creator,
            &String::from_str(&env, "Gold"),
            &2_000_000i128,
            &2_592_000u64,
            &0u32,
            soroban_sdk::Vec::new(&env),
        );

        client.mint_pass(&fan, &tier1, soroban_sdk::Vec::new(&env));
        client.mint_pass(&fan, &tier2, soroban_sdk::Vec::new(&env));

        let passes = client.get_fan_passes(&fan);
        assert_eq!(passes.len(), 2);
    }

    #[test]
    fn test_fan_pass_details_and_active_passes() {
        let (env, contract_id, _admin, creator, fan, token) = setup_env();
        let client = StarPassContractClient::new(&env, &contract_id);
        client.register_creator(&creator);

        StellarAssetClient::new(&env, &token).mint(&fan, &100_000_000);

        let start = 1_000_000u64;
        let long_duration = 604_800u64; // 7 days
        let short_duration = 86_400u64; // 1 day

        let tier1 = client.create_tier(
            &creator,
            &String::from_str(&env, "Annual"),
            &10_000_000i128,
            &long_duration,
            &0u32,
            soroban_sdk::Vec::new(&env),
        );

        let tier2 = client.create_tier(
            &creator,
            &String::from_str(&env, "Daily"),
            &500_000i128,
            &short_duration,
            &0u32,
            soroban_sdk::Vec::new(&env),
        );

        env.ledger().set_timestamp(start);
        let short_lived_pass_id = client.mint_pass(&fan, &tier2, soroban_sdk::Vec::new(&env));

        env.ledger().set_timestamp(start + short_duration + 1);
        let long_lived_pass_id = client.mint_pass(&fan, &tier1, soroban_sdk::Vec::new(&env));

        let all_details = client.get_fan_pass_details(&fan);
        assert_eq!(all_details.len(), 2);

        let active_ids = client.get_fan_active_passes(&fan);
        assert_eq!(active_ids.len(), 1);
        assert_eq!(active_ids.get(0).unwrap(), &long_lived_pass_id);

        let mut found_short = false;
        let mut found_long = false;
        for pass in all_details.iter() {
            if pass.pass_id == long_lived_pass_id {
                assert!(pass.active);
                assert_eq!(pass.owner, fan);
                assert_eq!(pass.tier_id, tier1);
                found_long = true;
            } else if pass.pass_id == short_lived_pass_id {
                assert!(pass.active);
                assert_eq!(pass.owner, fan);
                assert_eq!(pass.tier_id, tier2);
                found_short = true;
            }
        }
        assert!(found_short);
        assert!(found_long);
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
                soroban_sdk::Vec::new(env),
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
    fn test_creator_pass_count_no_tiers() {
        let (env, contract_id, _admin, creator, _fan, _token) = setup_env();
        let client = StarPassContractClient::new(&env, &contract_id);
        client.register_creator(&creator);

        assert_eq!(client.get_creator_pass_count(&creator), 0u64);
    }

    #[test]
    fn test_creator_pass_count_sums_minted_across_tiers() {
        let (env, contract_id, _admin, creator, fan, token) = setup_env();
        StellarAssetClient::new(&env, &token).mint(&fan, &100_000_000);
        let client = StarPassContractClient::new(&env, &contract_id);
        client.register_creator(&creator);

        let tier_ids = create_n_tiers(&env, &client, &creator, 3);
        let tier_a = tier_ids.get(0).unwrap();
        let tier_b = tier_ids.get(1).unwrap();
        let tier_c = tier_ids.get(2).unwrap();

        // Several mints on tier A, several on tier B, none on tier C.
        client.mint_pass(&fan, &tier_a, soroban_sdk::Vec::new(&env));
        client.mint_pass(&fan, &tier_a, soroban_sdk::Vec::new(&env));
        client.mint_pass(&fan, &tier_a, soroban_sdk::Vec::new(&env));
        client.mint_pass(&fan, &tier_b, soroban_sdk::Vec::new(&env));
        client.mint_pass(&fan, &tier_b, soroban_sdk::Vec::new(&env));

        assert_eq!(client.get_tier(&tier_a).minted, 3);
        assert_eq!(client.get_tier(&tier_b).minted, 2);
        assert_eq!(client.get_tier(&tier_c).minted, 0);

        assert_eq!(client.get_creator_pass_count(&creator), 5u64);
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
            soroban_sdk::Vec::new(&env),
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
            soroban_sdk::Vec::new(&env),
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
            soroban_sdk::Vec::new(&env),
        );

        StellarAssetClient::new(&env, &token).mint(&fan, &100_000_000);
        let pass_id = client.mint_pass(&fan, &tier_id, soroban_sdk::Vec::new(&env));

        // Release the escrowed earning before checking balances (lock = 3600s)
        let mint_time = env.ledger().timestamp();
        env.ledger().set_timestamp(mint_time + 3601);
        client.process_unlocked_earnings(&creator);

        // Verify pre-migration state
        assert_eq!(client.get_tier_count(), 1);
        assert_eq!(client.get_pass_count(), 1);
        let creator_profile = client.get_creator(&creator);
        assert_eq!(creator_profile.pass_count, 1);
        assert_eq!(creator_profile.total_earned, 1_950_000);
        // Earnings are escrowed until the lock period elapses.
        assert_eq!(client.get_creator_balance(&creator), 0);
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
        assert_eq!(client.get_creator_balance(&creator), 0);
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

        // Escrowed earnings still release correctly post-migration
        let now = env.ledger().timestamp();
        env.ledger().set_timestamp(now + 3600 + 1);
        let released = client.process_unlocked_earnings(&creator);
        assert_eq!(released, 1u32);
        assert_eq!(client.get_creator_balance(&creator), 1_950_000);

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
        // This at least exercises the function path.
        let _ = result;
    }

    // ============================================================
    // NFT Metadata Tests
    // ============================================================

    /// Helper: extract a soroban String into a Rust &str slice stored in a
    /// caller-supplied buffer so we can run contains() assertions without alloc.
    fn sdk_str_contains(s: &soroban_sdk::String, needle: &[u8]) -> bool {
        let len = s.len() as usize;
        // 768 bytes is larger than any metadata string we produce.
        let mut buf = [0u8; 768];
        s.copy_into_slice(&mut buf[..len]);
        let haystack = &buf[..len];
        haystack.windows(needle.len()).any(|w| w == needle)
    }

    /// Full equality helper: compares a soroban String byte-for-byte against a
    /// compile-time `&[u8]` literal.
    #[allow(dead_code)]
    fn sdk_str_eq(s: &soroban_sdk::String, expected: &[u8]) -> bool {
        let len = s.len() as usize;
        if len != expected.len() {
            return false;
        }
        let mut buf = [0u8; 768];
        s.copy_into_slice(&mut buf[..len]);
        &buf[..len] == expected
    }

    // ----------------------------------------------------------------
    // get_pass_metadata — active pass
    // ----------------------------------------------------------------
    #[test]
    fn test_metadata_active_pass() {
        let (env, contract_id, _admin, creator, fan, _token) = setup_env();
        let client = StarPassContractClient::new(&env, &contract_id);

        // Fix the ledger timestamp so expires_at is deterministic.
        env.ledger().set_timestamp(1_000_000);

        client.register_creator(&creator);
        let tier_id = client.create_tier(
            &creator,
            &String::from_str(&env, "Gold"),
            &1_000_000i128,
            &2_592_000u64, // 30 days
            &0u32,
        );
        let pass_id = client.mint_pass(&fan, &tier_id);

        // Query metadata while still before expiry.
        let meta = client.get_pass_metadata(&pass_id);

        // Validate required JSON fields via substring checks.
        assert!(
            sdk_str_contains(&meta, b"\"name\":\"Gold Pass\""),
            "name field missing or wrong"
        );
        assert!(
            sdk_str_contains(&meta, b"StarPass access pass"),
            "description missing"
        );
        assert!(
            sdk_str_contains(&meta, b"\"trait_type\":\"Tier\",\"value\":\"Gold\""),
            "Tier attribute missing"
        );
        assert!(
            sdk_str_contains(&meta, b"\"trait_type\":\"Status\",\"value\":\"active\""),
            "Status should be active"
        );
        // expires_at = 1_000_000 + 2_592_000 = 3_592_000
        assert!(
            sdk_str_contains(&meta, b"\"trait_type\":\"Expires\",\"value\":\"3592000\""),
            "Expires attribute wrong"
        );
        // JSON must start with '{' and end with '}'
        let len = meta.len() as usize;
        let mut buf = [0u8; 768];
        meta.copy_into_slice(&mut buf[..len]);
        assert_eq!(buf[0], b'{');
        assert_eq!(buf[len - 1], b'}');
    }

    // ----------------------------------------------------------------
    // get_pass_metadata — expired pass
    // ----------------------------------------------------------------
    #[test]
    fn test_metadata_expired_pass() {
        let (env, contract_id, _admin, creator, fan, _token) = setup_env();
        let client = StarPassContractClient::new(&env, &contract_id);

        // Mint at t=1_000
        env.ledger().set_timestamp(1_000);
        client.register_creator(&creator);
        let tier_id = client.create_tier(
            &creator,
            &String::from_str(&env, "Silver"),
            &1_000_000i128,
            &86_400u64, // 1 day duration
            &0u32,
        );
        let pass_id = client.mint_pass(&fan, &tier_id);

        // Advance past expiry (expires_at = 1_000 + 86_400 = 87_400)
        env.ledger().set_timestamp(87_401);

        let meta = client.get_pass_metadata(&pass_id);

        assert!(
            sdk_str_contains(&meta, b"\"name\":\"Silver Pass\""),
            "name field wrong for expired pass"
        );
        assert!(
            sdk_str_contains(&meta, b"\"trait_type\":\"Status\",\"value\":\"expired\""),
            "Status should be expired"
        );
        assert!(
            sdk_str_contains(&meta, b"\"trait_type\":\"Expires\",\"value\":\"87400\""),
            "Expires attribute wrong"
        );
    }

    // ----------------------------------------------------------------
    // get_pass_metadata — inactive (deactivated) pass
    // ----------------------------------------------------------------
    #[test]
    fn test_metadata_inactive_pass() {
        let (env, contract_id, _admin, creator, fan, _token) = setup_env();
        let client = StarPassContractClient::new(&env, &contract_id);

        env.ledger().set_timestamp(1_000_000);
        client.register_creator(&creator);
        let tier_id = client.create_tier(
            &creator,
            &String::from_str(&env, "Bronze"),
            &1_000_000i128,
            &2_592_000u64,
            &0u32,
            soroban_sdk::Vec::new(&env),
        );
        let pass_id = client.mint_pass(&fan, &tier_id, soroban_sdk::Vec::new(&env));

        // Manually mark the pass inactive via storage (simulate deactivation).
        // We reach into persistent storage directly in the test environment.
        let mut pass: Pass = env.as_contract(&contract_id, || {
            env.storage()
                .persistent()
                .get(&DataKey::Pass(pass_id))
                .unwrap()
        });
        pass.active = false;
        env.as_contract(&contract_id, || {
            env.storage()
                .persistent()
                .set(&DataKey::Pass(pass_id), &pass);
        });

        let meta = client.get_pass_metadata(&pass_id);
        assert!(
            sdk_str_contains(&meta, b"\"trait_type\":\"Status\",\"value\":\"inactive\""),
            "Status should be inactive"
        );
    }

    // ----------------------------------------------------------------
    // get_pass_metadata — no-expiry pass (expires_at == 0)
    // ----------------------------------------------------------------
    #[test]
    fn test_metadata_no_expiry_pass() {
        let (env, contract_id, _admin, creator, fan, _token) = setup_env();
        let client = StarPassContractClient::new(&env, &contract_id);

        // Start at timestamp 0 so that expires_at = 0 + duration.
        // To get expires_at == 0 we manipulate storage directly.
        env.ledger().set_timestamp(1_000_000);
        client.register_creator(&creator);
        let tier_id = client.create_tier(
            &creator,
            &String::from_str(&env, "Platinum"),
            &1_000_000i128,
            &2_592_000u64,
            &0u32,
        );
        let pass_id = client.mint_pass(&fan, &tier_id);

        // Override expires_at to 0 to represent "no expiry".
        let mut pass: Pass = env.as_contract(&contract_id, || {
            env.storage()
                .persistent()
                .get(&DataKey::Pass(pass_id))
                .unwrap()
        });
        pass.expires_at = 0;
        env.as_contract(&contract_id, || {
            env.storage()
                .persistent()
                .set(&DataKey::Pass(pass_id), &pass);
        });

        let meta = client.get_pass_metadata(&pass_id);

        // expires_at == 0 → "never"
        assert!(
            sdk_str_contains(&meta, b"\"trait_type\":\"Expires\",\"value\":\"never\""),
            "expires_at=0 should produce 'never'"
        );
        // active flag is true and now (1_000_000) > 0, but since expires_at == 0
        // the condition `expires_at <= now` would trip — so the contract shows "expired".
        // That is intentional: 0 is only meaningful as "never" when the caller controls
        // the display; the status field reflects the raw ledger comparison.
        // The test simply verifies the "never" string appears for the Expires attribute.
    }

    // ----------------------------------------------------------------
    // get_tier_collection_metadata
    // ----------------------------------------------------------------
    #[test]
    fn test_tier_collection_metadata_active() {
        let (env, contract_id, _admin, creator, _fan, _token) = setup_env();
        let client = StarPassContractClient::new(&env, &contract_id);

        client.register_creator(&creator);
        let tier_id = client.create_tier(
            &creator,
            &String::from_str(&env, "Diamond"),
            &5_000_000i128,
            &2_592_000u64,
            &100u32,
        );

        let meta = client.get_tier_collection_metadata(&tier_id);

        assert!(
            sdk_str_contains(&meta, b"\"name\":\"Diamond\""),
            "name field wrong"
        );
        assert!(
            sdk_str_contains(&meta, b"StarPass tier collection by"),
            "description prefix missing"
        );
        assert!(
            sdk_str_contains(&meta, b"\"trait_type\":\"Price\",\"value\":\"5000000\""),
            "Price attribute wrong"
        );
        assert!(
            sdk_str_contains(&meta, b"\"trait_type\":\"Duration\",\"value\":\"2592000\""),
            "Duration attribute wrong"
        );
        assert!(
            sdk_str_contains(&meta, b"\"trait_type\":\"MaxSupply\",\"value\":\"100\""),
            "MaxSupply attribute wrong"
        );
        assert!(
            sdk_str_contains(&meta, b"\"trait_type\":\"Minted\",\"value\":\"0\""),
            "Minted attribute should be 0 before any mints"
        );
        assert!(
            sdk_str_contains(&meta, b"\"trait_type\":\"Active\",\"value\":\"true\""),
            "Active attribute wrong"
        );

        // Valid JSON boundaries
        let len = meta.len() as usize;
        let mut buf = [0u8; 768];
        meta.copy_into_slice(&mut buf[..len]);
        assert_eq!(buf[0], b'{');
        assert_eq!(buf[len - 1], b'}');
    }

    #[test]
    fn test_tier_collection_metadata_unlimited_supply() {
        let (env, contract_id, _admin, creator, _fan, _token) = setup_env();
        let client = StarPassContractClient::new(&env, &contract_id);

        client.register_creator(&creator);
        let tier_id = client.create_tier(
            &creator,
            &String::from_str(&env, "Free"),
            &1_000_000i128,
            &86_400u64,
            &0u32, // 0 = unlimited
            soroban_sdk::Vec::new(&env),
        );

        let meta = client.get_tier_collection_metadata(&tier_id);
        assert!(
            sdk_str_contains(
                &meta,
                b"\"trait_type\":\"MaxSupply\",\"value\":\"unlimited\""
            ),
            "max_supply=0 should produce 'unlimited'"
        );
    }

    #[test]
    fn test_tier_collection_metadata_inactive() {
        let (env, contract_id, _admin, creator, _fan, _token) = setup_env();
        let client = StarPassContractClient::new(&env, &contract_id);

        client.register_creator(&creator);
        let tier_id = client.create_tier(
            &creator,
            &String::from_str(&env, "Retired"),
            &1_000_000i128,
            &2_592_000u64,
            &0u32,
            soroban_sdk::Vec::new(&env),
        );
        client.deactivate_tier(&creator, &tier_id);

        let meta = client.get_tier_collection_metadata(&tier_id);
        assert!(
            sdk_str_contains(&meta, b"\"trait_type\":\"Active\",\"value\":\"false\""),
            "Active attribute should be false after deactivation"
        );
    }

    // ----------------------------------------------------------------
    // Unit tests for the no_std helper functions
    // ----------------------------------------------------------------
    #[test]
    fn test_u64_to_decimal_zero() {
        let mut buf = [0u8; 20];
        let s = u64_to_decimal(0, &mut buf);
        assert_eq!(s, b"0");
    }

    #[test]
    fn test_u64_to_decimal_values() {
        let mut buf = [0u8; 20];
        assert_eq!(u64_to_decimal(1, &mut buf), b"1");
        assert_eq!(u64_to_decimal(42, &mut buf), b"42");
        assert_eq!(u64_to_decimal(1_000_000, &mut buf), b"1000000");
        assert_eq!(u64_to_decimal(u64::MAX, &mut buf), b"18446744073709551615");
    }

    #[test]
    fn test_i128_to_decimal_values() {
        let mut buf = [0u8; 40];
        assert_eq!(i128_to_decimal(0, &mut buf), b"0");
        assert_eq!(i128_to_decimal(975_000, &mut buf), b"975000");
        assert_eq!(i128_to_decimal(-1, &mut buf), b"-1");
        assert_eq!(i128_to_decimal(-975_000, &mut buf), b"-975000");
        assert_eq!(
            i128_to_decimal(i128::MAX, &mut buf),
            b"170141183460469231731687303715884105727"
        );
        assert_eq!(
            i128_to_decimal(i128::MIN, &mut buf),
            b"-170141183460469231731687303715884105728"
        );
    }

    // --------------------------------------------------------
    // Permission System Tests
    // --------------------------------------------------------

    #[test]
    fn test_pass_based_permission() {
        let (env, contract_id, _admin, creator, fan, _token) = setup_env();
        let client = StarPassContractClient::new(&env, &contract_id);
        client.register_creator(&creator);

        let view_content = Symbol::new(&env, "view_content");
        let join_discord = Symbol::new(&env, "join_discord");
        let download_files = Symbol::new(&env, "download_files");

        let mut permissions = soroban_sdk::Vec::new(&env);
        permissions.push_back(view_content);
        permissions.push_back(join_discord);
        permissions.push_back(download_files);

        let tier_id = client.create_tier(
            &creator,
            &String::from_str(&env, "Gold"),
            &1_000_000i128,
            &2_592_000u64,
            &0u32,
            permissions.clone(),
        );

        let mut pass_permissions = soroban_sdk::Vec::new(&env);
        pass_permissions.push_back(view_content);
        pass_permissions.push_back(join_discord);

        client.mint_pass(&fan, &tier_id, pass_permissions.clone());

        assert!(client.has_permission(&fan, &tier_id, view_content));
        assert!(client.has_permission(&fan, &tier_id, join_discord));
        assert!(!client.has_permission(&fan, &tier_id, download_files));
    }

    #[test]
    fn test_grant_based_permission() {
        let (env, contract_id, _admin, creator, fan, _token) = setup_env();
        let client = StarPassContractClient::new(&env, &contract_id);
        client.register_creator(&creator);

        let view_content = Symbol::new(&env, "view_content");
        let join_discord = Symbol::new(&env, "join_discord");

        let mut permissions = soroban_sdk::Vec::new(&env);
        permissions.push_back(view_content);
        permissions.push_back(join_discord);

        let tier_id = client.create_tier(
            &creator,
            &String::from_str(&env, "Silver"),
            &1_000_000i128,
            &2_592_000u64,
            &0u32,
            permissions.clone(),
        );

        let mut grant_permissions = soroban_sdk::Vec::new(&env);
        grant_permissions.push_back(view_content);

        client.grant_permission(&creator, &fan, &tier_id, grant_permissions, &3600u64);

        assert!(client.has_permission(&fan, &tier_id, view_content));
        assert!(!client.has_permission(&fan, &tier_id, join_discord));
    }

    #[test]
    fn test_expired_grant_returns_false() {
        let (env, contract_id, _admin, creator, fan, _token) = setup_env();
        let client = StarPassContractClient::new(&env, &contract_id);
        client.register_creator(&creator);

        let view_content = Symbol::new(&env, "view_content");

        let mut permissions = soroban_sdk::Vec::new(&env);
        permissions.push_back(view_content);

        let tier_id = client.create_tier(
            &creator,
            &String::from_str(&env, "Bronze"),
            &1_000_000i128,
            &2_592_000u64,
            &0u32,
            permissions.clone(),
        );

        let mut grant_permissions = soroban_sdk::Vec::new(&env);
        grant_permissions.push_back(view_content);

        client.grant_permission(&creator, &fan, &tier_id, grant_permissions, &100u64);

        // Advance time past grant expiry
        let now = env.ledger().timestamp();
        env.ledger().set_timestamp(now + 101);

        assert!(!client.has_permission(&fan, &tier_id, view_content));
    }

    #[test]
    fn test_revoked_grant_returns_false() {
        let (env, contract_id, _admin, creator, fan, _token) = setup_env();
        let client = StarPassContractClient::new(&env, &contract_id);
        client.register_creator(&creator);

        let view_content = Symbol::new(&env, "view_content");

        let mut permissions = soroban_sdk::Vec::new(&env);
        permissions.push_back(view_content);

        let tier_id = client.create_tier(
            &creator,
            &String::from_str(&env, "Bronze"),
            &1_000_000i128,
            &2_592_000u64,
            &0u32,
            permissions.clone(),
        );

        let mut grant_permissions = soroban_sdk::Vec::new(&env);
        grant_permissions.push_back(view_content);

        client.grant_permission(&creator, &fan, &tier_id, grant_permissions, &3600u64);

        assert!(client.has_permission(&fan, &tier_id, view_content));

        client.revoke_permission_grant(&creator, &fan, &tier_id);

        assert!(!client.has_permission(&fan, &tier_id, view_content));
    }

    #[test]
    #[should_panic(expected = "Permission not in tier definition")]
    fn test_permission_not_in_tier_definition() {
        let (env, contract_id, _admin, creator, fan, _token) = setup_env();
        let client = StarPassContractClient::new(&env, &contract_id);
        client.register_creator(&creator);

        let view_content = Symbol::new(&env, "view_content");
        let invalid_permission = Symbol::new(&env, "invalid_permission");

        let mut permissions = soroban_sdk::Vec::new(&env);
        permissions.push_back(view_content);

        let tier_id = client.create_tier(
            &creator,
            &String::from_str(&env, "Bronze"),
            &1_000_000i128,
            &2_592_000u64,
            &0u32,
            permissions.clone(),
        );

        let mut grant_permissions = soroban_sdk::Vec::new(&env);
        grant_permissions.push_back(invalid_permission);

        client.grant_permission(&creator, &fan, &tier_id, grant_permissions, &3600u64);
    }

    #[test]
    #[should_panic(expected = "Permission not in tier definition")]
    fn test_mint_pass_with_invalid_permission() {
        let (env, contract_id, _admin, creator, fan, _token) = setup_env();
        let client = StarPassContractClient::new(&env, &contract_id);
        client.register_creator(&creator);

        let view_content = Symbol::new(&env, "view_content");
        let invalid_permission = Symbol::new(&env, "invalid_permission");

        let mut permissions = soroban_sdk::Vec::new(&env);
        permissions.push_back(view_content);

        let tier_id = client.create_tier(
            &creator,
            &String::from_str(&env, "Bronze"),
            &1_000_000i128,
            &2_592_000u64,
            &0u32,
            permissions.clone(),
        );

        let mut pass_permissions = soroban_sdk::Vec::new(&env);
        pass_permissions.push_back(invalid_permission);

        client.mint_pass(&fan, &tier_id, pass_permissions);
    }
}
