use bytes::Bytes;

use crate::OxidArt;
use crate::value::{IntError, Value};

/// Error type for counter operations.
#[derive(Debug, PartialEq, Eq)]
pub enum CounterError {
    /// The stored value is not a valid integer.
    NotAnInteger,
    /// The operation would overflow i64.
    Overflow,
}

impl From<IntError> for CounterError {
    fn from(e: IntError) -> Self {
        match e {
            IntError::NotAnInteger => CounterError::NotAnInteger,
            IntError::Overflow => CounterError::Overflow,
        }
    }
}

impl OxidArt {
    /// Increments the integer value of a key by `delta`.
    ///
    /// Single tree traversal: finds the node, uses Value::incr() in-place.
    /// If the key does not exist (or is expired), it is initialized to `Int(delta)`.
    /// Existing TTL is preserved.
    pub fn incrby(&mut self, key: Bytes, delta: i64) -> Result<i64, CounterError> {
        if let Some(idx) = self.traverse_to_key(&key) {
            if let Some(val) = self.node_value_mut(idx) {
                return Ok(val.incr(delta)?);
            }
        }

        // Key doesn't exist or expired — create as Int directly
        self.set(key, Value::Int(delta));
        Ok(delta)
    }

    /// Increments the integer value of a key by 1.
    #[inline]
    pub fn incr(&mut self, key: Bytes) -> Result<i64, CounterError> {
        self.incrby(key, 1)
    }

    /// Decrements the integer value of a key by 1.
    #[inline]
    pub fn decr(&mut self, key: Bytes) -> Result<i64, CounterError> {
        self.incrby(key, -1)
    }

    /// Decrements the integer value of a key by `delta`.
    #[inline]
    pub fn decrby(&mut self, key: Bytes, delta: i64) -> Result<i64, CounterError> {
        self.incrby(key, delta.wrapping_neg())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn incr_new_key() {
        let mut tree = OxidArt::new();
        assert_eq!(tree.incr(Bytes::from_static(b"counter")), Ok(1));
    }

    #[test]
    fn incr_existing() {
        let mut tree = OxidArt::new();
        tree.set(Bytes::from_static(b"counter"), Value::String(Bytes::from_static(b"10")));
        assert_eq!(tree.incr(Bytes::from_static(b"counter")), Ok(11));
        assert_eq!(tree.incr(Bytes::from_static(b"counter")), Ok(12));
    }

    #[test]
    fn decr_below_zero() {
        let mut tree = OxidArt::new();
        tree.set(Bytes::from_static(b"counter"), Value::String(Bytes::from_static(b"1")));
        assert_eq!(tree.decr(Bytes::from_static(b"counter")), Ok(0));
        assert_eq!(tree.decr(Bytes::from_static(b"counter")), Ok(-1));
    }

    #[test]
    fn incrby_amount() {
        let mut tree = OxidArt::new();
        tree.set(Bytes::from_static(b"counter"), Value::String(Bytes::from_static(b"100")));
        assert_eq!(tree.incrby(Bytes::from_static(b"counter"), 50), Ok(150));
    }

    #[test]
    fn decrby_amount() {
        let mut tree = OxidArt::new();
        tree.set(Bytes::from_static(b"counter"), Value::String(Bytes::from_static(b"100")));
        assert_eq!(tree.decrby(Bytes::from_static(b"counter"), 30), Ok(70));
    }

    #[test]
    fn not_an_integer() {
        let mut tree = OxidArt::new();
        tree.set(Bytes::from_static(b"name"), Value::String(Bytes::from_static(b"alice")));
        assert_eq!(
            tree.incr(Bytes::from_static(b"name")),
            Err(CounterError::NotAnInteger)
        );
    }

    #[test]
    fn overflow() {
        let mut tree = OxidArt::new();
        tree.set(Bytes::from_static(b"big"), Value::String(Bytes::from(i64::MAX.to_string())));
        assert_eq!(
            tree.incr(Bytes::from_static(b"big")),
            Err(CounterError::Overflow)
        );
    }

    #[test]
    fn negative_value() {
        let mut tree = OxidArt::new();
        tree.set(Bytes::from_static(b"neg"), Value::String(Bytes::from_static(b"-5")));
        assert_eq!(tree.incr(Bytes::from_static(b"neg")), Ok(-4));
    }

    #[test]
    fn incr_converts_to_int() {
        let mut tree = OxidArt::new();
        tree.set(Bytes::from_static(b"counter"), Value::String(Bytes::from_static(b"42")));
        assert_eq!(tree.incr(Bytes::from_static(b"counter")), Ok(43));
        // Should now be Value::Int internally
        let val = tree.get(Bytes::from_static(b"counter")).unwrap();
        assert!(matches!(val, &Value::Int(43)));
    }

    #[test]
    fn expired_key_resets() {
        let mut tree = OxidArt::new();
        tree.set_now(100);
        tree.set_ttl(
            Bytes::from_static(b"counter"),
            std::time::Duration::from_secs(10),
            Value::String(Bytes::from_static(b"50")),
        );
        assert_eq!(tree.incr(Bytes::from_static(b"counter")), Ok(51));

        tree.set_now(200);
        assert_eq!(tree.incr(Bytes::from_static(b"counter")), Ok(1));
    }

    #[test]
    fn preserves_ttl() {
        let mut tree = OxidArt::new();
        tree.set_now(100);
        tree.set_ttl(
            Bytes::from_static(b"counter"),
            std::time::Duration::from_secs(60),
            Value::String(Bytes::from_static(b"10")),
        );
        assert_eq!(tree.incr(Bytes::from_static(b"counter")), Ok(11));
        let ttl = tree.get_ttl(Bytes::from_static(b"counter"));
        assert!(matches!(ttl, crate::TtlResult::KeyWithTtl(_)));
    }
}
