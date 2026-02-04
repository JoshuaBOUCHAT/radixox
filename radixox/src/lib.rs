//! # RadixOx Client
//!
//! High-performance async client for RadixOx key-value store.
//!
//! ## Features
//!
//! - **Async/await** - Built on monoio runtime with io_uring support
//! - **Request batching** - Automatic batching for maximum throughput
//! - **Ergonomic API** - Accept `&str`, `String`, `&[u8]`, `Bytes` for keys
//! - **JSON support** - Built-in serde serialization via `set_json`/`get_json`
//! - **Prefix operations** - `getn`/`deln` for pattern-based queries
//!
//! ## Quick Start
//!
//! ```no_run
//! use std::net::{SocketAddr, SocketAddrV4, Ipv4Addr};
//! use radixox::{ArtClient, monoio_client::monoio_art::SharedMonoIOClient};
//!
//! #[monoio::main]
//! async fn main() {
//!     let addr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 8379));
//!     let client = SharedMonoIOClient::new(addr).await.unwrap();
//!
//!     // Basic operations
//!     client.set("user:1", "Alice").await;
//!     let val = client.get("user:1").await.unwrap();
//!     assert_eq!(val, Some("Alice".into()));
//!
//!     // Prefix operations
//!     client.set("session:a", "data_a").await;
//!     client.set("session:b", "data_b").await;
//!     let sessions = client.getn("session").await;
//!     assert_eq!(sessions.len(), 2);
//!
//!     // Cleanup with prefix delete
//!     client.deln("session").await.unwrap();
//! }
//! ```
//!
//! ## JSON Serialization
//!
//! ```no_run
//! use serde::{Serialize, Deserialize};
//! use std::net::{SocketAddr, SocketAddrV4, Ipv4Addr};
//! use radixox::{ArtClient, monoio_client::monoio_art::SharedMonoIOClient};
//!
//! #[derive(Serialize, Deserialize)]
//! struct User {
//!     id: u64,
//!     name: String,
//! }
//!
//! #[monoio::main]
//! async fn main() {
//!     let addr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 8379));
//!     let client = SharedMonoIOClient::new(addr).await.unwrap();
//!
//!     let user = User { id: 42, name: "Alice".into() };
//!     client.set_json("user:42", &user).await.unwrap();
//!
//!     let retrieved: User = client.get_json("user:42").await.unwrap().unwrap();
//!     assert_eq!(retrieved.name, "Alice");
//! }
//! ```

use bytes::Bytes;
use std::future::Future;

pub mod monoio_client;
pub mod tokio_client;

#[cfg(test)]
mod tests;

// ============================================================================
// ERROR TYPE
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
    /// Response channel was closed
    ChannelClosed,
}

impl std::fmt::Display for ArtError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ArtError::Io(e) => write!(f, "IO error: {}", e),
            ArtError::Encode(e) => write!(f, "Encode error: {}", e),
            ArtError::Serialize(e) => write!(f, "Serialize error: {}", e),
            ArtError::Deserialize(e) => write!(f, "Deserialize error: {}", e),
            ArtError::ChannelClosed => write!(f, "Channel closed"),
        }
    }
}

impl std::error::Error for ArtError {}

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

// ============================================================================
// CLIENT TRAIT
// ============================================================================

/// Trait for RadixOx key-value store clients.
///
/// Provides an ergonomic API accepting flexible input types:
/// - **Keys**: `&[u8]`, `&str`, `String`, `Bytes`
/// - **Values**: `&[u8]`, `&str`, `String`, `Bytes`
///
/// # Operations
///
/// | Method | Description |
/// |--------|-------------|
/// | [`get`](ArtClient::get) | Get a single value by key |
/// | [`set`](ArtClient::set) | Set a key-value pair |
/// | [`del`](ArtClient::del) | Delete a key and return its value |
/// | [`getn`](ArtClient::getn) | Get all values matching a prefix |
/// | [`deln`](ArtClient::deln) | Delete all keys matching a prefix |
/// | [`get_json`](ArtClient::get_json) | Get and deserialize JSON |
/// | [`set_json`](ArtClient::set_json) | Serialize and set JSON |
///
/// # Example
///
/// ```no_run
/// use radixox::ArtClient;
///
/// async fn example(client: impl ArtClient) {
///     // All of these work for keys:
///     client.set("key", "value").await;
///     client.set(String::from("key"), "value").await;
///     client.set(b"key".as_slice(), "value").await;
/// }
/// ```
pub trait ArtClient {
    // ========================================================================
    // Single key operations
    // ========================================================================

    /// Get a value by key.
    ///
    /// Returns `Ok(None)` if the key doesn't exist.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use radixox::ArtClient;
    /// # async fn example(client: impl ArtClient) {
    /// let value = client.get("my:key").await?;
    /// match value {
    ///     Some(data) => println!("Found: {:?}", data),
    ///     None => println!("Key not found"),
    /// }
    /// # Ok::<(), radixox::ArtError>(())
    /// # }
    /// ```
    fn get(&self, key: impl AsRef<[u8]>) -> impl Future<Output = Result<Option<Bytes>, ArtError>>;

    /// Set a key-value pair.
    ///
    /// Overwrites any existing value.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use radixox::ArtClient;
    /// # async fn example(client: impl ArtClient) {
    /// client.set("user:1", "Alice").await;
    /// client.set("user:2", b"Bob".to_vec()).await;
    /// # Ok::<(), radixox::ArtError>(())
    /// # }
    /// ```
    fn set(
        &self,
        key: impl AsRef<[u8]>,
        value: impl Into<Bytes>,
    ) -> impl Future<Output = Result<(), ArtError>>;

    /// Delete a key and return its previous value.
    ///
    /// Returns `Ok(None)` if the key didn't exist.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use radixox::ArtClient;
    /// # async fn example(client: impl ArtClient) {
    /// let old_value = client.del("my:key").await?;
    /// # Ok::<(), radixox::ArtError>(())
    /// # }
    /// ```
    fn del(&self, key: impl AsRef<[u8]>) -> impl Future<Output = Result<Option<Bytes>, ArtError>>;

    // ========================================================================
    // Prefix operations (implicit wildcard suffix)
    // ========================================================================

    /// Get all values with keys starting with the given prefix.
    ///
    /// The prefix has an implicit `*` wildcard at the end.
    /// For example, `getn("user")` matches `user:1`, `user:2`, `user:admin`, etc.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use radixox::ArtClient;
    /// # async fn example(client: impl ArtClient) {
    /// client.set("item:a", "1").await;
    /// client.set("item:b", "2").await;
    /// client.set("other:x", "3").await;
    ///
    /// let items = client.getn("item").await?;
    /// assert_eq!(items.len(), 2); // Only "item:a" and "item:b"
    /// # Ok::<(), radixox::ArtError>(())
    /// # }
    /// ```
    fn getn(&self, prefix: impl AsRef<[u8]>) -> impl Future<Output = Result<Vec<Bytes>, ArtError>>;

    /// Delete all keys starting with the given prefix.
    ///
    /// The prefix has an implicit `*` wildcard at the end.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use radixox::ArtClient;
    /// # async fn example(client: impl ArtClient) {
    /// // Delete all session keys
    /// client.deln("session").await?;
    /// # Ok::<(), radixox::ArtError>(())
    /// # }
    /// ```
    fn deln(&self, prefix: impl AsRef<[u8]>) -> impl Future<Output = Result<(), ArtError>>;

    // ========================================================================
    // JSON convenience methods (default implementations)
    // ========================================================================

    /// Get and deserialize a JSON value.
    ///
    /// Returns `Ok(None)` if the key doesn't exist.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use radixox::ArtClient;
    /// # use serde::Deserialize;
    /// # #[derive(Deserialize)]
    /// # struct Config { debug: bool }
    /// # async fn example(client: impl ArtClient) {
    /// let config: Option<Config> = client.get_json("app:config").await?;
    /// # Ok::<(), radixox::ArtError>(())
    /// # }
    /// ```
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

    /// Serialize and set a JSON value.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use radixox::ArtClient;
    /// # use serde::Serialize;
    /// # #[derive(Serialize)]
    /// # struct User { name: String }
    /// # async fn example(client: impl ArtClient) {
    /// let user = User { name: "Alice".into() };
    /// client.set_json("user:1", &user).await?;
    /// # Ok::<(), radixox::ArtError>(())
    /// # }
    /// ```
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
