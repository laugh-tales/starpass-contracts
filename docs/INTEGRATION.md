# StarPass Integration Guide

This guide is for frontend and backend developers who want to gate access to content using StarPass. You do not need to understand the full contract internals — you only need to call one function: `has_valid_pass()`.

---

## Section 1: Overview

StarPass is a creator membership contract deployed on Stellar Soroban. Creators define membership tiers (e.g. Bronze, Silver, Gold), and fans purchase time-limited access passes by paying USDC on-chain.

The primary integration point for any application is:

```
has_valid_pass(fan: Address, tier_id: u32) → bool
```

**What it does:**
- Takes a fan's Stellar address and a tier ID
- Returns `true` if that fan owns at least one active, non-expired pass for that tier
- Returns `false` in all other cases (no pass, expired pass, never purchased)

**Why this matters:** Your app never needs to manage subscriptions, sessions, or expiry logic. The contract is the source of truth. Call `has_valid_pass()` before serving any gated content and act on the boolean result.

**Key facts:**
- Passes are time-bound (set by the creator's tier `duration` in seconds)
- A pass expires when `ledger.timestamp > pass.expires_at`
- Deactivating a tier stops new sales but does not invalidate existing passes
- The function requires no authentication — it is a free read-only simulation

---

## Section 2: JavaScript / TypeScript Integration

Use the official [`@stellar/stellar-sdk`](https://www.npmjs.com/package/@stellar/stellar-sdk) to simulate a call to `has_valid_pass()` from a browser or Node.js environment.

### Install

```bash
npm install @stellar/stellar-sdk
```

### starpass.ts — reusable helper

```typescript
import {
  Contract,
  Networks,
  TransactionBuilder,
  nativeToScVal,
  scValToNative,
  rpc,
} from "@stellar/stellar-sdk";

const RPC_URL = "https://soroban-testnet.stellar.org";
const CONTRACT_ID = "REPLACE_WITH_DEPLOYED_CONTRACT_ID"; // see Section 5
const NETWORK_PASSPHRASE = Networks.TESTNET;

/**
 * Returns true if `fanAddress` holds an active, non-expired pass for `tierId`.
 * Uses simulateTransaction — no fee, no signing required.
 */
export async function hasValidPass(
  fanAddress: string,
  tierId: number
): Promise<boolean> {
  const server = new rpc.Server(RPC_URL);
  const contract = new Contract(CONTRACT_ID);

  // simulateTransaction requires a source account on-chain.
  // Use the fan's own address as the source — it is not charged.
  const account = await server.getAccount(fanAddress);

  const tx = new TransactionBuilder(account, {
    fee: "100",
    networkPassphrase: NETWORK_PASSPHRASE,
  })
    .addOperation(
      contract.call(
        "has_valid_pass",
        nativeToScVal(fanAddress, { type: "address" }),
        nativeToScVal(tierId, { type: "u32" })
      )
    )
    .setTimeout(30)
    .build();

  const simResult = await server.simulateTransaction(tx);

  if ("error" in simResult) {
    // Contract panicked (e.g. tier_id does not exist) — treat as no access
    return false;
  }

  return scValToNative(simResult.result!.retval) as boolean;
}
```

### Usage example

```typescript
import { hasValidPass } from "./starpass";

const fanAddress = "GFANADDRESSHERE..."; // the logged-in user's Stellar address
const TIER_ID = 1;

const isSubscriber = await hasValidPass(fanAddress, TIER_ID);

if (isSubscriber) {
  showPremiumContent();
} else {
  showUpgradeBanner();
}
```

### Notes
- `simulateTransaction` is a read-only RPC call — it does not submit anything to the network and costs nothing.
- `nativeToScVal(fanAddress, { type: "address" })` converts a plain string address to the Soroban `ScVal` type the contract expects.
- `scValToNative()` converts the raw `ScVal` return value back to a plain JS `boolean`.
- Wrap calls in `try/catch` to handle RPC timeouts or network errors gracefully.

---

## Section 3: Backend API Gating

Gate a REST API route so only valid pass holders can access it. The pattern below works for both **NestJS** and **Express**.

### NestJS — Guard + Decorator

```typescript
// starpass.service.ts
import { Injectable } from "@nestjs/common";
import { hasValidPass } from "./starpass"; // the helper from Section 2

@Injectable()
export class StarPassService {
  async validatePass(fanAddress: string, tierId: number): Promise<boolean> {
    return hasValidPass(fanAddress, tierId);
  }
}
```

```typescript
// starpass.guard.ts
import {
  Injectable,
  CanActivate,
  ExecutionContext,
  ForbiddenException,
  UnauthorizedException,
} from "@nestjs/common";
import { Reflector } from "@nestjs/core";
import { StarPassService } from "./starpass.service";

export const TIER_KEY = "starpass_tier";

/** Attach to a route with @RequirePass(tierId) */
export const RequirePass = (tierId: number) =>
  SetMetadata(TIER_KEY, tierId);

@Injectable()
export class StarPassGuard implements CanActivate {
  constructor(
    private readonly reflector: Reflector,
    private readonly starPassService: StarPassService
  ) {}

  async canActivate(context: ExecutionContext): Promise<boolean> {
    const tierId = this.reflector.get<number>(TIER_KEY, context.getHandler());
    if (tierId === undefined) return true; // no tier required on this route

    const request = context.switchToHttp().getRequest();
    const fanAddress: string | undefined = request.user?.stellarAddress;

    if (!fanAddress) {
      throw new UnauthorizedException("Stellar address not found on request");
    }

    const hasPass = await this.starPassService.validatePass(fanAddress, tierId);

    if (!hasPass) {
      throw new ForbiddenException(
        `A valid StarPass for tier ${tierId} is required`
      );
    }

    return true;
  }
}
```

```typescript
// content.controller.ts
import { Controller, Get, UseGuards } from "@nestjs/common";
import { StarPassGuard, RequirePass } from "./starpass.guard";

@Controller("content")
@UseGuards(StarPassGuard)
export class ContentController {
  @Get("free")
  getFreeContent() {
    return { message: "This content is available to everyone." };
  }

  @Get("gold")
  @RequirePass(1) // tier ID 1 = Gold
  getGoldContent() {
    return { message: "Welcome, Gold member. Here is your exclusive content." };
  }

  @Get("platinum")
  @RequirePass(2) // tier ID 2 = Platinum
  getPlatinumContent() {
    return { message: "Platinum-only content." };
  }
}
```

Register the guard and service in your module:

```typescript
// app.module.ts (relevant excerpt)
import { APP_GUARD } from "@nestjs/core";
import { StarPassGuard } from "./starpass.guard";
import { StarPassService } from "./starpass.service";

@Module({
  providers: [
    StarPassService,
    { provide: APP_GUARD, useClass: StarPassGuard },
  ],
  controllers: [ContentController],
})
export class AppModule {}
```

### Express — Middleware factory

```typescript
// starpassMiddleware.ts
import { Request, Response, NextFunction } from "express";
import { hasValidPass } from "./starpass";

/**
 * Express middleware factory.
 * Usage: app.use("/premium", requirePass(1))
 */
export function requirePass(tierId: number) {
  return async (req: Request, res: Response, next: NextFunction) => {
    const fanAddress: string | undefined = (req as any).user?.stellarAddress;

    if (!fanAddress) {
      return res.status(401).json({ error: "Stellar address required" });
    }

    try {
      const valid = await hasValidPass(fanAddress, tierId);
      if (!valid) {
        return res
          .status(403)
          .json({ error: `Valid StarPass for tier ${tierId} required` });
      }
      next();
    } catch (err) {
      return res.status(503).json({ error: "StarPass check failed, try again" });
    }
  };
}
```

```typescript
// routes.ts
import express from "express";
import { requirePass } from "./starpassMiddleware";

const router = express.Router();

router.get("/public", (req, res) => {
  res.json({ message: "Public content" });
});

router.get("/premium", requirePass(1), (req, res) => {
  res.json({ message: "Premium content — tier 1 pass verified" });
});

export default router;
```

---

## Section 4: Contract-to-Contract Integration

Another Soroban contract can call `has_valid_pass()` directly using cross-contract invocation. This is the recommended approach when you need access control enforced entirely on-chain — no backend involved.

### Define the StarPass interface in your contract

```rust
#![no_std]

use soroban_sdk::{contract, contractclient, contractimpl, Address, Env};

/// Declare the StarPass contract interface.
/// Only list the functions your contract needs to call.
#[contractclient(name = "StarPassClient")]
pub trait StarPassInterface {
    fn has_valid_pass(env: Env, fan: Address, tier_id: u32) -> bool;
}
```

### Use it inside your contract

```rust
#[contract]
pub struct MyGatedContract;

#[contractimpl]
impl MyGatedContract {
    /// Perform a gated action that requires a valid StarPass.
    ///
    /// # Arguments
    /// * `caller`            - The user requesting access (must sign the transaction)
    /// * `starpass_contract` - The deployed StarPass contract address
    /// * `tier_id`           - The tier ID required for access
    pub fn gated_action(
        env: Env,
        caller: Address,
        starpass_contract: Address,
        tier_id: u32,
    ) {
        // Require the caller to have signed this transaction
        caller.require_auth();

        // Instantiate a cross-contract client pointing at the StarPass contract
        let starpass = StarPassClient::new(&env, &starpass_contract);

        // Call has_valid_pass — this executes synchronously within the same transaction
        let has_pass = starpass.has_valid_pass(&caller, &tier_id);

        assert!(has_pass, "A valid StarPass is required to perform this action");

        // Your gated logic goes here
        // e.g. record an entry, unlock a feature, emit an event
        env.events().publish(
            (soroban_sdk::Symbol::new(&env, "access_granted"),),
            (caller, tier_id),
        );
    }
}
```

### How it works

1. `StarPassClient::new(&env, &starpass_contract)` creates a typed client bound to the deployed StarPass contract address.
2. `starpass.has_valid_pass(&caller, &tier_id)` executes a synchronous cross-contract call within the same Soroban transaction. The result is available immediately.
3. If the fan does not hold a valid pass, `assert!` panics and the entire transaction is rolled back — no partial state changes occur.
4. The `starpass_contract` address should be passed in as a parameter (or stored in your contract's instance storage during initialization) rather than hardcoded, so it can be updated if the StarPass contract is redeployed.

### Cargo.toml dependency

Your contract's `Cargo.toml` only needs the standard Soroban SDK — no additional StarPass crate is required:

```toml
[dependencies]
soroban-sdk = { version = "21.0.0", features = [] }
```

The `#[contractclient]` macro generates the client entirely from the trait declaration at compile time.

---

## Section 5: Testnet Contract Address

The StarPass contract is deployed on Stellar Testnet. Replace the placeholder below with the actual contract ID after deployment.

```
STARPASS_CONTRACT_ID=REPLACE_WITH_DEPLOYED_CONTRACT_ID
NETWORK=testnet
RPC_URL=https://soroban-testnet.stellar.org
NETWORK_PASSPHRASE=Test SDF Network ; September 2015
```

**To get the deployed contract ID:**

1. Follow the steps in [DEPLOYMENT.md](./DEPLOYMENT.md) to deploy the contract.
2. Copy the contract address printed by `soroban contract deploy`.
3. Update `CONTRACT_ID` in your `starpass.ts` helper (Section 2) and in any environment config files.

You can verify the contract is live with:

```bash
soroban contract invoke \
  --id REPLACE_WITH_DEPLOYED_CONTRACT_ID \
  --network testnet \
  -- get_pass_count
```

This should return `0` on a fresh deployment and requires no signing or fees.
