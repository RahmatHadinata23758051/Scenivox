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
    #[error("SCENIVOX_ASSET_STORE_DIR must not be empty")]
    InvalidAssetStoreDir,
    #[error("SCENIVOX_ASSET_IMPORT_ROOTS contains an empty entry")]
    InvalidAssetImportRoots,
}

#[derive(Clone, Debug)]
pub struct Config {
    pub address: SocketAddr,
    pub database_url: String,
    pub skill_store_dir: PathBuf,
    pub trusted_skill_import_roots: Vec<PathBuf>,
    pub asset_store_dir: PathBuf,
    pub trusted_asset_import_roots: Vec<PathBuf>,
}

impl Config {
    pub fn from_env() -> Result<Self, ConfigError> {
        let mut config = Self::from_values_with_stores(
            env::var("SCENIVOX_ADDR").ok().as_deref(),
            env::var("SCENIVOX_DATABASE_URL").ok().as_deref(),
            env::var("SCENIVOX_SKILL_STORE_DIR").ok().as_deref(),
            env::var("SCENIVOX_ASSET_STORE_DIR").ok().as_deref(),
        )?;
        if let Some(raw_roots) = env::var_os("SCENIVOX_SKILL_IMPORT_ROOTS") {
            if !raw_roots.is_empty() {
                config.trusted_skill_import_roots =
                    parse_import_roots(&raw_roots).ok_or(ConfigError::InvalidSkillImportRoots)?;
            }
        }
        if let Some(raw_roots) = env::var_os("SCENIVOX_ASSET_IMPORT_ROOTS") {
            if !raw_roots.is_empty() {
                config.trusted_asset_import_roots =
                    parse_import_roots(&raw_roots).ok_or(ConfigError::InvalidAssetImportRoots)?;
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
        Self::from_values_with_stores(address, database_url, skill_store_dir, None)
    }

    pub fn from_values_with_stores(
        address: Option<&str>,
        database_url: Option<&str>,
        skill_store_dir: Option<&str>,
        asset_store_dir: Option<&str>,
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
            None => default_store_dir(database_url, "skills"),
        };

        let asset_store_dir = match asset_store_dir {
            Some(raw) => {
                if raw.trim().is_empty() {
                    return Err(ConfigError::InvalidAssetStoreDir);
                }
                PathBuf::from(raw)
            }
            None => default_store_dir(database_url, "assets"),
        };

        Ok(Self {
            address,
            database_url: database_url.to_owned(),
            skill_store_dir,
            trusted_skill_import_roots: Vec::new(),
            asset_store_dir,
            trusted_asset_import_roots: Vec::new(),
        })
    }
}

fn parse_import_roots(raw_roots: &std::ffi::OsStr) -> Option<Vec<PathBuf>> {
    let mut roots = Vec::new();
    for path in env::split_paths(raw_roots) {
        if path.as_os_str().is_empty() {
            return None;
        }
        roots.push(path);
    }
    Some(roots)
}

pub(crate) fn default_store_dir(database_url: &str, folder: &str) -> PathBuf {
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
                return parent.join(folder);
            }
        }
    }
    PathBuf::from(folder)
}
