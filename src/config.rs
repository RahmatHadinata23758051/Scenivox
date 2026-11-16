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
        Self::from_values(
            env::var("SCENIVOX_ADDR").ok().as_deref(),
            env::var("SCENIVOX_DATABASE_URL").ok().as_deref(),
        )
    }

    pub fn from_values(
        address: Option<&str>,
        database_url: Option<&str>,
    ) -> Result<Self, ConfigError> {
        let address = address.unwrap_or("127.0.0.1:3000").parse()?;
        let database_url = database_url.unwrap_or("sqlite://scenivox.db?mode=rwc");
        if !database_url.starts_with("sqlite:") {
            return Err(ConfigError::InvalidDatabaseUrl);
        }

        Ok(Self {
            address,
            database_url: database_url.to_owned(),
        })
    }
}
