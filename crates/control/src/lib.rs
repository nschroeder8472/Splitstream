pub mod config;

pub use config::{diff, load, ConfigDelta, ConfigError, ConfigWatcher, SUPPORTED_SCHEMA_VERSION};
