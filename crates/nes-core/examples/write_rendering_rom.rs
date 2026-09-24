//! 把 `test_support::rendering_rom()` 寫成 `.nes` 檔，方便用 `nes-test screenshot`
//! 目視檢查合成測試 ROM 的畫面。
//!
//! ```text
//! cargo run -p nes-core --features testing --example write_rendering_rom -- roms/shots/rendering.nes
//! ```

fn main() {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "rendering.nes".to_string());
    std::fs::write(&path, nes_core::test_support::rendering_rom()).expect("寫入 ROM 失敗");
    println!("已寫入 {path}");
}
