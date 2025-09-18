use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use base64::{Engine, engine::general_purpose};
use serde_json::json;
use solana_client::{
    nonblocking::{rpc_client::RpcClient, tpu_client::TpuClient},
    rpc_client::SerializableTransaction,
    rpc_config::RpcSendTransactionConfig,
};
use solana_program::pubkey;
use solana_quic_client::{QuicConfig, QuicConnectionManager, QuicPool};
use solana_sdk::{
    commitment_config::CommitmentConfig,
    hash::Hash,
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
    signature::Keypair,
    signer::Signer as _,
    transaction::{Transaction, VersionedTransaction},
};
use solana_system_interface::instruction as system_instruction;
use tokio::{
    sync::RwLock,
    time::interval,
};
use tracing::{error, info, instrument};
use futures;

use crate::{
    jito,
    swap::{self, SwapBaseIn},
};

pub struct TxSender {
    keypair: Keypair,
    client: Arc<RpcClient>,
    latest_blockhash: Arc<RwLock<Vec<Hash>>>,
    token_mint: Pubkey,
    jito_client: Option<jito::Client>,
    bloxroute_api_key: Option<String>,
    zeroslot_api_key: Option<String>,
    amount_in: u64,
    // Pre-computed optimizations
    base_instructions: Vec<Instruction>,
    tip_addresses: TipAddresses,
    http_client: Arc<reqwest::Client>,
    backup_rpc_client: Arc<RpcClient>,
}

#[derive(Clone)]
struct TipAddresses {
    zeroslot: Pubkey,
    bloxroute: Pubkey, 
    jito: Pubkey,
}

impl TxSender {
    pub async fn new(
        keypair: Keypair,
        client: Arc<RpcClient>,
        token_mint: Pubkey,
        jito_client: Option<jito::Client>,
        bloxroute_api_key: Option<String>,
        zeroslot_api_key: Option<String>,
        amount_in: u64,
    ) -> Self {
        // Get initial blockhash
        let initial_blockhash = client
            .get_latest_blockhash()
            .await
            .expect("Failed to get initial blockhash");
        let latest_blockhash = Arc::new(RwLock::new(vec![initial_blockhash]));

        tokio::spawn(Self::blockhash_update_worker(
            client.clone(),
            latest_blockhash.clone(),
        ));

        // Pre-compute base instructions (everything except swap instruction)
        let base_instructions = Self::build_base_instructions(&keypair, token_mint, amount_in);
        
        // Pre-define tip addresses
        let tip_addresses = TipAddresses {
            zeroslot: pubkey!("FCjUJZ1qozm1e8romw216qyfQMaaWKxWsuySnumVCCNe"),
            bloxroute: pubkey!("95cfoy472fcQHaw4tPGBTKpn6ZQnfEPfBgDQx6gcRmRg"),
            jito: pubkey!("DfXygSm4jCyNCybVYYK6DwvWqjKee8pbDmJGcLWNDXjh"),
        };
        
        // Create optimized HTTP client with connection pooling
        let http_client = Arc::new(reqwest::Client::builder()
            .timeout(Duration::from_millis(5000))
            .tcp_keepalive(Duration::from_secs(60))
            .pool_idle_timeout(Duration::from_secs(90))
            .pool_max_idle_per_host(10)
            .http2_prior_knowledge()
            .build()
            .expect("Failed to create HTTP client"));
            
        // Pre-create backup RPC client
        let backup_rpc_client = Arc::new(RpcClient::new_with_commitment(
            "https://acc.solayer.org".to_string(),
            CommitmentConfig::processed(),
        ));
        
        // Use provided Jito client or None

        Self {
            keypair,
            client,
            latest_blockhash,
            token_mint,
            jito_client,
            bloxroute_api_key,
            zeroslot_api_key,
            amount_in,
            base_instructions,
            tip_addresses,
            http_client,
            backup_rpc_client,
        }
    }

    fn build_base_instructions(keypair: &Keypair, token_mint: Pubkey, amount_in: u64) -> Vec<Instruction> {
        let cu_budget = 2_000_000u32;
        let cu_price = 500_000u64;

        let set_cu_budget_ix = Instruction {
            program_id: pubkey!("ComputeBudget111111111111111111111111111111"),
            accounts: vec![AccountMeta::new_readonly(
                pubkey!("jitodontfront11111111111111111111TrentLover"),
                false,
            )],
            data: {
                let mut data = vec![2u8]; // SetComputeUnitLimit discriminator
                data.extend_from_slice(&cu_budget.to_le_bytes());
                data
            },
        };

        let set_cu_price_ix = Instruction {
            program_id: pubkey!("ComputeBudget111111111111111111111111111111"),
            accounts: vec![],
            data: {
                let mut data = vec![3u8]; // SetComputeUnitPrice discriminator
                data.extend_from_slice(&cu_price.to_le_bytes());
                data
            },
        };

        let create_ata_base_ix = create_associated_token_account_idempotent(
            &keypair.pubkey(),
            &keypair.pubkey(),
            &swap::WSOL_MINT,
            &pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"),
        );

        let create_ata_quote_ix = create_associated_token_account_idempotent(
            &keypair.pubkey(),
            &keypair.pubkey(),
            &token_mint,
            &pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"),
        );

        let transfer_to_wsol = system_instruction::transfer(
            &keypair.pubkey(),
            &Pubkey::find_program_address(
                &[
                    &keypair.pubkey().to_bytes(),
                    &pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA").to_bytes(),
                    &swap::WSOL_MINT.to_bytes(),
                ],
                &pubkey!("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL"),
            )
            .0,
            amount_in,
        );

        let sync_native_ix = Instruction {
            program_id: pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"),
            accounts: vec![AccountMeta::new(
                Pubkey::find_program_address(
                    &[
                        &keypair.pubkey().to_bytes(),
                        &pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA").to_bytes(),
                        &swap::WSOL_MINT.to_bytes(),
                    ],
                    &pubkey!("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL"),
                )
                .0,
                false,
            )],
            data: vec![17], // SyncNative instruction discriminator
        };

        vec![
            set_cu_budget_ix,
            set_cu_price_ix,
            create_ata_base_ix,
            create_ata_quote_ix,
            transfer_to_wsol,
            sync_native_ix,
        ]
    }

    pub async fn blockhash_update_worker(client: Arc<RpcClient>, blockhash: Arc<RwLock<Vec<Hash>>>) {
        let mut interval = interval(Duration::from_millis(150)); // Faster: 150ms instead of 200ms
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        
        loop {
            interval.tick().await;

            // Use timeout to avoid blocking the worker
            match tokio::time::timeout(Duration::from_millis(100), client.get_latest_blockhash()).await {
                Ok(Ok(new_blockhash)) => {
                    // Use try_write for non-blocking operation
                    if let Ok(mut blockhashes) = blockhash.try_write() {
                        blockhashes.push(new_blockhash);
                        if blockhashes.len() > 3 { // Keep fewer for speed
                            blockhashes.remove(0);
                        }
                    }
                }
                Ok(Err(e)) => {
                    error!("Failed to get blockhash: {:?}", e);
                }
                Err(_) => {
                    error!("Blockhash request timed out");
                }
            }
        }
    }

    pub async fn run_loop(self) {
        info!("Starting tx sender loop - 50ms intervals");

        let mut interval = tokio::time::interval(Duration::from_millis(1000)); 
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            interval.tick().await;
            self.buy_optimized().await;
        }
    }
    
    // Optimized buy function with pre-computed instructions and parallel sending
    async fn buy_optimized(&self) {
        // Pool reserves: 85 SOL and 206,900,000 tokens (should be updated dynamically in production)
        const RESERVE_SOL: u64 = 85_000_000_000;
        const RESERVE_TOKEN: u64 = 206_900_000_000_000;
        
        let amount_out = calculate_amount_out(self.amount_in, RESERVE_SOL, RESERVE_TOKEN);
        let swap_base_in = SwapBaseIn::new(
            self.keypair.pubkey(),
            self.token_mint,
            self.amount_in,
            amount_out,
        );

        // Build complete instruction set with pre-computed base + swap
        let mut instructions = self.base_instructions.clone();
        instructions.push(swap_base_in.ix());

        // Get latest blockhash (non-blocking read)
        let blockhash = {
            let blockhashes = self.latest_blockhash.read().await;
            *blockhashes.last().expect("No blockhashes available")
        };

        // Build all transactions in parallel using spawn_blocking for CPU-intensive work
        let (tx_rpc, tx_0slot, tx_bloxroute, tx_jito) = tokio::task::spawn_blocking({
            let instructions = instructions.clone();
            let keypair = self.keypair.insecure_clone();
            let tip_addresses = self.tip_addresses.clone();
            
            move || {
                let payer = keypair.pubkey();
                let tip_amount = 10_000_000; // 0.01 SOL
                
                // Base transaction (no tip)
                let tx_rpc = Transaction::new_signed_with_payer(
                    &instructions,
                    Some(&payer),
                    &[&keypair],
                    blockhash,
                );

                // Build tip transactions efficiently
                let mut ixs_0slot = instructions.clone();
                ixs_0slot.push(system_instruction::transfer(&payer, &tip_addresses.zeroslot, tip_amount));
                let tx_0slot = Transaction::new_signed_with_payer(
                    &ixs_0slot,
                    Some(&payer),
                    &[&keypair],
                    blockhash,
                );

                let mut ixs_bloxroute = instructions.clone();
                ixs_bloxroute.push(system_instruction::transfer(&payer, &tip_addresses.bloxroute, tip_amount));
                let tx_bloxroute = Transaction::new_signed_with_payer(
                    &ixs_bloxroute,
                    Some(&payer),
                    &[&keypair],
                    blockhash,
                );

                let mut ixs_jito = instructions.clone();
                ixs_jito.push(system_instruction::transfer(&payer, &tip_addresses.jito, tip_amount));
                let tx_jito = Transaction::new_signed_with_payer(
                    &ixs_jito,
                    Some(&payer),
                    &[&keypair],
                    blockhash,
                );

                (tx_rpc, tx_0slot, tx_bloxroute, tx_jito)
            }
        }).await.expect("Failed to build transactions");

        // Send all transactions concurrently using spawn for true parallelism
        let mut handles = Vec::new();
        
        // Always send via RPC (2 different endpoints)
        handles.push(tokio::spawn(send_via_rpc_optimized(tx_rpc.clone(), self.client.clone())));
        handles.push(tokio::spawn(send_via_rpc_optimized(tx_rpc, self.backup_rpc_client.clone())));
        
        // Send via 0slot if API key is available
        if let Some(api_key) = &self.zeroslot_api_key {
            handles.push(tokio::spawn(send_via_0slot_optimized(
                tx_0slot,
                api_key.clone(),
                self.http_client.clone()
            )));
        }
        
        // Send via BloxRoute if API key is available
        if let Some(api_key) = &self.bloxroute_api_key {
            handles.push(tokio::spawn(send_via_bloxroute_optimized(
                tx_bloxroute,
                api_key.clone(),
                self.http_client.clone()
            )));
        }
        
        // Send via Jito if available
        if let Some(jito_client) = self.jito_client.clone() {
            handles.push(tokio::spawn(send_via_jito_optimized(tx_jito, jito_client)));
        }

        // Wait for all sends to complete (don't care about individual results for maximum speed)
        let _ = futures::future::join_all(handles).await;
    }
}

// Optimized sending functions with reused HTTP clients
#[instrument(skip_all, name = "RPC_OPT")]
async fn send_via_rpc_optimized(tx: Transaction, client: Arc<RpcClient>) {
    let start = Instant::now();
    match client
        .send_transaction_with_config(
            &tx,
            RpcSendTransactionConfig {
                skip_preflight: true,
                max_retries: Some(0), // No retries for maximum speed
                ..Default::default()
            },
        )
        .await
    {
        Ok(sig) => {
            info!(duration = ?start.elapsed(), "TX: {sig}");
        }
        Err(e) => {
            error!(duration = ?start.elapsed(), "RPC failed: {:#}", e);
        }
    }
}

#[instrument(skip_all, name = "0SLOT_OPT")]
async fn send_via_0slot_optimized(tx: Transaction, api_key: String, client: Arc<reqwest::Client>) {
    let start = Instant::now();
    
    // Pre-serialize transaction
    let serialized_tx = match bincode::serialize(&tx) {
        Ok(data) => general_purpose::STANDARD.encode(data),
        Err(e) => {
            error!(duration = ?start.elapsed(), "Serialization failed: {}", e);
            return;
        }
    };
    
    let request_body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "sendTransaction",
        "params": [
            serialized_tx,
            {"encoding": "base64", "skipPreflight": true}
        ]
    });

    match client
        .post(&format!("https://ny1.0slot.trade/?api-key={}", api_key))
        .json(&request_body)
        .send()
        .await
    {
        Ok(response) => {
            match response.json::<serde_json::Value>().await {
                Ok(json) => {
                    if let Some(signature) = json.get("result").and_then(|r| r.as_str()) {
                        info!(duration = ?start.elapsed(), "TX: {}", signature);
                    } else if let Some(error) = json.get("error") {
                        error!(duration = ?start.elapsed(), "0slot error: {}", error);
                    }
                }
                Err(e) => error!(duration = ?start.elapsed(), "JSON parse failed: {}", e),
            }
        }
        Err(e) => error!(duration = ?start.elapsed(), "0slot request failed: {}", e),
    }
}

#[instrument(skip_all, name = "BLOX_OPT")]
async fn send_via_bloxroute_optimized(tx: Transaction, api_key: String, client: Arc<reqwest::Client>) {
    let start = Instant::now();
    
    let serialized_tx = match bincode::serialize(&tx) {
        Ok(data) => general_purpose::STANDARD.encode(data),
        Err(e) => {
            error!(duration = ?start.elapsed(), "Serialization failed: {}", e);
            return;
        }
    };
    
    let request_body = json!({
        "transaction": {"content": serialized_tx}, 
        "frontRunningProtection": false,
        "submitProtection": "SP_LOW",
        "useStakedRPCs": true,
        "fastBestEffort": true,
    });

    match client
        .post("https://ny.solana.dex.blxrbdn.com/api/v2/submit")
        .header("Authorization", api_key)
        .json(&request_body)
        .send()
        .await
    {
        Ok(response) => {
            match response.json::<serde_json::Value>().await {
                Ok(json) => {
                    if let Some(signature) = json.get("signature").and_then(|s| s.as_str()) {
                        info!(duration = ?start.elapsed(), "TX: {}", signature);
                    } else if let Some(error) = json.get("error") {
                        error!(duration = ?start.elapsed(), "BloxRoute error: {}", error);
                    }
                }
                Err(e) => error!(duration = ?start.elapsed(), "JSON parse failed: {}", e),
            }
        }
        Err(e) => error!(duration = ?start.elapsed(), "BloxRoute request failed: {}", e),
    }
}

#[instrument(skip_all, name = "JITO_OPT")]
async fn send_via_jito_optimized(tx: Transaction, mut jito_client: jito::Client) {
    let start = Instant::now();
    let sig = *tx.get_signature();
    let versioned_tx = VersionedTransaction::from(tx);

    match jito::send_bundle_no_wait(&[versioned_tx], &mut jito_client).await {
        Ok(response) => {
            info!(duration = ?start.elapsed(), "TX: {sig}, Jito: {response:?}");
        }
        Err(e) => {
            error!(duration = ?start.elapsed(), "Jito failed: {}", e);
        }
    }
}

// Constant product formula with fee calculation
fn calculate_amount_out(amount_in: u64, reserve_in: u64, reserve_out: u64) -> u64 {
    if amount_in == 0 || reserve_in == 0 || reserve_out == 0 {
        return 0;
    }
    
    // Apply 0.25% fee (997/1000 = 0.9975)
    let amount_in_with_fee = (amount_in * 997) / 1000;
    
    // Constant product formula: amount_out = (amount_in_with_fee * reserve_out) / (reserve_in + amount_in_with_fee)
    let numerator = amount_in_with_fee * reserve_out;
    let denominator = reserve_in + amount_in_with_fee;
    
    numerator / denominator
}

async fn send_transaction_via_0slot(tx: &Transaction, api_key: &str) -> eyre::Result<String> {
    // Serialize transaction to base64
    let serialized_tx = bincode::serialize(tx)?;
    let base64_encoded_transaction = general_purpose::STANDARD.encode(serialized_tx);

    // Create JSON-RPC request
    let request_body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "sendTransaction",
        "params": [
            base64_encoded_transaction,
            {
                "encoding": "base64",
                "skipPreflight": true,
            }
        ]
    });

    // Send the request
    let response = reqwest::Client::new()
        .post(&format!("https://ny1.0slot.trade/?api-key={}", api_key))
        .json(&request_body)
        .send()
        .await?;

    // Parse the response
    let response_json: serde_json::Value = response.json().await?;

    if let Some(result) = response_json.get("result") {
        if let Some(signature) = result.as_str() {
            Ok(signature.to_string())
        } else {
            Err(eyre::eyre!(
                "Invalid response format: result is not a string"
            ))
        }
    } else if let Some(error) = response_json.get("error") {
        Err(eyre::eyre!("0slot API error: {}", error))
    } else {
        Err(eyre::eyre!("Unexpected response format"))
    }
}


async fn send_transaction_via_bloxroute(tx: &Transaction, api_key: &str ) -> eyre::Result<String> {
    // Serialize transaction to base64
    let serialized_tx = bincode::serialize(tx)?;
    let base64_encoded_transaction = general_purpose::STANDARD.encode(serialized_tx);

    // Create JSON-RPC request
    let request_body = json!({
        "transaction": {"content": base64_encoded_transaction}, 
        "frontRunningProtection": false,
        "submitProtection": "SP_LOW",
        "useStakedRPCs": true,
        "fastBestEffort": true,
    });

    // Send the request
    let response = reqwest::Client::new()
        .post("https://ny.solana.dex.blxrbdn.com/api/v2/submit")
        .header("Authorization", api_key)
        .json(&request_body)
        .send()
        .await?;

    // Parse the response
    let response_json: serde_json::Value = response.json().await?;

    if let Some(result) = response_json.get("signature") {
        if let Some(signature) = result.as_str() {
            Ok(signature.to_string())
        } else {
            Err(eyre::eyre!(
                "Invalid response format: signature is not a string"
            ))
        }
    } else if let Some(error) = response_json.get("error") {
        Err(eyre::eyre!("0slot API error: {}", error))
    } else {
        Err(eyre::eyre!("Unexpected response format"))
    }
}

#[instrument(skip_all, fields(signature = ?token_mint))]
async fn buy(
    amount_in: u64,
    token_mint: Pubkey,
    keypair: Keypair,
    latest_blockhash: Arc<RwLock<Vec<Hash>>>,
    client: Arc<RpcClient>,
    // tpu_client: Arc<TpuClient<QuicPool, QuicConnectionManager, QuicConfig>>,
    jito_client: Option<jito::Client>,
    zeroslot_api_key: Option<&str>,
    bloxroute_api_key: Option<&str>,
) {
    // Pool reserves: 85 SOL and 206,900,000 tokens
    let reserve_sol = 85_000_000_000; // 85 SOL in lamports
    let reserve_token = 206_900_000_000_000; // 206.9M tokens with 6 decimals
    let amount_out = calculate_amount_out(amount_in, reserve_sol, reserve_token);
    let swap_base_in = SwapBaseIn::new(
        keypair.pubkey(),
        token_mint,
        amount_in,
        amount_out,
    );

    let cu_budget = 2000_000u32;
    let cu_price = 500000u64;

    let set_cu_budget_ix = Instruction {
        program_id: pubkey!("ComputeBudget111111111111111111111111111111"),
        accounts: vec![AccountMeta::new_readonly(
            pubkey!("jitodontfront11111111111111111111TrentLover"), // <= change this to something else
            false,
        )],
        data: {
            let mut data = vec![2u8]; // SetComputeUnitLimit discriminator
            data.extend_from_slice(&cu_budget.to_le_bytes()); // 300k compute units
            data
        },
    };

    let set_cu_price_ix = Instruction {
        program_id: pubkey!("ComputeBudget111111111111111111111111111111"),
        accounts: vec![],
        data: {
            let mut data = vec![3u8]; // SetComputeUnitPrice discriminator
            data.extend_from_slice(&cu_price.to_le_bytes()); // 10k micro-lamports priority fee
            data
        },
    };

    let create_ata_base_ix = create_associated_token_account_idempotent(
        &keypair.pubkey(),
        &keypair.pubkey(),
        &swap::WSOL_MINT,
        &pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"),
    );

    let create_ata_quote_ix = create_associated_token_account_idempotent(
        &keypair.pubkey(),
        &keypair.pubkey(),
        &token_mint,
        &pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"),
    );

    let swap_ix = swap_base_in.ix();
    let transfer_to_wsol = system_instruction::transfer(
        &keypair.pubkey(),
        &Pubkey::find_program_address(
            &[
                &keypair.pubkey().to_bytes(),
                &pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA").to_bytes(),
                &swap::WSOL_MINT.to_bytes(),
            ],
            &pubkey!("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL"),
        )
        .0,
        amount_in,
    );

    let sync_native_ix = Instruction {
        program_id: pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"),
        accounts: vec![AccountMeta::new(
            Pubkey::find_program_address(
                &[
                    &keypair.pubkey().to_bytes(),
                    &pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA").to_bytes(),
                    &swap::WSOL_MINT.to_bytes(),
                ],
                &pubkey!("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL"),
            )
            .0,
            false,
        )],
        data: vec![17], // SyncNative instruction discriminator
    };

    let ixs = [
        set_cu_budget_ix.clone(),
        set_cu_price_ix.clone(),
        create_ata_base_ix.clone(),
        create_ata_quote_ix.clone(),
        transfer_to_wsol.clone(),
        sync_native_ix.clone(),
        swap_ix.clone(),
    ];

    fn build_ixs_with_tip(ixs: &[Instruction], tip_ix: &Instruction) -> Vec<Instruction> {
        ixs.iter()
            .chain(std::slice::from_ref(tip_ix))
            .cloned()
            .collect()
    }
    let blockhashes = latest_blockhash.read().await;
    let mut futures_0slot = Vec::new();
    let mut futures_bloxroute = Vec::new();
    let mut futures_rpc = Vec::new();
    // let mut futures_tpu = Vec::new();
    let mut futures_jito = Vec::new();

    for blockhash in blockhashes.iter().rev().take(1) {
        let tx_no_tip = Transaction::new_signed_with_payer(
            &ixs,
            Some(&keypair.pubkey()),
            &[&keypair],
            *blockhash,
        );

        let tx_0slot = Transaction::new_signed_with_payer(
            &build_ixs_with_tip(
                &ixs,
                &system_instruction::transfer(
                    &keypair.pubkey(),
                    &pubkey!("FCjUJZ1qozm1e8romw216qyfQMaaWKxWsuySnumVCCNe"),
                    10_000_000,
                ),
            ),
            Some(&keypair.pubkey()),
            &[&keypair],
            *blockhash,
        );

        let tx_bloxroute = Transaction::new_signed_with_payer(
            &build_ixs_with_tip(
                &ixs,
                &system_instruction::transfer(
                    &keypair.pubkey(),
                    &pubkey!("95cfoy472fcQHaw4tPGBTKpn6ZQnfEPfBgDQx6gcRmRg"),
                    10_000_000,
                ),
            ),      
            Some(&keypair.pubkey()),
            &[&keypair],
            *blockhash,
        );

        let tx_jito = Transaction::new_signed_with_payer(
            &build_ixs_with_tip(
                &ixs,
                &system_instruction::transfer(
                    &keypair.pubkey(),
                    &pubkey!("DfXygSm4jCyNCybVYYK6DwvWqjKee8pbDmJGcLWNDXjh"),
                    10_000_000,
                ),
            ),
            Some(&keypair.pubkey()),
            &[&keypair],
            *blockhash,
        );


        // Send via 0slot if API key is available
        if let Some(api_key) = zeroslot_api_key {
            futures_0slot.push(send_via_0slot(tx_0slot.clone(), api_key));
        }
        
        // Send via BloxRoute if API key is available
        if let Some(api_key) = bloxroute_api_key {
            futures_bloxroute.push(send_via_bloxroute(tx_bloxroute.clone(), api_key));
        }
        
        // Always send via RPC
        futures_rpc.push(send_via_rpc(tx_no_tip.clone(), client.clone()));
        futures_rpc.push(send_via_rpc(tx_no_tip.clone(), Arc::new(RpcClient::new_with_commitment(
            "https://acc.solayer.org".to_string(),
            CommitmentConfig::processed(),
        ))));
        
        // futures_tpu.push(send_via_tpu(tx_no_tip.clone(), tpu_client.clone()));
        
        // Send via Jito if client is available
        if let Some(client) = jito_client.clone() {
            futures_jito.push(send_via_jito(tx_jito.clone(), client));
        }
    }

    tokio::join!(
        futures::future::join_all(futures_0slot),
        futures::future::join_all(futures_bloxroute),
        futures::future::join_all(futures_rpc),
        // futures::future::join_all(futures_tpu),
        futures::future::join_all(futures_jito),
    );
}

#[instrument(skip_all, name = "0slot")]
async fn send_via_0slot(tx: Transaction, api_key: &str) {
    let start = Instant::now();
    match send_transaction_via_0slot(&tx, api_key).await {
        Ok(signature) => {
            info!(duration = ?start.elapsed(), "Tx sent: {signature}");
        }
        Err(e) => {
            error!(
                duration = ?start.elapsed(),
                "Failed to send transaction via 0slot: {:#}", e
            );
        }
    }
}

#[instrument(skip_all, name = "bloxroute")]
async fn send_via_bloxroute(tx: Transaction, api_key: &str) {
    let start = Instant::now();
    match send_transaction_via_bloxroute(&tx, api_key).await {
        Ok(signature) => {
            info!(duration = ?start.elapsed(), "Tx sent: {signature}");
        }
        Err(e) => {
            error!(
                duration = ?start.elapsed(),
                "Failed to send transaction via bloxroute: {:#}", e
            );
        }
    }
}

#[instrument(skip_all, name = "RPC")]
async fn send_via_rpc(tx: Transaction, client: Arc<RpcClient>) {
    let start = Instant::now();
    match client
        .send_transaction_with_config(
            &tx,
            RpcSendTransactionConfig {
                skip_preflight: true,
                ..Default::default()
            },
        )
        .await
    {
        Ok(sig) => {
            info!(duration = ?start.elapsed(), "Tx sent: {sig}");
        }
        Err(e) => {
            error!(
                duration = ?start.elapsed(),
                "Failed to send transaction via RPC: {:#}", e
            );
        }
    }
}

#[instrument(skip_all, name = "TPU")]
async fn send_via_tpu(
    tx: Transaction,
    tpu_client: Arc<TpuClient<QuicPool, QuicConnectionManager, QuicConfig>>,
) {
    let start = Instant::now();
    let sig = tx.get_signature();

    if tpu_client.send_transaction(&tx).await {
        info!(duration = ?start.elapsed(), "Tx sent: {sig}");
    } else {
        error!(duration = ?start.elapsed(), "Failed to send transaction");
    }
}

#[instrument(skip_all, name = "Jito")]
async fn send_via_jito(tx: Transaction, mut jito_client: jito::Client) {
    let sig = *tx.get_signature();
    let versioned_tx = VersionedTransaction::from(tx);
    let start = Instant::now();

    let r = jito::send_bundle_no_wait(&[versioned_tx], &mut jito_client).await;

    match r {
        Ok(a) => {
            info!(duration = ?start.elapsed(), "Tx sent: {sig}, Response: {a:?}");
        }
        Err(e) => {
            error!(duration = ?start.elapsed(), "Failed to send transaction via Jito: {:#}", e);
        }
    }
}

fn build_associated_token_account_instruction(
    funding_address: &Pubkey,
    wallet_address: &Pubkey,
    token_mint_address: &Pubkey,
    token_program_id: &Pubkey,
    instruction: u8,
) -> Instruction {
    let associated_account_address = Pubkey::find_program_address(
        &[
            &wallet_address.to_bytes(),
            &token_program_id.to_bytes(),
            &token_mint_address.to_bytes(),
        ],
        &pubkey!("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL"),
    )
    .0;
    // safety check, assert if not a creation instruction, which is only 0 or 1
    assert!(instruction <= 1);
    Instruction {
        program_id: pubkey!("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL"),
        accounts: vec![
            AccountMeta::new(*funding_address, true),
            AccountMeta::new(associated_account_address, false),
            AccountMeta::new_readonly(*wallet_address, false),
            AccountMeta::new_readonly(*token_mint_address, false),
            AccountMeta::new_readonly(solana_system_interface::program::ID, false),
            AccountMeta::new_readonly(*token_program_id, false),
        ],
        data: vec![instruction],
    }
}

pub fn create_associated_token_account_idempotent(
    funding_address: &Pubkey,
    wallet_address: &Pubkey,
    token_mint_address: &Pubkey,
    token_program_id: &Pubkey,
) -> Instruction {
    build_associated_token_account_instruction(
        funding_address,
        wallet_address,
        token_mint_address,
        token_program_id,
        1, // AssociatedTokenAccountInstruction::CreateIdempotent
    )
}

