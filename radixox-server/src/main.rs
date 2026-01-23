mod oxidart;

use bytes::Bytes;

use radixox_common::protocol::{GetAction, SetAction};

use crate::oxidart::arena_oxid_art::OxidArtArena;

type IOResult<T> = std::io::Result<T>;

#[monoio::main]
async fn main() -> IOResult<()> {
    //let listener = TcpListener::bind("127.0.0.1:8379")?;
    let mut oxid_art = OxidArtArena::new();
    let key = Bytes::from_static(b"user:joshua");
    let val = Bytes::from_static(b"BOUCHAT");
    let set_action = SetAction::new(key.clone(), val).expect("get action invalid");

    oxid_art.set(set_action);
    let get_action = GetAction::new(key).expect("get action invalid");
    if let Some(bytes) = oxid_art.get(get_action) {
        println!(
            "Voici la réponse du get: {}",
            str::from_utf8(&bytes).unwrap_or("Invalide bytes")
        )
    } else {
        println!("Oh dommage ya rien Joshua est triste")
    }
    dbg!(oxid_art);

    Ok(())
}
