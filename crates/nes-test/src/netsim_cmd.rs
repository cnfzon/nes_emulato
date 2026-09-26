//! `netsim` 子命令：在同一個行程內建立兩個 session（lockstep 或 rollback），經由 `SimulatedTransport` 連線，
//! 用**虛擬時鐘**與腳本化的雙方輸入跑指定幀數（不需要真的等待，結果完全可重現）。
//!
//! 輸出是純文字的表格（`|` 分隔，可直接貼進 Markdown／試算表）：每個種子一列，最後一列彙總。
//! 欄位：
//!
//! - **兩端相同**：兩端逐幀（已確認幀）的行為指紋是否完全相同；
//! - **＝離線重播**：兩端是否都等於「雙方腳本合併後離線重播」的結果（獨立算出的標準答案）；
//! - **replay 相同**：兩端各自記錄的 replay 與離線 replay 的位元組是否完全相同；
//! - **stall A/B**：兩端各自累計的 stall 次數與總時間（lockstep：等對方的輸入；rollback：預測視窗已滿、暫停推進）；
//! - **虛擬耗時**：跑完全部幀的虛擬時間（理想是 幀數 ÷ 60.0988 秒）與換算的平均幀率；
//! - **本地輸入延遲**：從按下按鍵到畫面反應的額外延遲（毫秒）。lockstep ＝ input delay ＋ 每幀平均 stall；
//!   rollback ＝ input delay（本地輸入立即套用，網路延遲由預測吸收）；
//! - **rollback（僅 rollback）**：每秒 rollback 次數、重跑深度（平均／最大）、每次重跑的真實耗時（平均／最大，
//!   `--release` 才有意義）、兩端目前幀差的平均絕對值、預測準確率；
//! - **頻寬**：A→B、B→A 平均每秒的 UDP payload 位元組（不含 UDP／IP 標頭）。
//!
//! `--compare`：對 4b 的三種網路條件各跑 `--runs` 組種子，lockstep 與 rollback 並排，輸出報告用的對照表。
//!
//! 結束碼：所有種子都「兩端相同 ＋ ＝離線重播 ＋ 跑完」→ 0；否則 1；ROM 讀不了 → 2。

use std::path::Path;
use std::process::ExitCode;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use nes_net::sim::{MatchConfig, MatchReport, NTSC_FPS, run_match};
use nes_net::{Mode, NetworkConfig};

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
    /// `None`＝依模式的預設（lockstep 2、rollback 1）。
    pub input_delay: Option<u8>,
    pub no_redundancy: bool,
    pub script_seed: u64,
    pub blackout_at: Option<f64>,
    pub mode: Mode,
    /// rollback 的預測視窗 K。
    pub window: u32,
    /// 端點 B 的幀時鐘比 A 快的百分比（負數＝慢）。
    pub clock_skew: f64,
    /// 腳本輸入變化的頻率（0–1）：每次取樣改變輸入的機率的近似，實際為每 `round(1/rate)` 次取樣換一次。
    pub input_change_rate: f64,
    pub no_time_sync: bool,
    /// 腳本包含 Reset。
    pub resets: bool,
    pub compare: bool,
}

fn yes_no(b: bool) -> &'static str {
    if b { "是" } else { "否" }
}

/// 量測「重跑」真實耗時用的時間來源（`nes-net` 自己不讀系統時間，由這個 CLI 提供）。
fn wall_clock() -> Duration {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed()
}

fn input_hold(rate: f64) -> u32 {
    if rate.is_finite() && rate > 0.0 {
        ((1.0 / rate.min(1.0)).round() as u32).max(1)
    } else {
        nes_net::sim::DEFAULT_INPUT_HOLD
    }
}

fn default_delay(mode: Mode) -> u8 {
    match mode {
        Mode::Lockstep => nes_net::DEFAULT_INPUT_DELAY,
        Mode::Rollback => nes_net::rollback::DEFAULT_INPUT_DELAY,
    }
}

fn frame_ms() -> f64 {
    1000.0 / NTSC_FPS
}

/// 一組設定的所有種子（跨執行緒平行；結果依種子排序，與執行緒數量無關）。
fn run_seeds(
    rom: &[u8],
    base: &MatchConfig,
    first_seed: u64,
    runs: u64,
) -> Result<Vec<MatchReport>, String> {
    let expected = base
        .expected(rom)
        .map_err(|e| format!("無法載入 ROM: {e}"))?;
    let threads = std::thread::available_parallelism()
        .map_or(4, |n| n.get())
        .clamp(1, 16);
    let mut out: Vec<(u64, MatchReport)> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..threads as u64)
            .map(|t| {
                let expected = &expected;
                scope.spawn(move || {
                    (0..runs)
                        .filter(|i| i % threads as u64 == t)
                        .map(|i| {
                            let cfg = MatchConfig {
                                seed: first_seed + i,
                                ..base.clone()
                            };
                            let report = run_match(rom, &cfg, expected).map_err(|e| e.to_string());
                            (i, report)
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().unwrap_or_default())
            .map(|(i, r)| r.map(|r| (i, r)))
            .collect::<Result<Vec<_>, String>>()
    })?;
    out.sort_by_key(|(i, _)| *i);
    Ok(out.into_iter().map(|(_, r)| r).collect())
}

fn config_of(args: &Args<'_>, mode: Mode, network: NetworkConfig) -> MatchConfig {
    MatchConfig {
        frames: args.frames,
        network,
        input_delay: args.input_delay.unwrap_or_else(|| default_delay(mode)),
        redundancy: !args.no_redundancy,
        script_seed: args.script_seed,
        input_hold: input_hold(args.input_change_rate),
        resets: args.resets,
        checkpoint_interval: 60,
        blackout_at: args.blackout_at.map(Duration::from_secs_f64),
        mode,
        window: args.window,
        clock_skew: args.clock_skew / 100.0,
        time_sync: !args.no_time_sync,
        wall_clock: Some(wall_clock),
        ..MatchConfig::default()
    }
}

pub fn run(args: &Args<'_>) -> ExitCode {
    let rom = match std::fs::read(args.rom) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("讀取 ROM 檔案失敗: {e}");
            return ExitCode::from(2);
        }
    };
    let result = if args.compare {
        run_compare(&rom, args)
    } else {
        run_single(&rom, args)
    };
    match result {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(e) => {
            eprintln!("{e}");
            ExitCode::from(2)
        }
    }
}

fn run_single(rom: &[u8], args: &Args<'_>) -> Result<bool, String> {
    let network = NetworkConfig {
        loss: args.loss / 100.0,
        delay: Duration::from_millis(args.delay_ms),
        jitter: Duration::from_millis(args.jitter_ms),
        duplicate: args.duplicate / 100.0,
    };
    let base = config_of(args, args.mode, network);
    let delays = base.effective_delays();
    println!(
        "netsim：ROM {}｜{} 幀｜模式 {}｜丟包 {}%｜單程延遲 {} ms｜抖動 ±{} ms｜重複 {}%｜input delay {}｜{}冗餘傳送 {}｜腳本種子 {:#x}｜輸入每 {} 次取樣換一次{}｜B 時鐘 {:+}%｜時間同步 {}｜網路種子 {}..{}",
        args.rom.display(),
        args.frames,
        args.mode,
        args.loss,
        args.delay_ms,
        args.jitter_ms,
        args.duplicate,
        delays[0],
        if args.mode == Mode::Rollback {
            format!("預測視窗 K={}｜", args.window)
        } else {
            String::new()
        },
        if args.no_redundancy { "關" } else { "開" },
        args.script_seed,
        base.input_hold,
        if args.resets { "｜含 Reset" } else { "" },
        args.clock_skew,
        if args.mode == Mode::Rollback && !args.no_time_sync {
            "開"
        } else {
            "關"
        },
        args.seed,
        args.seed + args.runs.saturating_sub(1),
    );
    println!(
        "| 種子 | 完成 | 兩端相同 | ＝離線重播 | replay 相同 | stall A（次／ms） | stall B（次／ms） | 虛擬耗時（s） | 平均幀率 | 本地輸入延遲（ms） | rollback（次／s） | 重跑深度（平均／最大） | 重跑耗時 ms（平均／最大） | 幀差 \\|A−B\\| | 預測準確率 | A→B（B/s） | B→A（B/s） |"
    );
    println!("|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|");

    let reports = run_seeds(rom, &base, args.seed, args.runs)?;
    let offline_replay = base
        .expected(rom)
        .map_err(|e| format!("無法載入 ROM: {e}"))?
        .to_replay(args.frames, 60)
        .encode();
    let mut all_ok = true;
    let mut rows = Vec::new();
    for (i, report) in reports.iter().enumerate() {
        let row = Row::of(report, &offline_replay, &base);
        all_ok &= row.ok();
        println!("{}", row.render(&(args.seed + i as u64).to_string()));
        rows.push(row);
    }
    println!("{}", Agg::of(&rows).render_summary(args.runs));
    Ok(all_ok)
}

/// 4b 的三種網路條件。
fn conditions() -> [(&'static str, NetworkConfig); 3] {
    let ms = Duration::from_millis;
    [
        ("理想網路", NetworkConfig::IDEAL),
        (
            "10% 丟包、單程 100 ms、抖動 ±30 ms",
            NetworkConfig {
                loss: 0.10,
                delay: ms(100),
                jitter: ms(30),
                duplicate: 0.0,
            },
        ),
        (
            "30% 丟包、單程 200 ms、抖動 ±80 ms、5% 重複",
            NetworkConfig {
                loss: 0.30,
                delay: ms(200),
                jitter: ms(80),
                duplicate: 0.05,
            },
        ),
    ]
}

/// lockstep 與 rollback 的對照表：三種網路條件，各 `runs` 組種子，兩種模式並排。
fn run_compare(rom: &[u8], args: &Args<'_>) -> Result<bool, String> {
    println!(
        "lockstep 對 rollback：ROM {}｜{} 幀｜每種條件 {} 組種子（{}..{}）｜lockstep input delay {}、rollback input delay {} ／ K={}｜輸入每 {} 次取樣換一次{}｜時間同步 {}",
        args.rom.display(),
        args.frames,
        args.runs,
        args.seed,
        args.seed + args.runs.saturating_sub(1),
        args.input_delay.unwrap_or(default_delay(Mode::Lockstep)),
        args.input_delay.unwrap_or(default_delay(Mode::Rollback)),
        args.window,
        input_hold(args.input_change_rate),
        if args.resets { "｜含 Reset" } else { "" },
        if args.no_time_sync { "關" } else { "開" },
    );
    println!(
        "| 網路條件 | 模式 | 等價 | 平均幀率 | 虛擬耗時（s） | stall 每端（次／ms） | 本地輸入延遲（ms） | rollback（次／s） | 重跑深度（平均／最大） | 重跑耗時 ms（平均／最大） | 預測準確率 | 頻寬 A→B／B→A（B/s） |"
    );
    println!("|---|---|---|---|---|---|---|---|---|---|---|---|");
    let mut all_ok = true;
    for (name, network) in conditions() {
        for mode in [Mode::Lockstep, Mode::Rollback] {
            let base = config_of(args, mode, network);
            let reports = run_seeds(rom, &base, args.seed, args.runs)?;
            let offline = base
                .expected(rom)
                .map_err(|e| format!("無法載入 ROM: {e}"))?
                .to_replay(args.frames, 60)
                .encode();
            let rows: Vec<Row> = reports
                .iter()
                .map(|r| Row::of(r, &offline, &base))
                .collect();
            all_ok &= rows.iter().all(Row::ok);
            println!("{}", Agg::of(&rows).render_compare(name, mode));
        }
    }
    Ok(all_ok)
}

#[derive(Clone, Copy, Default)]
struct RbRow {
    per_sec: f64,
    avg_depth: f64,
    max_depth: f64,
    resim_avg_ms: f64,
    resim_max_ms: f64,
    accuracy: Option<f64>,
}

struct Row {
    completed: bool,
    ends_equal: bool,
    equals_offline: bool,
    replays_equal: bool,
    stalls: [(u32, f64); 2],
    elapsed: f64,
    fps: f64,
    local_delay_ms: f64,
    rb: Option<RbRow>,
    /// 兩端目前幀差的平均絕對值（rollback）。
    advantage: Option<f64>,
    a_to_b: f64,
    b_to_a: f64,
}

impl Row {
    fn of(r: &MatchReport, offline_replay: &[u8], cfg: &MatchConfig) -> Self {
        let elapsed = r.virtual_elapsed.as_secs_f64().max(1e-9);
        let completed = !r.timed_out && r.ends.iter().all(|e| e.frames >= r.target_frames);
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
        let delays = cfg.effective_delays();
        let mean_delay = f64::from(delays[0]) / 2.0 + f64::from(delays[1]) / 2.0;
        let frames = f64::from(r.target_frames.max(1));
        let stall_per_frame_ms = (stall(0).1 + stall(1).1) / 2.0 / frames;
        let local_delay_ms = match cfg.mode {
            Mode::Lockstep => mean_delay * frame_ms() + stall_per_frame_ms,
            Mode::Rollback => mean_delay * frame_ms(),
        };
        let rb = (cfg.mode == Mode::Rollback).then(|| {
            let ends: Vec<_> = r.ends.iter().filter_map(|e| e.rollback()).collect();
            let n = ends.len().max(1) as f64;
            let ms = |d: Duration| d.as_secs_f64() * 1000.0;
            let accuracy: Vec<f64> = ends
                .iter()
                .filter_map(|s| s.prediction_accuracy().map(f64::from))
                .collect();
            RbRow {
                per_sec: ends.iter().map(|s| f64::from(s.rollbacks)).sum::<f64>() / n / elapsed,
                avg_depth: ends.iter().map(|s| f64::from(s.avg_depth)).sum::<f64>() / n,
                max_depth: ends
                    .iter()
                    .map(|s| f64::from(s.max_depth))
                    .fold(0.0, f64::max),
                resim_avg_ms: ends.iter().map(|s| ms(s.resim_time_avg)).sum::<f64>() / n,
                resim_max_ms: ends
                    .iter()
                    .map(|s| ms(s.resim_time_max))
                    .fold(0.0, f64::max),
                accuracy: (!accuracy.is_empty())
                    .then(|| accuracy.iter().sum::<f64>() / accuracy.len() as f64),
            }
        });
        let advantage = (cfg.mode == Mode::Rollback && !r.advantage.is_empty()).then(|| {
            r.advantage
                .iter()
                .map(|s| f64::from(s.a_minus_b.abs()))
                .sum::<f64>()
                / r.advantage.len() as f64
        });
        Self {
            completed,
            ends_equal: completed && r.a_vs_b.is_none(),
            equals_offline: completed && r.vs_expected.iter().all(Option::is_none),
            replays_equal,
            stalls: [stall(0), stall(1)],
            elapsed,
            fps: f64::from(r.target_frames) / elapsed,
            local_delay_ms,
            rb,
            advantage,
            a_to_b: r.ends[0].stats.bytes_sent as f64 / elapsed,
            b_to_a: r.ends[1].stats.bytes_sent as f64 / elapsed,
        }
    }

    fn ok(&self) -> bool {
        self.completed && self.ends_equal && self.equals_offline && self.replays_equal
    }

    fn render(&self, seed: &str) -> String {
        let dash = "—".to_string();
        let (rate, depth, resim, acc) = match &self.rb {
            Some(rb) => (
                format!("{:.2}", rb.per_sec),
                format!("{:.2}／{:.0}", rb.avg_depth, rb.max_depth),
                format!("{:.3}／{:.3}", rb.resim_avg_ms, rb.resim_max_ms),
                rb.accuracy
                    .map_or(dash.clone(), |a| format!("{:.1}%", a * 100.0)),
            ),
            None => (dash.clone(), dash.clone(), dash.clone(), dash.clone()),
        };
        format!(
            "| {seed} | {} | {} | {} | {} | {}／{:.0} | {}／{:.0} | {:.1} | {:.1} | {:.1} | {rate} | {depth} | {resim} | {} | {acc} | {:.0} | {:.0} |",
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
            self.local_delay_ms,
            self.advantage.map_or(dash, |a| format!("{a:.2}")),
            self.a_to_b,
            self.b_to_a,
        )
    }
}

/// 一組種子的平均。
struct Agg {
    runs: usize,
    ok: usize,
    completed: usize,
    stalls: [(f64, f64); 2],
    elapsed: f64,
    fps: f64,
    local_delay_ms: f64,
    rb: Option<RbRow>,
    advantage: Option<f64>,
    a_to_b: f64,
    b_to_a: f64,
}

impl Agg {
    fn of(rows: &[Row]) -> Self {
        let n = rows.len().max(1) as f64;
        let mean = |f: &dyn Fn(&Row) -> f64| rows.iter().map(f).sum::<f64>() / n;
        let rb_rows: Vec<RbRow> = rows.iter().filter_map(|r| r.rb).collect();
        let rn = rb_rows.len().max(1) as f64;
        let accs: Vec<f64> = rb_rows.iter().filter_map(|r| r.accuracy).collect();
        let advs: Vec<f64> = rows.iter().filter_map(|r| r.advantage).collect();
        Self {
            runs: rows.len(),
            ok: rows.iter().filter(|r| r.ok()).count(),
            completed: rows.iter().filter(|r| r.completed).count(),
            stalls: [
                (
                    mean(&|r| f64::from(r.stalls[0].0)),
                    mean(&|r| r.stalls[0].1),
                ),
                (
                    mean(&|r| f64::from(r.stalls[1].0)),
                    mean(&|r| r.stalls[1].1),
                ),
            ],
            elapsed: mean(&|r| r.elapsed),
            fps: mean(&|r| r.fps),
            local_delay_ms: mean(&|r| r.local_delay_ms),
            rb: (!rb_rows.is_empty()).then(|| RbRow {
                per_sec: rb_rows.iter().map(|r| r.per_sec).sum::<f64>() / rn,
                avg_depth: rb_rows.iter().map(|r| r.avg_depth).sum::<f64>() / rn,
                max_depth: rb_rows.iter().map(|r| r.max_depth).fold(0.0, f64::max),
                resim_avg_ms: rb_rows.iter().map(|r| r.resim_avg_ms).sum::<f64>() / rn,
                resim_max_ms: rb_rows.iter().map(|r| r.resim_max_ms).fold(0.0, f64::max),
                accuracy: (!accs.is_empty()).then(|| accs.iter().sum::<f64>() / accs.len() as f64),
            }),
            advantage: (!advs.is_empty()).then(|| advs.iter().sum::<f64>() / advs.len() as f64),
            a_to_b: mean(&|r| r.a_to_b),
            b_to_a: mean(&|r| r.b_to_a),
        }
    }

    fn render_summary(&self, runs: u64) -> String {
        let dash = "—".to_string();
        let (rate, depth, resim, acc) = match &self.rb {
            Some(rb) => (
                format!("{:.2}", rb.per_sec),
                format!("{:.2}／{:.0}", rb.avg_depth, rb.max_depth),
                format!("{:.3}／{:.3}", rb.resim_avg_ms, rb.resim_max_ms),
                rb.accuracy
                    .map_or(dash.clone(), |a| format!("{:.1}%", a * 100.0)),
            ),
            None => (dash.clone(), dash.clone(), dash.clone(), dash.clone()),
        };
        let _ = runs;
        format!(
            "| **平均／彙總** | {}/{} | 全部相同：{}/{} | | | {:.1}／{:.0} | {:.1}／{:.0} | {:.1} | {:.1}（理想 {NTSC_FPS}） | {:.1} | {rate} | {depth} | {resim} | {} | {acc} | {:.0} | {:.0} |",
            self.completed,
            self.runs,
            self.ok,
            self.runs,
            self.stalls[0].0,
            self.stalls[0].1,
            self.stalls[1].0,
            self.stalls[1].1,
            self.elapsed,
            self.fps,
            self.local_delay_ms,
            self.advantage.map_or(dash, |a| format!("{a:.2}")),
            self.a_to_b,
            self.b_to_a,
        )
    }

    fn render_compare(&self, condition: &str, mode: Mode) -> String {
        let dash = "—".to_string();
        let (rate, depth, resim, acc) = match &self.rb {
            Some(rb) => (
                format!("{:.2}", rb.per_sec),
                format!("{:.2}／{:.0}", rb.avg_depth, rb.max_depth),
                format!("{:.3}／{:.3}", rb.resim_avg_ms, rb.resim_max_ms),
                rb.accuracy
                    .map_or(dash.clone(), |a| format!("{:.1}%", a * 100.0)),
            ),
            None => (dash.clone(), dash.clone(), dash.clone(), dash),
        };
        format!(
            "| {condition} | {mode} | {}/{} | {:.1} | {:.1} | {:.0}／{:.0} | {:.1} | {rate} | {depth} | {resim} | {acc} | {:.0}／{:.0} |",
            self.ok,
            self.runs,
            self.fps,
            self.elapsed,
            (self.stalls[0].0 + self.stalls[1].0) / 2.0,
            (self.stalls[0].1 + self.stalls[1].1) / 2.0,
            self.local_delay_ms,
            self.a_to_b,
            self.b_to_a,
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
            input_delay: None,
            no_redundancy: false,
            script_seed: 0x5EED,
            blackout_at: None,
            mode: Mode::Lockstep,
            window: 8,
            clock_skew: 0.0,
            input_change_rate: 1.0 / 3.0,
            no_time_sync: false,
            resets: false,
            compare: false,
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
    fn rollback_mode_passes_all_checks_on_a_lossy_network() {
        let rom = probe_rom_file("rollback");
        let code = run(&Args {
            mode: Mode::Rollback,
            loss: 10.0,
            delay_ms: 60,
            jitter_ms: 10,
            duplicate: 5.0,
            runs: 2,
            resets: true,
            input_change_rate: 0.5,
            clock_skew: 1.0,
            ..args(&rom)
        });
        std::fs::remove_file(&rom).ok();
        assert_eq!(code, ExitCode::SUCCESS);
    }

    #[test]
    fn the_compare_table_runs_both_modes_over_all_three_conditions() {
        let rom = probe_rom_file("compare");
        let code = run(&Args {
            frames: 180,
            compare: true,
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

    #[test]
    fn the_input_change_rate_maps_to_a_hold_length() {
        assert_eq!(input_hold(1.0), 1);
        assert_eq!(input_hold(0.5), 2);
        assert_eq!(input_hold(1.0 / 3.0), 3);
        assert_eq!(input_hold(0.0), nes_net::sim::DEFAULT_INPUT_HOLD);
        assert_eq!(input_hold(f64::NAN), nes_net::sim::DEFAULT_INPUT_HOLD);
        assert_eq!(input_hold(5.0), 1);
    }
}
