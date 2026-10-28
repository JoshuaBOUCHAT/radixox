//! `maxmemory` (MEMORY_ARCH.md §10.5) : politiques, tailles, `CONFIG`.
//!
//! La borne porte sur le **total** compté par le registre des pages
//! (`radixox_lib::pages`), découpé en budgets : entrée, sortie, réserve de save, données.
//! Les politiques qui n'écrivent rien à la lecture sont dans tout binaire ; LRU et LFU
//! (une écriture par accès) demandent la feature cargo `eviction`.

/// Politique d'éviction (`maxmemory-policy`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Policy {
    /// `-OOM` à la limite, rien n'est évincé (défaut, comme Redis).
    #[default]
    NoEviction,
    /// Toute clé, par balayage des slots (pas de tirage : chaque clé au plus une fois par
    /// tour, à peu près FIFO).
    AllKeysRandom,
    /// Clés avec TTL seulement, par balayage du bitmap tagué.
    VolatileRandom,
    /// Clés avec TTL, l'échéance la plus proche d'abord (sur une fenêtre du balayage).
    VolatileTtl,
    AllKeysLru,
    AllKeysLfu,
    VolatileLru,
    VolatileLfu,
}

impl Policy {
    pub const ALL: [Policy; 8] = [
        Policy::NoEviction,
        Policy::AllKeysRandom,
        Policy::VolatileRandom,
        Policy::VolatileTtl,
        Policy::AllKeysLru,
        Policy::AllKeysLfu,
        Policy::VolatileLru,
        Policy::VolatileLfu,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Policy::NoEviction => "noeviction",
            Policy::AllKeysRandom => "allkeys-random",
            Policy::VolatileRandom => "volatile-random",
            Policy::VolatileTtl => "volatile-ttl",
            Policy::AllKeysLru => "allkeys-lru",
            Policy::AllKeysLfu => "allkeys-lfu",
            Policy::VolatileLru => "volatile-lru",
            Policy::VolatileLfu => "volatile-lfu",
        }
    }

    /// Écrit à chaque lecture : seulement avec la feature `eviction`.
    pub fn needs_feature(self) -> bool {
        matches!(self, Policy::AllKeysLru | Policy::AllKeysLfu | Policy::VolatileLru | Policy::VolatileLfu)
    }

    /// Ne regarde que les clés avec TTL.
    pub fn volatile(self) -> bool {
        matches!(self, Policy::VolatileRandom | Policy::VolatileTtl | Policy::VolatileLru | Policy::VolatileLfu)
    }

    /// Le nom, ou un message d'erreur. Une politique qui demande la feature `eviction`
    /// est refusée si le binaire ne l'a pas : pas de repli silencieux.
    pub fn parse(s: &str) -> Result<Policy, String> {
        let p = Policy::ALL
            .into_iter()
            .find(|p| p.name().eq_ignore_ascii_case(s))
            .ok_or_else(|| format!("unknown maxmemory-policy '{s}'"))?;
        if p.needs_feature() && !cfg!(feature = "eviction") {
            return Err(format!(
                "maxmemory-policy '{}' needs a binary built with the 'eviction' feature (cargo install radixox --features eviction)",
                p.name()
            ));
        }
        Ok(p)
    }

    /// Politiques disponibles dans ce binaire (`INFO`).
    pub fn available() -> impl Iterator<Item = Policy> {
        Policy::ALL.into_iter().filter(|p| !p.needs_feature() || cfg!(feature = "eviction"))
    }
}

/// Taille à la Redis : `1048576`, `100mb`, `2gb`, `512k`… (unités en 1024).
pub fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim().to_ascii_lowercase();
    let split = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let n: u64 = num.parse().ok()?;
    let mul: u64 = match unit {
        "" | "b" => 1,
        "k" | "kb" => 1 << 10,
        "m" | "mb" => 1 << 20,
        "g" | "gb" => 1 << 30,
        "t" | "tb" => 1 << 40,
        _ => return None,
    };
    n.checked_mul(mul)
}

/// Motif de `CONFIG GET` : `*` et `?`, sans classes (comme les noms de paramètres).
pub fn glob(pattern: &[u8], name: &[u8]) -> bool {
    match pattern.split_first() {
        None => name.is_empty(),
        Some((b'*', rest)) => (0..=name.len()).any(|i| glob(rest, &name[i..])),
        Some((&c, rest)) => name
            .split_first()
            .is_some_and(|(&n, tail)| (c == b'?' || c.eq_ignore_ascii_case(&n)) && glob(rest, tail)),
    }
}

/// Message de Redis, reconnu par les clients.
pub const OOM: &str = "OOM command not allowed when used memory > 'maxmemory'.";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(parse_size("1048576"), Some(1 << 20));
        assert_eq!(parse_size("100mb"), Some(100 << 20));
        assert_eq!(parse_size("2GB"), Some(2 << 30));
        assert_eq!(parse_size("512k"), Some(512 << 10));
        assert_eq!(parse_size("0"), Some(0));
        assert_eq!(parse_size("12xb"), None);
        assert_eq!(parse_size("mb"), None);
    }

    #[test]
    fn policies() {
        assert_eq!(Policy::parse("noeviction"), Ok(Policy::NoEviction));
        assert_eq!(Policy::parse("VOLATILE-TTL"), Ok(Policy::VolatileTtl));
        assert!(Policy::parse("allkeys-whatever").is_err());
        assert_eq!(Policy::parse("allkeys-lfu").is_ok(), cfg!(feature = "eviction"));
        for p in Policy::ALL {
            assert_eq!(Policy::parse(p.name()).is_ok(), !p.needs_feature() || cfg!(feature = "eviction"));
        }
    }

    #[test]
    fn globs() {
        assert!(glob(b"maxmemory*", b"maxmemory-policy"));
        assert!(glob(b"*", b"anything"));
        assert!(glob(b"MAXMEMORY", b"maxmemory"));
        assert!(glob(b"max?emory", b"maxmemory"));
        assert!(!glob(b"maxmemory", b"maxmemory-policy"));
    }
}
