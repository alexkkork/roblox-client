//! CLI wrapper around the same bounded decoder linked into the Rust client.
use animation_codec::{decode, MAX_INPUT};
use std::io::{self, Read, Write};

fn main() {
    let mut data = Vec::new();
    let result = io::stdin()
        .take((MAX_INPUT + 1) as u64)
        .read_to_end(&mut data)
        .map_err(|_| "input_read_failed")
        .and_then(|_| decode(&data));
    match result {
        Ok(value) => {
            let bytes = serde_json::to_vec(&value).unwrap();
            if bytes.len() > 8 * 1024 * 1024 {
                eprintln!("output_limit");
                std::process::exit(2);
            }
            io::stdout().write_all(&bytes).unwrap();
        }
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(2);
        }
    }
}
