use std::{
    env,
    net::SocketAddr,
    path::{Path, PathBuf},
};

use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("invalid SCENIVOX_ADDR: {0}")]
    Address(#[from] std::net::AddrParseError),
    #[error("SCENIVOX_DATABASE_URL must use sqlite:")]
    InvalidDatabaseUrl,
    #[error("SCENIVOX_SKILL_STORE_DIR must not be empty")]
    InvalidSkillStoreDir,
    #[error("SCENIVOX_SKILL_IMPORT_ROOTS contains an empty entry")]
    InvalidSkillImportRoots,
}

#[derive(Clone, Debug)]
pub struct Config {
    pub address: SocketAddr,
    pub database_url: String,
    pub skill_store_dir: PathBuf,
    pub trusted_skill_import_roots: Vec<PathBuf>,
}

impl Config {
    pub fn from_env() -> Result<Self, ConfigError> {
        let mut config = Self::from_values_with_skill_store(
            env::var("SCENIVOX_ADDR").ok().as_deref(),
            env::var("SCENIVOX_DATABASE_URL").ok().as_deref(),
            env::var("SCENIVOX_SKILL_STORE_DIR").ok().as_deref(),
        )?;
        if let Some(raw_roots) = env::var_os("SCENIVOX_SKILL_IMPORT_ROOTS") {
            if !raw_roots.is_empty() {
                let mut roots = Vec::new();
                for path in env::split_paths(&raw_roots) {
                    if path.as_os_str().is_empty() {
                        return Err(ConfigError::InvalidSkillImportRoots);
                    }
                    roots.push(path);
                }
                config.trusted_skill_import_roots = roots;
            }
        }
        Ok(config)
    }

    pub fn from_values(
        address: Option<&str>,
        database_url: Option<&str>,
    ) -> Result<Self, ConfigError> {
        Self::from_values_with_skill_store(address, database_url, None)
    }

    pub fn from_values_with_skill_store(
        address: Option<&str>,
        database_url: Option<&str>,
        skill_store_dir: Option<&str>,
    ) -> Result<Self, ConfigError> {
        let address = address.unwrap_or("127.0.0.1:3000").parse()?;
        let database_url = database_url.unwrap_or("sqlite://scenivox.db?mode=rwc");
        if !database_url.starts_with("sqlite:") {
            return Err(ConfigError::InvalidDatabaseUrl);
        }

        let skill_store_dir = match skill_store_dir {
            Some(raw) => {
                if raw.trim().is_empty() {
                    return Err(ConfigError::InvalidSkillStoreDir);
                }
                PathBuf::from(raw)
            }
            None => default_skill_store_dir(database_url),
        };

        Ok(Self {
            address,
            database_url: database_url.to_owned(),
            skill_store_dir,
            trusted_skill_import_roots: Vec::new(),
        })
    }
}

pub(crate) fn default_skill_store_dir(database_url: &str) -> PathBuf {
    let raw_path = database_url
        .strip_prefix("sqlite://")
        .or_else(|| database_url.strip_prefix("sqlite:"))
        .unwrap_or("")
        .split('?')
        .next()
        .unwrap_or("");
    if !raw_path.is_empty() && raw_path != ":memory:" {
        if let Some(parent) = Path::new(raw_path).parent() {
            if !parent.as_os_str().is_empty() {
                return parent.join("skills");
            }
        }
    }
    PathBuf::from("skills")
}
