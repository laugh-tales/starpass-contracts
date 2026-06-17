# Architecture

## Overview

StarPass is a single Soroban smart contract deployed on Stellar. All membership state — creators, tiers, passes, balances — lives on-chain. There is no backend required to verify access; any application calls `has_valid_pass()` directly against the contract.

## Core Entities

### Creator
A Stellar address that has called `register_creator()`. Stores total lifetime earnings and pass count. Creators define tiers and withdraw earnings.

### Tier
A membership level defined by a creator. Has a name, USDC price, duration in seconds, and optional max supply cap. Tiers can be deactivated (no new passes) or have their price updated.

### Pass
A time-bound access record minted when a fan purchases a tier. Stores the owner, tier, creator, purchase timestamp, and expiry timestamp. Passes are never deleted — they simply expire.

## Storage Model

| Key | Storage Type | Description |
|---|---|---|
| `Admin` | `instance` | Contract admin address |
| `Token` | `instance` | USDC token contract address |
| `ProtocolFeeBps` | `instance` | Protocol fee in basis points |
| `TierCount` | `instance` | Global tier ID counter |
| `PassCount` | `instance` | Global pass ID counter |
| `Creator(address)` | `persistent` | Creator profile |
| `CreatorBalance(address)` | `persistent` | Pending withdrawal balance |
| `CreatorTiers(address)` | `persistent` | Vec of tier IDs per creator |
| `Tier(tier_id)` | `persistent` | Tier struct |
| `Pass(pass_id)` | `persistent` | Pass struct |
| `FanPasses(address)` | `persistent` | Vec of pass IDs per fan |

`instance` storage shares a single TTL with the contract instance — used for global config.
`persistent` storage has independent TTL per key — used for user data that must survive long-term.

## Fee Flow

```
Fan pays: tier.price (USDC)
    │
    ├── protocol_fee = price × fee_bps / 10_000  → stays in contract (admin withdraws)
    └── creator_amount = price - protocol_fee     → credited to CreatorBalance(creator)

Creator calls withdraw() → creator_amount transferred to creator wallet
```

## Access Control

| Function | Auth Required |
|---|---|
| `initialize` | Admin signature |
| `set_fee` | Admin signature |
| `withdraw_fees` | Admin signature |
| `register_creator` | Creator signature |
| `create_tier` | Creator signature (must be registered) |
| `deactivate_tier` | Creator signature (must own the tier) |
| `update_tier_price` | Creator signature (must own the tier) |
| `mint_pass` | Fan signature |
| `renew_pass` | Fan signature (must own the pass) |
| `withdraw` | Creator signature |
| All `get_*` functions | None (read-only) |

## Functions

- `has_any_valid_pass(env: Env, fan: Address, creator: Address) -> bool`
  - Returns `true` if the specified fan holds any active, non‑expired pass issued by the given creator. Iterates over the fan’s passes and checks `pass.creator == creator`, `pass.active`, and `pass.expires_at > now`.
  - Used to determine creator‑exclusive content access regardless of tier.

- `renew_pass(env: Env, fan: Address, pass_id: u64) -> u64`
  - Extends an existing pass instead of minting a new one for the same fan/tier, keeping pass history clean and avoiding duplicate records. Requires `fan.require_auth()`, `pass.owner == fan`, and `pass.active`. Charges `tier.price` USDC with the same fee split as `mint_pass`, crediting `CreatorBalance` and `Creator.total_earned` (does not increment `pass_count`, since no new pass is created).
  - New expiry is `tier.duration` seconds added to `max(now, pass.expires_at)` — renewing before expiry stacks onto remaining time; renewing after expiry starts the new period from now. Returns the new `expires_at`.

| `initialized` | admin, token, fee_bps |
| `creator_registered` | creator, timestamp |
| `tier_created` | tier_id, creator, price, duration |
| `tier_deactivated` | tier_id, creator |
| `tier_price_updated` | tier_id, old_price, new_price |
| `pass_minted` | pass_id, tier_id, fan, expires_at |
| `pass_renewed` | pass_id, fan, new_expires_at |
| `creator_withdrew` | creator, amount |
| `fees_withdrawn` | recipient, amount |
