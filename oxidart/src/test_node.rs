//! Fuzz du nœud v2 contre un `BTreeMap` de référence.

use std::collections::BTreeMap;
use std::mem::ManuallyDrop;

use radixox_lib::shared_byte::SharedByte;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

use crate::node::{Entry, EntryRef, Kind, Leaf, Node, alloc_class};
use crate::compact_path::CompactPath;
use crate::value::{Tag, ValUnion};

#[derive(Clone, Debug, PartialEq)]
enum RefVal {
    Int(i64),
    Bytes(SharedByte),
}

#[derive(Clone, Debug, PartialEq)]
enum RefEntry {
    Child(u32),
    Leaf {
        val: RefVal,
        ttl: Option<u64>,
        sfx: Option<Vec<u8>>,
    },
}

impl RefEntry {
    fn to_entry(&self) -> Entry {
        match self {
            RefEntry::Child(idx) => Entry::Child(*idx),
            RefEntry::Leaf { val, ttl, sfx } => {
                let (tag, val) = match val {
                    RefVal::Int(n) => (Tag::Int, ValUnion { integer: *n }),
                    RefVal::Bytes(b) => (
                        Tag::Bytes,
                        ValUnion {
                            bytes: ManuallyDrop::new(b.clone()),
                        },
                    ),
                };
                Entry::Leaf(Leaf {
                    tag,
                    val,
                    ttl: *ttl,
                    sfx: sfx.as_deref().map(CompactPath::from_slice),
                })
            }
        }
    }
}

fn read_val(tag: Tag, val: &ValUnion) -> RefVal {
    unsafe {
        match tag {
            Tag::Int => RefVal::Int(val.integer),
            Tag::Bytes => RefVal::Bytes((*val.bytes).clone()),
            _ => panic!("tag inattendu"),
        }
    }
}

fn from_ref(e: EntryRef<'_>) -> RefEntry {
    match e {
        EntryRef::Child(idx) => RefEntry::Child(idx),
        EntryRef::Leaf { tag, val, ttl, sfx } => RefEntry::Leaf {
            val: read_val(tag, val),
            ttl,
            sfx: sfx.map(|p| p.to_vec()),
        },
    }
}

fn from_owned(e: Entry) -> RefEntry {
    match e {
        Entry::Child(idx) => RefEntry::Child(idx),
        Entry::Leaf(leaf) => RefEntry::Leaf {
            val: read_val(leaf.tag, &leaf.val),
            ttl: leaf.ttl,
            sfx: leaf.sfx.as_ref().map(|p| p.to_vec()),
        },
    }
}

fn assert_same(node: &Node, reference: &BTreeMap<u8, RefEntry>) {
    node.check();
    assert_eq!(node.len(), reference.len());
    let got: Vec<(u8, RefEntry)> = node.iter().map(|(r, e)| (r, from_ref(e))).collect();
    let want: Vec<(u8, RefEntry)> = reference.iter().map(|(r, e)| (*r, e.clone())).collect();
    assert_eq!(got, want);
}

fn random_entry(rng: &mut StdRng, pool: &[SharedByte]) -> RefEntry {
    if rng.gen_bool(0.3) {
        return RefEntry::Child(rng.r#gen());
    }
    let val = if rng.gen_bool(0.5) {
        RefVal::Int(rng.r#gen())
    } else {
        RefVal::Bytes(pool[rng.gen_range(0..pool.len())].clone())
    };
    let ttl = rng.gen_bool(0.3).then(|| rng.gen_range(0..1u64 << 56));
    let sfx = rng.gen_bool(0.4).then(|| {
        // inline (≤ 7) et heap
        let len = rng.gen_range(1..14);
        (0..len).map(|_| rng.r#gen::<u8>()).collect()
    });
    RefEntry::Leaf { val, ttl, sfx }
}

#[derive(Default, Debug)]
struct Stats {
    /// pas passés dans chaque forme : Inline, Overflow, Large, VeryLarge
    kinds: [usize; 4],
    max_len: usize,
}

fn fuzz(seed: u64, ops: usize, radix_space: u16, insert_bias: u32, stats: &mut Stats) {
    let mut rng = StdRng::seed_from_u64(seed);
    let pool: Vec<SharedByte> = (0..8)
        .map(|i| SharedByte::from_slice(format!("val-{i}")))
        .collect();
    let mut node = Node::new(CompactPath::from_slice(b"pfx"), 0, 0);
    let mut reference: BTreeMap<u8, RefEntry> = BTreeMap::new();

    for _ in 0..ops {
        let radix = rng.gen_range(0..radix_space) as u8;
        match rng.gen_range(0..10 + insert_bias) {
            // insertion (ou remplacement = remove + insert)
            0..=4 => {
                if let Some(old) = reference.remove(&radix) {
                    let got = node.remove(radix).map(from_owned);
                    assert_eq!(got, Some(old));
                }
                let entry = random_entry(&mut rng, &pool);
                node.insert(radix, entry.to_entry());
                reference.insert(radix, entry);
            }
            5..=6 => {
                let got = node.remove(radix).map(from_owned);
                assert_eq!(got, reference.remove(&radix));
            }
            7..=8 => {
                let ttl = rng.gen_bool(0.5).then(|| rng.gen_range(0..1u64 << 56));
                let is_leaf = matches!(reference.get(&radix), Some(RefEntry::Leaf { .. }));
                if let Some(RefEntry::Leaf { ttl: t, .. }) = reference.get_mut(&radix) {
                    *t = ttl;
                }
                assert_eq!(node.set_ttl(radix, ttl), is_leaf);
            }
            9 => {
                let n = rng.r#gen::<i64>();
                if let Some((tag, val)) = node.leaf_val_mut(radix) {
                    unsafe { crate::value::drop_raw(*tag, val) };
                    *tag = Tag::Int;
                    *val = ValUnion { integer: n };
                    let Some(RefEntry::Leaf { val, .. }) = reference.get_mut(&radix) else {
                        panic!("leaf_val_mut sur une entrée absente de la référence");
                    };
                    *val = RefVal::Int(n);
                } else {
                    assert!(!matches!(
                        reference.get(&radix),
                        Some(RefEntry::Leaf { .. })
                    ));
                }
            }
            // biais d'insertion : radix absent seulement
            _ => {
                if let std::collections::btree_map::Entry::Vacant(slot) = reference.entry(radix) {
                    let entry = random_entry(&mut rng, &pool);
                    node.insert(radix, entry.to_entry());
                    slot.insert(entry);
                }
            }
        }
        assert_same(&node, &reference);
        stats.kinds[node.kind() as usize] += 1;
        stats.max_len = stats.max_len.max(node.len());
    }

    drop(node);
    drop(reference);
    for b in &pool {
        assert_eq!(b.rc(), 1, "fuite ou double libération de SharedByte");
    }
}

#[test]
fn v2_node_fuzz_small() {
    // autour de la frontière petite forme / Large
    let mut stats = Stats::default();
    // 6 radixs : Inline (48 o de cellules) ; 12 et 20 : Overflow et Large
    for seed in 0..300 {
        let space = [6, 12, 20][seed as usize % 3];
        fuzz(seed, 2_000, space, 0, &mut stats);
    }
    eprintln!("{stats:?}");
    assert!(stats.kinds[..3].iter().all(|&k| k > 20_000));
}

#[test]
fn v2_node_fuzz_mid() {
    // autour de Large / VeryLarge (descente à 24, montée à 33)
    let mut stats = Stats::default();
    for seed in 500..560 {
        fuzz(seed, 3_000, 40, 3, &mut stats);
    }
    eprintln!("{stats:?}");
    assert!(stats.kinds[2] > 10_000 && stats.kinds[3] > 10_000);
}

#[test]
fn v2_node_fuzz_full() {
    // jusqu'à 256 entrées
    let mut stats = Stats::default();
    for seed in 1_000..1_012 {
        fuzz(seed, 5_000, 256, 20, &mut stats);
    }
    eprintln!("{stats:?}");
    assert!(stats.max_len >= 200);
}

#[test]
fn v2_node_fill_all_256() {
    use rand::seq::SliceRandom;
    let pool: Vec<SharedByte> = (0..4)
        .map(|i| SharedByte::from_slice(format!("v{i}")))
        .collect();
    for seed in 0..10 {
        let mut rng = StdRng::seed_from_u64(seed);
        let mut node = Node::new(CompactPath::new(), 0, 0);
        let mut reference = BTreeMap::new();
        let mut order: Vec<u8> = (0..=255).collect();
        order.shuffle(&mut rng);
        let mut kinds = Vec::new();
        for &r in &order {
            let entry = random_entry(&mut rng, &pool);
            node.insert(r, entry.to_entry());
            reference.insert(r, entry);
            assert_same(&node, &reference);
            if kinds.last() != Some(&node.kind()) {
                kinds.push(node.kind());
            }
        }
        assert_eq!(node.len(), 256);
        assert_eq!(kinds.last(), Some(&Kind::VeryLarge));
        order.shuffle(&mut rng);
        for &r in &order {
            assert_eq!(node.remove(r).map(from_owned), reference.remove(&r));
            assert_same(&node, &reference);
        }
        assert_eq!(node.kind(), Kind::Inline);
    }
    for b in &pool {
        assert_eq!(b.rc(), 1);
    }
}

#[test]
fn v2_alloc_class() {
    let classes: Vec<usize> = [8, 9, 64, 65, 80, 81, 128, 129, 200, 257, 300]
        .iter()
        .map(|&s| alloc_class(s))
        .collect();
    assert_eq!(classes, [8, 16, 64, 80, 80, 96, 128, 160, 224, 320, 320]);
}

#[test]
fn v2_node_layout() {
    // user:{n} : 10 feuilles de 8 o → 6 dans le nœud (48 o) + 4 en page
    let mut node = Node::new(CompactPath::from_slice(b"user:1"), 0, b'1');
    for d in b'0'..=b'9' {
        let leaf = Leaf {
            tag: Tag::Int,
            val: ValUnion { integer: d as i64 },
            ttl: None,
            sfx: None,
        };
        node.insert(d, Entry::Leaf(leaf));
    }
    node.check();
    assert_eq!(node.kind(), Kind::Overflow);
    for d in b'0'..=b'9' {
        assert!(matches!(
            node.get(d),
            Some(EntryRef::Leaf { tag: Tag::Int, .. })
        ));
    }
    for d in b'0'..=b'9' {
        node.remove(d);
        node.check();
    }
    assert_eq!(node.kind(), Kind::Inline);
}
