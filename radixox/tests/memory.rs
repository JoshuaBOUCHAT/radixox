//! Comptabilité mémoire (MEMORY_ARCH.md §10.1) : `INFO` compte tout ce que le processus
//! tient en propre — pool, `large`, chaque HiSlab, tables — et l'écart avec le RSS reste
//! fixe quand les données grandissent ; un HiSlab vidé rend sa mémoire.

mod common;

const PORT: u16 = 16410;

fn info(c: &mut redis::Connection) -> std::collections::HashMap<String, i64> {
    let s: String = redis::cmd("INFO").query(c).unwrap();
    s.lines().filter_map(|l| l.split_once(':')).filter_map(|(k, v)| Some((k.to_string(), v.trim().parse().ok()?))).collect()
}

const MB: i64 = 1 << 20;

#[test]
fn rss_is_accounted_and_emptied_slabs_shrink() {
    common::start_server(PORT);
    let mut c = common::conn(PORT);
    let empty = info(&mut c)["mem_untracked_bytes"];

    // Toutes les formes : strings, petits et gros Hash, Set, ZSet (index, B+tree, poignées),
    // List, grandes valeurs.
    let mut p = redis::pipe();
    for i in 0..100_000 {
        p.cmd("SET").arg(format!("s:{i}")).arg(vec![b'x'; 20 + i % 180]).ignore();
    }
    for i in 0..30_000 {
        p.cmd("HSET").arg(format!("h:{i}")).arg("f1").arg("v").arg("f2").arg(i).ignore();
    }
    for i in 0..500 {
        let mut z = redis::cmd("ZADD");
        z.arg(format!("z:{i}"));
        for j in 0..400 {
            z.arg(j).arg(format!("member:{j}"));
        }
        p.add_command(z).ignore();
    }
    for i in 0..500 {
        p.cmd("RPUSH").arg(format!("l:{i}")).arg((0..100).map(|j| format!("item-{j}")).collect::<Vec<_>>()).ignore();
    }
    for i in 0..100 {
        p.cmd("SET").arg(format!("big:{i}")).arg(vec![b'y'; 100_000]).ignore();
    }
    p.query::<()>(&mut c).unwrap();

    let m = info(&mut c);
    let untracked = m["mem_untracked_bytes"];
    assert!(m["mem_rss_anon_bytes"] > 60 * MB, "jeu chargé : {} o", m["mem_rss_anon_bytes"]);
    // pile, binaire, libc pour elle-même : quelques Mo, qui ne grandissent pas avec les
    // données (une région oubliée en ferait des dizaines)
    assert!(untracked.abs() < 6 * MB && (untracked - empty).abs() < 4 * MB, "à vide {empty} o, chargé {untracked} o");
    // plus rien chez la libc ; les mappings directs ne servent que les cas hors du tas
    assert_eq!(m["mem_direct_bytes"], 0, "mappings directs");
    let zset = m["mem_hislab_zset_tree_bytes"] + m["mem_hislab_zset_index_bytes"];
    assert!(zset > 4 * MB, "slabs des ZSet : {zset} o");

    // Tous les ZSet supprimés : rien sous leur dernier occupé, seul le rognage rend la fin.
    let mut p = redis::pipe();
    for i in 0..500 {
        p.cmd("DEL").arg(format!("z:{i}")).ignore();
    }
    p.query::<()>(&mut c).unwrap();
    // libération paresseuse des grands ZSet puis rognage (minuteur de 100 ms) ; le binaire
    // de test est en debug, bien plus lent
    let (mut after, mut live) = (0, 0);
    for _ in 0..200 {
        std::thread::sleep(std::time::Duration::from_millis(100));
        let m = info(&mut c);
        after = m["mem_hislab_zset_tree_bytes"] + m["mem_hislab_zset_index_bytes"];
        live = m["mem_hislab_zset_tree_live_bytes"] + m["mem_hislab_zset_index_live_bytes"];
        // au plus 2 Mio gardés + 2 Mio de seuil par slab
        if live == 0 && after <= 8 * MB {
            break;
        }
    }
    assert!(live == 0 && after <= 8 * MB, "slabs des ZSet après suppression : {after} o résidents, {live} o vivants (avant {zset})");
    assert!(after < zset, "rien rendu : {after} o (avant {zset})");
    let _ = redis::cmd("SHUTDOWN").arg("NOSAVE").query::<()>(&mut c);
}
