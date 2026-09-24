//! 把 UxROM / CNROM 的合成測試 ROM 寫成 `.nes`，讓 `nes-test blargg` 直接判定。
//!
//! ```text
//! cargo run -p nes-core --features testing --example write_mapper_test_roms -- roms/shots
//! cargo run --release -p nes-test -- blargg roms/shots/uxrom_test.nes
//! ```

use nes_core::test_support::{cnrom_test_rom, uxrom_test_rom};

fn main() {
    let dir = std::env::args().nth(1).unwrap_or_else(|| ".".to_string());
    for (name, rom) in [
        ("uxrom_test.nes", uxrom_test_rom()),
        ("cnrom_test.nes", cnrom_test_rom()),
    ] {
        let path = format!("{dir}/{name}");
        std::fs::write(&path, rom).expect("寫入 ROM 失敗");
        println!("已寫入 {path}");
    }
}
