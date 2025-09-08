#!/bin/bash
set -e

# --------------------------
# Config
# --------------------------
RPC_URL="https://api.devnet.solana.com"
KEYPAIR="./keypair.json"
TOKEN_MINT="EzvhQ9QkpT6T5Aj6wKpYmrhZyx33L9T1AEGzv5tHxVYd"  # USDC devnet
AMOUNT_LAMPORTS=1000000  # 0.001 SOL

# --------------------------
# Step 1: Make sure we’re in repo root
# --------------------------
cd "$(dirname "$0")"

# --------------------------
# Step 2: Ensure wallet exists
# --------------------------
if [ ! -f "$KEYPAIR" ]; then
  echo "[*] Generating new Solana keypair..."
  solana-keygen new -o "$KEYPAIR" --no-bip39-passphrase
  solana config set --url $RPC_URL
  echo "[*] Requesting airdrop of 2 SOL..."
  solana airdrop 2 "$KEYPAIR" || true
fi

# --------------------------
# Step 3: Build if needed
# --------------------------
if [ ! -f "target/release/bonkfun-sniper" ]; then
  echo "[*] Building bonkfun-sniper..."
  cargo build --release
fi

# --------------------------
# Step 4: Run bot
# --------------------------
echo "[*] Running sniper on devnet..."
cargo run --release -- \
  --rpc-url $RPC_URL \
  --keypair-path $KEYPAIR \
  --token-mint $TOKEN_MINT \
  --amount-in-lamports $AMOUNT_LAMPORTS

