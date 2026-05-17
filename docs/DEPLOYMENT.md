# Deployment Guide

## Testnet Deployment

### 1. Install Prerequisites

```bash
# Install Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
rustup target add wasm32-unknown-unknown

# Install Soroban CLI
cargo install --locked soroban-cli
```

### 2. Build the Contract

```bash
cargo build --target wasm32-unknown-unknown --release
```

Output: `target/wasm32-unknown-unknown/release/starpass.wasm`

### 3. Set Up Testnet Identity

```bash
# Generate deployer keypair
soroban keys generate --global deployer --network testnet

# Fund from Stellar Friendbot
soroban keys fund deployer --network testnet

# Check balance
soroban keys address deployer
```

### 4. Deploy USDC Token (Testnet Only)

For testnet, deploy a mock USDC token:

```bash
# Deploy a Stellar Asset Contract as mock USDC
soroban contract deploy \
  --wasm ~/.soroban/stellar-asset-contract.wasm \
  --network testnet \
  --source deployer
```

Note the TOKEN_ADDRESS output.

### 5. Deploy StarPass Contract

```bash
soroban contract deploy \
  --wasm target/wasm32-unknown-unknown/release/starpass.wasm \
  --network testnet \
  --source deployer
```

Note the CONTRACT_ID output.

### 6. Initialize the Contract

```bash
soroban contract invoke \
  --id CONTRACT_ID \
  --network testnet \
  --source deployer \
  -- initialize \
  --admin $(soroban keys address deployer) \
  --token TOKEN_ADDRESS \
  --fee_bps 250
```

### 7. Verify Deployment

```bash
# Check tier count (should be 0)
soroban contract invoke \
  --id CONTRACT_ID \
  --network testnet \
  -- get_tier_count

# Check pass count (should be 0)
soroban contract invoke \
  --id CONTRACT_ID \
  --network testnet \
  -- get_pass_count
```

## Example Contract Interactions

### Register as Creator

```bash
soroban contract invoke \
  --id CONTRACT_ID \
  --network testnet \
  --source my-creator-account \
  -- register_creator \
  --creator $(soroban keys address my-creator-account)
```

### Create a Membership Tier

```bash
soroban contract invoke \
  --id CONTRACT_ID \
  --network testnet \
  --source my-creator-account \
  -- create_tier \
  --creator $(soroban keys address my-creator-account) \
  --name "Gold Member" \
  --price 10000000 \
  --duration 2592000 \
  --max_supply 0
```

### Mint a Pass (Fan)

```bash
soroban contract invoke \
  --id CONTRACT_ID \
  --network testnet \
  --source fan-account \
  -- mint_pass \
  --fan $(soroban keys address fan-account) \
  --tier_id 1
```

### Check Valid Pass

```bash
soroban contract invoke \
  --id CONTRACT_ID \
  --network testnet \
  -- has_valid_pass \
  --fan FAN_ADDRESS \
  --tier_id 1
```

### Creator Withdraw

```bash
soroban contract invoke \
  --id CONTRACT_ID \
  --network testnet \
  --source my-creator-account \
  -- withdraw \
  --creator $(soroban keys address my-creator-account)
```
