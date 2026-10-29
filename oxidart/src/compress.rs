//! Compression zstd du dump, sur le thread de service (SAVE_DESIGN.md §5
//! « Compression »).
//!
//! Le fichier compressé est **une seule frame zstd standard** contenant le
//! fichier brut (flux + pied) : `zstd -d dump.rxd` le redonne. Au reload,
//! [`OxidArt::load_file`](crate::OxidArt::load_file) reconnaît le magic zstd et
//! décompresse au fil de la lecture ([`ZstdReader`]).
//!
//! Une seule frame plutôt que des morceaux indépendants (mesuré,
//! `examples/zstd_tranche.rs`) : à partir du niveau 12, chaque début de frame
//! coûte 3–6 ms d'un bloc (initialisation des tables), quel que soit le
//! découpage ; une frame unique ne le paie qu'une fois par save, et gagne
//! ~3 % de ratio. Les morceaux indépendants ne servaient qu'à décompresser en
//! parallèle, inutile en mono-thread ; zstd stocke déjà en brut un bloc
//! incompressible.
//!
//! # Tranches
//!
//! zstd travaille par bloc : un appel qui complète un bloc le compresse en
//! entier, quelle que soit la taille de l'entrée donnée. La taille max de bloc
//! (`ZSTD_c_maxBlockSize`) est donc choisie selon le niveau pour qu'un appel
//! reste court ([`block_size`]), et [`ZstdStream::compress`] donne l'entrée
//! bloc par bloc jusqu'à épuiser son temps.

#[allow(unused_imports)]
use crate::prelude::*;
use std::io::Read;
use std::time::{Duration, Instant};

use zstd_safe::zstd_sys::ZSTD_EndDirective;
use zstd_safe::{CCtx, CParameter, DCtx, InBuffer, OutBuffer};

/// Premiers octets d'une frame zstd (magic 0xFD2FB528, little-endian).
pub const ZSTD_MAGIC: [u8; 4] = [0x28, 0xB5, 0x2F, 0xFD];

/// Taille max de bloc selon le niveau. Mesures sur un dump de 8 MB (valeurs
/// façon JSON), durée p99 / max d'un appel :
///
/// | niveau | bloc  | p99      | max      | ratio | MB/s |
/// |--------|-------|----------|----------|-------|------|
/// | 1–3    | 32 KB | ~0,1 ms  | ~0,3 ms  | 4,2   | ~400 |
/// | 9      | 8 KB  | ~0,16 ms | ~0,2 ms  | 4,6   | ~80  |
/// | 12     | 2 KB  | ~0,1 ms  | ~0,4 ms  | 4,4   | ~37  |
/// | 19     | 2 KB  | ~0,55 ms | ~0,9 ms  | 5,1   | ~6   |
///
/// Niveaux ≥ 12 avec tables plafonnées ([`MAX_TABLE_LOG`]), une seule frame.
pub fn block_size(level: i32) -> usize {
    match level {
        ..=5 => 32 << 10,
        6..=11 => 8 << 10,
        12..=15 => 2 << 10,
        // Minimum de zstd.
        _ => 1 << 10,
    }
}

/// Plafond de `windowLog`, `hashLog` et `chainLog` (fenêtre de 1 MB). Aux
/// niveaux hauts, les tables par défaut (~80 MB au niveau 19) sont remises à
/// zéro au premier appel : 8,6 ms d'un bloc au niveau 19. Plafonnées à 2^20 :
/// premier appel ~1 ms, ratio −1,7 %, débit ×2 (tables en cache). Les niveaux
/// bas sont déjà en dessous et ne changent pas.
pub const MAX_TABLE_LOG: u32 = 20;

/// Compresseur en flux, une frame pour tout le fichier.
pub struct ZstdStream {
    cctx: CCtx<'static>,
    block: usize,
}

impl ZstdStream {
    pub fn new(level: i32) -> Self {
        let mut cctx = CCtx::create();
        let block = block_size(level);
        cctx.set_parameter(CParameter::CompressionLevel(level)).expect("niveau zstd");
        cctx.set_parameter(CParameter::MaxBlockSize(block as u32)).expect("taille de bloc zstd");
        // SAFETY: fonction pure de libzstd (paramètres par défaut du niveau,
        // taille de source inconnue).
        let p = unsafe { zstd_safe::zstd_sys::ZSTD_getCParams(level, 0, 0) };
        for param in [
            CParameter::WindowLog(p.windowLog.min(MAX_TABLE_LOG)),
            CParameter::HashLog(p.hashLog.min(MAX_TABLE_LOG)),
            CParameter::ChainLog(p.chainLog.min(MAX_TABLE_LOG)),
        ] {
            cctx.set_parameter(param).expect("paramètre zstd");
        }
        Self { cctx, block }
    }

    fn reserve(&self, out: &mut Vec<u8>) {
        let need = zstd_safe::compress_bound(self.block) + 64;
        if out.capacity() - out.len() < need {
            out.reserve(need.max(out.len()));
        }
    }

    /// Compresse le début de `input` dans `out`, bloc par bloc, jusqu'à
    /// avoir passé `max` (toujours au moins un bloc). Renvoie le nombre
    /// d'octets d'entrée consommés.
    pub fn compress(&mut self, input: &[u8], out: &mut Vec<u8>, max: Duration) -> usize {
        let t = Instant::now();
        let mut consumed = 0;
        while consumed < input.len() {
            let piece = &input[consumed..input.len().min(consumed + self.block)];
            let mut ib = InBuffer::around(piece);
            while ib.pos() < piece.len() {
                self.reserve(out);
                let used = out.len();
                let mut ob = OutBuffer::around_pos(out, used);
                self.cctx
                    .compress_stream2(&mut ob, &mut ib, ZSTD_EndDirective::ZSTD_e_continue)
                    .expect("compression zstd");
            }
            consumed += piece.len();
            if t.elapsed() >= max {
                break;
            }
        }
        consumed
    }

    /// Termine la frame (compresse ce que zstd garde en interne, au plus un
    /// bloc, puis l'épilogue). `true` quand la frame est complète.
    pub fn finish(&mut self, out: &mut Vec<u8>, max: Duration) -> bool {
        let t = Instant::now();
        loop {
            self.reserve(out);
            let used = out.len();
            let mut ob = OutBuffer::around_pos(out, used);
            let mut ib = InBuffer::around(&[]);
            let rem = self
                .cctx
                .compress_stream2(&mut ob, &mut ib, ZSTD_EndDirective::ZSTD_e_end)
                .expect("fin de frame zstd");
            if rem == 0 {
                return true;
            }
            if t.elapsed() >= max {
                return false;
            }
        }
    }
}

/// Décompresse une frame zstd au fil de la lecture : la source est lue par
/// morceaux, rien n'est gardé en entier.
pub struct ZstdReader<R> {
    src: R,
    dctx: DCtx<'static>,
    buf: Vec<u8>,
    pos: usize,
    end: usize,
    eof: bool,
    frame_done: bool,
}

const ZSTD_READ_CHUNK: usize = 1 << 20;

impl<R: Read> ZstdReader<R> {
    pub fn new(src: R) -> Self {
        Self {
            src,
            dctx: DCtx::create(),
            buf: vec![0; ZSTD_READ_CHUNK],
            pos: 0,
            end: 0,
            eof: false,
            frame_done: false,
        }
    }
}

fn invalid(msg: &'static str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, msg)
}

impl<R: Read> Read for ZstdReader<R> {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        loop {
            if self.frame_done {
                // Une seule frame par fichier : rien ne doit suivre.
                if self.pos < self.end {
                    return Err(invalid("octets après la frame zstd"));
                }
                if !self.eof {
                    match self.src.read(&mut self.buf)? {
                        0 => self.eof = true,
                        _ => return Err(invalid("octets après la frame zstd")),
                    }
                }
                return Ok(0);
            }
            if self.pos == self.end && !self.eof {
                self.pos = 0;
                self.end = self.src.read(&mut self.buf)?;
                self.eof = self.end == 0;
            }
            let mut ib = InBuffer::around(&self.buf[self.pos..self.end]);
            let mut ob = OutBuffer::around(&mut *out);
            let rem = self
                .dctx
                .decompress_stream(&mut ob, &mut ib)
                .map_err(|_| invalid("frame zstd invalide"))?;
            self.pos += ib.pos();
            if rem == 0 {
                self.frame_done = true;
            }
            if ob.pos() > 0 {
                return Ok(ob.pos());
            }
            if self.frame_done {
                continue;
            }
            if self.eof && self.pos == self.end {
                return Err(invalid("frame zstd tronquée"));
            }
        }
    }
}
