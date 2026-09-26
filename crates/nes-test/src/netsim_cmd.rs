//! `netsim` 子命令：在同一個行程內建立兩個 lockstep session，經由 `SimulatedTransport` 連線，
//! 用**虛擬時鐘**與腳本化的雙方輸入跑指定幀數（不需要真的等待，結果完全可重現）。
//!
//! 輸出是純文字的表格（`|` 分隔，可直接貼進 Markdown／試算表）：每個種子一列，最後一列彙總。
//! 欄位：
//!
//! - **兩端相同**：兩端逐幀的行為指紋是否完全相同；
//! - **＝離線重播**：兩端是否都等於「雙方腳本合併後離線重播」的結果（獨立算出的標準答案）；
//! - **replay 相同**：兩端各自記錄的 replay 與離線 replay 的位元組是否完全相同；
//! - **stall A/B**：兩端各自累計的 stall 次數（一次連續等不到輸入算一次）與總時間；
//! - **虛擬耗時**：跑完全部幀的虛擬時間（理想是 幀數 ÷ 60.0988 秒）與換算的平均幀率；
//! - **頻寬**：A→B、B→A 平均每秒的 UDP payload 位元組（不含 UDP／IP 標頭）。
//!
//! 結束碼：所有種子都「兩端相同 ＋ ＝離線重播 ＋ 跑完」→ 0；否則 1；ROM 讀不了 → 2。

use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

use nes_net::NetworkConfig;
use nes_net::sim::{MatchConfig, MatchReport, NTSC_FPS, expected_log, run_match};

pub struct Args<'a> {
    pub rom: &'a Path,
    pub frames: u32,
    /// 百分比（0–100）。
    pub loss: f64,
    pub delay_ms: u64,
    pub jitter_ms: u64,
    /// 百分比（0–100）。
    pub duplicate: f64,
    pub seed: u64,
    pub runs: u64,
    pub input_delay: u8,
    pub no_redundancy: bool,
    pub script_seed: u64,
    pub blackout_at: Option<f64>,
}

fn yes_no(b: bool) -> &'static str {
    if b { "是" } else { "否" }
}

pub fn run(args: &Args<'_>) -> ExitCode {
    let rom = match std::fs::read(args.rom) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("讀取 ROM 檔案失敗: {e}");
            return ExitCode::from(2);
        }
    };
    let input_delay = args.input_delay.min(nes_net::MAX_INPUT_DELAY);
    let expected = match expected_log(&rom, args.script_seed, input_delay, args.frames) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("無法載入 ROM: {e}");
            return ExitCode::from(2);
        }
    };
    let offline_replay = expected.to_replay(args.frames, 60).encode();
    let network = NetworkConfig {
        loss: args.loss / 100.0,
        delay: Duration::from_millis(args.delay_ms),
        jitter: Duration::from_millis(args.jitter_ms),
        duplicate: args.duplicate / 100.0,
    };

    println!(
        "netsim：ROM {}｜{} 幀｜丟包 {}%｜單程延遲 {} ms｜抖動 ±{} ms｜重複 {}%｜input delay {}｜冗餘傳送 {}｜腳本種子 {:#x}｜網路種子 {}..{}",
        args.rom.display(),
        args.frames,
        args.loss,
        args.delay_ms,
        args.jitter_ms,
        args.duplicate,
        input_delay,
        if args.no_redundancy { "關" } else { "開" },
        args.script_seed,
        args.seed,
        args.seed + args.runs.saturating_sub(1),
    );
    println!(
        "| 種子 | 完成 | 兩端相同 | ＝離線重播 | replay 相同 | stall A（次／ms） | stall B（次／ms） | 虛擬耗時（s） | 平均幀率 | A→B（B/s） | B→A（B/s） |"
    );
    println!("|---|---|---|---|---|---|---|---|---|---|---|");

    let mut all_ok = true;
    let mut totals = Totals::default();
    for i in 0..args.runs {
        let seed = args.seed + i;
        let cfg = MatchConfig {
            frames: args.frames,
            network,
            seed,
            input_delay,
            redundancy: !args.no_redundancy,
            script_seed: args.script_seed,
            checkpoint_interval: 60,
            blackout_at: args.blackout_at.map(Duration::from_secs_f64),
            ..MatchConfig::default()
        };
        let report = match run_match(&rom, &cfg, &expected) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("無法載入 ROM: {e}");
                return ExitCode::from(2);
            }
        };
        let row = Row::of(&report, &offline_replay);
        all_ok &= row.ok();
        println!("{}", row.render(seed));
        totals.add(&row);
    }
    println!("{}", totals.render(args.runs));
    if all_ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

struct Row {
    completed: bool,
    ends_equal: bool,
    equals_offline: bool,
    replays_equal: bool,
    stalls: [(u32, f64); 2],
    elapsed: f64,
    fps: f64,
    a_to_b: f64,
    b_to_a: f64,
}

impl Row {
    fn of(r: &MatchReport, offline_replay: &[u8]) -> Self {
        let elapsed = r.virtual_elapsed.as_secs_f64().max(1e-9);
        let completed = !r.timed_out && r.ends.iter().all(|e| e.frames == r.target_frames);
        let replays_equal = completed
            && r.ends
                .iter()
                .all(|e| e.log.to_replay(r.target_frames, 60).encode() == offline_replay);
        let stall = |i: usize| {
            (
                r.ends[i].stats.stalls,
                r.ends[i].stats.stall_time.as_secs_f64() * 1000.0,
            )
        };
        Self {
            completed,
            ends_equal: completed && r.a_vs_b.is_none(),
            equals_offline: completed && r.vs_expected.iter().all(Option::is_none),
            replays_equal,
            stalls: [stall(0), stall(1)],
            elapsed,
            fps: f64::from(r.target_frames.min(r.ends[0].frames)) / elapsed,
            a_to_b: r.ends[0].stats.bytes_sent as f64 / elapsed,
            b_to_a: r.ends[1].stats.bytes_sent as f64 / elapsed,
        }
    }

    fn ok(&self) -> bool {
        self.completed && self.ends_equal && self.equals_offline && self.replays_equal
    }

    fn render(&self, seed: u64) -> String {
        format!(
            "| {seed} | {} | {} | {} | {} | {}／{:.0} | {}／{:.0} | {:.1} | {:.1} | {:.0} | {:.0} |",
            yes_no(self.completed),
            yes_no(self.ends_equal),
            yes_no(self.equals_offline),
            yes_no(self.replays_equal),
            self.stalls[0].0,
            self.stalls[0].1,
            self.stalls[1].0,
            self.stalls[1].1,
            self.elapsed,
            self.fps,
            self.a_to_b,
            self.b_to_a,
        )
    }
}

#[derive(Default)]
struct Totals {
    runs: u64,
    ok: u64,
    completed: u64,
    stalls: [(f64, f64); 2],
    elapsed: f64,
    fps: f64,
    a_to_b: f64,
    b_to_a: f64,
}

impl Totals {
    fn add(&mut self, r: &Row) {
        self.runs += 1;
        self.ok += u64::from(r.ok());
        self.completed += u64::from(r.completed);
        for i in 0..2 {
            self.stalls[i].0 += f64::from(r.stalls[i].0);
            self.stalls[i].1 += r.stalls[i].1;
        }
        self.elapsed += r.elapsed;
        self.fps += r.fps;
        self.a_to_b += r.a_to_b;
        self.b_to_a += r.b_to_a;
    }

    fn render(&self, runs: u64) -> String {
        let n = runs.max(1) as f64;
        format!(
            "| **平均／彙總** | {}/{} | 全部相同：{}/{} | | | {:.1}／{:.0} | {:.1}／{:.0} | {:.1} | {:.1}（理想 {NTSC_FPS}） | {:.0} | {:.0} |",
            self.completed,
            self.runs,
            self.ok,
            self.runs,
            self.stalls[0].0 / n,
            self.stalls[0].1 / n,
            self.stalls[1].0 / n,
            self.stalls[1].1 / n,
            self.elapsed / n,
            self.fps / n,
            self.a_to_b / n,
            self.b_to_a / n,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn probe_rom_file(tag: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("netsim-{tag}-{}.nes", std::process::id()));
        std::fs::write(&path, nes_core::test_support::input_probe_rom()).unwrap();
        path
    }

    fn args(rom: &Path) -> Args<'_> {
        Args {
            rom,
            frames: 240,
            loss: 0.0,
            delay_ms: 0,
            jitter_ms: 0,
            duplicate: 0.0,
            seed: 1,
            runs: 1,
            input_delay: 2,
            no_redundancy: false,
            script_seed: 0x5EED,
            blackout_at: None,
        }
    }

    #[test]
    fn a_lossy_network_still_passes_all_checks() {
        let rom = probe_rom_file("lossy");
        let code = run(&Args {
            loss: 10.0,
            delay_ms: 30,
            jitter_ms: 10,
            duplicate: 5.0,
            runs: 2,
            ..args(&rom)
        });
        std::fs::remove_file(&rom).ok();
        assert_eq!(code, ExitCode::SUCCESS);
    }

    #[test]
    fn disabling_redundancy_still_passes() {
        let rom = probe_rom_file("noredundancy");
        let code = run(&Args {
            loss: 10.0,
            delay_ms: 20,
            no_redundancy: true,
            ..args(&rom)
        });
        std::fs::remove_file(&rom).ok();
        assert_eq!(code, ExitCode::SUCCESS);
    }

    #[test]
    fn a_dead_network_is_reported_as_a_failure() {
        let rom = probe_rom_file("blackout");
        let code = run(&Args {
            frames: 600,
            blackout_at: Some(1.0),
            ..args(&rom)
        });
        std::fs::remove_file(&rom).ok();
        assert_eq!(code, ExitCode::FAILURE, "斷線時沒有跑完，不能算通過");
    }

    #[test]
    fn a_missing_rom_exits_with_2() {
        let missing = PathBuf::from("this-rom-does-not-exist.nes");
        assert_eq!(run(&args(&missing)), ExitCode::from(2));
    }
}
