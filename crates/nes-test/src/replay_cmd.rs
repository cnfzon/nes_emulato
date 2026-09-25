//! `replay` 系列子命令與 `diff-state`、`save-state` 的實作（檔案 I/O 在這裡，`nes-core` 只處理位元組）。
//!
//! 結束碼：`replay verify` 全部檢查點相符 → 0，任何不符／拒絕／讀檔失敗 → 非 0；
//! `diff-state` 兩份存檔完全相同 → 0、有差異 → 1、無法讀取或解碼 → 2（與 `diff` 慣例相同）。

use std::path::Path;
use std::process::ExitCode;
use std::time::Instant;

use nes_core::replay::{Replay, ReplayError, ReplayPlayer, ReplayRecorder};
use nes_core::{Buttons, FrameInput, Nes};

use crate::diff_state;

/// NES 的畫面更新率（NTSC），只用來把幀數換算成秒數顯示。
const NTSC_FPS: f64 = 60.0988;

fn read_rom(path: &Path) -> Result<(Vec<u8>, Nes), String> {
    let bytes = std::fs::read(path).map_err(|e| format!("讀取 ROM 檔案失敗: {e}"))?;
    let nes = Nes::from_rom(&bytes).map_err(|e| format!("無法載入 ROM: {e}"))?;
    Ok((bytes, nes))
}

fn read_replay(path: &Path) -> Result<Replay, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("讀取 replay 檔案失敗: {e}"))?;
    Replay::decode(&bytes).map_err(|e| format!("replay 檔案無效: {e}"))
}

fn fail(message: impl std::fmt::Display) -> ExitCode {
    eprintln!("{message}");
    ExitCode::FAILURE
}

/// `replay info <replay>`：顯示 header、總幀數、檢查點數量。
pub fn info(path: &Path) -> ExitCode {
    let replay = match read_replay(path) {
        Ok(r) => r,
        Err(e) => return fail(e),
    };
    let current = nes_core::CORE_BEHAVIOR_VERSION;
    println!("檔案: {}", path.display());
    println!(
        "replay 格式版本: {}",
        nes_core::replay::REPLAY_FORMAT_VERSION
    );
    println!(
        "錄製時的核心行為版本: {}（目前的核心: {current}，{}）",
        replay.core_behavior_version,
        if replay.core_behavior_version == current {
            "相符"
        } else {
            "不符：拒絕播放"
        }
    );
    println!(
        "rom_id: {}（前 16 字元 {}）",
        replay.rom_id,
        replay.rom_id.short()
    );
    println!(
        "總幀數: {}（約 {:.1} 秒）",
        replay.total_frames,
        f64::from(replay.total_frames) / NTSC_FPS
    );
    println!(
        "輸入串流: {} 段（RLE，平均每段 {:.1} 幀）",
        replay.runs.len(),
        f64::from(replay.total_frames) / replay.runs.len().max(1) as f64
    );
    let resets: Vec<u32> = {
        let mut frame = 0u32;
        let mut out = Vec::new();
        for run in &replay.runs {
            if run.input.reset {
                out.extend((1..=run.count).map(|i| frame + i));
            }
            frame += run.count;
        }
        out
    };
    println!(
        "reset: {} 次{}",
        resets.len(),
        if resets.is_empty() {
            String::new()
        } else {
            let shown: Vec<String> = resets
                .iter()
                .take(10)
                .map(|f| format!("第 {f} 幀"))
                .collect();
            format!(
                "（{}{}）",
                shown.join("、"),
                if resets.len() > 10 { "…" } else { "" }
            )
        }
    );
    println!(
        "檢查點: 每 {} 幀一個，共 {} 個（含第 0 幀的開機狀態與最後一幀）",
        replay.checkpoint_interval,
        replay.checkpoints.len()
    );
    ExitCode::SUCCESS
}

/// `replay verify <rom> <replay>`：驗證全部檢查點（關閉輸出以加速），失敗時結束碼非 0。
pub fn verify(rom: &Path, replay_path: &Path) -> ExitCode {
    let (rom_bytes, _) = match read_rom(rom) {
        Ok(v) => v,
        Err(e) => return fail(e),
    };
    let replay = match read_replay(replay_path) {
        Ok(r) => r,
        Err(e) => return fail(e),
    };
    let started = Instant::now();
    match nes_core::replay::verify(&rom_bytes, &replay) {
        Ok(report) => {
            let elapsed = started.elapsed();
            println!(
                "通過：{} 幀、{} 個檢查點全數相符（rom_id {}）",
                report.frames,
                report.checkpoints_verified,
                replay.rom_id.short()
            );
            println!(
                "耗時 {:.1} ms（驗證模式，輸出關閉；{:.0} 幀/秒）",
                elapsed.as_secs_f64() * 1000.0,
                f64::from(report.frames) / elapsed.as_secs_f64().max(1e-9)
            );
            ExitCode::SUCCESS
        }
        Err(ReplayError::Mismatch(m)) => {
            eprintln!("驗證失敗：{m}");
            let range = m.suspect_frames();
            eprintln!(
                "第一個不符的檢查點：第 {} 幀；分歧可能開始的幀範圍：第 {}–{} 幀（第 n 幀＝第 n 次 run_frame，輸入索引 n−1）",
                m.frame,
                range.start(),
                range.end()
            );
            ExitCode::FAILURE
        }
        Err(e) => fail(format!("拒絕驗證：{e}")),
    }
}

/// 一個決定性的小型亂數（xorshift64*），只用來產生示範用的輸入腳本。
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

/// `replay generate <rom> <out>`：從開機依「偽隨機的雙人輸入腳本」錄一份 replay（測試與效能量測用，
/// 不需要 GUI）。輸入每隔 1–30 幀換一組按鍵；`--reset-at` 指定哪幾幀（1 起算）按 reset。
pub fn generate(
    rom: &Path,
    out: &Path,
    frames: u32,
    seed: u64,
    interval: u16,
    reset_at: &[u32],
) -> ExitCode {
    let (_, mut nes) = match read_rom(rom) {
        Ok(v) => v,
        Err(e) => return fail(e),
    };
    let mut recorder = match ReplayRecorder::new(&nes, interval) {
        Ok(r) => r,
        Err(e) => return fail(e),
    };
    let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    let mut hold = 0u32;
    let mut pads = [Buttons::empty(); 2];
    for frame in 1..=frames {
        if hold == 0 {
            hold = 1 + (rng.next() % 30) as u32;
            let r = rng.next();
            // 每個按鍵約 1/4 的機率按下。
            let pick = |bits: u64| Buttons::from_bits_truncate((bits & (bits >> 8)) as u8);
            pads = [pick(r), pick(r >> 16 ^ rng.next())];
        }
        hold -= 1;
        let mut input = FrameInput::new(pads[0], pads[1]);
        input.reset = reset_at.contains(&frame);
        nes.run_frame(input);
        if let Err(e) = recorder.record_frame(input, &nes) {
            return fail(e);
        }
    }
    let replay = match recorder.finish(&nes) {
        Ok(r) => r,
        Err(e) => return fail(e),
    };
    if let Err(e) = std::fs::write(out, replay.encode()) {
        return fail(format!("寫入 replay 失敗: {e}"));
    }
    println!(
        "已寫入 {}：{} 幀、{} 段輸入、{} 個檢查點（rom_id {}）",
        out.display(),
        replay.total_frames,
        replay.runs.len(),
        replay.checkpoints.len(),
        replay.rom_id.short()
    );
    ExitCode::SUCCESS
}

/// `save-state <rom> <out>`：從開機跑到指定幀數（可選：依 replay 的輸入）之後，把存檔寫成檔案，
/// 供 `diff-state` 比對（例如與 GUI 在同一幀存的檔案比對）。沒有 `--replay` 時不按任何鍵。
pub fn save_state(
    rom: &Path,
    out: &Path,
    replay_path: Option<&Path>,
    frames: Option<u32>,
) -> ExitCode {
    let (_, mut nes) = match read_rom(rom) {
        Ok(v) => v,
        Err(e) => return fail(e),
    };
    let mut ran = 0u32;
    if let Some(path) = replay_path {
        let replay = match read_replay(path) {
            Ok(r) => r,
            Err(e) => return fail(e),
        };
        let limit = frames
            .unwrap_or(replay.total_frames)
            .min(replay.total_frames);
        let mut player = match ReplayPlayer::new(replay, &nes) {
            Ok(p) => p,
            Err(e) => return fail(format!("拒絕播放：{e}")),
        };
        while ran < limit {
            match player.step(&mut nes) {
                Ok(true) => ran += 1,
                Ok(false) => break,
                Err(m) => {
                    eprintln!("警告：{m}");
                    ran += 1;
                    break;
                }
            }
        }
    } else {
        for _ in 0..frames.unwrap_or(0) {
            nes.run_frame(FrameInput::NONE);
            ran += 1;
        }
    }
    if let Err(e) = std::fs::write(out, nes.save_state()) {
        return fail(format!("寫入存檔失敗: {e}"));
    }
    println!(
        "已寫入 {}：第 {ran} 幀（行為指紋 {:#018x}）",
        out.display(),
        nes.behavior_fingerprint()
    );
    ExitCode::SUCCESS
}

fn load_state_json(path: &Path) -> Result<serde_json::Value, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("讀取 {} 失敗: {e}", path.display()))?;
    let nes: Nes = nes_core::state::decode(&bytes)
        .map_err(|e| format!("解碼 {} 失敗: {e}", path.display()))?;
    serde_json::to_value(&nes).map_err(|e| format!("轉換 {} 失敗: {e}", path.display()))
}

/// `diff-state <a> <b>`。
pub fn diff_state(a: &Path, b: &Path) -> ExitCode {
    let (va, vb) = match (load_state_json(a), load_state_json(b)) {
        (Ok(va), Ok(vb)) => (va, vb),
        (Err(e), _) | (_, Err(e)) => {
            eprintln!("{e}");
            return ExitCode::from(2);
        }
    };
    let lines = diff_state::diff(&va, &vb);
    if lines.is_empty() {
        println!("兩份存檔的狀態完全相同");
        return ExitCode::SUCCESS;
    }
    for line in &lines {
        println!("{line}");
    }
    println!("共 {} 個欄位不同", lines.len());
    ExitCode::from(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nes_core::test_support::input_probe_rom;

    /// 每個測試用自己的暫存目錄（平行執行時互不干擾）。
    struct Scratch(std::path::PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("nes_test_{name}_{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
        fn path(&self, file: &str) -> std::path::PathBuf {
            self.0.join(file)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn generate_then_info_and_verify_succeed() {
        let dir = Scratch::new("gen_verify");
        let (rom, replay) = (dir.path("probe.nes"), dir.path("demo.replay"));
        std::fs::write(&rom, input_probe_rom()).unwrap();

        assert_eq!(
            generate(&rom, &replay, 400, 7, 60, &[150, 300]),
            ExitCode::SUCCESS
        );
        assert_eq!(info(&replay), ExitCode::SUCCESS);
        assert_eq!(verify(&rom, &replay), ExitCode::SUCCESS);

        let decoded = Replay::decode(&std::fs::read(&replay).unwrap()).unwrap();
        assert_eq!(decoded.total_frames, 400);
        assert_eq!(decoded.checkpoint_interval, 60);
        assert_eq!(decoded.inputs().filter(|i| i.reset).count(), 2);
        // 兩位玩家都有按鍵（腳本不是全空）。
        assert!(decoded.inputs().any(|i| !i.p1.is_empty()));
        assert!(decoded.inputs().any(|i| !i.p2.is_empty()));
    }

    /// 竄改 replay 檔案裡的某幀輸入 → `verify` 的結束碼非 0。
    #[test]
    fn verify_fails_with_a_nonzero_exit_code_when_an_input_is_tampered() {
        let dir = Scratch::new("tamper");
        let (rom, path) = (dir.path("probe.nes"), dir.path("demo.replay"));
        std::fs::write(&rom, input_probe_rom()).unwrap();
        assert_eq!(generate(&rom, &path, 300, 3, 60, &[]), ExitCode::SUCCESS);

        let mut replay = Replay::decode(&std::fs::read(&path).unwrap()).unwrap();
        let mut inputs: Vec<FrameInput> = replay.inputs().collect();
        inputs[199].p1 ^= Buttons::SELECT; // 第 200 幀：切換 Select，輸入一定不同
        replay.runs = Replay::compress(inputs);
        std::fs::write(&path, replay.encode()).unwrap();

        assert_eq!(verify(&rom, &path), ExitCode::FAILURE);
    }

    #[test]
    fn verify_and_info_reject_bad_files_and_mismatched_roms() {
        let dir = Scratch::new("reject");
        let (rom, other, path, junk) = (
            dir.path("probe.nes"),
            dir.path("other.nes"),
            dir.path("demo.replay"),
            dir.path("junk.replay"),
        );
        std::fs::write(&rom, input_probe_rom()).unwrap();
        std::fs::write(&other, nes_core::test_support::rendering_rom()).unwrap();
        assert_eq!(generate(&rom, &path, 30, 1, 10, &[]), ExitCode::SUCCESS);

        assert_eq!(verify(&other, &path), ExitCode::FAILURE, "ROM 不符");
        std::fs::write(&junk, b"this is not a replay").unwrap();
        assert_eq!(info(&junk), ExitCode::FAILURE);
        assert_eq!(verify(&rom, &junk), ExitCode::FAILURE);
        assert_eq!(verify(&rom, &dir.path("missing.replay")), ExitCode::FAILURE);
    }

    #[test]
    fn save_state_follows_the_replay_and_diff_state_finds_the_difference() {
        let dir = Scratch::new("diff");
        let (rom, replay, s100, s100b, s101) = (
            dir.path("probe.nes"),
            dir.path("demo.replay"),
            dir.path("a.state"),
            dir.path("b.state"),
            dir.path("c.state"),
        );
        std::fs::write(&rom, input_probe_rom()).unwrap();
        assert_eq!(generate(&rom, &replay, 200, 5, 60, &[]), ExitCode::SUCCESS);

        assert_eq!(
            save_state(&rom, &s100, Some(&replay), Some(100)),
            ExitCode::SUCCESS
        );
        assert_eq!(
            save_state(&rom, &s100b, Some(&replay), Some(100)),
            ExitCode::SUCCESS
        );
        assert_eq!(
            save_state(&rom, &s101, Some(&replay), Some(101)),
            ExitCode::SUCCESS
        );

        assert_eq!(
            diff_state(&s100, &s100b),
            ExitCode::SUCCESS,
            "同一幀的存檔完全相同"
        );
        assert_eq!(
            diff_state(&s100, &s101),
            ExitCode::from(1),
            "差一幀就有差異"
        );
        assert_eq!(
            diff_state(&s100, &dir.path("nope.state")),
            ExitCode::from(2)
        );
        std::fs::write(dir.path("bad.state"), b"garbage").unwrap();
        assert_eq!(diff_state(&s100, &dir.path("bad.state")), ExitCode::from(2));

        // 差異的內容包含 frame_count 與 PPU 的欄位路徑。
        let (a, b) = (
            load_state_json(&s100).unwrap(),
            load_state_json(&s101).unwrap(),
        );
        let lines = diff_state::diff(&a, &b);
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("frame_count: 0x64 vs 0x65")),
            "{lines:?}"
        );
        assert!(lines.iter().any(|l| l.starts_with("ppu.")), "{lines:?}");
    }

    #[test]
    fn save_state_without_a_replay_writes_the_boot_state_by_default() {
        let dir = Scratch::new("boot");
        let (rom, out) = (dir.path("probe.nes"), dir.path("boot.state"));
        std::fs::write(&rom, input_probe_rom()).unwrap();
        assert_eq!(save_state(&rom, &out, None, None), ExitCode::SUCCESS);
        let saved = std::fs::read(&out).unwrap();
        let boot = Nes::from_rom(&input_probe_rom()).unwrap();
        assert_eq!(saved, boot.save_state());
    }
}
