use anyhow::Result;
use clap::Parser;
use tracing::info;

use sarne::config::Config;
use sarne::{
    connect_to_database, connect_to_lnd, create_lightning_clients, init_tracing_subscriber,
};

#[derive(Parser, Debug)]
#[command(name = "sarne-policy-agent")]
struct Args {
    /// Path to the configuration file
    #[arg(short, long, default_value = "./sarne.toml")]
    config: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing_subscriber();

    let args = Args::parse();

    info!("Loading config from {}", &args.config);

    let config = Config::from_file(&args.config)?;

    let (_db_client, (_lightning_client, _router_client)) = tokio::try_join!(
        // PostgreSQL
        connect_to_database(&config),
        // LND
        async {
            let (channel, macaroon) = connect_to_lnd(&config).await?;
            create_lightning_clients(channel, macaroon).await
        },
    )?;

    Ok(())
}
