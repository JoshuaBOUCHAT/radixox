use crate::value::Value;

use radixox_lib::shared_byte::SharedByte;

use crate::OxidArt;

#[test]
fn test_node_size() {
    let size = std::mem::size_of::<crate::Node>();
    eprintln!("Node size: {} bytes", size);
    assert!(size <= 128, "Node should be <= 128 bytes, got {}", size);
}

#[test]
fn test_get_set_basic() {
    let mut art = OxidArt::new();
    let key = SharedByte::from_str("Joshua");
    let val = Value::from_str("BOUCHAT");
    art.set(key.clone(), val.clone());
    assert_eq!(art.get(&key), Some(val));
}

#[test]
fn test_empty_key() {
    let mut art = OxidArt::new();
    let key = SharedByte::from_str("");
    let val = Value::from_str("root_value");
    art.set(key.clone(), val.clone());
    assert_eq!(art.get(&key), Some(val));
}

#[test]
fn test_get_nonexistent() {
    let mut art = OxidArt::new();
    assert_eq!(art.get(&SharedByte::from_str("missing")), None);
}

#[test]
fn test_overwrite_value() {
    let mut art = OxidArt::new();
    let key = SharedByte::from_str("key");
    let val1 = Value::from_str("value1");
    let val2 = Value::from_str("value2");

    art.set(key.clone(), val1.clone());
    assert_eq!(art.get(&key), Some(val1));

    art.set(key.clone(), val2.clone());
    assert_eq!(art.get(&key), Some(val2));
}

#[test]
fn test_common_prefix_split() {
    // Test le split: "user" et "uso" partagent "us"
    let mut art = OxidArt::new();

    art.set(SharedByte::from_str("user"), Value::from_str("val_user"));
    art.set(SharedByte::from_str("uso"), Value::from_str("val_uso"));

    assert_eq!(
        art.get(&SharedByte::from_str("user")),
        Some(Value::from_str("val_user"))
    );
    assert_eq!(
        art.get(&SharedByte::from_str("uso")),
        Some(Value::from_str("val_uso"))
    );
    // "us" n'a pas de valeur
    assert_eq!(art.get(&SharedByte::from_str("us")), None);
}

#[test]
fn test_prefix_is_also_key() {
    // "us" est un préfixe de "user" mais aussi une clé
    let mut art = OxidArt::new();

    art.set(SharedByte::from_str("user"), Value::from_str("val_user"));
    art.set(SharedByte::from_str("us"), Value::from_str("val_us"));

    assert_eq!(
        art.get(&SharedByte::from_str("user")),
        Some(Value::from_str("val_user"))
    );
    assert_eq!(
        art.get(&SharedByte::from_str("us")),
        Some(Value::from_str("val_us"))
    );
}

#[test]
fn test_multiple_branches() {
    let mut art = OxidArt::new();

    art.set(SharedByte::from_str("apple"), Value::from_str("1"));
    art.set(SharedByte::from_str("application"), Value::from_str("2"));
    art.set(SharedByte::from_str("banana"), Value::from_str("3"));
    art.set(SharedByte::from_str("band"), Value::from_str("4"));

    assert_eq!(
        art.get(&SharedByte::from_str("apple")),
        Some(Value::from_str("1"))
    );
    assert_eq!(
        art.get(&SharedByte::from_str("application")),
        Some(Value::from_str("2"))
    );
    assert_eq!(
        art.get(&SharedByte::from_str("banana")),
        Some(Value::from_str("3"))
    );
    assert_eq!(
        art.get(&SharedByte::from_str("band")),
        Some(Value::from_str("4"))
    );

    // Clés partielles qui n'existent pas
    assert_eq!(art.get(&SharedByte::from_str("app")), None);
    assert_eq!(art.get(&SharedByte::from_str("ban")), None);
}

#[test]
fn test_del_basic() {
    let mut art = OxidArt::new();
    let key = SharedByte::from_str("hello");
    let val = Value::from_str("world");

    art.set(key.clone(), val.clone());
    assert_eq!(art.get(&key), Some(val.clone()));

    let deleted = art.del(&key);
    assert_eq!(deleted, Some(val));
    assert_eq!(art.get(&key), None);
}

#[test]
fn test_del_nonexistent() {
    let mut art = OxidArt::new();
    assert_eq!(art.del(b"missing"), None);
}

#[test]
fn test_del_empty_key() {
    let mut art = OxidArt::new();
    let val = Value::from_str("root");

    art.set(SharedByte::from_str(""), val.clone());
    assert_eq!(art.del(b""), Some(val));
    assert_eq!(art.get(&SharedByte::from_str("")), None);
}

#[test]
fn test_del_with_recompression() {
    // us -> {er, o}  après del("uso") -> "user"
    let mut art = OxidArt::new();

    art.set(SharedByte::from_str("user"), Value::from_str("val_user"));
    art.set(SharedByte::from_str("uso"), Value::from_str("val_uso"));

    // Supprimer "uso"
    let deleted = art.del(b"uso");
    assert_eq!(deleted, Some(Value::from_str("val_uso")));

    // "user" doit toujours exister
    assert_eq!(
        art.get(&SharedByte::from_str("user")),
        Some(Value::from_str("val_user"))
    );
    // "uso" n'existe plus
    assert_eq!(art.get(&SharedByte::from_str("uso")), None);
}

#[test]
fn test_del_intermediate_node_with_children() {
    // Supprimer un node intermédiaire qui a des enfants
    let mut art = OxidArt::new();

    art.set(SharedByte::from_str("a"), Value::from_str("val_a"));
    art.set(SharedByte::from_str("ab"), Value::from_str("val_ab"));
    art.set(SharedByte::from_str("abc"), Value::from_str("val_abc"));

    // Supprimer "ab" qui est intermédiaire
    let deleted = art.del(b"ab");
    assert_eq!(deleted, Some(Value::from_str("val_ab")));

    // "a" et "abc" doivent toujours exister
    assert_eq!(
        art.get(&SharedByte::from_str("a")),
        Some(Value::from_str("val_a"))
    );
    assert_eq!(
        art.get(&SharedByte::from_str("abc")),
        Some(Value::from_str("val_abc"))
    );
    assert_eq!(art.get(&SharedByte::from_str("ab")), None);
}

#[test]
fn test_many_keys_same_prefix() {
    let mut art = OxidArt::new();

    // Beaucoup de clés avec le même préfixe pour tester les huge_childs
    for i in 1..=20u8 {
        let key = SharedByte::from_byte(vec![b'x', i]);
        let val = Value::String(SharedByte::from_byte(vec![i]));
        art.set(key, val);
    }

    for i in 1..=20u8 {
        let key = SharedByte::from_byte(vec![b'x', i]);
        let expected = Value::String(SharedByte::from_byte(vec![i]));
        assert_eq!(art.get(&key), Some(expected));
    }
}

#[test]
fn test_long_keys() {
    let mut art = OxidArt::new();

    let key1 = SharedByte::from_byte(vec![b'a'; 100]);
    let key2 = SharedByte::from_byte(vec![b'a'; 50]);
    let val1 = Value::from_str("long");
    let val2 = Value::from_str("medium");

    art.set(key1.clone(), val1.clone());
    art.set(key2.clone(), val2.clone());

    assert_eq!(art.get(&key1), Some(val1));
    assert_eq!(art.get(&key2), Some(val2));
}

#[test]
fn test_del_then_reinsert() {
    let mut art = OxidArt::new();
    let key = SharedByte::from_str("key");
    let val1 = Value::from_str("val1");
    let val2 = Value::from_str("val2");

    art.set(key.clone(), val1.clone());
    art.del(&key);
    art.set(key.clone(), val2.clone());

    assert_eq!(art.get(&key), Some(val2));
}

#[test]
fn test_del_all_keys() {
    let mut art = OxidArt::new();

    art.set(SharedByte::from_str("a"), Value::from_str("1"));
    art.set(SharedByte::from_str("b"), Value::from_str("2"));
    art.set(SharedByte::from_str("c"), Value::from_str("3"));

    art.del(b"a");
    art.del(b"b");
    art.del(b"c");

    assert_eq!(art.get(&SharedByte::from_str("a")), None);
    assert_eq!(art.get(&SharedByte::from_str("b")), None);
    assert_eq!(art.get(&SharedByte::from_str("c")), None);
}

#[test]
fn test_partial_key_not_found() {
    let mut art = OxidArt::new();

    art.set(SharedByte::from_str("hello_world"), Value::from_str("val"));

    // Clés partielles ne doivent pas matcher
    assert_eq!(art.get(&SharedByte::from_str("hello")), None);
    assert_eq!(art.get(&SharedByte::from_str("hello_")), None);
    assert_eq!(art.get(&SharedByte::from_str("hello_worl")), None);
    // Clé trop longue non plus
    assert_eq!(art.get(&SharedByte::from_str("hello_world!")), None);
}

// ============ Tests pour getn ============

#[test]
fn test_getn_basic() {
    let mut art = OxidArt::new();

    art.set(
        SharedByte::from_str("user:alice"),
        Value::from_str("alice_data"),
    );
    art.set(
        SharedByte::from_str("user:bob"),
        Value::from_str("bob_data"),
    );
    art.set(
        SharedByte::from_str("user:charlie"),
        Value::from_str("charlie_data"),
    );
    art.set(SharedByte::from_str("post:1"), Value::from_str("post_1"));

    let results = art.getn(SharedByte::from_str("user:"));

    assert_eq!(results.len(), 3);
    assert!(results.contains(&(
        SharedByte::from_str("user:alice"),
        Value::from_str("alice_data")
    )));
    assert!(results.contains(&(
        SharedByte::from_str("user:bob"),
        Value::from_str("bob_data")
    )));
    assert!(results.contains(&(
        SharedByte::from_str("user:charlie"),
        Value::from_str("charlie_data")
    )));
}

#[test]
fn test_getn_empty_prefix() {
    let mut art = OxidArt::new();

    art.set(SharedByte::from_str("a"), Value::from_str("1"));
    art.set(SharedByte::from_str("b"), Value::from_str("2"));
    art.set(SharedByte::from_str("c"), Value::from_str("3"));

    let results = art.getn(SharedByte::from_str(""));

    assert_eq!(results.len(), 3);
}

#[test]
fn test_getn_no_match() {
    let mut art = OxidArt::new();

    art.set(SharedByte::from_str("user:alice"), Value::from_str("data"));

    let results = art.getn(SharedByte::from_str("post:"));

    assert!(results.is_empty());
}

#[test]
fn test_getn_exact_key() {
    let mut art = OxidArt::new();

    art.set(SharedByte::from_str("user"), Value::from_str("user_val"));
    art.set(
        SharedByte::from_str("user:alice"),
        Value::from_str("alice_val"),
    );

    // Préfixe exact "user" doit retourner "user" et "user:alice"
    let results = art.getn(SharedByte::from_str("user"));

    assert_eq!(results.len(), 2);
    assert!(results.contains(&(SharedByte::from_str("user"), Value::from_str("user_val"))));
    assert!(results.contains(&(
        SharedByte::from_str("user:alice"),
        Value::from_str("alice_val")
    )));
}

#[test]
fn test_getn_prefix_in_compression() {
    // Test quand le préfixe se termine au milieu d'une compression
    let mut art = OxidArt::new();

    art.set(
        SharedByte::from_str("application"),
        Value::from_str("app_val"),
    );

    // "app" est un préfixe de "application"
    let results = art.getn(SharedByte::from_str("app"));

    assert_eq!(results.len(), 1);
    assert!(results.contains(&(
        SharedByte::from_str("application"),
        Value::from_str("app_val")
    )));
}

#[test]
fn test_getn_with_nested_keys() {
    let mut art = OxidArt::new();

    art.set(SharedByte::from_str("a"), Value::from_str("1"));
    art.set(SharedByte::from_str("ab"), Value::from_str("2"));
    art.set(SharedByte::from_str("abc"), Value::from_str("3"));
    art.set(SharedByte::from_str("abcd"), Value::from_str("4"));
    art.set(SharedByte::from_str("abd"), Value::from_str("5"));

    let results = art.getn(SharedByte::from_str("ab"));

    assert_eq!(results.len(), 4); // ab, abc, abcd, abd
    assert!(results.contains(&(SharedByte::from_str("ab"), Value::from_str("2"))));
    assert!(results.contains(&(SharedByte::from_str("abc"), Value::from_str("3"))));
    assert!(results.contains(&(SharedByte::from_str("abcd"), Value::from_str("4"))));
    assert!(results.contains(&(SharedByte::from_str("abd"), Value::from_str("5"))));
}

#[test]
fn test_getn_single_char_prefix() {
    let mut art = OxidArt::new();

    art.set(SharedByte::from_str("aa"), Value::from_str("1"));
    art.set(SharedByte::from_str("ab"), Value::from_str("2"));
    art.set(SharedByte::from_str("ba"), Value::from_str("3"));

    let results = art.getn(SharedByte::from_str("a"));

    assert_eq!(results.len(), 2);
    assert!(results.contains(&(SharedByte::from_str("aa"), Value::from_str("1"))));
    assert!(results.contains(&(SharedByte::from_str("ab"), Value::from_str("2"))));
}

#[test]
fn test_getn_many_children() {
    let mut art = OxidArt::new();

    // Plus de 10 enfants pour tester huge_childs
    for i in 1..=20u8 {
        let key = SharedByte::from_byte(vec![b'x', b':', i]);
        let val = Value::String(SharedByte::from_byte(vec![i]));
        art.set(key, val);
    }

    let results = art.getn(SharedByte::from_str("x:"));

    assert_eq!(results.len(), 20);
}

// ============ Tests pour deln ============

#[test]
fn test_deln_basic() {
    let mut art = OxidArt::new();

    art.set(
        SharedByte::from_str("user:alice"),
        Value::from_str("alice_data"),
    );
    art.set(
        SharedByte::from_str("user:bob"),
        Value::from_str("bob_data"),
    );
    art.set(
        SharedByte::from_str("user:charlie"),
        Value::from_str("charlie_data"),
    );
    art.set(SharedByte::from_str("post:1"), Value::from_str("post_1"));

    let deleted = art.deln(b"user:");

    assert_eq!(deleted, 3);
    assert_eq!(art.get(&SharedByte::from_str("user:alice")), None);
    assert_eq!(art.get(&SharedByte::from_str("user:bob")), None);
    assert_eq!(art.get(&SharedByte::from_str("user:charlie")), None);
    // post:1 doit toujours exister
    assert_eq!(
        art.get(&SharedByte::from_str("post:1")),
        Some(Value::from_str("post_1"))
    );
}

#[test]
fn test_deln_empty_prefix() {
    let mut art = OxidArt::new();

    art.set(SharedByte::from_str("a"), Value::from_str("1"));
    art.set(SharedByte::from_str("b"), Value::from_str("2"));
    art.set(SharedByte::from_str("c"), Value::from_str("3"));

    let deleted = art.deln(b"");

    assert_eq!(deleted, 3);
    assert_eq!(art.get(&SharedByte::from_str("a")), None);
    assert_eq!(art.get(&SharedByte::from_str("b")), None);
    assert_eq!(art.get(&SharedByte::from_str("c")), None);
}

#[test]
fn test_deln_no_match() {
    let mut art = OxidArt::new();

    art.set(SharedByte::from_str("user:alice"), Value::from_str("data"));

    let deleted = art.deln(b"post:");

    assert_eq!(deleted, 0);
    // user:alice doit toujours exister
    assert_eq!(
        art.get(&SharedByte::from_str("user:alice")),
        Some(Value::from_str("data"))
    );
}

#[test]
fn test_deln_exact_key_with_children() {
    let mut art = OxidArt::new();

    art.set(SharedByte::from_str("user"), Value::from_str("user_val"));
    art.set(
        SharedByte::from_str("user:alice"),
        Value::from_str("alice_val"),
    );
    art.set(SharedByte::from_str("user:bob"), Value::from_str("bob_val"));

    // Supprimer "user" et tous ses descendants
    let deleted = art.deln(b"user");

    assert_eq!(deleted, 3);
    assert_eq!(art.get(&SharedByte::from_str("user")), None);
    assert_eq!(art.get(&SharedByte::from_str("user:alice")), None);
    assert_eq!(art.get(&SharedByte::from_str("user:bob")), None);
}

#[test]
fn test_deln_prefix_in_compression() {
    let mut art = OxidArt::new();

    art.set(
        SharedByte::from_str("application"),
        Value::from_str("app_val"),
    );
    art.set(SharedByte::from_str("apple"), Value::from_str("apple_val"));

    // "app" est un préfixe commun
    let deleted = art.deln(b"app");

    assert_eq!(deleted, 2);
    assert_eq!(art.get(&SharedByte::from_str("application")), None);
    assert_eq!(art.get(&SharedByte::from_str("apple")), None);
}

#[test]
fn test_deln_with_nested_keys() {
    let mut art = OxidArt::new();

    art.set(SharedByte::from_str("a"), Value::from_str("1"));
    art.set(SharedByte::from_str("ab"), Value::from_str("2"));
    art.set(SharedByte::from_str("abc"), Value::from_str("3"));
    art.set(SharedByte::from_str("abcd"), Value::from_str("4"));
    art.set(SharedByte::from_str("abd"), Value::from_str("5"));
    art.set(SharedByte::from_str("b"), Value::from_str("6"));

    let deleted = art.deln(b"ab");

    assert_eq!(deleted, 4); // ab, abc, abcd, abd
    assert_eq!(
        art.get(&SharedByte::from_str("a")),
        Some(Value::from_str("1"))
    );
    assert_eq!(art.get(&SharedByte::from_str("ab")), None);
    assert_eq!(art.get(&SharedByte::from_str("abc")), None);
    assert_eq!(
        art.get(&SharedByte::from_str("b")),
        Some(Value::from_str("6"))
    );
}

#[test]
fn test_deln_many_children() {
    let mut art = OxidArt::new();

    // Plus de 10 enfants pour tester huge_childs
    for i in 1..=20u8 {
        let key = SharedByte::from_byte(vec![b'x', b':', i]);
        let val = Value::String(SharedByte::from_byte(vec![i]));
        art.set(key, val);
    }

    let deleted = art.deln(b"x:");

    assert_eq!(deleted, 20);

    // Vérifier qu'ils sont tous supprimés
    for i in 1..=20u8 {
        let key = SharedByte::from_byte(vec![b'x', b':', i]);
        assert_eq!(art.get(&key), None);
    }
}

#[test]
fn test_deln_then_insert() {
    let mut art = OxidArt::new();

    art.set(SharedByte::from_str("user:alice"), Value::from_str("old"));
    art.deln(b"user:");

    // Réinsérer après suppression
    art.set(SharedByte::from_str("user:bob"), Value::from_str("new"));

    assert_eq!(art.get(&SharedByte::from_str("user:alice")), None);
    assert_eq!(
        art.get(&SharedByte::from_str("user:bob")),
        Some(Value::from_str("new"))
    );
}

#[test]
fn test_deln_partial_match() {
    let mut art = OxidArt::new();

    art.set(SharedByte::from_str("hello"), Value::from_str("1"));
    art.set(SharedByte::from_str("help"), Value::from_str("2"));
    art.set(SharedByte::from_str("world"), Value::from_str("3"));

    // "hel" matche "hello" et "help"
    let deleted = art.deln(b"hel");

    assert_eq!(deleted, 2);
    assert_eq!(art.get(&SharedByte::from_str("hello")), None);
    assert_eq!(art.get(&SharedByte::from_str("help")), None);
    assert_eq!(
        art.get(&SharedByte::from_str("world")),
        Some(Value::from_str("3"))
    );
}

// ============ Tests TTL ============

#[test]
fn test_ttl_expired_on_get() {
    use std::time::Duration;

    let mut art = OxidArt::new();
    art.set_now(0);

    // Insert with short TTL (1 second)
    art.set_ttl(
        SharedByte::from_str("expired"),
        Duration::from_secs(1),
        Value::from_str("old"),
    );
    // Insert with longer TTL (100 seconds)
    art.set_ttl(
        SharedByte::from_str("valid"),
        Duration::from_secs(100),
        Value::from_str("new"),
    );
    // Insert with no expiry
    art.set(SharedByte::from_str("forever"), Value::from_str("eternal"));

    // Move time forward past first TTL
    art.set_now(50);

    // Expired key should return None and be cleaned up
    assert_eq!(art.get(&SharedByte::from_str("expired")), None);
    // Valid key should still work
    assert_eq!(
        art.get(&SharedByte::from_str("valid")),
        Some(Value::from_str("new"))
    );
    // No expiry key should work
    assert_eq!(
        art.get(&SharedByte::from_str("forever")),
        Some(Value::from_str("eternal"))
    );

    // Move time forward, valid should expire
    art.set_now(150);
    assert_eq!(art.get(&SharedByte::from_str("valid")), None);
    // No expiry still works
    assert_eq!(
        art.get(&SharedByte::from_str("forever")),
        Some(Value::from_str("eternal"))
    );
}

#[test]
fn test_ttl_getn_filters_expired() {
    use std::time::Duration;

    let mut art = OxidArt::new();
    art.set_now(0);

    // Short TTL - will expire
    art.set_ttl(
        SharedByte::from_str("user:expired"),
        Duration::from_secs(1),
        Value::from_str("old"),
    );
    // Longer TTL - won't expire
    art.set_ttl(
        SharedByte::from_str("user:valid"),
        Duration::from_secs(100),
        Value::from_str("new"),
    );
    // No expiry
    art.set(
        SharedByte::from_str("user:forever"),
        Value::from_str("eternal"),
    );

    // Move time forward past first TTL
    art.set_now(50);

    let results = art.getn(SharedByte::from_str("user:"));

    // Should only return 2 (valid and forever), not the expired one
    assert_eq!(results.len(), 2);
    assert!(
        !results
            .iter()
            .any(|(k, _)| k == &SharedByte::from_str("user:expired"))
    );
    assert!(
        results
            .iter()
            .any(|(k, _)| k == &SharedByte::from_str("user:valid"))
    );
    assert!(
        results
            .iter()
            .any(|(k, _)| k == &SharedByte::from_str("user:forever"))
    );
}

#[test]
fn test_ttl_cleanup_on_expired_get() {
    use std::time::Duration;

    let mut art = OxidArt::new();
    art.set_now(0);

    // Create a path: user -> er (with short TTL)
    art.set_ttl(
        SharedByte::from_str("user"),
        Duration::from_secs(1),
        Value::from_str("expired_user"),
    );
    // Longer TTL
    art.set_ttl(
        SharedByte::from_str("username"),
        Duration::from_secs(100),
        Value::from_str("valid"),
    );

    // Move time forward
    art.set_now(50);

    // Get the expired key - should trigger cleanup
    let user = art.get(&SharedByte::from_str("user"));

    assert_eq!(
        user,
        None,
        "expiracy: {:?}",
        art.get_ttl(SharedByte::from_str("user"))
    );

    // The valid key should still work

    let valid = art.get(&SharedByte::from_str("username"));
    assert_eq!(
        valid,
        Some(Value::from_str("valid")),
        "expiracy: {:?}",
        art.get_ttl(SharedByte::from_str("username"))
    );
}

#[test]
fn test_evict_expired_basic() {
    use std::time::Duration;

    let mut art = OxidArt::new();
    art.set_now(0);

    // Insert 50 keys with short TTL
    for i in 1..=50u8 {
        let key = SharedByte::from_byte(vec![b'k', i]);
        art.set_ttl(key, Duration::from_secs(1), Value::from_str("val"));
    }

    // Insert 10 keys with long TTL
    for i in 1..=10u8 {
        let key = SharedByte::from_byte(vec![b'l', i]);
        art.set_ttl(key, Duration::from_secs(1000), Value::from_str("val"));
    }

    // Insert 10 keys without TTL
    for i in 1..=10u8 {
        let key = SharedByte::from_byte(vec![b'n', i]);
        art.set(key, Value::from_str("val"));
    }

    // Move time forward - short TTL keys are now expired
    art.set_now(100);

    // Evict expired entries (may need multiple calls due to probabilistic sampling)
    let mut total_evicted = 0;
    for _ in 0..30 {
        let evicted = art.evict_expired();
        total_evicted += evicted;
        if evicted == 0 {
            break;
        }
    }

    // Should have evicted all 50 expired keys
    assert_eq!(total_evicted, 50);

    // Long TTL keys should still exist
    for i in 1..=10u8 {
        let key = SharedByte::from_byte(vec![b'l', i]);
        assert_eq!(art.get(&key), Some(Value::from_str("val")));
    }

    // No TTL keys should still exist
    for i in 1..=10u8 {
        let key = SharedByte::from_byte(vec![b'n', i]);
        assert_eq!(art.get(&key), Some(Value::from_str("val")));
    }
}

#[test]
fn test_evict_expired_partial() {
    use std::time::Duration;

    let mut art = OxidArt::new();
    art.set_now(0);

    // Insert 10 keys with short TTL (will expire)
    for i in 1..=10u8 {
        let key = SharedByte::from_byte(vec![b'e', i]);
        art.set_ttl(key, Duration::from_secs(1), Value::from_str("val"));
    }

    // Insert 90 keys with long TTL (won't expire)
    for i in 1..=90u8 {
        let key = SharedByte::from_byte(vec![b'v', i]);
        art.set_ttl(key, Duration::from_secs(1000), Value::from_str("val"));
    }

    // Move time forward
    art.set_now(100);

    // Evict - should stop after one round since < 25% expired
    let evicted = art.evict_expired();

    // Should have evicted at most the 10 expired keys
    assert!(evicted <= 10);
}

// ============ Tests avec dictionnaire français ============

#[test]
fn test_recompress_transfers_overflow() {
    // Scenario: child absorbed by try_recompress has overflow children.
    // Before the fix, overflow_idx was not transferred → children past CHILDS_SIZE were lost.
    //
    // Tree after inserting "xc1".."xc7":
    //   root →['x']→ node{comp:"c"} →['1'..'7']   (7 children, 6 inline + 1 in overflow)
    //
    // After inserting "x" (split at comp boundary):
    //   root →['x']→ node{comp:"", val="x", 1 child 'c'}
    //                            →['c']→ node{comp:"", 7 children (overflow)}
    //
    // Deleting "x" triggers try_recompress: the 'x' node (1 child, no val) absorbs 'c'.
    // Without fix: overflow_idx of 'c' is not copied → "xc7" disappears.
    let mut art = OxidArt::new();

    // 7 children forces one entry into overflow (CHILDS_SIZE = 6)
    for i in 1u8..=7 {
        let key = format!("xc{i}");
        art.set(SharedByte::from_str(&key), Value::from_str(&format!("v{i}")));
    }

    // Creates intermediate node "x" with exactly 1 child ('c') carrying the overflow
    art.set(SharedByte::from_str("x"), Value::from_str("xval"));

    // Deleting "x" triggers try_recompress which must transfer overflow_idx
    let deleted = art.del(b"x");
    assert!(deleted.is_some(), "del(x) should return the value");

    // All 7 children must remain accessible after recompress
    for i in 1u8..=7 {
        let key = format!("xc{i}");
        let got = art.get(key.as_bytes());
        assert!(
            got.is_some(),
            "key \"{key}\" lost after try_recompress (overflow_idx not transferred)"
        );
    }

    // Prefix scan must also return all 7
    let results = art.getn(SharedByte::from_str("xc"));
    assert_eq!(results.len(), 7, "getn(xc) should find all 7 keys after recompress");
}

/*#[test]
fn test_ensure() {
    let mut art = OxidArt::new();
    art.set_now(0);
    const KEY: &[u8] = b"Hello, World!";
    let idx = art.ensure_key(KEY);
    let val = Value::String(SharedByte::from_byte(KEY));
    let node = art.get_node_mut(idx);
    node.val = Some(val.clone());
    node.exp_and_radix.set_exp(1 << 50);
    assert_eq!(art.get(KEY), Some(val));
}
*/
