use anyhow::{Context, Result};
use serde::Deserialize;
use std::fs;
use std::path::Path;

/// Configuration for the LND connection
#[derive(Debug, Deserialize)]
pub struct LndConfig {
    /// The gRPC endpoint of the LND node (e.g., "https://localhost:10009")
    pub endpoint: String,

    /// Path to the macaroon file for authentication
    pub macaroon: String,

    /// Path to the TLS certificate file
    /// Note: Currently not used as certificate validation is disabled for development.
    /// See security notes in project documentation.
    #[allow(dead_code)]
    pub cert: String,
}

/// Configuration for the database connection
#[derive(Debug, Deserialize)]
pub struct DatabaseConfig {
    /// PostgreSQL connection URL
    pub url: String,
}

/// Root configuration structure
#[derive(Debug, Deserialize)]
pub struct Config {
    pub lnd: LndConfig,
    pub database: DatabaseConfig,
}

impl Config {
    /// Load configuration from a TOML file
    pub fn from_file<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref();
        let content = fs::read_to_string(path)
            .with_context(|| format!("Failed to read config file: {}", path.display()))?;

        let config: Config = toml::from_str(&content)
            .with_context(|| format!("Failed to parse config file: {}", path.display()))?;

        Ok(config)
    }
}
