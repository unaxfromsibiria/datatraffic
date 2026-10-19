pub mod config;
pub mod data;
pub mod ffi;
pub mod redis_client;
pub mod settings;
pub mod transport;

pub use bytes::Bytes;
pub use config::Config;
pub use data::{ClientData, DataError, RawData};
pub use redis_client::{TrafficClient, project_topic, presence_set_key, PRESENCE_TOPIC};
pub use settings::Settings;
pub use transport::{DataFile, TransportError};

/// Error codes returned by the C ABI (see `ffi`); shared with Rust and C/Python consumers.
pub const FFI_OK: i32 = 0;
/// A pointer/length argument was invalid or a text field is not valid UTF-8.
pub const FFI_ERR_BAD_INPUT: i32 = 1;
/// Failed to create the traffic client (bad settings or Redis connection).
pub const FFI_ERR_CLIENT_CREATE: i32 = 2;
/// The outbox send failed because the channel was closed / client already freed.
pub const FFI_ERR_SEND: i32 = 3;
