pub mod config;

pub use config::{
    diff, group_rules, load, ConfigDelta, ConfigError, ConfigWatcher, SUPPORTED_SCHEMA_VERSION,
};
