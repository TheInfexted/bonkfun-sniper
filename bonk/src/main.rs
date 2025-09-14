pub mod sender;
pub mod swap;
pub mod utils;
pub mod jito;

use clap::Parser;
use eyre::Result;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_program::pubkey;
use solana_sdk::signature::read_keypair_file;
use solana_sdk::{
    commitment_config::CommitmentConfig,
    pubkey::Pubkey,
};
use std::{str::FromStr, sync::Arc};

use crate::sender::TxSender;

const MIGRATOR: Pubkey = pubkey!("RAYpQbFNq9i3mu6cKpTKKRwwHFDeK5AuZz8xvxUrCgw");

#[derive(Parser, Debug)]
struct Args {
    #[arg(
        long,
        default_value = "http://localhost:8899"
    )]
    rpc_url: String,

    #[arg(long, default_value = "keypair.json")]
    keypair_path: String,

    #[arg(long)]
    token_mint: String,

    #[arg(long)]
    jito_keypair_path: Option<String>,

    #[arg(long)]
    bloxroute_api_key: Option<String>,  

    #[arg(long)]
    zeroslot_api_key: Option<String>,

    #[arg(long, default_value = "1000000000")]
    amount_in_lamports: u64,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let args = Args::parse();

    let rpc_client = Arc::new(RpcClient::new_with_commitment(
        args.rpc_url,
        CommitmentConfig::processed(),
    ));

    // Initialize Jito client if keypair is provided
    let jito_client = if let Some(jito_path) = args.jito_keypair_path {
        println!("Initializing Jito client with keypair: {}", jito_path);
        match read_keypair_file(&jito_path) {
            Ok(jito_keypair) => {
                match crate::jito::get_searcher_client_auth(
                    "https://ny.mainnet.block-engine.jito.wtf",
                    &Arc::new(jito_keypair),
                ).await {
                    Ok(client) => Some(client),
                    Err(e) => {
                        println!("Warning: Failed to initialize Jito client: {}", e);
                        None
                    }
                }
            }
            Err(e) => {
                println!("Warning: Cannot read Jito keypair: {}", e);
                None
            }
        }
    } else {
        println!("No Jito keypair provided, skipping Jito initialization");
        None
    };

    // Handle BloxRoute API key
    let bloxroute_api_key = if let Some(blox_key) = args.bloxroute_api_key {
        println!("BloxRoute client initialized");
        Some(blox_key)
    } else {
        println!("No BloxRoute API key provided, skipping BloxRoute initialization");
        None
    };

    // Handle ZeroSlot API key  
    let zeroslot_api_key = if let Some(zeroslot_key) = args.zeroslot_api_key {
        println!("ZeroSlot client initialized");
        Some(zeroslot_key)
    } else {
        println!("No ZeroSlot API key provided, skipping ZeroSlot initialization");
        None
    };

    let tx_sender = TxSender::new(
        read_keypair_file(&args.keypair_path).expect("Cannot read keypair"),
        rpc_client.clone(),
        Pubkey::from_str(&args.token_mint).expect("Invalid token mint"),
        jito_client,
        bloxroute_api_key,
        zeroslot_api_key,
        args.amount_in_lamports,
    )
    .await;
    let tx_sender_handle = tokio::spawn(tx_sender.run_loop());
    tx_sender_handle.await?;

    Ok(())
}
