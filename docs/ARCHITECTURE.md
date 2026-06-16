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
| `withdraw` | Creator signature |
| All `get_*` functions | None (read-only) |

## Functions

- `has_any_valid_pass(env: Env, fan: Address, creator: Address) -> bool`
  - Returns `true` if the specified fan holds any active, non-expired pass issued by the given creator. Iterates over the fan's passes and checks `pass.creator == creator`, `pass.active`, and `pass.expires_at > now`.
  - Used to determine creator-exclusive content access regardless of tier.

- `get_creator_tiers(env: Env, creator: Address) -> Vec<u32>`
  - Returns all tier IDs for a creator as a single list. Suitable for creators with a small number of tiers.

- `get_creator_tiers_page(env: Env, creator: Address, offset: u32, limit: u32) -> Vec<u32>`
  - Returns a paginated slice of a creator's tier IDs.
  - `offset` is the zero-based start index; `limit` is the max number of items to return.
  - `limit` is capped at **20** — panics with `"limit cannot exceed 20"` if exceeded.
  - Returns an empty `Vec` when `offset` is beyond the end of the list (does not panic).
  - Use this instead of `get_creator_tiers` for creators with many tiers to avoid hitting Soroban instruction limits.

## Events

| Event | Data |
|---|---|
| `initialized` | admin, token, fee_bps |
| `creator_registered` | creator, timestamp |
| `tier_created` | tier_id, creator, price, duration |
| `tier_deactivated` | tier_id, creator |
| `tier_price_updated` | tier_id, old_price, new_price |
| `pass_minted` | pass_id, tier_id, fan, expires_at |
| `creator_withdrew` | creator, amount |
| `fees_withdrawn` | recipient, amount |
