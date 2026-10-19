//! État d'une connexion, coupé en deux pour l'emprunteur (RUNTIME_ARCH.md §4.3) :
//! [`ConnIn`] (frames reçues, empruntées en lecture pendant l'exécution) et
//! [`ConnOut`] (réponses, envoi, mode, mutables pendant l'exécution).

use std::collections::VecDeque;

use crate::exec::client::Client;

use super::bufs::Bid;

/// Une frame complète en attente d'exécution (RUNTIME_ARCH.md §5.4).
#[derive(Debug, Clone, Copy)]
pub enum Item {
    /// Dans un tampon de l'anneau (`refs[bid]` compté).
    Ring { bid: Bid, off: u32, len: u32 },
    /// Dans `ConnIn::rx` (frame qui était coupée entre deux morceaux).
    Local { off: u32, len: u32 },
    /// Flux invalide à cet endroit : répondre l'erreur puis fermer.
    ProtoErr(&'static str),
}

#[derive(Default)]
pub struct ConnIn {
    pub queue: VecDeque<Item>,
    /// Reliquat de frame coupée et frames recopiées (`Item::Local`).
    pub rx: Vec<u8>,
    /// `rx[..framed]` est découpé en frames ; `rx[framed..]` est une frame
    /// incomplète.
    pub framed: usize,
    /// Un `ProtoErr` est en file : la suite du flux est ignorée.
    pub broken: bool,
}

impl ConnIn {
    /// Une frame incomplète attend la suite.
    pub fn has_partial(&self) -> bool {
        self.rx.len() > self.framed
    }

    /// Rend la place des frames locales déjà exécutées.
    pub fn compact(&mut self) {
        if self.queue.iter().any(|it| matches!(it, Item::Local { .. })) {
            return;
        }
        self.rx.drain(..self.framed);
        self.framed = 0;
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
    pub send_cancelled: bool,
    pub client: Client,
    /// Dans la liste des connexions sales du tour.
    pub dirty: bool,
    /// Dans la liste des connexions qui ont des frames en attente.
    pub ready: bool,
    /// Tour de lot où la connexion a exécuté une barrière (§2.1).
    pub stop_turn: u64,
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
            sent: 0,
            send_cancelled: false,
            // Remplacé à l'accept (`Client::new` consomme un CLIENT ID).
            client: Client { authed: false, id: 0, name: None },
            dirty: false,
            ready: false,
            stop_turn: u64::MAX,
        }
    }

    pub fn send_in_flight(&self) -> bool {
        !self.sending.is_empty()
    }
}
