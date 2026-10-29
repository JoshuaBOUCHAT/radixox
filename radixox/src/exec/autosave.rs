//! Saves automatiques et état de la dernière save, comme `save <secondes>
//! <changements>`, `stop-writes-on-bgsave-error`, `LASTSAVE` et `INFO
//! persistence` de Redis / Valkey. Sans I/O : le cœur lance le BGSAVE quand
//! [`Persistence::due`] le demande et rapporte son issue.

use std::fmt::Write as _;
use core::time::Duration;

use radixox_lib::sys::{self, Instant};

/// Règles de Valkey sans fichier de configuration.
pub const DEFAULT_RULES: &str = "3600 1 300 100 60 10000";

/// Après un échec, pas de nouvel essai automatique avant ce délai (Redis :
/// `CONFIG_BGSAVE_RETRY_DELAY`).
const RETRY_DELAY: Duration = Duration::from_secs(5);

/// Save si au moins `changes` écritures et plus de `secs` secondes depuis la
/// dernière save réussie.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rule {
    pub secs: u64,
    pub changes: u64,
}

/// `"3600 1 300 100"` → deux règles ; `""` → aucune (saves automatiques
/// désactivées, comme `save ""`).
pub fn parse_rules(s: &str) -> Result<Vec<Rule>, String> {
    let nums = s
        .split_whitespace()
        .map(str::parse::<u64>)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| format!("save « {s} » : entiers positifs attendus"))?;
    if nums.len() % 2 != 0 {
        return Err(format!("save « {s} » : paires <secondes> <changements> attendues"));
    }
    Ok(nums.chunks(2).map(|p| Rule { secs: p[0], changes: p[1] }).collect())
}

/// Règles de `RADIXOX_SAVE` (défaut : celles de Valkey).
pub fn rules_from_env() -> Result<Vec<Rule>, String> {
    parse_rules(sys::env("RADIXOX_SAVE").unwrap_or(DEFAULT_RULES))
}

/// `RADIXOX_STOP_WRITES_ON_BGSAVE_ERROR` (défaut : oui, comme Valkey).
pub fn stop_writes_from_env() -> bool {
    sys::env("RADIXOX_STOP_WRITES_ON_BGSAVE_ERROR")
        .map_or(true, |v| !matches!(v.to_ascii_lowercase().as_str(), "0" | "no" | "false" | "off"))
}

fn unix_now() -> u64 {
    sys::wall_secs()
}

pub struct Persistence {
    rules: Vec<Rule>,
    stop_writes_on_error: bool,
    /// Écritures depuis la dernière save réussie (`rdb_changes_since_last_save`).
    dirty: u64,
    /// `dirty` au début de la save en cours : ce qu'elle couvre.
    dirty_at_start: u64,
    /// Dernière save réussie (au départ : le démarrage), murale (`LASTSAVE`)
    /// et monotone (règles).
    last_save_unix: u64,
    last_save: Instant,
    last_try: Option<Instant>,
    last_ok: bool,
    last_duration: Option<Duration>,
    /// Début du BGSAVE en cours.
    current: Option<Instant>,
    saves: u64,
}

impl Persistence {
    pub fn new(rules: Vec<Rule>, stop_writes_on_error: bool, now: Instant) -> Self {
        Self {
            rules,
            stop_writes_on_error,
            dirty: 0,
            dirty_at_start: 0,
            last_save_unix: unix_now(),
            last_save: now,
            last_try: None,
            last_ok: true,
            last_duration: None,
            current: None,
            saves: 0,
        }
    }

    pub fn has_rules(&self) -> bool {
        !self.rules.is_empty()
    }

    pub fn note_writes(&mut self, n: u64) {
        self.dirty += n;
    }

    /// Règle qui demande un BGSAVE maintenant, s'il n'y en a pas déjà un.
    pub fn due(&self, now: Instant) -> Option<Rule> {
        if self.current.is_some() {
            return None;
        }
        if !self.last_ok && self.last_try.is_some_and(|t| now.saturating_duration_since(t) <= RETRY_DELAY) {
            return None;
        }
        let since = now.saturating_duration_since(self.last_save);
        self.rules.iter().copied().find(|r| self.dirty >= r.changes && since > Duration::from_secs(r.secs))
    }

    /// Écritures refusées (`-MISCONF`) : des règles sont actives et le dernier
    /// BGSAVE a échoué.
    pub fn refuse_writes(&self) -> bool {
        self.stop_writes_on_error && self.has_rules() && !self.last_ok
    }

    pub fn bgsave_started(&mut self, now: Instant) {
        self.current = Some(now);
        self.last_try = Some(now);
        self.dirty_at_start = self.dirty;
    }

    /// Fin d'un BGSAVE (ou échec à son démarrage).
    pub fn bgsave_finished(&mut self, now: Instant, ok: bool) {
        let start = self.current.take().unwrap_or(now);
        self.last_duration = Some(now.saturating_duration_since(start));
        self.last_ok = ok;
        if ok {
            // Les écritures faites pendant la save ne sont pas dans le dump.
            self.dirty -= self.dirty_at_start.min(self.dirty);
            self.saved(now);
        }
    }

    /// SAVE (bloquante) réussie : tout est dans le dump.
    pub fn save_done(&mut self, now: Instant) {
        self.dirty = 0;
        self.last_ok = true;
        self.saved(now);
    }

    fn saved(&mut self, now: Instant) {
        self.last_save = now;
        self.last_save_unix = unix_now();
        self.saves += 1;
    }

    pub fn last_save_unix(&self) -> u64 {
        self.last_save_unix
    }

    /// Section `# Persistence` d'`INFO`, champs de Valkey (sans ceux de l'AOF ni
    /// la ligne vide de fin, ajoutés par le cœur).
    pub fn info(&self, now: Instant, out: &mut String) {
        let secs = |d: Duration| d.as_secs() as i64;
        let _ = write!(
            out,
            "# Persistence\r\nloading:0\r\nrdb_changes_since_last_save:{}\r\nrdb_bgsave_in_progress:{}\r\n\
             rdb_last_save_time:{}\r\nrdb_last_bgsave_status:{}\r\nrdb_last_bgsave_time_sec:{}\r\n\
             rdb_current_bgsave_time_sec:{}\r\nrdb_saves:{}\r\n",
            self.dirty,
            u8::from(self.current.is_some()),
            self.last_save_unix,
            if self.last_ok { "ok" } else { "err" },
            self.last_duration.map_or(-1, secs),
            self.current.map_or(-1, |t| secs(now.saturating_duration_since(t))),
            self.saves,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rules_parse_like_redis() {
        assert_eq!(parse_rules("").unwrap(), vec![]);
        assert_eq!(
            parse_rules(DEFAULT_RULES).unwrap(),
            vec![Rule { secs: 3600, changes: 1 }, Rule { secs: 300, changes: 100 }, Rule { secs: 60, changes: 10000 }]
        );
        assert!(parse_rules("60").is_err());
        assert!(parse_rules("60 -1").is_err());
    }

    #[test]
    fn due_after_secs_and_changes_then_reset() {
        let t0 = Instant::now();
        let mut p = Persistence::new(parse_rules("60 3").unwrap(), true, t0);
        p.note_writes(3);
        assert_eq!(p.due(t0 + Duration::from_secs(60)), None, "strictement plus de 60 s");
        let t1 = t0 + Duration::from_secs(61);
        assert!(p.due(t1).is_some());
        p.bgsave_started(t1);
        assert_eq!(p.due(t1), None, "déjà en cours");
        p.note_writes(2); // pendant la save : pas dans le dump
        p.bgsave_finished(t1 + Duration::from_secs(1), true);
        assert_eq!(p.dirty, 2);
        assert_eq!(p.saves, 1);
        p.note_writes(1);
        assert_eq!(p.due(t1 + Duration::from_secs(30)), None, "60 s depuis la dernière save");
        assert!(p.due(t1 + Duration::from_secs(62)).is_some());
    }

    #[test]
    fn failure_refuses_writes_and_waits_before_retry() {
        let t0 = Instant::now();
        let mut p = Persistence::new(parse_rules("1 1").unwrap(), true, t0);
        p.note_writes(1);
        let t1 = t0 + Duration::from_secs(2);
        p.bgsave_started(t1);
        p.bgsave_finished(t1, false);
        assert!(p.refuse_writes());
        assert_eq!(p.due(t1 + Duration::from_secs(5)), None, "attente de 5 s après un échec");
        assert!(p.due(t1 + Duration::from_secs(6)).is_some());
        p.save_done(t1 + Duration::from_secs(6));
        assert!(!p.refuse_writes());
        assert_eq!(p.dirty, 0);
        let off = Persistence::new(parse_rules("1 1").unwrap(), false, t0);
        assert!(!off.refuse_writes());
    }
}
