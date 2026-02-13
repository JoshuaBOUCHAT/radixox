//! Monoio integration for OxidArt.
//!
//! This module provides async utilities for single-threaded monoio runtimes,
//! including automatic timestamp management for TTL functionality.
//!
//! # Example
//!
//! ```rust,ignore
//! use oxidart::OxidArt;
//! use std::time::Duration;
//!
//! #[monoio::main(enable_timer = true)]
//! async fn main() {
//!     // Recommended: creates shared tree with automatic ticker
//!     let tree = OxidArt::shared_with_ticker(Duration::from_millis(100));
//!
//!     // Your server loop here...
//!     tree.borrow_mut().set(/* ... */);
//! }
//! ```

use crate::OxidArt;
use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

/// Shared OxidArt type for monoio (single-threaded).
pub type SharedArt = Rc<RefCell<OxidArt>>;

impl OxidArt {
    /// Creates a new shared OxidArt with an automatic background ticker.
    ///
    /// This is the recommended constructor when using TTL features with monoio.
    /// It returns an `Rc<RefCell<OxidArt>>` and spawns a background task that
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
    /// #[monoio::main(enable_timer = true)]
    /// async fn main() {
    ///     let tree = OxidArt::shared_with_ticker(Duration::from_millis(100));
    ///
    ///     tree.borrow_mut().set_ttl(
    ///         Bytes::from_static(b"key"),
    ///         Duration::from_secs(60),
    ///         Bytes::from_static(b"value"),
    ///     );
    /// }
    /// ```
    pub fn shared_with_ticker(interval: Duration) -> SharedArt {
        let art = Rc::new(RefCell::new(Self::new()));
        art.borrow_mut().tick(); // Initial tick
        spawn_ticker(art.clone(), interval);
        art
    }

    /// Creates a new shared OxidArt with automatic background ticker and evictor.
    ///
    /// This is the recommended constructor for production use with TTL features.
    /// It returns an `Rc<RefCell<OxidArt>>` and spawns two background tasks:
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
    /// #[monoio::main(enable_timer = true)]
    /// async fn main() {
    ///     let tree = OxidArt::shared_with_evictor(
    ///         Duration::from_millis(100),
    ///         Duration::from_secs(1),
    ///     );
    ///
    ///     tree.borrow_mut().set_ttl(
    ///         Bytes::from_static(b"key"),
    ///         Duration::from_secs(60),
    ///         Bytes::from_static(b"value"),
    ///     );
    /// }
    /// ```
    pub fn shared_with_evictor(tick_interval: Duration, evict_interval: Duration) -> SharedArt {
        let art = Rc::new(RefCell::new(Self::new()));
        art.borrow_mut().tick(); // Initial tick
        spawn_ticker(art.clone(), tick_interval);
        spawn_evictor(art.clone(), evict_interval);
        art
    }

    /// Updates the internal timestamp to the current system time.
    ///
    /// This is a convenience method for single-threaded async runtimes.
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
/// This is designed for single-threaded monoio runtimes. The ticker runs
/// cooperatively with other tasks in the same thread, updating at each
/// `interval` to keep TTL checks accurate.
///
/// # Arguments
///
/// * `art` - A shared reference to the tree (typically `Rc<RefCell<OxidArt>>`)
/// * `interval` - How often to update the timestamp (e.g., 100ms)
///
/// # Example
///
/// ```rust,ignore
/// use oxidart::OxidArt;
/// use std::cell::RefCell;
/// use std::rc::Rc;
/// use std::time::Duration;
///
/// #[monoio::main]
/// async fn main() {
///     let shared_art = Rc::new(RefCell::new(OxidArt::new()));
///
///     // Spawn ticker - updates every 100ms
///     oxidart::monoio::spawn_ticker(shared_art.clone(), Duration::from_millis(100));
///
///     loop {
///         // handle connections...
///         // No need to manually call tick(), it's handled automatically
///     }
/// }
/// ```
pub fn spawn_ticker(art: Rc<RefCell<OxidArt>>, interval: Duration) {
    monoio::spawn(async move {
        loop {
            monoio::time::sleep(interval).await;
            art.borrow_mut().tick();
        }
    });
}

/// Spawns a background task that periodically evicts expired entries.
///
/// This implements Redis-style probabilistic eviction: samples random entries
/// with TTL and removes expired ones. If many are expired, it continues sampling.
///
/// # Arguments
///
/// * `art` - A shared reference to the tree (`Rc<RefCell<OxidArt>>`)
/// * `interval` - How often to run eviction (e.g., 1s)
///
/// # Example
///
/// ```rust,ignore
/// use oxidart::OxidArt;
/// use std::cell::RefCell;
/// use std::rc::Rc;
/// use std::time::Duration;
///
/// #[monoio::main(enable_timer = true)]
/// async fn main() {
///     let shared_art = Rc::new(RefCell::new(OxidArt::new()));
///
///     // Spawn ticker and evictor
///     oxidart::monoio::spawn_ticker(shared_art.clone(), Duration::from_millis(100));
///     oxidart::monoio::spawn_evictor(shared_art.clone(), Duration::from_secs(1));
/// }
/// ```
pub fn spawn_evictor(art: Rc<RefCell<OxidArt>>, interval: Duration) {
    monoio::spawn(async move {
        loop {
            monoio::time::sleep(interval).await;
            art.borrow_mut().evict_expired();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use crate::value::Value;

    #[monoio::test(enable_timer = true)]
    async fn test_ttl_expiration_with_ticker() {
        let art = Rc::new(RefCell::new(OxidArt::new()));

        // Spawn the ticker (updates every 100ms)
        spawn_ticker(art.clone(), Duration::from_millis(100));

        // Initial tick to set current time
        art.borrow_mut().tick();

        // batch:1 expires in 1 second
        art.borrow_mut().set_ttl(
            Bytes::from_static(b"batch:1"),
            Duration::from_secs(1),
            Value::String(Bytes::from_static(b"expires_soon")),
        );

        // batch:2 never expires
        art.borrow_mut().set(
            Bytes::from_static(b"batch:2"),
            Value::String(Bytes::from_static(b"forever")),
        );

        // Both should exist initially
        let guard = art.borrow();
        let results = guard.getn(Bytes::from_static(b"batch:"));
        assert_eq!(results.len(), 2, "should have 2 entries before expiration");
        drop(results);
        drop(guard);

        // Wait 2 seconds for batch:1 to expire
        monoio::time::sleep(Duration::from_secs(2)).await;

        // Yield to let the ticker task run and update the timestamp
        monoio::time::sleep(Duration::from_millis(150)).await;

        // Only batch:2 should remain
        let guard = art.borrow();
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

    #[monoio::test(enable_timer = true)]
    async fn test_shared_with_ticker_constructor() {
        // Use the convenience constructor
        let art = OxidArt::shared_with_ticker(Duration::from_millis(100));

        // Set a key with TTL
        art.borrow_mut().set_ttl(
            Bytes::from_static(b"test"),
            Duration::from_secs(1),
            Value::String(Bytes::from_static(b"value")),
        );

        // Should exist initially
        assert!(art.borrow_mut().get(Bytes::from_static(b"test")).is_some());

        // Wait for expiration
        monoio::time::sleep(Duration::from_secs(2)).await;
        monoio::time::sleep(Duration::from_millis(150)).await;

        // Should be expired
        assert!(art.borrow_mut().get(Bytes::from_static(b"test")).is_none());
    }

    #[monoio::test(enable_timer = true)]
    async fn test_spawn_evictor_100_entries() {
        let art = Rc::new(RefCell::new(OxidArt::new()));

        // Set initial time
        art.borrow_mut().set_now(1000);

        // Create 100 entries with TTL of 10 seconds (expire at t=1010)
        for i in 0..100 {
            let key = Bytes::from(format!("key:{:03}", i));
            let val = Value::String(Bytes::from(format!("value:{:03}", i)));
            art.borrow_mut().set_ttl(key, Duration::from_secs(10), val);
        }

        // Verify all 100 entries exist
        let guard = art.borrow();
        let all_entries = guard.getn(Bytes::from_static(b"key:"));
        assert_eq!(all_entries.len(), 100, "should have 100 entries initially");
        drop(all_entries);
        drop(guard);

        // Spawn evictor with 1ms interval (no ticker - we control time manually)
        spawn_evictor(art.clone(), Duration::from_millis(1));

        // Let evictor run a bit - nothing should be evicted yet (time hasn't moved)
        monoio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            art.borrow().getn(Bytes::from_static(b"key:")).len(),
            100,
            "no entries should be evicted yet"
        );

        // Advance time past expiration (t=1011)
        art.borrow_mut().set_now(1011);

        // Let evictor run and clean up expired entries
        // With 1ms interval and 100 entries, give it enough time
        monoio::time::sleep(Duration::from_millis(100)).await;

        // All should be evicted now
        let guard = art.borrow();
        let remaining = guard.getn(Bytes::from_static(b"key:"));
        assert_eq!(
            remaining.len(),
            0,
            "all entries should be evicted, but {} remain",
            remaining.len()
        );
    }

    #[monoio::test(enable_timer = true)]
    async fn test_spawn_evictor_partial_expiration() {
        let art = Rc::new(RefCell::new(OxidArt::new()));

        art.borrow_mut().set_now(1000);

        // 50 entries expire at t=1010
        for i in 0..50 {
            let key = Bytes::from(format!("short:{:03}", i));
            let val = Value::String(Bytes::from(format!("value:{:03}", i)));
            art.borrow_mut().set_ttl(key, Duration::from_secs(10), val);
        }

        // 50 entries expire at t=1100
        for i in 0..50 {
            let key = Bytes::from(format!("long:{:03}", i));
            let val = Value::String(Bytes::from(format!("value:{:03}", i)));
            art.borrow_mut().set_ttl(key, Duration::from_secs(100), val);
        }

        // Verify all 100 entries exist
        assert_eq!(art.borrow().getn(Bytes::from_static(b"")).len(), 100);

        // Spawn evictor with 1ms interval
        spawn_evictor(art.clone(), Duration::from_millis(1));

        // Advance to t=1011 - only "short:" entries are expired
        art.borrow_mut().set_now(1011);

        // Let evictor clean up
        monoio::time::sleep(Duration::from_millis(100)).await;

        // Should have evicted the 50 short ones
        let guard = art.borrow();
        let remaining = guard.getn(Bytes::from_static(b""));
        assert_eq!(remaining.len(), 50, "50 long entries should remain");

        // All remaining should be "long:" entries
        for (key, _) in &remaining {
            assert!(
                key.starts_with(b"long:"),
                "remaining key should be long: {:?}",
                key
            );
        }
        drop(remaining);
        drop(guard);

        // Advance to t=1101 - now "long:" entries are also expired
        art.borrow_mut().set_now(1101);

        // Let evictor clean up again
        monoio::time::sleep(Duration::from_millis(100)).await;

        // All should be gone
        assert_eq!(art.borrow().getn(Bytes::from_static(b"")).len(), 0);
    }
}
