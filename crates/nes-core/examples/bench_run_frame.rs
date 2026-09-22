//! `Nes::run_frame` 的簡易效能量測。跑法：
//!
//! ```text
//! cargo run --release -p nes-core --example bench_run_frame
//! ```
//!
//! 兩種情境：
//! 1. 全部填 `NOP`（`$EA`）：CPU 分支預測友善、記憶體存取最少的上限情境。
//! 2. 一個具代表性的小迴圈（zero-page 讀取、abs,X 寫入、INX/CPX/BNE、外層
//!    JMP）：比較接近真實遊戲程式碼會用到的指令組合。

use std::time::Instant;

use nes_core::{Buttons, Nes};

const WARMUP_FRAMES: u32 = 60;
const BENCH_FRAMES: u32 = 600;

fn make_rom(prg: &[u8; 0x8000]) -> Vec<u8> {
    let mut rom = vec![0u8; 16];
    rom[0..4].copy_from_slice(b"NES\x1A");
    rom[4] = 2; // PRG banks (32KB，reset vector 落在最後一個 bank)
    rom[5] = 1; // CHR banks
    rom.extend_from_slice(prg);
    rom.extend(vec![0u8; 8192]);
    rom
}

fn bench(name: &str, prg: &[u8; 0x8000]) {
    let rom = make_rom(prg);
    let mut nes = Nes::from_rom(&rom).expect("valid rom");
    let input = [Buttons::empty(), Buttons::empty()];

    for _ in 0..WARMUP_FRAMES {
        nes.run_frame(input);
    }

    let start = Instant::now();
    for _ in 0..BENCH_FRAMES {
        nes.run_frame(input);
    }
    let elapsed = start.elapsed();

    println!("[{name}]");
    println!("  {BENCH_FRAMES} frames in {elapsed:?}");
    println!("  平均每幀: {:?}", elapsed / BENCH_FRAMES);
    println!(
        "  換算 fps 上限（純 CPU 工作量，不含 GUI/VSync）: {:.0}",
        BENCH_FRAMES as f64 / elapsed.as_secs_f64()
    );
}

fn main() {
    let mut all_nop = [0xEAu8; 0x8000];
    all_nop[0x7FFC] = 0x00;
    all_nop[0x7FFD] = 0x80;
    bench("全部 NOP", &all_nop);

    let mut loop_prg = [0u8; 0x8000];
    let code: [u8; 15] = [
        0xA2, 0x00, // LDX #$00
        0xB5, 0x00, // LDA $00,X
        0x9D, 0x00, 0x02, // STA $0200,X
        0xE8, // INX
        0xE0, 0x00, // CPX #$00
        0xD0, 0xF6, // BNE $8002
        0x4C, 0x00, 0x80, // JMP $8000
    ];
    loop_prg[..code.len()].copy_from_slice(&code);
    loop_prg[0x7FFC] = 0x00;
    loop_prg[0x7FFD] = 0x80;
    bench("LDA/STA/INX/CPX/BNE 迴圈", &loop_prg);
}
