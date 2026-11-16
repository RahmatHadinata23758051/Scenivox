use std::{env, net::SocketAddr};

use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("invalid SCENIVOX_ADDR: {0}")]
    Address(#[from] std::net::AddrParseError),
    #[error("SCENIVOX_DATABASE_URL must use sqlite:")]
    InvalidDatabaseUrl,
}

#[derive(Clone, Debug)]
pub struct Config {
    pub address: SocketAddr,
    pub database_url: String,
}

impl Config {
    pub fn from_env() -> Result<Self, ConfigError> {
        let address = env::var("SCENIVOX_ADDR")
            .unwrap_or_else(|_| "127.0.0.1:3000".to_owned())
            .parse()?;
        let database_url = env::var("SCENIVOX_DATABASE_URL")
            .unwrap_or_else(|_| "sqlite://scenivox.db?mode=rwc".to_owned());
        if !database_url.starts_with("sqlite:") {
            return Err(ConfigError::InvalidDatabaseUrl);
        }

        Ok(Self {
            address,
            database_url,
        })
    }
}
