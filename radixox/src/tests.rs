use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::str::from_utf8;

use monoio::time::Instant;

use crate::ArtClient;
use crate::monoio_client::monoio_art::SharedMonoIOClient;

const SERVER_ADDR: SocketAddr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 8379));

// ============================================================================
// BASIC OPERATIONS
// ============================================================================

#[monoio::test(enable_timer = true)]
async fn test_set_get() {
    let client = SharedMonoIOClient::new(SERVER_ADDR)
        .await
        .expect("connection failed");

    // Test with &str
    client.set("test:key1", "value1").await.unwrap();
    let val = client.get("test:key1").await.unwrap();
    assert_eq!(val, Some("value1".into()));

    // Test with String
    let key = String::from("test:key2");
    client.set(&key, "value2").await.unwrap();
    let val = client.get(&key).await.unwrap();
    assert_eq!(val, Some("value2".into()));

    // Cleanup
    client.del("test:key1").await.unwrap();
    client.del("test:key2").await.unwrap();
}

#[monoio::test(enable_timer = true)]
async fn test_del() {
    let client = SharedMonoIOClient::new(SERVER_ADDR)
        .await
        .expect("connection failed");

    client.set("test:del", "to_delete").await.unwrap();

    let deleted = client.del("test:del").await.unwrap();
    assert_eq!(deleted, Some("to_delete".into()));

    // Should be None after deletion
    let val = client.get("test:del").await.unwrap();
    assert_eq!(val, None);
}

#[monoio::test(enable_timer = true)]
async fn test_get_missing_key() {
    let client = SharedMonoIOClient::new(SERVER_ADDR)
        .await
        .expect("connection failed");

    let val = client.get("nonexistent:key:12345").await.unwrap();
    assert_eq!(val, None);
}

// ============================================================================
// PREFIX OPERATIONS
// ============================================================================

#[monoio::test(enable_timer = true)]
async fn test_getn() {
    let client = SharedMonoIOClient::new(SERVER_ADDR)
        .await
        .expect("connection failed");

    // Setup test data
    client.set("prefix:a", "val_a").await.unwrap();
    client.set("prefix:b", "val_b").await.unwrap();
    client.set("prefix:c", "val_c").await.unwrap();
    client.set("other:x", "val_x").await.unwrap();

    // Get all with prefix
    let values = client.getn("prefix").await.unwrap();
    for word in values.iter().filter_map(|i| from_utf8(i).ok()) {
        println!("Words: {}", word);
    }

    assert_eq!(values.len(), 3);

    // Cleanup
    client.deln("prefix").await.unwrap();
    client.del("other:x").await.unwrap();
}

#[monoio::test(enable_timer = true)]
async fn test_deln() {
    let client = SharedMonoIOClient::new(SERVER_ADDR)
        .await
        .expect("connection failed");

    // Setup test data
    client.set("batch:1", "v1").await.unwrap();
    client.set("batch:2", "v2").await.unwrap();
    client.set("batch:3", "v3").await.unwrap();

    // Delete all with prefix
    client.deln("batch").await.unwrap();

    // Verify deletion
    let values = client.getn("batch").await.unwrap();
    assert!(values.is_empty());
}

// ============================================================================
// JSON SERIALIZATION
// ============================================================================

#[monoio::test(enable_timer = true)]
async fn test_json_serde() {
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Serialize, Deserialize, PartialEq)]
    struct User {
        id: u64,
        name: String,
    }

    let client = SharedMonoIOClient::new(SERVER_ADDR)
        .await
        .expect("connection failed");

    let user = User {
        id: 42,
        name: "Alice".into(),
    };

    client.set_json("user:42", &user).await.unwrap();

    let retrieved: Option<User> = client.get_json("user:42").await.unwrap();
    assert_eq!(retrieved, Some(user));

    // Cleanup
    client.del("user:42").await.unwrap();
}

// ============================================================================
// PERFORMANCE / STRESS TESTS
// ============================================================================

const CHUNCK_SIZE: usize = 50;

#[monoio::test(enable_timer = true)]
async fn test_throughput() {
    let client = SharedMonoIOClient::new(SERVER_ADDR)
        .await
        .expect("connection failed");

    let words: Vec<String> = include_str!("../../list.txt")
        .lines()
        .map(|s| s.to_string())
        .collect();

    let total_words = words.len();
    let now = Instant::now();

    // --- PHASE 1: SET ---
    let mut handles = Vec::new();
    for chunk in words.chunks(CHUNCK_SIZE) {
        let chunk_data = chunk.to_vec();
        let c = client.clone();

        let handle = monoio::spawn(async move {
            for word in chunk_data {
                // Pass owned String for both key and value
                c.set(word.clone(), word).await.unwrap();
            }
        });
        handles.push(handle);
    }

    for h in handles {
        h.await;
    }
    println!("SET phase: {:.3}s", now.elapsed().as_secs_f32());

    // --- PHASE 2: GET ---
    let mut handles = Vec::new();
    for chunk in words.chunks(CHUNCK_SIZE) {
        let chunk_data = chunk.to_vec();
        let c = client.clone();

        let handle = monoio::spawn(async move {
            for word in chunk_data {
                let r_word = c.get(&word).await.unwrap().unwrap();
                if word.as_bytes() != r_word {
                    assert!(false, "invalide response");
                }
            }
        });
        handles.push(handle);
    }

    for h in handles {
        h.await;
    }

    let elapsed = now.elapsed();
    let throughput = (total_words * 2) as f32 / elapsed.as_secs_f32();

    println!("---------------------------------------");
    println!("Total: {:.3}s", elapsed.as_secs_f32());
    println!("Throughput: {:.0} req/s", throughput);
    println!("---------------------------------------");

    let mut del_handles = Vec::new();
    for chunk in words.chunks(CHUNCK_SIZE) {
        // On peut prendre des chunks plus gros pour le cleanup
        let chunk_data = chunk.to_vec();
        let c = client.clone();

        let handle = monoio::spawn(async move {
            for word in chunk_data {
                let _ = c.del(&word).await;
            }
        });
        del_handles.push(handle);
    }

    for h in del_handles {
        h.await;
    }
}
