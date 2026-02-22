pub mod config;
pub mod crypto;
pub mod database;
#[cfg(feature = "sqlite")]
pub mod database_sqlite;
pub mod logger;
pub mod network;
pub mod storage;
#[cfg(feature = "storage-local")]
pub mod storage_local;

use std::sync::Arc;

/// Services holds all platform service implementations.
pub struct Services {
    pub database: Option<Arc<dyn database::DatabaseService>>,
    pub storage: Option<Arc<dyn storage::StorageService>>,
    pub logger: Option<Arc<dyn logger::LoggerService>>,
    pub crypto: Option<Arc<dyn crypto::CryptoService>>,
    pub config: Option<Arc<dyn config::ConfigService>>,
    pub network: Option<Arc<dyn network::NetworkService>>,
}

impl Default for Services {
    fn default() -> Self {
        Self {
            database: None,
            storage: None,
            logger: None,
            crypto: None,
            config: None,
            network: None,
        }
    }
}
