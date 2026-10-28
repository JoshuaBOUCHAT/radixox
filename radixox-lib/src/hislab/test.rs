use std::time::Instant;

use super::HiSlab;
use super::TaggedHiSlab;


#[test]
fn test_basic_insert_delete() {
    let count = (1 << 18) + 1; // 262,144 éléments
    let slab = HiSlab::<u8>::new(0, 1 << 19).expect("can't allocate");

    println!("🚀 Insertion de {} éléments...", count);

    let start = Instant::now();
    for i in 0..count {
        let idx = slab.insert((i % 255) as u8);

        if idx != i as u32 {
            panic!("Erreur d'indexage à {}: reçu {}", i, idx);
        }
    }
    let duration = start.elapsed();

    println!("✅ Insertion terminée en : {:?}", duration);
    println!("⏱️  Moyenne par insertion : {:?}", duration / count as u32);

    println!("--- État de la hiérarchie ---");
    println!("Lvl4 (résumé) : {:08b}", unsafe { &*slab.tree.get() }.lvl4);

    println!("\n🗑️ Suppression de tous les éléments...");
    let start_remove = Instant::now();
    for i in 0..count {
        slab.remove(i as u32);
    }
    let duration_remove = start_remove.elapsed();

    println!("✅ Suppression terminée en : {:?}", duration_remove);

    assert_eq!(
        unsafe { &*slab.tree.get() }.lvl4,
        0,
        "La hiérarchie n'est pas revenue à zéro !"
    );
    println!("\n✨ Test réussi : La structure est cohérente et rapide.");
}

#[test]
fn test_stress_lifecycle() {
    let slab = HiSlab::<u32>::new(0, 65536).expect("can't allocate");
    let iterations = 10_000;
    let mut indices = Vec::with_capacity(iterations);

    println!("🏗️ Phase 1 : Insertion massive...");
    for i in 0..iterations {
        let idx = slab.insert(i as u32 * 10);
        indices.push(idx);
    }

    for (i, &idx) in indices.iter().enumerate() {
        assert_eq!(slab.get(idx), Some(&(i as u32 * 10)));
    }

    println!("🗑️ Phase 2 : Fragmentation (suppression d'un élément sur deux)...");
    for i in (0..iterations).step_by(2) {
        let idx = indices[i];
        slab.remove(idx);
        assert!(slab.get(idx).is_none(), "L'index {} devrait être vide", idx);
    }

    println!("🧐 Phase 3 : Vérification de la persistance des autres...");
    for i in (1..iterations).step_by(2) {
        let idx = indices[i];
        assert_eq!(slab.get(idx), Some(&(i as u32 * 10)));
    }

    println!("♻️ Phase 4 : Ré-insertion dans les trous...");
    for _ in 0..1000 {
        let new_val = 999_999;
        let idx = slab.insert(new_val);
        assert!(
            idx.is_multiple_of(2),
            "L'insertion devrait combler les trous (index pair), reçu {}",
            idx
        );
        assert_eq!(slab[idx], new_val);
    }

    println!("🎯 Phase 5 : Test des accès Panic/Unchecked...");
    let valid_idx = indices[1];

    let _ = slab[valid_idx];

    unsafe {
        assert_eq!(*slab.get_unchecked(valid_idx), 10);
    }

    println!(
        "✨ Test réussi ! Cohérence maintenue après {} itérations.",
        iterations
    );
}

#[test]
fn test_iterator_order_and_stability() {
    let slab = HiSlab::<&str>::new(0, 65536).expect("can't allocate");

    slab.insert("Rust"); // Index 0
    slab.insert("is"); // Index 1
    slab.insert("fast"); // Index 2

    println!("Test de l'ordre simple...");
    let mut iter = (&slab).into_iter();

    assert_eq!(iter.next(), Some((0, &"Rust")));
    assert_eq!(iter.next(), Some((1, &"is")));
    assert_eq!(iter.next(), Some((2, &"fast")));
    assert_eq!(iter.next(), None);

    let slab2 = HiSlab::<&str>::new(0, 65536).expect("can't allocate");
    slab2.insert("A"); // 0
    let idx_b = slab2.insert("B"); // 1
    slab2.insert("C"); // 2

    slab2.remove(idx_b);

    println!("Test apres suppression (doit sauter l'index 1)...");
    let mut iter2 = (&slab2).into_iter();
    assert_eq!(iter2.next(), Some((0, &"A")));
    assert_eq!(iter2.next(), Some((2, &"C")));
    assert_eq!(iter2.next(), None);

    slab2.insert("D");

    println!("Test apres re-insertion (D doit apparaitre entre A et C)...");
    let mut iter3 = (&slab2).into_iter();
    assert_eq!(iter3.next(), Some((0, &"A")));
    assert_eq!(iter3.next(), Some((1, &"D")));
    assert_eq!(iter3.next(), Some((2, &"C")));
}

#[test]
fn test_iter_mut() {
    let mut slab = HiSlab::<i32>::new(0, 65536).expect("can't allocate");

    slab.insert(10); // 0
    slab.insert(20); // 1
    slab.insert(30); // 2
    slab.remove(1); // trou à 1

    for (idx, val) in &mut slab {
        *val += idx as i32;
    }

    assert_eq!(slab.get(0), Some(&10)); // 10 + 0
    assert_eq!(slab.get(1), None); // supprimé
    assert_eq!(slab.get(2), Some(&32)); // 30 + 2
}

#[test]
fn test_into_iter_partial_consumption() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static DROP_COUNT: AtomicUsize = AtomicUsize::new(0);

    #[derive(Default)]
    #[allow(dead_code)]
    struct DropCounter(Arc<()>);
    impl Drop for DropCounter {
        fn drop(&mut self) {
            DROP_COUNT.fetch_add(1, Ordering::SeqCst);
        }
    }

    DROP_COUNT.store(0, Ordering::SeqCst);

    let slab = HiSlab::<DropCounter>::new(0, 65536).expect("can't allocate");
    for _ in 0..10 {
        slab.insert(DropCounter(Arc::new(())));
    }

    let mut iter = slab.into_iter();
    let _ = iter.next();
    let _ = iter.next();
    let _ = iter.next();

    drop(iter);

    assert_eq!(
        DROP_COUNT.load(Ordering::SeqCst),
        10,
        "Fuite mémoire détectée!"
    );
}

#[test]
fn test_empty_slab_iteration() {
    let slab = HiSlab::<i32>::new(0, 65536).expect("can't allocate");

    let mut count = 0;
    for _ in &slab {
        count += 1;
    }
    assert_eq!(count, 0);

    let slab2 = HiSlab::<i32>::new(0, 65536).expect("can't allocate");
    let collected: Vec<_> = slab2.into_iter().collect();
    assert!(collected.is_empty());
}

// ============================================================================
// Tags
// ============================================================================

#[test]
fn test_insert_tagged_basic() {
    let slab = TaggedHiSlab::<i32>::new(0, 65536).expect("can't allocate");

    let idx1 = slab.insert(100);
    assert!(!slab.is_tagged(idx1));

    let idx2 = slab.insert_tagged(200);
    assert!(slab.is_tagged(idx2));

    assert!(slab.is_occupied(idx1));
    assert!(slab.is_occupied(idx2));

    assert_eq!(slab.get(idx1), Some(&100));
    assert_eq!(slab.get(idx2), Some(&200));
}

#[test]
fn test_tag_untag() {
    let slab = TaggedHiSlab::<i32>::new(0, 65536).expect("can't allocate");

    let idx = slab.insert(42);
    assert!(!slab.is_tagged(idx));

    assert!(slab.tag(idx));
    assert!(slab.is_tagged(idx));

    assert!(!slab.tag(idx));

    assert!(slab.untag(idx));
    assert!(!slab.is_tagged(idx));

    assert!(!slab.untag(idx));
}

#[test]
fn test_remove_clears_tag() {
    let slab = TaggedHiSlab::<i32>::new(0, 65536).expect("can't allocate");

    let idx = slab.insert_tagged(42);
    assert!(slab.is_tagged(idx));

    slab.remove(idx);
    assert!(!slab.is_occupied(idx));
    assert!(!slab.is_tagged(idx));
}

#[test]
fn test_insert_after_tagged_remove() {
    let slab = TaggedHiSlab::<i32>::new(0, 65536).expect("can't allocate");

    let idx1 = slab.insert_tagged(100);
    slab.remove(idx1);

    let idx2 = slab.insert(200);
    assert_eq!(idx1, idx2);

    assert!(!slab.is_tagged(idx2));
}

#[test]
fn copy_occupancy_matches_is_occupied() {
    let slab: HiSlab<u64> = HiSlab::new(0, 1 << 20).unwrap();
    let idx: Vec<u32> = (0..3000).map(|i| slab.insert(i)).collect();
    for &i in idx.iter().step_by(3) {
        slab.remove(i);
    }
    let mut out = Vec::new();
    let n = slab.copy_occupancy(&mut out);
    assert_eq!(n as usize, out.len() * 64);
    for i in 0..n {
        let bit = out[(i / 64) as usize] >> (i % 64) & 1 == 1;
        assert_eq!(bit, slab.is_occupied(i), "slot {i}");
    }
}

#[test]
fn next_occupied_walks_all_occupied_slots() {
    let slab: TaggedHiSlab<u64> = TaggedHiSlab::new(0, 1 << 20).unwrap();
    assert_eq!(slab.next_occupied(0), None);
    let idx: Vec<u32> = (0..5000).map(|i| slab.insert(i)).collect();
    // trous de toutes tailles, dont des mots de 64 et un bloc de 512 entiers vides
    for &i in &idx {
        if i % 3 == 0 || (700..1300).contains(&i) || i > 4900 {
            slab.remove(i);
        }
    }
    let expect: Vec<u32> = (0..slab.slot_span()).filter(|&i| slab.is_occupied(i)).collect();
    let mut got = Vec::new();
    let mut from = 0;
    while let Some(i) = slab.next_occupied(from) {
        got.push(i);
        from = i + 1;
    }
    assert_eq!(got, expect);
    assert!(slab.slot_span() >= 5000 && slab.slot_span() % 512 == 0);
    assert_eq!(slab.next_occupied(slab.slot_span()), None);
    assert_eq!(slab.next_occupied(u32::MAX), None);
    assert_eq!(slab.next_occupied(701), Some(1300));
}

#[test]
fn compaction_primitives() {
    // 128 o par slot, comme les nœuds d'oxidart : 100K slots = 12,8 Mo
    const T: usize = 2 << 20;
    let c0 = crate::pages::charged();
    let slab: TaggedHiSlab<[u64; 16]> = TaggedHiSlab::new(0, 1 << 20).unwrap();
    assert_eq!((slab.len(), slab.last_occupied()), (0, None));
    let idx: Vec<u32> = (0..100_000u64).map(|i| slab.insert([i; 16])).collect();
    assert_eq!(slab.len(), 100_000);
    assert_eq!(slab.last_occupied(), Some(99_999));
    for &i in &idx {
        if i % 4 != 0 {
            slab.remove(i);
        }
    }
    slab.tag(99_996);
    assert_eq!(slab.len(), 25_000);
    assert_eq!(slab.last_occupied(), Some(99_996));
    // Compactage : le plus haut descend dans le plus bas trou, le tag suit.
    let dst = unsafe { slab.move_down(99_996) }.unwrap();
    assert_eq!(dst, 1);
    assert!(!slab.is_occupied(99_996) && slab.is_occupied(1));
    assert_eq!(unsafe { *slab.get_unchecked(1) }, [99_996; 16]);
    assert!(slab.is_tagged(1) && !slab.is_tagged(99_996));
    assert_eq!(slab.len(), 25_000);
    // Jusqu'au bout : plus aucun trou sous le dernier.
    while let Some(last) = slab.last_occupied() {
        if unsafe { slab.move_down(last) }.is_none() {
            break;
        }
    }
    assert_eq!(slab.last_occupied(), Some(24_999));
    let mut seen: Vec<u64> = (0..25_000).map(|i| unsafe { slab.get_unchecked(i)[0] }).collect();
    seen.sort_unstable();
    assert!(seen.iter().zip((0..100_000).step_by(4)).all(|(&a, b)| a == b), "valeurs intactes");
    // Octets couverts : tranches de 2 Mio jusqu'au plus haut écrit, trous compris, jusqu'au
    // rendu ; le registre en a autant.
    let touched = (100_000 * 128usize).next_multiple_of(T);
    assert_eq!(slab.touched_bytes(), touched);
    assert_eq!(crate::pages::charged() - c0, touched as u64);
    let resident = slab.resident_bytes();
    assert!(resident >= 100_000 * 128 && resident <= touched, "{resident}");
    // Rendu de la fin : pages entières au-dessus du dernier occupé.
    let released = unsafe { slab.release_above(25_000) };
    assert_eq!(released % (2 << 20), 0);
    let after = slab.touched_bytes();
    assert_eq!(after + released, touched);
    assert_eq!(crate::pages::charged() - c0, after as u64, "le rendu revient au registre");
    assert!(slab.touched_bytes() >= 25_000 * 128, "les occupés restent comptés");
    assert!(slab.resident_bytes() <= slab.touched_bytes() + (2 << 20), "le rendu se voit au mincore");
    assert!(released >= 8 << 20, "12,8 Mo touchés, 3,2 Mo gardés : au moins 4 pages rendues");
    assert_eq!(unsafe { slab.release_above(25_000) }, 0, "déjà rendu");
    // Réutilisation après rendu : les slots se remplissent à nouveau.
    for i in 0..80_000u64 {
        slab.insert([i; 16]);
    }
    assert_eq!(slab.len(), 105_000);
    assert_eq!(unsafe { *slab.get_unchecked(104_999) }, [79_999; 16]);
    assert_eq!(slab.touched_bytes(), (105_000 * 128usize).next_multiple_of(T));
    drop(slab);
    assert_eq!(crate::pages::charged(), c0);
}

#[test]
fn pipelined_compaction_primitives() {
    let slab: TaggedHiSlab<[u64; 16]> = TaggedHiSlab::new(0, 1 << 20).unwrap();
    for i in 0..2_000u64 {
        slab.insert([i; 16]);
    }
    for i in (0..2_000).filter(|i| i % 3 != 0) {
        slab.remove(i);
    }
    slab.tag(1_998);
    assert_eq!(slab.prev_occupied(2_000), Some(1_998));
    assert_eq!(slab.prev_occupied(1_998), Some(1_995));
    assert_eq!(slab.prev_occupied(64), Some(63));
    assert_eq!(slab.prev_occupied(1), Some(0));
    assert_eq!(slab.prev_occupied(0), None);
    let (a, b) = unsafe { (slab.reserve_lowest(), slab.reserve_lowest()) };
    assert_eq!((a, b), (1, 2));
    unsafe { slab.unreserve(b) };
    unsafe { slab.move_to(1_998, a) };
    assert!(slab.is_occupied(1) && !slab.is_occupied(2) && !slab.is_occupied(1_998));
    assert_eq!(unsafe { *slab.get_unchecked(1) }, [1_998; 16]);
    assert!(slab.is_tagged(1) && !slab.is_tagged(1_998));
    assert_eq!(slab.slot_ptr(3) as usize - slab.slot_ptr(1) as usize, 256);
}

#[test]
fn random_tagged_is_uniform_over_tagged() {
    let slab = TaggedHiSlab::<u32>::new(0, 1 << 16).unwrap();
    assert!(slab.random_tagged(|_| 0).is_none());
    let tagged: Vec<u32> = (0..2000).map(|i| if i % 7 == 0 { slab.insert_tagged(i) } else { slab.insert(i) }).filter(|&i| slab.is_tagged(i)).collect();
    // Chaque rang tiré désigne un tagué distinct, dans l'ordre des indices.
    let mut seen = Vec::new();
    for k in 0..tagged.len() as u32 {
        let (idx, &v) = slab.random_tagged(|n| { assert_eq!(n as usize, tagged.len()); k }).unwrap();
        assert!(slab.is_tagged(idx) && v == idx);
        seen.push(idx);
    }
    assert_eq!(seen, tagged);
}

#[test]
fn next_tagged_walks_tagged_slots_only() {
    let slab = TaggedHiSlab::<u32>::new(0, 1 << 16).unwrap();
    let tagged: Vec<u32> = (0..5000u32).map(|i| if i % 13 == 0 { slab.insert_tagged(i) } else { slab.insert(i) }).filter(|&i| slab.is_tagged(i)).collect();
    let (mut at, mut seen) = (0, Vec::new());
    while let Some(i) = slab.next_tagged(at) {
        seen.push(i);
        at = i + 1;
    }
    assert_eq!(seen, tagged);
    slab.untag(tagged[1]);
    assert_eq!(slab.next_tagged(tagged[0] + 1), Some(tagged[2]));
    assert_eq!(slab.next_tagged(1 << 20), None);
}
