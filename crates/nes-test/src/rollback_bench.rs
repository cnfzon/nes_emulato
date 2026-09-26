//! `rollback-bench`：量測 rollback 的重跑成本（Phase 4c 的效能驗收）。
//!
//! 最壞情況：**每個幀節拍都**還原到 K 幀之前，重跑 K 幀（輸出關閉，每幀存快照並算行為指紋），再推進新的一幀
//! （輸出開啟，取走音訊、複製 framebuffer 給 UI，跟 emu 執行緒做的事一樣）。這段的總耗時必須在一幀的時間預算
//! （1 ÷ 60.0988 ≈ 16.64 ms）之內，否則 emu 執行緒跟不上 60 fps。
//!
//! 同時量沒有 rollback 的一般幀（新的一幀＋存快照）當作對照。

use std::path::Path;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use nes_core::{Buttons, FrameInput, Nes};
use nes_net::rollback::{NTSC_FPS, Request};
use nes_net::snapshot::{SnapshotRing, execute};

fn input(i: u32) -> FrameInput {
    FrameInput::new(
        Buttons::from_bits_truncate((i / 2).wrapping_mul(37) as u8),
        Buttons::from_bits_truncate((i / 3).wrapping_mul(91) as u8),
    )
}

/// 超過幀預算的這個比例的節拍，會被重跑 10 次做「雜訊 vs 真實成本」的判別。
const SLOW_FRACTION: f64 = 0.85;

struct Summary {
    p50: Duration,
    p90: Duration,
    p99: Duration,
    p999: Duration,
    max: Duration,
    avg: Duration,
}

fn summarize(mut samples: Vec<Duration>) -> Summary {
    samples.sort();
    let total: Duration = samples.iter().sum();
    let n = samples.len().max(1);
    // 第 q 個百分位（最近排名法）。
    let at = |q: f64| samples[(((n as f64) * q).ceil() as usize).clamp(1, n) - 1];
    Summary {
        p50: at(0.50),
        p90: at(0.90),
        p99: at(0.99),
        p999: at(0.999),
        max: samples.last().copied().unwrap_or_default(),
        avg: total / n as u32,
    }
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

pub fn run(rom_path: &Path, depth: u32, iterations: u32) -> ExitCode {
    let rom = match std::fs::read(rom_path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("讀取 ROM 檔案失敗: {e}");
            return ExitCode::from(2);
        }
    };
    let mut nes = match Nes::from_rom(&rom) {
        Ok(n) => n,
        Err(e) => {
            eprintln!("無法載入 ROM: {e}");
            return ExitCode::from(2);
        }
    };
    let depth = depth.clamp(1, nes_net::rollback::MAX_WINDOW);
    let mut ring = SnapshotRing::new(depth, &nes);
    let mut sink = 0u64; // 防止最佳化掉指紋
    let mut audio = Vec::new();
    let mut cur = 0u32;
    ring.save(0, &nes);

    // 暖機：跑 600 幀（10 秒）讓遊戲進入「有畫面、有聲音」的狀態，同時把環形緩衝填滿。
    for _ in 0..600 {
        nes.set_output_enabled(true);
        nes.run_frame(input(cur));
        cur += 1;
        ring.save(cur, &nes);
        audio.clear();
        nes.drain_audio(&mut audio);
    }

    // 對照：沒有 rollback 的一般幀（新的一幀 ＋ 存快照 ＋ 取走音訊 ＋ 複製 framebuffer）。
    let mut normal = Vec::with_capacity(iterations as usize);
    for _ in 0..iterations {
        let started = Instant::now();
        nes.set_output_enabled(true);
        nes.run_frame(input(cur));
        cur += 1;
        ring.save(cur, &nes);
        sink ^= nes.behavior_fingerprint();
        audio.clear();
        nes.drain_audio(&mut audio);
        sink ^= nes.frame_buffer().clone().hash64() & 1;
        normal.push(started.elapsed());
    }

    // 最壞情況：每個節拍都還原到 K 幀之前、重跑 K 幀、再推進新的一幀。
    let mut worst = Vec::with_capacity(iterations as usize);
    let mut slow: Vec<(u32, Duration, Duration)> = Vec::new();
    for _ in 0..iterations {
        // 規劃器的不變式：只有 `cur − C < K` 才會推進新的一幀，所以一個節拍最壞是「重跑 K−1 幀＋新的一幀」
        // （或視窗已滿時重跑 K 幀、沒有新的一幀），合計 K 幀。
        let from = cur - (depth - 1);
        let mut requests = Vec::with_capacity(depth as usize * 2 + 4);
        requests.push(Request::LoadState { frame: from });
        for f in from..cur {
            requests.push(Request::AdvanceFrame {
                frame: f,
                input: input(f ^ 1), // 與先前不同的輸入（預測失誤）
                output_enabled: false,
            });
            requests.push(Request::SaveState { frame: f + 1 });
        }
        requests.push(Request::AdvanceFrame {
            frame: cur,
            input: input(cur),
            output_enabled: true,
        });
        requests.push(Request::SaveState { frame: cur + 1 });

        let started = Instant::now();
        let result = execute(&requests, &mut nes, &mut ring, |_, fp| sink ^= fp);
        audio.clear();
        nes.drain_audio(&mut audio);
        sink ^= nes.frame_buffer().clone().hash64() & 1;
        let elapsed = started.elapsed();
        if let Err(e) = result {
            eprintln!("內部錯誤：{e}");
            return ExitCode::FAILURE;
        }
        // 偶發的慢節拍：**用同一份請求清單立刻重跑 10 次**（`LoadState` 會回到同一個狀態，所以每次做的是完全一樣的事）。
        // 重跑的最小值 ≈ 這個節拍「本身的成本」；第一次遠大於它，代表多出來的是作業系統雜訊（排程、快取被別的行程
        // 洗掉）；兩者相近，代表是這一段遊戲內容本來就重（不是雜訊）。
        if elapsed.as_secs_f64() > SLOW_FRACTION * (1.0 / NTSC_FPS) {
            let repeat_min = (0..10)
                .map(|_| {
                    let t = Instant::now();
                    let _ = execute(&requests, &mut nes, &mut ring, |_, fp| sink ^= fp);
                    audio.clear();
                    nes.drain_audio(&mut audio);
                    sink ^= nes.frame_buffer().clone().hash64() & 1;
                    t.elapsed()
                })
                .min()
                .unwrap_or_default();
            slow.push((cur, elapsed, repeat_min));
        }
        cur += 1;
        worst.push(elapsed);
    }

    let budget = Duration::from_secs_f64(1.0 / NTSC_FPS);
    let normal = summarize(normal);
    let worst = summarize(worst);
    println!(
        "rollback-bench：ROM {}｜重跑深度 K={depth}｜{iterations} 次｜幀預算 {:.2} ms（{NTSC_FPS} Hz）",
        rom_path.display(),
        ms(budget)
    );
    println!("| 情境 | 平均 | p50 | p90 | p99 | p99.9 | 最大 | p99 ÷ 預算 | 最大 ÷ 預算 |");
    println!("|---|---|---|---|---|---|---|---|---|");
    for (name, s) in [
        (
            "一般幀（不 rollback：新的一幀＋快照＋指紋；作業系統雜訊的基準）".to_string(),
            &normal,
        ),
        (
            format!(
                "**最壞：還原＋重跑 {} 幀＋新的一幀（共 {depth} 幀）**",
                depth - 1
            ),
            &worst,
        ),
    ] {
        println!(
            "| {name} | {:.3} | {:.3} | {:.3} | {:.3} | {:.3} | {:.3} | {:.1}% | {:.1}% |",
            ms(s.avg),
            ms(s.p50),
            ms(s.p90),
            ms(s.p99),
            ms(s.p999),
            ms(s.max),
            100.0 * ms(s.p99) / ms(budget),
            100.0 * ms(s.max) / ms(budget)
        );
    }
    println!(
        "（單位 ms；每個情境 {iterations} 個節拍；幀預算 {:.2} ms）",
        ms(budget)
    );
    println!("（防最佳化雜湊 {sink:#x}）");
    if !slow.is_empty() {
        println!(
            "慢節拍（> {:.0}% 預算）共 {} 個；同一份請求立刻重跑 10 次取最小值：",
            SLOW_FRACTION * 100.0,
            slow.len()
        );
        println!("| 節拍（幀號） | 第一次（ms） | 重跑最小值（ms） | 第一次 ÷ 重跑 |");
        println!("|---|---|---|---|");
        for (frame, first, again) in slow.iter().take(12) {
            println!(
                "| {frame} | {:.3} | {:.3} | {:.2}× |",
                ms(*first),
                ms(*again),
                ms(*first) / ms(*again).max(1e-9)
            );
        }
    }
    // 目標：p99 在預算內。最大值超出時，要看重跑最小值（真實成本）是否也超出。
    println!(
        "結論：最壞情況 p99 {:.3} ms {} 幀預算 {:.2} ms；最大 {:.3} ms {} 預算（一般幀最大 {:.3} ms、p99.9 {:.3} ms）。",
        ms(worst.p99),
        if worst.p99 <= budget {
            "在"
        } else {
            "**超出**"
        },
        ms(budget),
        ms(worst.max),
        if worst.max <= budget { "在" } else { "超出" },
        ms(normal.max),
        ms(normal.p999),
    );
    if worst.p99 <= budget {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
