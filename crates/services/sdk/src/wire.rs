//! the shared serde-json wire codec — ONE generic encode/decode pair that every
//! module's per-type wire wrappers delegate to. the bytes are serde_json's,
//! byte-for-byte unchanged; each module keeps its named `encode_msg` /
//! `decode_msg` fns as one-line delegates, because those names are the public
//! API the wasm guests and cross-module consumers import. this replaces the 122
//! identical `serde_json::to_vec(..).expect(..)` / `from_slice(..).map_err(..)`
//! copies that used to live in every `interface.rs`, so the encoding contract
//! (the `expect` message and the `Result<T, String>` error mapping) is one
//! place, not eighteen.

use serde::{Serialize, de::DeserializeOwned};

/// serialize a wire value to its canonical serde_json bytes. infallible for the
/// module wire types (every one derives `Serialize`); a value that cannot
/// serialize is a build-time contract violation, hence the `expect`.
pub fn encode<T: Serialize>(value: &T) -> Vec<u8> {
    serde_json::to_vec(value).expect("serializable")
}

/// decode a wire value from serde_json bytes, mapping the parse error to its
/// string — the uniform `Result<T, String>` every module wrapper returns.
pub fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, String> {
    serde_json::from_slice(bytes).map_err(|e| e.to_string())
}
