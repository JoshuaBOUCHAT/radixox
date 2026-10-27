//! Mémoire résidente d'une région écrite par le début (HiSlab, table des poignées, tables
//! par bloc du pool et par granule de `large`) : ces pages sont prises par le noyau à la
//! première écriture, sans appel à l'allocateur. Mesurée par `mincore`, pas estimée : avec
//! les huge pages transparentes en `always`, une première écriture prend tantôt 2 Mio,
//! tantôt 4 Kio (alignement, `khugepaged`) ; l'estimation à 2 Mio comptait 8 Mo de trop.
//! Pour `INFO` seulement (un appel système et un octet par 4 Kio de région).

const PAGE: usize = 4096;
/// Au-delà de la partie touchée : une huge page prise à la première écriture déborde
/// jusqu'à la frontière de 2 Mio suivante (dans la réservation virtuelle de la région).
const HUGE: usize = 2 << 20;

/// Octets résidents de `[base, base + touched)`, frontière de 2 Mio suivante comprise.
/// `reserved` : taille de la réservation virtuelle qui commence à `base` (borne de la
/// mesure).
pub fn region(base: usize, touched: usize, reserved: usize) -> u64 {
    if touched == 0 {
        return 0;
    }
    let start = base / PAGE * PAGE;
    let end = (base + touched).next_multiple_of(HUGE).min((base + reserved).next_multiple_of(PAGE));
    let n = (end - start) / PAGE;
    let mut vec = vec![0u8; n];
    // SAFETY: `[start, end)` est dans la réservation de la région ; `vec` a une case par page.
    let r = unsafe { libc::mincore(start as *mut libc::c_void, end - start, vec.as_mut_ptr()) };
    if r != 0 {
        return (base + touched).next_multiple_of(PAGE) as u64 - start as u64;
    }
    vec.iter().filter(|&&v| v & 1 != 0).count() as u64 * PAGE as u64
}
