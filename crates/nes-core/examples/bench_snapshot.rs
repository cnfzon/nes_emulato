//! rollback 快照成本的量測（Phase 4c，`docs/architecture.md` §20.1）。跑法：
//!
//! ```text
//! cargo run --release -p nes-core --features testing --example bench_snapshot
//! cargo run --release -p nes-core --features testing --example bench_snapshot -- Spacegulls-1.1.nes
//! ```
//!
//! 比較「存一份快照（或還原一份）」的每次耗時：
//!
//! 1. **完整 clone**：`nes.clone()`——配置並複製全部（含 framebuffer 約 240 KB、音訊輸出管線、
//!    PRG-ROM／CHR-ROM）。最直接、最多餘。
//! 2. **完整 `clone_from`**：`slot.clone_from(&nes)`。`Nes` 的 `Clone` 是 derive 的，預設的
//!    `clone_from` 就是「clone 再取代」，沒有重複利用配置。
//! 3. **只複製模擬狀態**（`Nes::copy_state_from`，本階段採用）：沿用槽裡既有的 `Vec` 容量、
//!    不複製輸出與靜態 ROM。
//! 4. **模擬狀態 ＋ 把 framebuffer 也複製進槽**：想像「快照包含輸出，但重複利用配置」的最佳情況——
//!    用 `copy_state_from` 再加上 240 KB 的 `copy_from_slice`。
//!
//! 每個情境跑固定次數，印出平均每次的耗時。ROM 的狀態先跑 120 幀，讓 RAM／OAM／PPU 是「遊戲中」的內容。

use std::hint::black_box;
use std::time::Instant;

use nes_core::Nes;
use nes_core::test_support::{input_probe_rom, rendering_rom};

const ITERATIONS: u32 = 20_000;

fn per_call(name: &str, mut f: impl FnMut()) {
    // 暖機
    for _ in 0..200 {
        f();
    }
    let start = Instant::now();
    for _ in 0..ITERATIONS {
        f();
    }
    let ns = start.elapsed().as_nanos() as f64 / f64::from(ITERATIONS);
    println!("  {name:<58} {:>9.2} µs", ns / 1000.0);
}

fn bench_rom(name: &str, rom: &[u8]) {
    println!("[{name}]");
    let mut nes = Nes::from_rom(rom).expect("valid rom");
    for _ in 0..120 {
        nes.run_frame([nes_core::Buttons::A, nes_core::Buttons::empty()]);
    }
    let fb_len = nes.frame_buffer().as_bytes().len();
    println!(
        "  framebuffer {fb_len} 位元組；存檔（postcard）{} 位元組",
        nes.save_state().len()
    );

    let mut slot = nes.clone();
    per_call("存：完整 clone（nes.clone()）", || {
        black_box(nes.clone());
    });
    per_call("存：完整 clone_from（slot.clone_from(&nes)）", || {
        slot.clone_from(black_box(&nes));
    });
    per_call(
        "存：只複製模擬狀態（slot.copy_state_from(&nes)）★採用",
        || {
            black_box(slot.copy_state_from(black_box(&nes)));
        },
    );
    let mut fb = vec![0u8; fb_len];
    per_call(
        "存：模擬狀態 ＋ framebuffer 也複製（最佳情況）",
        || {
            slot.copy_state_from(black_box(&nes));
            fb.copy_from_slice(black_box(nes.frame_buffer().as_bytes()));
        },
    );
    per_call("存：序列化（save_state，僅供對照）", || {
        black_box(nes.save_state());
    });

    let mut target = nes.clone();
    per_call(
        "還原：只複製模擬狀態（nes.copy_state_from(&slot)）★採用",
        || {
            black_box(target.copy_state_from(black_box(&slot)));
        },
    );
    let bytes = nes.save_state();
    per_call(
        "還原：load_state（反序列化，僅供對照）",
        || {
            target.load_state(black_box(&bytes)).unwrap();
        },
    );
    println!();
}

fn main() {
    println!("每次耗時（{ITERATIONS} 次平均，release）\n");
    bench_rom("input_probe_rom（NROM、無 CHR-RAM）", &input_probe_rom());
    bench_rom("rendering_rom（NROM、開啟渲染）", &rendering_rom());
    if let Some(path) = std::env::args().nth(1) {
        match std::fs::read(&path) {
            Ok(rom) => bench_rom(&format!("命令列 ROM {path}"), &rom),
            Err(e) => eprintln!("讀取 {path} 失敗: {e}"),
        }
    }
}
