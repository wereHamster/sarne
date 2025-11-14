use anyhow::Result;
use clap::Parser;
use tracing::info;

use sarne::config::Config;

#[derive(Parser, Debug)]
#[command(name = "sarned")]
struct Args {
    /// Path to the configuration file
    #[arg(short, long, default_value = "./sarne.toml")]
    config: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    info!("Loading config from {}", &args.config);

    let _config = Config::from_file(&args.config)?;

    Ok(())
}
