//! `rollback-profile`（`profile` feature）：重跑一幀（輸出關閉）的時間分配。
//!
//! # 量測方法
//!
//! `nes-core` 不讀時間，而且一幀有數萬次元件呼叫，在每次呼叫前後插入計時點的開銷（每次約 20–30 ns）會比
//! 被量測的東西還大。所以用**元件隔離**：對遊戲中每個取樣幀的狀態 S（幀邊界），用 `copy_state_from` 複製到暫時實例，
//! 分別量：
//!
//! - **完整一幀**：`run_frame`（輸出關閉）——這是要分配的總量；
//! - **PPU（含渲染）**：只推進 PPU 一幀份的 dot（29781 個 CPU cycle × 3），以每次 3 個 CPU cycle 為單位；
//! - **PPU（渲染關閉）**：同上但 `mask = 0`：只剩逐 dot 的時序迴圈；兩者相減 ＝ 掃描線渲染（背景＋精靈）的成本；
//! - **APU**：只推進 APU 一幀份的 cycle，同樣每次 3 個 cycle；
//! - **mapper**：29781 次 PRG 讀取加 29781 次 CHR 讀取——這是**上限**（實際的 PRG 讀取約是每個 CPU cycle 一次以內，
//!   CHR 讀取只在渲染時發生）；
//! - **其餘（CPU＋匯流排＋dispatch）** ＝ 完整一幀 − PPU − APU（mapper 讀取包含在 PPU 與其餘裡，不另外扣）。
//!
//! # 誤差來源（如實）
//!
//! 1. 元件隔離時 PPU／APU 的暫存器不變（真實執行中 CPU 會在幀中途寫暫存器），所以渲染成本是「幀邊界時的設定」的近似；
//! 2. 每次 3 個 cycle 的呼叫粒度是近似（真實是每條指令 2–7 個 cycle，而且每條指令分兩次推進）；
//! 3. 快取狀態：隔離跑時只有單一元件的資料在快取，完整一幀是交錯的，所以**元件相加通常略小於完整一幀**——差額
//!    放進「其餘」，會讓「其餘」偏大；
//! 4. 每項取 3 次重複的最小值以壓低作業系統雜訊，會略微偏低。
//!
//! 「其餘」是差額，所以三者相加恆等於完整一幀；能檢驗方法好壞的是「幀間標準差」欄與 `copy_state_from` 的量測開銷（0.2 µs，可忽略）。

use std::hint::black_box;
use std::path::Path;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use nes_core::{Buttons, FrameInput, Nes};

const CYCLES_PER_FRAME: u32 = 29_781;
const CHUNK: u32 = 3;
const REPS: usize = 3;

fn input(i: u32) -> FrameInput {
    FrameInput::new(
        Buttons::from_bits_truncate((i / 2).wrapping_mul(37) as u8),
        Buttons::from_bits_truncate((i / 3).wrapping_mul(91) as u8),
    )
}

fn min_of(mut f: impl FnMut()) -> Duration {
    (0..REPS)
        .map(|_| {
            let t = Instant::now();
            f();
            t.elapsed()
        })
        .min()
        .unwrap_or_default()
}

pub fn run(rom_path: &Path, frames: u32) -> ExitCode {
    let rom = match std::fs::read(rom_path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("讀取 ROM 檔案失敗: {e}");
            return ExitCode::from(2);
        }
    };
    let Ok(mut nes) = Nes::from_rom(&rom) else {
        eprintln!("無法載入 ROM");
        return ExitCode::from(2);
    };
    let mut work = nes.clone();
    work.set_output_enabled(false);
    let mut n = 0u32;
    for _ in 0..600 {
        nes.run_frame(input(n));
        n += 1;
    }

    let mut sums = [Duration::ZERO; 6];
    let mut sq = [0f64; 6];
    let mut sink = 0u8;
    for _ in 0..frames {
        nes.run_frame(input(n));
        let next = input(n + 1);
        n += 1;
        let mut t = [Duration::ZERO; 6];
        t[0] = min_of(|| {
            work.copy_state_from(&nes);
            black_box(work.run_frame(next));
        });
        t[1] = min_of(|| {
            work.copy_state_from(&nes);
            work.profile_ppu(CYCLES_PER_FRAME, CHUNK, true);
        });
        t[2] = min_of(|| {
            work.copy_state_from(&nes);
            work.profile_ppu(CYCLES_PER_FRAME, CHUNK, false);
        });
        t[3] = min_of(|| {
            work.copy_state_from(&nes);
            work.profile_apu(CYCLES_PER_FRAME, CHUNK);
        });
        t[4] = min_of(|| {
            sink ^= work.profile_mapper(CYCLES_PER_FRAME);
        });
        t[5] = min_of(|| {
            work.copy_state_from(&nes);
        });
        for i in 0..6 {
            sums[i] += t[i];
            sq[i] += t[i].as_secs_f64().powi(2);
        }
    }
    black_box(sink);
    let f = f64::from(frames);
    let us = |i: usize| sums[i].as_secs_f64() * 1e6 / f;
    let sd = |i: usize| {
        let m = sums[i].as_secs_f64() / f;
        ((sq[i] / f - m * m).max(0.0)).sqrt() * 1e6
    };
    let (total, ppu, ppu_idle, apu, mapper, copy) = (us(0), us(1), us(2), us(3), us(4), us(5));
    let render = (ppu - ppu_idle).max(0.0);
    let rest = (total - ppu - apu).max(0.0);
    let pct = |x: f64| 100.0 * x / total;
    println!(
        "rollback-profile：ROM {}｜取樣 {frames} 個遊戲中的幀（每項取 {REPS} 次最小值）｜每幀 {CYCLES_PER_FRAME} 個 CPU cycle",
        rom_path.display()
    );
    println!("| 元件 | 平均每幀（µs） | 占完整一幀 | 幀間標準差（µs） |");
    println!("|---|---|---|---|");
    println!(
        "| **完整一幀（輸出關閉）** | {total:.1} | 100% | {:.1} |",
        sd(0)
    );
    println!(
        "| PPU（逐 dot 時序迴圈＋掃描線渲染） | {ppu:.1} | {:.1}% | {:.1} |",
        pct(ppu),
        sd(1)
    );
    println!(
        "| 　├ 逐 dot 時序迴圈（渲染關閉） | {ppu_idle:.1} | {:.1}% | {:.1} |",
        pct(ppu_idle),
        sd(2)
    );
    println!(
        "| 　└ 掃描線渲染（背景＋精靈評估） | {render:.1} | {:.1}% | — |",
        pct(render)
    );
    println!("| APU | {apu:.1} | {:.1}% | {:.1} |", pct(apu), sd(3));
    println!(
        "| 其餘（CPU＋匯流排＋dispatch；差額） | {rest:.1} | {:.1}% | — |",
        pct(rest)
    );
    println!(
        "| （參考）mapper 讀取上限：{CYCLES_PER_FRAME} 次 PRG＋{CYCLES_PER_FRAME} 次 CHR | {mapper:.1} | {:.1}% | {:.1} |",
        pct(mapper),
        sd(4)
    );
    println!(
        "| （參考）量測本身：`copy_state_from` | {copy:.2} | {:.2}% | {:.2} |",
        pct(copy),
        sd(5)
    );
    ExitCode::SUCCESS
}
