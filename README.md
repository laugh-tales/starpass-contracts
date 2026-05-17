# StarPass Contracts

Soroban smart contracts for StarPass — a creator membership platform on Stellar where fans mint on-chain access passes to exclusive content tiers.

[![CI](https://github.com/laugh-tales/starpass-contracts/actions/workflows/ci.yml/badge.svg)](https://github.com/laugh-tales/starpass-contracts/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)

## What It Does

StarPass enables creators to define tiered memberships (Bronze, Silver, Gold) on-chain. Fans purchase access passes for a tier by paying USDC. The contract handles fee splitting, pass expiry, and creator withdrawals — all trustlessly on Stellar Soroban.

**How it works:**
1. A creator registers and defines membership tiers with a name, USDC price, and duration
2. A fan mints a pass for a tier — USDC is transferred, a Pass record is created on-chain
3. Any app or contract can call `has_valid_pass()` to gate access to content
4. The creator withdraws their earnings at any time

## Contract Functions

| Function | Who Calls It | Description |
|---|---|---|
| `initialize(admin, token, fee_bps)` | Deployer | Set up contract with USDC token and protocol fee |
| `register_creator(creator)` | Creator | Register as a creator on StarPass |
| `create_tier(creator, name, price, duration, max_supply)` | Creator | Define a new membership tier |
| `deactivate_tier(creator, tier_id)` | Creator | Stop new pass sales for a tier |
| `update_tier_price(creator, tier_id, new_price)` | Creator | Change tier price for future purchases |
| `mint_pass(fan, tier_id)` | Fan | Buy a pass — pays USDC, receives access |
| `withdraw(creator)` | Creator | Withdraw accumulated earnings |
| `has_valid_pass(fan, tier_id)` | Anyone | Check if fan has active non-expired pass |
| `get_pass(pass_id)` | Anyone | Get pass details |
| `get_tier(tier_id)` | Anyone | Get tier details |
| `get_creator(creator)` | Anyone | Get creator profile |
| `get_creator_balance(creator)` | Anyone | Get creator's pending withdrawal balance |
| `get_fan_passes(fan)` | Anyone | Get all pass IDs owned by a fan |
| `get_creator_tiers(creator)` | Anyone | Get all tier IDs for a creator |

## Pass Lifecycle
Creator                    Contract                      Fan
|                           |                          |
|-- register_creator() --> |                          |
|-- create_tier() -------> | (tier stored on-chain)   |
|                           |                          |
|                           | <---- mint_pass() ------ |
|                           | (USDC transferred)       |
|                           | (Pass record created)    |
|                           | (Creator balance credited)|
|                           |                          |
|                           | <-- has_valid_pass()? -- |
|                           | --> true/false           |
|                           |                          |
|-- withdraw() ----------> |                          |
|                           | (USDC sent to creator)   |

## Fee Model

StarPass charges a configurable protocol fee (default 2.5%) on every pass purchase.

- Fan pays: `tier.price` (full USDC amount)
- Creator receives: `tier.price × (1 - fee_bps / 10000)`
- Protocol receives: `tier.price × (fee_bps / 10000)`

Example: Fan buys a 10 USDC pass with 2.5% fee → Creator gets 9.75 USDC, protocol gets 0.25 USDC.

## Build

**Prerequisites:**
- Rust 1.75+
- Soroban CLI ([install guide](https://developers.stellar.org/docs/smart-contracts/getting-started/setup))

```bash
# Install WASM target
rustup target add wasm32-unknown-unknown

# Clone and build
git clone https://github.com/laugh-tales/starpass-contracts
cd starpass-contracts
cargo build

# Run all tests
cargo test

# Build WASM contract
cargo build --target wasm32-unknown-unknown --release
```

## Deploy to Testnet

```bash
# Generate a testnet identity (first time only)
soroban keys generate --global deployer --network testnet

# Fund from friendbot
soroban keys fund deployer --network testnet

# Deploy the contract
soroban contract deploy \
  --wasm target/wasm32-unknown-unknown/release/starpass.wasm \
  --network testnet \
  --source deployer

# Initialize (replace CONTRACT_ID and TOKEN_ADDRESS)
soroban contract invoke \
  --id CONTRACT_ID \
  --network testnet \
  --source deployer \
  -- initialize \
  --admin $(soroban keys address deployer) \
  --token TOKEN_ADDRESS \
  --fee_bps 250
```

## Project Structure
starpass-contracts/
├── contracts/
│   └── starpass/            # Main Soroban contract
│       ├── src/
│       │   └── lib.rs       # Contract + tests (11 test cases)
│       └── Cargo.toml
├── docs/
│   ├── ARCHITECTURE.md      # System design and data model
│   └── DEPLOYMENT.md        # Testnet and mainnet deployment guide
├── scripts/
│   └── deploy.sh            # Deploy to testnet
├── .github/
│   └── workflows/
│       └── ci.yml           # Test + lint + WASM build
├── CONTRIBUTING.md
├── Cargo.toml
└── README.md

## Contributing

This project participates in the [Stellar Wave Program](https://drips.network/wave/stellar) on Drips. Contributors earn USDC rewards for resolving issues.

See [CONTRIBUTING.md](./CONTRIBUTING.md) for setup and contribution guidelines.

Issues are labeled by complexity:
- 🟢 `good first issue` — Tests, docs, small fixes (100 pts)
- 🟡 `enhancement` — New features, improvements (150 pts)
- 🔴 `high complexity` — Architecture changes, new modules (200 pts)

## Related Repositories

| Repo | Description |
|---|---|
| [starpass-backend](https://github.com/laugh-tales/starpass-backend) | NestJS API and Soroban event indexer |
| [starpass-frontend](https://github.com/laugh-tales/starpass-frontend) | Next.js creator and fan dashboard |

## License

MIT — see [LICENSE](./LICENSE) for details.
