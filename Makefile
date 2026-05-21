# StarPass Contracts — Makefile

.PHONY: build test fmt lint deploy clean

## Build the WASM contract
build:
	cargo build --target wasm32-unknown-unknown --release

## Run all tests
test:
	cargo test

## Format code
fmt:
	cargo fmt

## Run clippy linter
lint:
	cargo clippy -- -D warnings

## Check formatting without fixing
fmt-check:
	cargo fmt --all -- --check

## Build + test + lint in one command (mirrors CI)
ci: fmt-check lint test build

## Deploy to testnet (requires TOKEN_ADDRESS and soroban CLI)
deploy:
	@echo "Building contract..."
	cargo build --target wasm32-unknown-unknown --release
	@echo "Deploying to testnet..."
	soroban contract deploy \
		--wasm target/wasm32-unknown-unknown/release/starpass.wasm \
		--network testnet \
		--source deployer

## Clean build artifacts
clean:
	cargo clean

## Install required tooling
setup:
	rustup target add wasm32-unknown-unknown
	cargo install --locked soroban-cli
