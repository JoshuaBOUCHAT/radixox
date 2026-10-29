//! Pic mémoire du rechargement d'un gros ZSet (index YCSB `_indices`).
//!   zset_reload save <n> <fichier>   ZSet de n membres `user<19 chiffres>`, sauvé
//!   zset_reload load <fichier>       rechargé ; pic et ventilation du résident
//!   zset_reload loadfile <dump.rxd>  dump du serveur, lu en flux comme au démarrage
use oxidart::OxidArt;
use radixox_lib::shared_byte::SharedByte;

fn status(k: &str) -> u64 {
    let s = std::fs::read_to_string("/proc/self/status").unwrap();
    s.lines().find(|l| l.starts_with(k)).and_then(|l| l.split_whitespace().nth(1)).unwrap().parse::<u64>().unwrap() / 1024
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    match a[1].as_str() {
        "save" => {
            let n: u64 = a[2].parse().unwrap();
            let mut t = OxidArt::new();
            let key = SharedByte::from_slice(b"_indices");
            let mut batch: Vec<(f64, Vec<u8>)> = Vec::with_capacity(1000);
            for i in 0..n {
                let id = i.wrapping_mul(0x9e37_79b9_7f4a_7c15) >> 1;
                batch.push(((id % 1_000_000_007) as f64, format!("user{id:019}").into_bytes()));
                if batch.len() == 1000 || i + 1 == n {
                    t.cmd_zadd(key.clone(), &batch, None).unwrap();
                    batch.clear();
                }
            }
            let mut out = Vec::new();
            let mut cur = t.save_start(&mut out);
            while !t.save_step(&mut cur, &mut out, 4096) {}
            drop(cur);
            t.finish_save();
            std::fs::write(&a[3], &out).unwrap();
            println!("dump {} Mo", out.len() >> 20);
        }
        _ => {
            let base = status("VmRSS:");
            let mut t = OxidArt::new();
            let t0 = std::time::Instant::now();
            if a[1] == "loadfile" {
                // dump du serveur (pied CRC), lu en flux comme au démarrage
                t.load_file(std::fs::File::open(&a[2]).unwrap()).unwrap();
            } else {
                // dump sans pied : lu d'avance, compté dans la base
                let dump = std::fs::read(&a[2]).unwrap();
                t.load_dump(&dump).unwrap();
            }
            let dt = t0.elapsed();
            let (hwm, rss) = (status("VmHWM:"), status("VmRSS:"));
            println!("rechargé en {:.2} s : pic +{} Mo, RSS +{} Mo (base {} Mo, dump compris)", dt.as_secs_f64(), hwm - base, rss - base, base);
            for (name, (touched, live, res)) in t.slab_mem() {
                println!("  slab {name:<12} résident {:>5} Mo  touché {:>5} Mo  vivant {:>5} Mo", res >> 20, touched >> 20, live >> 20);
            }
            println!(
                "  pool {} Mo  large {} Mo  poignées {} Mo",
                radixox_lib::pool::resident() >> 20,
                radixox_lib::large::resident() >> 20,
                radixox_lib::handle::table_resident() >> 20
            );
        }
    }
}
