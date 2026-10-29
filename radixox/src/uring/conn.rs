//! État d'une connexion, coupé en deux pour l'emprunteur (RUNTIME_ARCH.md §4.3) :
//! [`ConnIn`] (frames reçues, empruntées en lecture pendant l'exécution) et
//! [`ConnOut`] (réponses, envoi, mode, mutables pendant l'exécution).

#[allow(unused_imports)]
use crate::prelude::*;

use alloc::collections::VecDeque;
use radixox_lib::sys::Instant;

use radixox_lib::shared_byte::SharedByte;

use crate::exec::client::Client;

use super::bufs::Bid;

/// Morceau reçu, pas encore exécuté (RUNTIME_ARCH.md §5.4) : des octets bruts
/// dans un tampon de l'anneau, découpés en frames seulement à l'exécution,
/// fenêtre par fenêtre (une seule passe, juste avant d'exécuter).
#[derive(Debug, Clone, Copy)]
pub struct Span {
    pub bid: Bid,
    pub off: u32,
    pub len: u32,
}

#[derive(Default)]
pub struct ConnIn {
    /// Morceaux en attente, dans l'ordre (un refcount de tampon par span).
    pub spans: VecDeque<Span>,
    /// Frame coupée entre deux morceaux, reconstituée ici (cas des frontières).
    pub rx: Vec<u8>,
    /// `rx[..rx_ready]` est une frame complète prête à exécuter (0 : aucune).
    pub rx_ready: usize,
    /// `rx` a changé et peut contenir une frame complète : à réexaminer.
    pub rx_check: bool,
    /// Flux invalide : l'erreur est à répondre, puis la connexion se ferme.
    pub proto_err: Option<&'static str>,
    /// La suite du flux est ignorée (fermeture en cours, erreur de protocole).
    pub broken: bool,
    /// Taille de `rx` déjà comptée dans le total d'entrée du cœur (`account_rx`).
    pub rx_counted: usize,
    /// Réception suspendue par le budget global d'entrée (RUNTIME_ARCH.md §5.3) : pas
    /// réarmée tant que le total n'est pas redescendu.
    pub throttled: bool,
    /// Frame en cours plus grosse que ce que le budget d'entrée laisse attendre, admise
    /// parce qu'elle tient dans la mémoire libre : pas suspendue jusqu'à sa fin.
    pub oversize: bool,
    /// Même cas, mais elle ne tient pas : `-OOM` puis fermeture au prochain relevé.
    pub refused: bool,
}

impl ConnIn {
    /// Il y a de quoi exécuter sans attendre de nouvelles données.
    pub fn has_work(&self) -> bool {
        self.rx_ready > 0 || self.rx_check || !self.spans.is_empty() || self.proto_err.is_some()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Slot libre.
    Free,
    Normal,
    /// Fin propre (QUIT, EOF du client, erreur de protocole) : plus de
    /// lecture ; ce qui est déjà reçu est exécuté, les réponses partent, puis
    /// `Draining`.
    Closing,
    /// Fin : rien n'est plus exécuté ni soumis ; `CLOSE` quand `refs == 0`.
    Draining,
}

pub struct ConnOut {
    pub mode: Mode,
    /// Incrémenté à chaque accept (et plus tard à chaque parquage), jamais
    /// remis à zéro : marque les inscriptions des registres (§4.3).
    pub epoch: u32,
    /// Opérations noyau en vol qui désignent ce slot (§4.2).
    pub refs: u8,
    pub recv_armed: bool,
    pub recv_cancelled: bool,
    /// Réponses en cours d'accumulation.
    pub out: Vec<u8>,
    /// Tampon en vol (vide : pas de send en vol). Le noyau le lit par pointeur
    /// jusqu'au CQE : jamais modifié pendant ce temps.
    pub sending: Vec<u8>,
    /// Octets de `sending` déjà envoyés (filet de sécurité si un envoi rend
    /// moins que demandé malgré `MSG_WAITALL`).
    pub sent: usize,
    /// Grosse réponse d'un job (KEYS), envoyée depuis son tampon sans copie
    /// dans `out`, avant tout ce que `out` contient (MEMORY_ARCH.md §18.11) :
    /// tampon, début de la réponse dedans.
    pub big: Option<(radixox_lib::big_buf::BigBuf, usize)>,
    /// Le send en vol est celui de `big` (`sent` compte alors dans `big`).
    pub big_sending: bool,
    /// Grande valeur d'une réponse (`exec::BIG_REPLY`), envoyée depuis l'arbre sans copie
    /// **après** tout `out` (qui finit par son en-tête `$len`) ; son `\r\n` final est remis
    /// dans `out` une fois envoyée. La connexion n'exécute plus rien d'ici là.
    pub tail: Option<SharedByte>,
    /// Le send en vol est celui de `tail`.
    pub tail_sending: bool,
    pub send_cancelled: bool,
    pub client: Client,
    /// Dans la liste des connexions sales du tour.
    pub dirty: bool,
    /// Dans la liste des connexions qui ont des frames en attente.
    pub ready: bool,
    /// Tour de lot où la connexion a exécuté une barrière (§2.1).
    pub stop_turn: u64,
    /// Dernière réception (balayage d'inactivité).
    pub last_active: Instant,
    /// Connexion parquée (§7) : un job travaille pour elle ; elle n'est plus
    /// tirée, ce qu'elle reçoit attend dans `rx`, elle reprend à la réponse.
    pub job_pending: bool,
    /// Écrivain parqué : le flux de l'AOF a trop de retard sur le disque
    /// (DURABILITY_DESIGN.md §7.6). Comme `job_pending` : plus tirée, ce
    /// qu'elle reçoit attend dans `rx`.
    pub aof_wait: bool,
    /// `WAIT` : parquée jusqu'à ce que assez de réplicas aient acquitté sa
    /// dernière écriture (DURABILITY_DESIGN.md §10.6), comme `aof_wait`.
    pub repl_wait: bool,
    /// Commande bloquante (BLPOP, BLMOVE, BZPOPMIN…) en attente : son inscription
    /// dans le registre (`core::block`), comme `repl_wait`. Le producteur qui la
    /// sert la remet à `None` et écrit la réponse.
    pub block_wait: Option<u64>,
    /// Offset du flux atteint par la dernière écriture journalisée de la
    /// connexion (marque du §7.5) : ce que `WAIT` attend des réplicas.
    pub write_mark: u64,
    /// Réponses retenues jusqu'au fsync qui les couvre (`appendfsync always`,
    /// DURABILITY_DESIGN.md §7.5) : (début de la réponse dans `out`, marque).
    /// Rien à partir du premier début retenu ne part (ordre des réponses).
    pub holds: alloc::collections::VecDeque<(usize, u64)>,
    /// Canaux abonnés (Pub/Sub) ; non vide = mode abonné.
    pub channels: Vec<SharedByte>,
    /// Depuis quand `out` dépasse la limite souple des abonnés.
    pub over_soft: Option<Instant>,
    /// Lien sortant de réplication (DURABILITY_DESIGN.md §10) : ce qu'il reçoit
    /// sont des réponses de la cible, lues par le cœur, jamais exécutées.
    pub link: bool,
    /// `connect` en vol (lien sortant).
    pub connecting: bool,
}

impl ConnOut {
    pub fn free() -> Self {
        Self {
            mode: Mode::Free,
            epoch: 0,
            refs: 0,
            recv_armed: false,
            recv_cancelled: false,
            out: Vec::new(),
            sending: Vec::new(),
            big: None,
            big_sending: false,
            tail: None,
            tail_sending: false,
            sent: 0,
            send_cancelled: false,
            // Remplacé à l'accept (`Client::new` consomme un CLIENT ID).
            client: Client { authed: false, id: 0, name: None },
            dirty: false,
            ready: false,
            stop_turn: u64::MAX,
            last_active: Instant::now(),
            job_pending: false,
            aof_wait: false,
            repl_wait: false,
            block_wait: None,
            write_mark: 0,
            holds: Default::default(),
            channels: Vec::new(),
            over_soft: None,
            link: false,
            connecting: false,
        }
    }

    /// Parquée : plus tirée, ce qu'elle reçoit attend dans `rx` (job, AOF
    /// en retard, `WAIT`, commande bloquante).
    pub fn parked(&self) -> bool {
        self.job_pending || self.aof_wait || self.repl_wait || self.block_wait.is_some()
    }

    /// Octets de sortie en attente : `out`, le send en vol, la grosse réponse.
    pub fn pending_out(&self) -> usize {
        self.out.len() + self.sending.len() + self.big.as_ref().map_or(0, |(b, at)| b.len().saturating_sub(*at))
    }

    pub fn send_in_flight(&self) -> bool {
        !self.sending.is_empty() || self.big_sending || self.tail_sending
    }

    /// Rien à envoyer : ni `out`, ni grosse réponse en attente.
    pub fn nothing_to_send(&self) -> bool {
        self.out.is_empty() && self.big.is_none() && self.tail.is_none()
    }

    /// Octets de `out` qui peuvent partir : les réponses retenues dont la
    /// marque est couverte sont libérées. `upto` : offset durable (`None` :
    /// rien n'est retenu).
    pub fn sendable(&mut self, upto: Option<u64>) -> usize {
        if let Some(upto) = upto {
            while self.holds.front().is_some_and(|&(_, m)| m <= upto) {
                self.holds.pop_front();
            }
        }
        self.holds.front().map_or(self.out.len(), |&(start, _)| start)
    }
}
