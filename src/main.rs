use dotenv::dotenv;
use env_logger::Env;
use log::{error, info};
use num_traits::ToPrimitive;
use sqlx::postgres::PgRow;
use sqlx::Row;
use sqlx::{postgres::PgPoolOptions, Pool, Postgres};
use starknet::core::types::{BlockId, EventFilter, Felt};
use starknet::core::utils::get_selector_from_name;
use starknet::providers::{jsonrpc::HttpTransport, JsonRpcClient, Provider};
use std::env;
use std::time::Duration;
use url::Url;

#[derive(Debug)]
struct EventTimeout {
    event_address: String,
    event_outcome: u8,
    timestamp: u64,
}

#[tokio::main]
async fn main() {
    dotenv().ok();

    env_logger::Builder::from_env(Env::default().default_filter_or("info")).init();

    let rpc_endpoint = env::var("RPC_ENDPOINT").expect("RPC_ENDPOINT must be set");
    let rpc_url = Url::parse(&rpc_endpoint).expect("Invalid RPC URL");

    let transport = HttpTransport::new(rpc_url);
    let provider = JsonRpcClient::new(transport);

    let pool = setup_database().await;

    
    loop {
        let contract_addresses = fetch_contract_addresses(&pool).await;
        process_new_events(&provider, &contract_addresses, &pool).await;
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
}

async fn setup_database() -> Pool<Postgres> {
    let database_url = env::var("DATABASE_URL").expect("DATABASE_URL must be set");

    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect(&database_url)
        .await
        .expect("Failed to create database pool");

    setup_block_state_trunks(&pool).await;

    pool
}

async fn setup_block_state_trunks(pool: &Pool<Postgres>) {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS block_state_trunks (
            id INTEGER PRIMARY KEY,
            last_processed_block BIGINT NOT NULL
        )",
    )
    .execute(pool)
    .await
    .expect("Failed to create block_state_trunks table");

    sqlx::query(
        "INSERT INTO block_state_trunks (id, last_processed_block)
         VALUES (1, 0)
         ON CONFLICT (id) DO NOTHING",
    )
    .execute(pool)
    .await
    .expect("Failed to initialize block_state_trunks");
}

async fn fetch_contract_addresses(pool: &Pool<Postgres>) -> Vec<Felt> {
    let contract_addresses: Vec<Felt> =
        sqlx::query("SELECT address FROM events WHERE is_active = true")
            .map(|row: PgRow| {
                let address: String = row.get("address");
                let felt_address = Felt::from_hex(&address).expect("Invalid Felt");

                info!(
                    "Fetched contract address: {} (Felt: {:?})",
                    address, felt_address
                );

                felt_address
            })
            .fetch_all(pool)
            .await
            .expect("Failed to fetch contract addresses");

    contract_addresses
}

async fn process_new_events(
    provider: &JsonRpcClient<HttpTransport>,
    contract_addresses: &[Felt],
    pool: &Pool<Postgres>,
) {
    let last_processed_block = get_last_processed_block(pool).await;
    let latest_block = provider
        .block_number()
        .await
        .expect("Failed to get latest block number");

    info!("Last processed block: {}", last_processed_block);
    info!("Latest block: {}", latest_block);

    if latest_block > last_processed_block {
        info!(
            "🔀 Processing blocks from {} to {}",
            last_processed_block + 1,
            latest_block
        );
        for block_number in (last_processed_block + 1)..=latest_block {
            for &contract_address in contract_addresses {
                process_block(provider, block_number, contract_address, pool).await;
            }
            update_last_processed_block(pool, block_number).await;
        }
    } else {
        info!("📡 No new blocks to process.");
    }
}

async fn get_last_processed_block(pool: &Pool<Postgres>) -> u64 {
    let row: (i64,) =
        sqlx::query_as("SELECT last_processed_block FROM block_state_trunks WHERE id = 1")
            .fetch_one(pool)
            .await
            .expect("Failed to fetch last_processed_block");

    row.0 as u64
}

async fn update_last_processed_block(pool: &Pool<Postgres>, block_number: u64) {
    if let Err(e) =
        sqlx::query("UPDATE block_state_trunks SET last_processed_block = $1 WHERE id = 1")
            .bind(block_number as i64)
            .execute(pool)
            .await
    {
        error!("Failed to update last_processed_block: {}", e);
    }
}

async fn process_block(
    provider: &JsonRpcClient<HttpTransport>,
    block_number: u64,
    contract_address: Felt,
    pool: &Pool<Postgres>,
) {
    info!(
        "Listening for events on contract address: {} (Felt: {:?}) in block {}",
        format_address(&contract_address.to_hex_string()),
        contract_address,
        block_number,
    );

    let filter = EventFilter {
        from_block: Some(BlockId::Number(block_number)),
        to_block: Some(BlockId::Number(block_number)),
        address: Some(contract_address),
        keys: Some(vec![vec![event_timeout_event_key()]]),
    };

    let chunk_size = 100;
    let events_page = match provider.get_events(filter, None, chunk_size).await {
        Ok(page) => page,
        Err(err) => {
            error!("Error fetching events: {}", err);
            return;
        }
    };

    info!(
        "Number of EventTimeout events fetched: {}",
        events_page.events.len()
    );

    if events_page.events.is_empty() {
        info!(
            "No EventTimeout events found for block {} on contract {}",
            block_number, contract_address
        );
    }

    for event in events_page.events {
        let data = event.data.clone();

        if let Some(event_finished) = parse_event_finished_event(&data) {
            info!("✨ New EventFinished event: {:?}", event_finished);
            update_database_for_event_finished(event_finished, pool).await;
        } else {
            error!(
                "❌ Failed to parse EventFinished event with data: {:?}",
                data
            );
        }
    }
}

fn event_timeout_event_key() -> Felt {
    let selector =
        get_selector_from_name("EventTimeout").expect("Failed to compute event selector");
    info!("EventTimeout selector: {:?}", selector);
    selector
}

fn parse_event_finished_event(data: &[Felt]) -> Option<EventTimeout> {
    if data.len() >= 3 {
        let event_address = format_address(&data[0].to_fixed_hex_string());
        let event_outcome = data[1].to_u8().unwrap_or(0);
        let timestamp = data[2].to_u64().unwrap_or(0);

        Some(EventTimeout {
            event_address,
            event_outcome,
            timestamp,
        })
    } else {
        None
    }
}

async fn update_database_for_event_finished(event: EventTimeout, pool: &Pool<Postgres>) {
    let result =
        sqlx::query("UPDATE events SET is_active = FALSE, outcome = $1 WHERE address = $2")
            .bind(event.event_outcome as i32)
            .bind(&event.event_address)
            .execute(pool)
            .await;

    if let Err(e) = result {
        error!("Failed to update events table: {}", e);
        return;
    }

    info!(
        "Updated events table for event_address: {}",
        event.event_address
    );

    let outcome_as_int = if event.event_outcome == 1 { 1 } else { 0 };
    let result = sqlx::query(
        "UPDATE bets SET is_claimable = TRUE
        WHERE \"event_address\" = $1 AND bet = $2",
    )
    .bind(&event.event_address)
    .bind(outcome_as_int)
    .execute(pool)
    .await;

    if let Err(e) = result {
        error!("Failed to update bets table: {}", e);
    } else {
        info!(
            "Updated bets table for event_address: {}",
            event.event_address
        );
    }
}

fn format_address(address: &str) -> String {
    let hex_str = if address.starts_with("0x") {
        &address[2..]
    } else {
        address
    };
    let formatted = format!("0x{:0>64}", hex_str);

    info!("Formatted address: {}", formatted);
    formatted
}
