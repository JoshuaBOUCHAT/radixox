//! Chargement par le chemin du serveur en pipeline : fenêtres de `W` clés,
//! `amac_descend` puis `set_cond_hinted` dans une fenêtre ouverte.
//!   amac_load <n> [uuid|ycsb] [W]
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use radixox_lib::shared_byte::SharedByte;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let n: usize = a.get(1).and_then(|s| s.parse().ok()).unwrap_or(200_000);
    let shape = a.get(2).map_or("uuid", String::as_str).to_string();
    let w: usize = a.get(3).and_then(|s| s.parse().ok()).unwrap_or(64);
    let mut rng = StdRng::seed_from_u64(7);
    let keys: Vec<Vec<u8>> = (0..n)
        .map(|_| match shape.as_str() {
            "ycsb" => format!("user{}", rng.r#gen::<u64>() >> 1).into_bytes(),
            _ => {
                let x: u128 = rng.r#gen();
                let h = format!("{x:032x}");
                format!("{}-{}-{}-{}-{}", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..]).into_bytes()
            }
        })
        .collect();
    let val = SharedByte::from_slice([b'x'; 100]);
    let mut t = oxidart::OxidArt::new();
    let mut hints = vec![oxidart::Hint::NONE; w];
    for chunk in keys.chunks(w) {
        let refs: Vec<&[u8]> = chunk.iter().map(|k| k.as_slice()).collect();
        let h = &mut hints[..refs.len()];
        h.fill(oxidart::Hint::NONE);
        t.amac_descend(&refs, 16, h);
        t.begin_window();
        for (k, &hint) in refs.iter().zip(h.iter()) {
            t.set_cond_hinted(k, None, oxidart::Value::String(val.clone()), None, hint);
        }
        t.end_window();
    }
    println!("{} clés", t.len());
}
