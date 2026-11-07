//! Bitset, type à part (BITSET_DESIGN.md §10) : réponses et erreurs de Redis pour
//! SETBIT / GETBIT / BITCOUNT / BITPOS, `WRONGTYPE` dans les deux sens, bascules de forme
//! (classique ↔ compacte) invisibles du client.

mod common;

use std::sync::OnceLock;

const PORT: u16 = 16450;

static INIT: OnceLock<()> = OnceLock::new();
fn server() -> redis::Connection {
    INIT.get_or_init(|| common::start_server(PORT));
    common::conn(PORT)
}

fn q<T: redis::FromRedisValue>(c: &mut redis::Connection, args: &[&str]) -> T {
    let mut cmd = redis::cmd(args[0]);
    for a in &args[1..] {
        cmd.arg(*a);
    }
    cmd.query(c).unwrap_or_else(|e| panic!("{args:?} : {e}"))
}

fn err(c: &mut redis::Connection, args: &[&str]) -> String {
    let mut cmd = redis::cmd(args[0]);
    for a in &args[1..] {
        cmd.arg(*a);
    }
    match cmd.query::<redis::Value>(c) {
        Err(e) => e.to_string(),
        Ok(v) => panic!("{args:?} : erreur attendue, reçu {v:?}"),
    }
}

#[test]
fn setbit_getbit_and_type() {
    let mut c = server();
    let _: () = q(&mut c, &["DEL", "b:basic"]);
    assert_eq!(q::<i64>(&mut c, &["SETBIT", "b:basic", "7", "1"]), 0);
    assert_eq!(q::<i64>(&mut c, &["SETBIT", "b:basic", "7", "1"]), 1);
    assert_eq!(q::<i64>(&mut c, &["GETBIT", "b:basic", "7"]), 1);
    assert_eq!(q::<i64>(&mut c, &["GETBIT", "b:basic", "6"]), 0);
    assert_eq!(q::<i64>(&mut c, &["GETBIT", "b:absent", "6"]), 0);
    assert_eq!(q::<String>(&mut c, &["TYPE", "b:basic"]), "bitset");
    assert_eq!(q::<i64>(&mut c, &["SETBIT", "b:basic", "4294967295", "1"]), 0);
    assert_eq!(q::<i64>(&mut c, &["GETBIT", "b:basic", "4294967295"]), 1);
    // le dernier bit à 1 effacé : la clé part ; SETBIT 0 sur une clé absente ne la crée pas
    assert_eq!(q::<i64>(&mut c, &["SETBIT", "b:basic", "7", "0"]), 1);
    assert_eq!(q::<i64>(&mut c, &["SETBIT", "b:basic", "4294967295", "0"]), 1);
    assert_eq!(q::<i64>(&mut c, &["EXISTS", "b:basic"]), 0);
    assert_eq!(q::<i64>(&mut c, &["SETBIT", "b:basic", "3", "0"]), 0);
    assert_eq!(q::<i64>(&mut c, &["EXISTS", "b:basic"]), 0);
}

#[test]
fn errors_like_redis() {
    let mut c = server();
    for (args, msg) in [
        (&["SETBIT", "b:e", "-1", "1"][..], "bit offset is not an integer or out of range"),
        (&["SETBIT", "b:e", "4294967296", "1"], "bit offset is not an integer or out of range"),
        (&["SETBIT", "b:e", "1", "2"], "bit is not an integer or out of range"),
        (&["GETBIT", "b:e", "x"], "bit offset is not an integer or out of range"),
        (&["BITPOS", "b:e", "2"], "The bit argument must be 1 or 0"),
        (&["BITCOUNT", "b:e", "1", "2", "WORD"], "syntax error"),
        (&["BITCOUNT", "b:e", "a", "2"], "not an integer"),
    ] {
        let e = err(&mut c, args);
        assert!(e.contains(msg), "{args:?} : {e}");
    }
    // WRONGTYPE dans les deux sens
    let _: () = q(&mut c, &["SET", "b:str", "x"]);
    common::assert_wrongtype(&redis::cmd("SETBIT").arg("b:str").arg(1).arg(1).query::<i64>(&mut c).unwrap_err());
    common::assert_wrongtype(&redis::cmd("BITCOUNT").arg("b:str").query::<i64>(&mut c).unwrap_err());
    let _: i64 = q(&mut c, &["SETBIT", "b:bits", "1", "1"]);
    common::assert_wrongtype(&redis::cmd("GET").arg("b:bits").query::<String>(&mut c).unwrap_err());
    common::assert_wrongtype(&redis::cmd("LPUSH").arg("b:bits").arg("x").query::<i64>(&mut c).unwrap_err());
}

/// Valeurs de la doc de Redis (BITCOUNT / BITPOS), avec la string construite bit par bit.
#[test]
fn bitcount_bitpos_like_redis() {
    let mut c = server();
    let _: () = q(&mut c, &["DEL", "b:cnt", "b:pos"]);
    // "foobar" : 66 6f 6f 62 61 72
    for (i, byte) in b"foobar".iter().enumerate() {
        for j in 0..8 {
            if byte & (0x80 >> j) != 0 {
                let _: i64 = q(&mut c, &["SETBIT", "b:cnt", &(i * 8 + j).to_string(), "1"]);
            }
        }
    }
    assert_eq!(q::<i64>(&mut c, &["BITCOUNT", "b:cnt"]), 26);
    assert_eq!(q::<i64>(&mut c, &["BITCOUNT", "b:cnt", "0", "0"]), 4);
    assert_eq!(q::<i64>(&mut c, &["BITCOUNT", "b:cnt", "1", "1"]), 6);
    assert_eq!(q::<i64>(&mut c, &["BITCOUNT", "b:cnt", "1", "1", "BYTE"]), 6);
    assert_eq!(q::<i64>(&mut c, &["BITCOUNT", "b:cnt", "5", "30", "BIT"]), 17);
    assert_eq!(q::<i64>(&mut c, &["BITCOUNT", "b:cnt", "-2", "-1"]), 7);
    assert_eq!(q::<i64>(&mut c, &["BITCOUNT", "b:cnt", "1"]), 22, "start seul (Valkey 8+)");
    assert_eq!(q::<i64>(&mut c, &["BITCOUNT", "b:absent"]), 0);
    // "\xff\xf0\x00"... sans l'octet nul de fin (longueur déduite du contenu) : ff f0
    for i in 0..12 {
        let _: i64 = q(&mut c, &["SETBIT", "b:pos", &i.to_string(), "1"]);
    }
    assert_eq!(q::<i64>(&mut c, &["BITPOS", "b:pos", "0"]), 12);
    assert_eq!(q::<i64>(&mut c, &["BITPOS", "b:pos", "1"]), 0);
    assert_eq!(q::<i64>(&mut c, &["BITPOS", "b:pos", "1", "1"]), 8);
    assert_eq!(q::<i64>(&mut c, &["BITPOS", "b:pos", "1", "2"]), -1);
    assert_eq!(q::<i64>(&mut c, &["BITPOS", "b:pos", "1", "7", "15", "BIT"]), 7);
    assert_eq!(q::<i64>(&mut c, &["BITPOS", "b:pos", "0", "0", "0"]), -1, "0 cherché dans une plage pleine, end donné");
    assert_eq!(q::<i64>(&mut c, &["BITPOS", "b:absent", "0"]), 0);
    assert_eq!(q::<i64>(&mut c, &["BITPOS", "b:absent", "1"]), -1);
    // octets pleins sans end : le bit juste après la longueur
    let _: () = q(&mut c, &["DEL", "b:pos"]);
    for i in 0..16 {
        let _: i64 = q(&mut c, &["SETBIT", "b:pos", &i.to_string(), "1"]);
    }
    assert_eq!(q::<i64>(&mut c, &["BITPOS", "b:pos", "0"]), 16);
}

/// Bascules de forme pendant les écritures : dense en bas puis offsets lointains, puis
/// effacements ; les réponses suivent un modèle.
#[test]
fn form_switches_are_invisible() {
    let mut c = server();
    let _: () = q(&mut c, &["DEL", "b:sw"]);
    let mut model = std::collections::BTreeSet::new();
    let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut rnd = || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    for round in 0..6 {
        let mut pipe = redis::pipe();
        let mut ops = Vec::new();
        for i in 0..3000u64 {
            let off = match round % 3 {
                0 => rnd() % 20_000,
                1 if i % 40 == 0 => rnd() % (1 << 32),
                _ => rnd() % 20_000,
            } as u32;
            let on = round % 3 != 2;
            pipe.cmd("SETBIT").arg("b:sw").arg(off).arg(on as u8);
            ops.push((off, on));
        }
        let old: Vec<i64> = pipe.query(&mut c).unwrap();
        for ((off, on), old) in ops.into_iter().zip(old) {
            assert_eq!(old == 1, model.contains(&off), "SETBIT {off}");
            if on { model.insert(off) } else { model.remove(&off) };
        }
        assert_eq!(q::<i64>(&mut c, &["BITCOUNT", "b:sw"]), model.len() as i64);
        let first = model.first().map_or(-1, |&k| k as i64);
        assert_eq!(q::<i64>(&mut c, &["BITPOS", "b:sw", "1"]), first);
        let mid = model.range(10_000..).count() as i64;
        assert_eq!(q::<i64>(&mut c, &["BITCOUNT", "b:sw", "10000", "4294967295", "BIT"]), mid);
    }
}
