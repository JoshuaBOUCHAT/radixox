mod oxidart;

use std::time::Instant;

use bytes::Bytes;

use radixox_common::protocol::{DelAction, GetAction, SetAction};
use rand::seq::SliceRandom;

use crate::oxidart::arena_oxid_art::OxidArtArena;

type IOResult<T> = std::io::Result<T>;

#[monoio::main]
async fn main() -> IOResult<()> {
    //let listener = TcpListener::bind("127.0.0.1:8379")?;
    /*let mut oxid_art = OxidArtArena::new();
    set(&mut oxid_art, "user:1", "Foo");
    set(&mut oxid_art, "user:2", "Bar");
    println!("user 1: {}", get(&oxid_art, "user:1").unwrap());
    println!("user 2: {}", get(&oxid_art, "user:2").unwrap());
    del(&mut oxid_art, "user:1");
    println!(
        "user 1: {}",
        get(&oxid_art, "user:1").unwrap_or(String::from("Well destoyed"))
    );
    println!("user 2: {}", get(&oxid_art, "user:2").unwrap());*/
    test_speed();
    Ok(())
}
fn test_speed() {
    let mut oxid_art = OxidArtArena::new();
    let mut words: Vec<&str> = include_str!("../list.txt").lines().collect();
    words.shuffle(&mut rand::rng());
    let now = Instant::now();
    for &line in &words {
        let key = Bytes::from(line);
        let action = SetAction::new(key.clone(), key).expect("invalid set");
        oxid_art.set(action);
    }
    println!("Le temps total a été de: {}s", now.elapsed().as_secs_f32());
    let now = Instant::now();
    let mut dummy_count = 0;
    for line in words {
        let key = Bytes::from(line);
        let action = GetAction::new(key.clone()).expect("invalid set");
        dummy_count += oxid_art
            .get(action)
            .expect("An item has not been inserted")
            .len();
    }
    println!(
        "Dummy:{dummy_count} time to re-get all the words: {}s",
        now.elapsed().as_secs_f32()
    )
}

// === Test helpers ===

fn set(art: &mut OxidArtArena, key: &str, val: &str) {
    let action = SetAction::new(Bytes::from(key.to_owned()), Bytes::from(val.to_owned()))
        .expect("invalid set");
    art.set(action);
}

fn get(art: &OxidArtArena, key: &str) -> Option<String> {
    let action = GetAction::new(Bytes::from(key.to_owned())).expect("invalid get");
    art.get(action)
        .map(|b| String::from_utf8_lossy(&b).to_string())
}

fn del(art: &mut OxidArtArena, key: &str) -> Option<String> {
    let action = DelAction::new(Bytes::from(key.to_owned())).expect("invalid del");
    art.delete(action)
        .map(|b| String::from_utf8_lossy(&b).to_string())
}
