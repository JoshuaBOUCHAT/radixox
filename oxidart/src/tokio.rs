//! Tokio integration for OxidArt.
//!
//! This module provides async utilities for tokio runtimes (single or multi-threaded),
//! including automatic timestamp management for TTL functionality.
//!
//! Since tokio supports multi-threaded runtimes, this module uses `Arc<tokio::sync::Mutex<T>>`
//! for thread-safe shared access.
//!
//! # Example
//!
//! ```rust,ignore
//! use oxidart::OxidArt;
//! use std::time::Duration;
//!
//! #[tokio::main]
//! async fn main() {
//!     // Recommended: creates shared tree with automatic ticker
//!     let tree = OxidArt::shared_with_ticker(Duration::from_millis(100));
//!
//!     // Your server loop here...
//!     tree.lock().await.set(/* ... */);
//! }
//! ```

use crate::OxidArt;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

/// Shared OxidArt type for tokio (thread-safe).
pub type SharedArt = Arc<Mutex<OxidArt>>;

impl OxidArt {
    /// Creates a new shared OxidArt with an automatic background ticker.
    ///
    /// This is the recommended constructor when using TTL features with tokio.
    /// It returns an `Arc<Mutex<OxidArt>>` and spawns a background task that
    /// periodically updates the internal timestamp.
    ///
    /// # Arguments
    ///
    /// * `interval` - How often to update the timestamp (e.g., 100ms)
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use oxidart::OxidArt;
    /// use std::time::Duration;
    ///
    /// #[tokio::main]
    /// async fn main() {
    ///     let tree = OxidArt::shared_with_ticker(Duration::from_millis(100)).await;
    ///
    ///     tree.lock().await.set_ttl(
    ///         Bytes::from_static(b"key"),
    ///         Duration::from_secs(60),
    ///         Bytes::from_static(b"value"),
    ///     );
    /// }
    /// ```
    pub async fn shared_with_ticker(interval: Duration) -> SharedArt {
        let art = Arc::new(Mutex::new(Self::new()));
        art.lock().await.tick(); // Initial tick
        spawn_ticker(art.clone(), interval);
        art
    }

    /// Creates a new shared OxidArt with automatic background ticker and evictor.
    ///
    /// This is the recommended constructor for production use with TTL features.
    /// It returns an `Arc<Mutex<OxidArt>>` and spawns two background tasks:
    /// - A ticker that periodically updates the internal timestamp
    /// - An evictor that removes expired entries using Redis-style sampling
    ///
    /// # Arguments
    ///
    /// * `tick_interval` - How often to update the timestamp (e.g., 100ms)
    /// * `evict_interval` - How often to run eviction (e.g., 1s)
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use oxidart::OxidArt;
    /// use std::time::Duration;
    ///
    /// #[tokio::main]
    /// async fn main() {
    ///     let tree = OxidArt::shared_with_evictor(
    ///         Duration::from_millis(100),
    ///         Duration::from_secs(1),
    ///     ).await;
    ///
    ///     tree.lock().await.set_ttl(
    ///         Bytes::from_static(b"key"),
    ///         Duration::from_secs(60),
    ///         Bytes::from_static(b"value"),
    ///     );
    /// }
    /// ```
    pub async fn shared_with_evictor(
        tick_interval: Duration,
        evict_interval: Duration,
    ) -> SharedArt {
        let art = Arc::new(Mutex::new(Self::new()));
        art.lock().await.tick(); // Initial tick
        spawn_ticker(art.clone(), tick_interval);
        spawn_evictor(art.clone(), evict_interval);
        art
    }

    /// Updates the internal timestamp to the current system time.
    ///
    /// This is a convenience method for async runtimes.
    /// Call this at the start of each event loop iteration, or use
    /// [`shared_with_ticker`](Self::shared_with_ticker) to automate this.
    #[inline]
    pub fn tick(&mut self) {
        self.now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system time before UNIX epoch")
            .as_secs();
    }
}

/// Spawns a background task that periodically updates the tree's internal timestamp.
///
/// This works with both single-threaded (`current_thread`) and multi-threaded tokio runtimes.
/// The ticker runs cooperatively, updating at each `interval` to keep TTL checks accurate.
///
/// # Arguments
///
/// * `art` - A shared reference to the tree (`Arc<Mutex<OxidArt>>`)
/// * `interval` - How often to update the timestamp (e.g., 100ms)
///
/// # Returns
///
/// A `JoinHandle` that can be used to abort the ticker if needed.
///
/// # Example
///
/// ```rust,ignore
/// use oxidart::OxidArt;
/// use std::sync::Arc;
/// use tokio::sync::Mutex;
/// use std::time::Duration;
///
/// #[tokio::main]
/// async fn main() {
///     let shared_art = Arc::new(Mutex::new(OxidArt::new()));
///
///     // Spawn ticker - updates every 100ms
///     let ticker_handle = oxidart::tokio::spawn_ticker(shared_art.clone(), Duration::from_millis(100));
///
///     // Later, if you need to stop the ticker:
///     // ticker_handle.abort();
/// }
/// ```
pub fn spawn_ticker(
    art: Arc<Mutex<OxidArt>>,
    interval: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval_timer = tokio::time::interval(interval);
        loop {
            interval_timer.tick().await;
            art.lock().await.tick();
        }
    })
}

/// Spawns a background task that periodically evicts expired entries.
///
/// This implements Redis-style probabilistic eviction: samples random entries
/// with TTL and removes expired ones. If many are expired, it continues sampling.
///
/// # Arguments
///
/// * `art` - A shared reference to the tree (`Arc<Mutex<OxidArt>>`)
/// * `interval` - How often to run eviction (e.g., 1s)
///
/// # Returns
///
/// A `JoinHandle` that can be used to abort the evictor if needed.
///
/// # Example
///
/// ```rust,ignore
/// use oxidart::OxidArt;
/// use std::sync::Arc;
/// use tokio::sync::Mutex;
/// use std::time::Duration;
///
/// #[tokio::main]
/// async fn main() {
///     let shared_art = Arc::new(Mutex::new(OxidArt::new()));
///
///     // Spawn ticker and evictor
///     oxidart::tokio::spawn_ticker(shared_art.clone(), Duration::from_millis(100));
///     oxidart::tokio::spawn_evictor(shared_art.clone(), Duration::from_secs(1));
/// }
/// ```
pub fn spawn_evictor(
    art: Arc<Mutex<OxidArt>>,
    interval: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval_timer = tokio::time::interval(interval);
        loop {
            interval_timer.tick().await;
            art.lock().await.evict_expired();
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use crate::value::Value;

    #[tokio::test]
    async fn test_ttl_expiration_with_ticker() {
        let art = Arc::new(Mutex::new(OxidArt::new()));

        // Spawn the ticker (updates every 100ms)
        let _handle = spawn_ticker(art.clone(), Duration::from_millis(100));

        // Initial tick to set current time
        art.lock().await.tick();

        // batch:1 expires in 1 second
        art.lock().await.set_ttl(
            Bytes::from_static(b"batch:1"),
            Duration::from_secs(1),
            Value::String(Bytes::from_static(b"expires_soon")),
        );

        // batch:2 never expires
        art.lock().await.set(
            Bytes::from_static(b"batch:2"),
            Value::String(Bytes::from_static(b"forever")),
        );

        // Both should exist initially
        let guard = art.lock().await;
        let results = guard.getn(Bytes::from_static(b"batch:"));
        assert_eq!(results.len(), 2, "should have 2 entries before expiration");
        drop(results);
        drop(guard);

        // Wait 2 seconds for batch:1 to expire
        tokio::time::sleep(Duration::from_secs(2)).await;

        // Yield to let the ticker task run and update the timestamp
        tokio::time::sleep(Duration::from_millis(150)).await;

        // Only batch:2 should remain
        let guard = art.lock().await;
        let results = guard.getn(Bytes::from_static(b"batch:"));
        assert_eq!(results.len(), 1, "should have 1 entry after expiration");
        assert_eq!(
            results[0],
            (
                Bytes::from_static(b"batch:2"),
                &Value::String(Bytes::from_static(b"forever"))
            )
        );
    }

    #[tokio::test]
    async fn test_shared_with_ticker_constructor() {
        // Use the convenience constructor
        let art = OxidArt::shared_with_ticker(Duration::from_millis(100)).await;

        // Set a key with TTL
        art.lock().await.set_ttl(
            Bytes::from_static(b"test"),
            Duration::from_secs(1),
            Value::String(Bytes::from_static(b"value")),
        );

        // Should exist initially
        assert!(art.lock().await.get(Bytes::from_static(b"test")).is_some());

        // Wait for expiration
        tokio::time::sleep(Duration::from_secs(2)).await;
        tokio::time::sleep(Duration::from_millis(150)).await;

        // Should be expired
        assert!(art.lock().await.get(Bytes::from_static(b"test")).is_none());
    }
}
