//! Commandes longues, découpées en tranches (RUNTIME_ARCH.md §8) : KEYS,
//! UNLINK `prefix*`, libération de FLUSHDB.
//!
//! Un job est un curseur synchrone : [`Job::step`] avance d'une tranche et dit
//! s'il reste du travail. N'importe quelle boucle peut le piloter (la boucle
//! io_uring, les tests). Entre deux tranches, il ne
//! garde rien de daté : une **clé** de reprise (KEYS) ou des sous-arbres
//! détachés qui n'appartiennent qu'à lui (UNLINK, FLUSHDB).
//!
//! La réponse est encodée en octets au fil du parcours, pas reconstruite à la
//! fin depuis des `SharedByte` : la copie de chaque clé se superpose aux accès
//! mémoire du parcours.

use oxidart::regex::{GlobKind, glob_kind, glob_literal_prefix, glob_match};
use oxidart::{Detached, OxidArt};
use radixox_lib::big_buf::BigBuf;
use radixox_lib::shared_byte::SharedByte;
use radixox_lib::shared_frame::encode_array_len;

/// Clés examinées par tranche de KEYS (~quelques dizaines de µs).
pub const KEYS_BUDGET: usize = 256;
/// Nœuds libérés par tranche d'UNLINK / FLUSHDB.
pub const FREE_BUDGET: usize = 256;

/// Place réservée en tête de réponse pour l'en-tête `*N\r\n` (N ≤ 20 chiffres).
const HEADER_ROOM: usize = 1 + 20 + 2;

/// Où en est un job après une tranche.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    More,
    Done,
}

impl Step {
    fn from_done(done: bool) -> Self {
        if done { Step::Done } else { Step::More }
    }
}

/// KEYS pattern : parcours par pages du sous-arbre du préfixe littéral du
/// motif, reprise sur la dernière clé examinée (une clé reste une question
/// valide même si l'arbre a changé entre deux tranches, comme SCAN).
/// Résultat « au mieux », comme le KEYS de Redis découpé.
pub struct KeysJob {
    pattern: SharedByte,
    /// Reprendre strictement après cette clé ; `None` au départ.
    from: Option<Vec<u8>>,
    /// Réponse : `HEADER_ROOM` octets réservés, puis les clés en bulk strings ;
    /// mapping à lui, grandi sans copie (MEMORY_ARCH.md §18.11).
    out: BigBuf,
    count: usize,
    done: bool,
}

impl KeysJob {
    pub fn new(pattern: SharedByte) -> Self {
        let mut out = BigBuf::new();
        out.extend_from_slice(&[0; HEADER_ROOM]);
        Self { pattern, from: None, out, count: 0, done: false }
    }

    pub fn step(&mut self, art: &mut OxidArt, budget: usize) -> Step {
        if self.done {
            return Step::Done;
        }
        let pat = self.pattern.as_slice();
        let (prefix, exact) = match glob_kind(pat) {
            // `KEYS user:1` : la clé exacte seulement (pas user:10, user:100…).
            GlobKind::Exact(key) => {
                let mut expired = Vec::new();
                if art.exists_count(&[key], &mut expired) == 1 {
                    self.out.push_bulk(key);
                    self.count += 1;
                }
                if !expired.is_empty() {
                    art.purge_expired(&[key], &expired);
                }
                self.done = true;
                return Step::Done;
            }
            GlobKind::Prefix(p) => (p, true),
            GlobKind::Pattern => (glob_literal_prefix(pat), false),
        };
        let from = self.from.as_deref().unwrap_or(b"");
        let page = art.scan_keys(prefix, from, budget, budget, |key, _| exact || glob_match(pat, key));
        for key in page.iter() {
            self.out.push_bulk(key);
        }
        self.count += page.len();
        self.from = page.next;
        self.done = self.from.is_none();
        Step::from_done(self.done)
    }

    /// Réponse complète (tableau RESP), à appeler une fois le job terminé.
    pub fn response(&mut self) -> &[u8] {
        let start = write_header(self.out.as_mut_slice(), self.count);
        &self.out.as_slice()[start..]
    }

    /// Réponse complète, sans copie : le tampon et le début de la réponse dedans
    /// (après la place non utilisée de l'en-tête).
    pub fn into_response(mut self) -> (BigBuf, usize) {
        let start = write_header(self.out.as_mut_slice(), self.count);
        (self.out, start)
    }
}

/// Écrit `*count\r\n` calé à droite de la place réservée ; rend son début.
fn write_header(out: &mut [u8], count: usize) -> usize {
    let mut head = Vec::with_capacity(HEADER_ROOM);
    encode_array_len(&mut head, count);
    let start = HEADER_ROOM - head.len();
    out[start..HEADER_ROOM].copy_from_slice(&head);
    start
}

/// UNLINK key [key …] : une clé exacte est supprimée tout de suite ; `prefix*`
/// supprime tout le sous-arbre, détaché d'un coup (les clés disparaissent
/// immédiatement) puis libéré par tranches. Pendant une save, la libération
/// attend la fin de la save. Réponse : le nombre de clés supprimées, une fois
/// tout libéré. Pendant la libération, `DBSIZE` compte encore les clés pas
/// encore libérées (la taille d'un sous-arbre n'est connue qu'en le
/// parcourant).
pub struct UnlinkJob {
    detached: Detached,
    exact: usize,
}

impl UnlinkJob {
    pub fn start(art: &mut OxidArt, keys: &[SharedByte]) -> Self {
        let mut job = Self { detached: Detached::default(), exact: 0 };
        for key in keys {
            match key.as_slice().strip_suffix(b"*") {
                Some(prefix) => art.detach_prefix(prefix, &mut job.detached),
                None => job.exact += art.del(key.as_slice()) as usize,
            }
        }
        job
    }

    pub fn step(&mut self, art: &mut OxidArt, budget: usize) -> Step {
        Step::from_done(art.free_detached(&mut self.detached, budget))
    }

    pub fn count(&self) -> usize {
        self.exact + self.detached.freed
    }
}

/// FLUSHDB : toute la base est détachée d'un coup (`DBSIZE` vaut 0 tout de
/// suite, la réponse part sans attendre) ; la mémoire est rendue en fond.
pub struct FlushJob {
    detached: Detached,
}

impl FlushJob {
    pub fn start(art: &mut OxidArt) -> Self {
        let mut detached = Detached::default();
        art.detach_all(&mut detached);
        Self { detached }
    }

    pub fn step(&mut self, art: &mut OxidArt, budget: usize) -> Step {
        Step::from_done(art.free_detached(&mut self.detached, budget))
    }

    pub fn is_done(&self) -> bool {
        self.detached.is_done()
    }
}

/// Le reste d'un UNLINK dont la réponse ne compte plus (rejeu du flux) :
/// seulement de la mémoire à rendre.
impl From<UnlinkJob> for FlushJob {
    fn from(job: UnlinkJob) -> Self {
        Self { detached: job.detached }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxidart::value::Value;

    fn art(keys: &[&str]) -> OxidArt {
        let mut a = OxidArt::new();
        for k in keys {
            a.set(SharedByte::from_str(k), Value::from_str("v"));
        }
        a
    }

    fn keys(a: &mut OxidArt, pattern: &str, budget: usize) -> String {
        let mut job = KeysJob::new(SharedByte::from_str(pattern));
        while job.step(a, budget) == Step::More {}
        String::from_utf8(job.response().to_vec()).unwrap()
    }

    #[test]
    fn keys_by_slices_matches_one_shot() {
        let names: Vec<String> = (0..1000).map(|i| format!("user:{i}")).collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let mut a = art(&refs);
        a.set(SharedByte::from_str("other"), Value::from_str("v"));
        let one_shot = keys(&mut a, "user:*", usize::MAX);
        assert!(one_shot.starts_with("*1000\r\n"));
        assert_eq!(keys(&mut a, "user:*", 7), one_shot);
        assert_eq!(keys(&mut a, "*", 3).lines().next(), Some("*1001"));
    }

    #[test]
    fn keys_exact_and_pattern() {
        let mut a = art(&["user:1", "user:10", "user:2", "admin:1"]);
        assert_eq!(keys(&mut a, "user:1", 1), "*1\r\n$6\r\nuser:1\r\n");
        assert_eq!(keys(&mut a, "user:?", 1), "*2\r\n$6\r\nuser:1\r\n$6\r\nuser:2\r\n");
        assert_eq!(keys(&mut a, "*:1", 2), "*2\r\n$7\r\nadmin:1\r\n$6\r\nuser:1\r\n");
        assert_eq!(keys(&mut a, "ghost", 1), "*0\r\n");
    }

    /// UNLINK d'une clé exacte ne touche pas les clés qui la prolongent
    /// (avant : `UNLINK user:1` supprimait aussi user:10, user:100…).
    #[test]
    fn unlink_exact_vs_prefix() {
        let mut a = art(&["user:1", "user:10", "user:100", "user:2", "admin:1"]);
        let mut job = UnlinkJob::start(&mut a, &[SharedByte::from_str("user:1")]);
        while job.step(&mut a, 1) == Step::More {}
        assert_eq!(job.count(), 1);
        assert_eq!(a.len(), 4);
        let mut job = UnlinkJob::start(&mut a, &[SharedByte::from_str("user:*"), SharedByte::from_str("ghost")]);
        assert_eq!(keys(&mut a, "*", 10), "*1\r\n$7\r\nadmin:1\r\n", "invisible avant la libération");
        while job.step(&mut a, 1) == Step::More {}
        assert_eq!(job.count(), 3);
        assert_eq!(a.len(), 1);
    }

    #[test]
    fn flush_counts_zero_immediately() {
        let mut a = art(&["a", "b", "c:1", "c:2"]);
        let mut job = FlushJob::start(&mut a);
        assert_eq!(a.len(), 0);
        while job.step(&mut a, 1) == Step::More {}
        assert!(job.is_done());
        assert_eq!(a.len(), 0);
    }
}
