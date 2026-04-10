pub(crate) mod delayed;
mod hash;
mod sset;
pub(crate) mod string;
mod zset;

pub use hash::{
    cmd_hdel, cmd_hexists, cmd_hget, cmd_hgetall, cmd_hincrby, cmd_hkeys, cmd_hlen, cmd_hmget,
    cmd_hmset, cmd_hset, cmd_hvals,
};
pub use sset::{cmd_sadd, cmd_scard, cmd_sismember, cmd_smembers, cmd_spop, cmd_srem};

pub use zset::{cmd_zadd, cmd_zcard, cmd_zincrby, cmd_zrange, cmd_zrem, cmd_zscore};

/// Returns true if the pattern is a simple prefix (no glob chars except a trailing `*`).
pub(crate) fn is_simple_prefix(pattern: &[u8]) -> bool {
    let end = if pattern.ends_with(b"*") {
        pattern.len() - 1
    } else {
        pattern.len()
    };
    !pattern[..end]
        .iter()
        .any(|&b| b == b'*' || b == b'?' || b == b'[' || b == b']')
}

/// Converts a Redis glob pattern to an anchored regex string.
///
/// Redis glob rules:
///   `*`     → `.*`      (any sequence)
///   `?`     → `.`       (one char)
///   `[abc]` → `[abc]`   (character class, passed through)
///   `\x`    → `\x`      (escape, passed through)
///   other   → escaped literal
pub(crate) fn glob_to_regex(pattern: &[u8]) -> String {
    let mut regex = String::with_capacity(pattern.len() * 2);
    regex.push('^');
    let mut i = 0;
    while i < pattern.len() {
        match pattern[i] {
            b'*' => regex.push_str(".*"),
            b'?' => regex.push('.'),
            b'[' => {
                regex.push('[');
                i += 1;
                while i < pattern.len() && pattern[i] != b']' {
                    regex.push(pattern[i] as char);
                    i += 1;
                }
                if i < pattern.len() {
                    regex.push(']');
                }
            }
            b'\\' if i + 1 < pattern.len() => {
                regex.push('\\');
                i += 1;
                regex.push(pattern[i] as char);
            }
            b'.' | b'+' | b'^' | b'$' | b'{' | b'}' | b'(' | b')' | b'|' | b'#' | b'&' | b'~' => {
                regex.push('\\');
                regex.push(pattern[i] as char);
            }
            b => regex.push(b as char),
        }
        i += 1;
    }
    regex.push('$');
    regex
}
