//! Release signing for the self-updater.
//!   cargo run --release --example sign -- keygen          prints a new seed (secret) and the public key
//!   LITTY_SIGNING_KEY=<seed hex> cargo run --release --example sign -- FILE...   writes FILE.sig
use ed25519_compact::{KeyPair, Seed};

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len() / 2).map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).expect("hex")).collect()
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("keygen") {
        let mut seed = [0u8; 32];
        std::fs::File::open("/dev/urandom").and_then(|mut f| std::io::Read::read_exact(&mut f, &mut seed)).expect("random seed");
        let kp = KeyPair::from_seed(Seed::new(seed));
        println!("seed (keep secret): {}", hex(&seed));
        println!("public key: {}", hex(&kp.pk[..]));
        println!("rust: [{}]", kp.pk.iter().map(|b| format!("0x{b:02x}")).collect::<Vec<_>>().join(", "));
        return;
    }
    let seed: [u8; 32] = unhex(std::env::var("LITTY_SIGNING_KEY").expect("LITTY_SIGNING_KEY").trim()).try_into().expect("32-byte seed");
    let kp = KeyPair::from_seed(Seed::new(seed));
    for file in args {
        let sig = kp.sk.sign(std::fs::read(&file).expect("read file"), None);
        std::fs::write(format!("{file}.sig"), &sig[..]).expect("write signature");
        println!("signed {file}");
    }
}
