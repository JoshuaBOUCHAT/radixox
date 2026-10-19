//! Sous-arbres détachés (UNLINK `prefix*`, FLUSHDB) : détachement immédiat,
//! libération par tranches, pause pendant une save.

use radixox_lib::shared_byte::SharedByte;

use crate::value::Value;
use crate::{Detached, OxidArt};

fn make_art() -> OxidArt {
    let mut a = OxidArt::new();
    for (k, v) in [
        ("user:1:name", "alice"),
        ("user:1:role", "admin"),
        ("user:2:name", "bob"),
        ("user:2:role", "viewer"),
        ("user:3:name", "carol"),
        ("session:abc", "tok1"),
        ("session:def", "tok2"),
        ("config:timeout", "30"),
    ] {
        a.set(SharedByte::from_str(k), Value::String(SharedByte::from_str(v)));
    }
    a
}

/// Détache puis libère par tranches de 1 nœud ; renvoie le nombre de
/// valeurs vivantes libérées.
fn unlink(a: &mut OxidArt, prefix: &str) -> usize {
    let mut d = Detached::default();
    a.detach_prefix(prefix.as_bytes(), &mut d);
    while !a.free_detached(&mut d, 1) {}
    d.freed
}

fn keys(a: &OxidArt, prefix: &str) -> usize {
    a.getn(SharedByte::from_str(prefix)).len()
}

#[test]
fn detach_prefix() {
    let mut a = make_art();
    assert_eq!(unlink(&mut a, "user:"), 5);
    assert_eq!(keys(&a, "user:"), 0);
    assert_eq!(keys(&a, "session:"), 2);
    assert_eq!(keys(&a, "config:"), 1);
    assert_eq!(a.len(), 3);
}

#[test]
fn detach_empty_prefix_clears_everything_and_root_value() {
    let mut a = make_art();
    a.set(SharedByte::from_str(""), Value::from_str("root"));
    assert_eq!(unlink(&mut a, ""), 9);
    assert_eq!(keys(&a, ""), 0);
    assert_eq!(a.get(b""), None);
    assert_eq!(a.len(), 0);
}

#[test]
fn detach_partial_and_missing_prefix() {
    let mut a = make_art();
    assert_eq!(unlink(&mut a, "ghost:"), 0);
    assert_eq!(unlink(&mut a, "user:1:"), 2);
    assert_eq!(keys(&a, "user:"), 3);
    assert_eq!(unlink(&mut a, "session:abc"), 1);
    assert_eq!(keys(&a, ""), 5);
}

/// FLUSHDB : le compte tombe à 0 dès le détachement, les clés écrites
/// pendant la libération sont comptées normalement.
#[test]
fn detach_all_counts_zero_immediately() {
    let mut a = make_art();
    let mut d = Detached::default();
    a.detach_all(&mut d);
    assert_eq!(a.len(), 0);
    assert_eq!(keys(&a, ""), 0);
    a.set(SharedByte::from_str("new"), Value::from_str("x"));
    while !a.free_detached(&mut d, 1) {}
    assert_eq!(d.freed, 8);
    assert_eq!(a.len(), 1);
}

/// Pendant une save : détachement immédiat (la clé disparaît de la vue
/// vivante), aucune libération avant la fin de la save, vue T0 complète.
#[test]
fn free_waits_for_save() {
    let mut a = make_art();
    let expect = crate::save::reference::expected(&a);
    let nodes = a.node_count();
    let mut out = Vec::new();
    let mut cur = a.save_start(&mut out);
    let mut d = Detached::default();
    a.detach_prefix(b"user:", &mut d);
    assert!(!a.free_detached(&mut d, usize::MAX));
    assert_eq!(keys(&a, "user:"), 0);
    assert_eq!(a.node_count(), nodes, "rien n'est libéré pendant la save");
    while !a.save_step(&mut cur, &mut out, 1) {
        assert!(!a.free_detached(&mut d, usize::MAX));
    }
    drop(cur);
    a.finish_save();
    assert_eq!(crate::save::reference::decoded(&out), expect);
    assert!(a.free_detached(&mut d, usize::MAX));
    assert_eq!(d.freed, 5);
    assert!(a.node_count() < nodes);
}

