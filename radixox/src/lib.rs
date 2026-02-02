use bytes::Bytes;

pub mod monoio_client;
pub mod tokio_client;

#[cfg(test)]
mod tests;

// ============================================================================
// CLIENT TRAIT - Ergonomic API for key-value operations
// ============================================================================

/// Error type for ART client operations
#[derive(Debug)]
pub enum ArtError {
    /// Network/IO error
    Io(std::io::Error),
    /// Encoding error (protobuf)
    Encode(prost::EncodeError),
    /// Serialization error (JSON)
    Serialize(String),
    /// Deserialization error (JSON)
    Deserialize(String),
    /// Channel receive error
    ChannelClosed,
}

impl From<std::io::Error> for ArtError {
    fn from(e: std::io::Error) -> Self {
        ArtError::Io(e)
    }
}

impl From<prost::EncodeError> for ArtError {
    fn from(e: prost::EncodeError) -> Self {
        ArtError::Encode(e)
    }
}

/// Trait for key-value store clients
///
/// Provides ergonomic methods accepting flexible input types:
/// - Keys: `&[u8]`, `&str`, `String`, `Bytes`
/// - Values: `&[u8]`, `&str`, `String`, `Bytes`, or any `Serialize` type via `_json` methods
pub trait ArtClient {
    // ========================================================================
    // Single key operations
    // ========================================================================

    /// Get a value by key
    fn get(&self, key: impl AsRef<[u8]>) -> impl Future<Output = Result<Option<Bytes>, ArtError>>;

    /// Set a key-value pair
    fn set(
        &self,
        key: impl AsRef<[u8]>,
        value: impl Into<Bytes>,
    ) -> impl Future<Output = Result<(), ArtError>>;

    /// Delete a key and return its value
    fn del(&self, key: impl AsRef<[u8]>) -> impl Future<Output = Result<Option<Bytes>, ArtError>>;

    // ========================================================================
    // Prefix operations (implicit wildcard suffix)
    // ========================================================================

    /// Get all values with keys starting with the given prefix
    fn getn(&self, prefix: impl AsRef<[u8]>) -> impl Future<Output = Result<Vec<Bytes>, ArtError>>;

    /// Delete all keys starting with the given prefix
    fn deln(&self, prefix: impl AsRef<[u8]>) -> impl Future<Output = Result<(), ArtError>>;

    // ========================================================================
    // JSON convenience methods
    // ========================================================================

    /// Get and deserialize a JSON value
    fn get_json<T: serde::de::DeserializeOwned>(
        &self,
        key: impl AsRef<[u8]>,
    ) -> impl Future<Output = Result<Option<T>, ArtError>> {
        async {
            let Some(data) = self.get(key).await? else {
                return Ok(None);
            };
            serde_json::from_slice(&data)
                .map(Some)
                .map_err(|e| ArtError::Deserialize(e.to_string()))
        }
    }

    /// Serialize and set a JSON value
    fn set_json<T: serde::Serialize>(
        &self,
        key: impl AsRef<[u8]>,
        value: &T,
    ) -> impl Future<Output = Result<(), ArtError>> {
        async {
            let json = serde_json::to_vec(value).map_err(|e| ArtError::Serialize(e.to_string()))?;
            self.set(key, json).await
        }
    }
}

use std::future::Future;
