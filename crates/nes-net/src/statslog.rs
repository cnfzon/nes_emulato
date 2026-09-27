//! 統計紀錄（Phase 4d）：連線中每秒一筆的 CSV，以及讀回 CSV 後的摘要（平均、最小、p50、p95、最大）。
//!
//! **純資料處理**：不做 I/O、不讀系統時間。`nes-app` 的 emu 執行緒把 [`crate::session::Event::Stats`] 餵給
//! [`StatsRecorder`]、把 [`StatsRow::to_csv_line`] 寫進檔案；`nes-test stats-summary` 用 [`parse_csv`] 與
//! [`summarize`] 讀回來整理成表（期末報告的實機數據）。測試用虛擬時鐘，往返（寫出 → 解析 → 摘要）完全可重現。
//!
//! # 欄位（名稱帶單位；「每秒」欄位是**這一秒內**的值，不是累計）
//!
//! | 欄位 | 單位 | 說明 |
//! |---|---|---|
//! | `time_s` | 秒 | 自連線成功起的時間 |
//! | `mode` | — | `lockstep`／`rollback` |
//! | `ping_ms` | 毫秒 | 平滑後的往返時間（Ping／Pong）；還沒量到＝空 |
//! | `fps` | 幀／秒 | 這一秒內**已確認**（lockstep：已完成）的幀數 ÷ 經過的秒數 |
//! | `rollbacks` | 次 | 這一秒內的 rollback 次數（lockstep＝空） |
//! | `resim_depth_avg` | 幀 | 這一秒內的平均重跑深度（沒有 rollback＝空） |
//! | `resim_depth_max` | 幀 | 這一秒內最深的一次重跑（沒有 rollback＝空或 0） |
//! | `resim_ms_avg` | 毫秒 | 這一秒內每次重跑（含還原）的平均耗時（沒有＝空） |
//! | `stalls` | 次 | 這一秒內新增的 stall 次數 |
//! | `stall_ms` | 毫秒 | 這一秒內新增的 stall 時間 |
//! | `frame_advantage` | 幀 | rollback 的本地幀數優勢（正數＝領先；lockstep＝空） |
//! | `prediction_accuracy_pct` | % | 累計的預測準確率（還沒有＝空；lockstep＝空） |
//! | `send_bytes_per_s`／`recv_bytes_per_s` | 位元組／秒 | UDP payload（不含 28 位元組的 UDP／IP 標頭／封包） |
//! | `audio_underruns` | 次 | 這一秒內新增的音訊 underrun 次數 |

use std::fmt::{self, Write as _};
use std::time::Duration;

use thiserror::Error;

use crate::protocol::Mode;
use crate::session::Stats;

/// CSV 的第一行。
pub const CSV_HEADER: &str = "time_s,mode,ping_ms,fps,rollbacks,resim_depth_avg,resim_depth_max,resim_ms_avg,stalls,stall_ms,frame_advantage,prediction_accuracy_pct,send_bytes_per_s,recv_bytes_per_s,audio_underruns";

/// 每一個數值欄位（不含 `time_s`、`mode`）的名稱與單位，順序＝[`StatsRow::values`]。
pub const METRICS: [(&str, &str); 13] = [
    ("ping_ms", "ms"),
    ("fps", "幀/秒"),
    ("rollbacks", "次/秒"),
    ("resim_depth_avg", "幀"),
    ("resim_depth_max", "幀"),
    ("resim_ms_avg", "ms"),
    ("stalls", "次/秒"),
    ("stall_ms", "ms/秒"),
    ("frame_advantage", "幀"),
    ("prediction_accuracy_pct", "%"),
    ("send_bytes_per_s", "B/s"),
    ("recv_bytes_per_s", "B/s"),
    ("audio_underruns", "次/秒"),
];

/// 一秒的紀錄。
#[derive(Debug, Clone, PartialEq)]
pub struct StatsRow {
    pub time_s: f64,
    pub mode: Mode,
    pub ping_ms: Option<f64>,
    pub fps: f64,
    pub rollbacks: Option<u32>,
    pub resim_depth_avg: Option<f64>,
    pub resim_depth_max: Option<u32>,
    pub resim_ms_avg: Option<f64>,
    pub stalls: u32,
    pub stall_ms: f64,
    pub frame_advantage: Option<f64>,
    pub prediction_accuracy_pct: Option<f64>,
    pub send_bytes_per_s: u32,
    pub recv_bytes_per_s: u32,
    pub audio_underruns: u64,
}

fn opt(v: Option<f64>, decimals: usize) -> String {
    v.map_or(String::new(), |v| format!("{v:.decimals$}"))
}

impl StatsRow {
    /// 一行 CSV（不含換行）。
    pub fn to_csv_line(&self) -> String {
        let mut s = String::with_capacity(96);
        let _ = write!(
            s,
            "{:.1},{},{},{:.2},{},{},{},{},{},{:.1},{},{},{},{},{}",
            self.time_s,
            self.mode,
            opt(self.ping_ms, 1),
            self.fps,
            self.rollbacks.map_or(String::new(), |v| v.to_string()),
            opt(self.resim_depth_avg, 2),
            self.resim_depth_max
                .map_or(String::new(), |v| v.to_string()),
            opt(self.resim_ms_avg, 3),
            self.stalls,
            self.stall_ms,
            opt(self.frame_advantage, 2),
            opt(self.prediction_accuracy_pct, 1),
            self.send_bytes_per_s,
            self.recv_bytes_per_s,
            self.audio_underruns,
        );
        s
    }

    /// 解析一行 CSV。
    pub fn parse(line: &str) -> Result<StatsRow, String> {
        let f: Vec<&str> = line.trim().split(',').collect();
        let expected = CSV_HEADER.split(',').count();
        if f.len() != expected {
            return Err(format!("欄位數 {} 不是 {expected}", f.len()));
        }
        fn num<T: std::str::FromStr>(name: &str, s: &str) -> Result<T, String> {
            s.parse()
                .map_err(|_| format!("欄位 {name} 不是數字：「{s}」"))
        }
        fn opt_num<T: std::str::FromStr>(name: &str, s: &str) -> Result<Option<T>, String> {
            if s.is_empty() {
                Ok(None)
            } else {
                num(name, s).map(Some)
            }
        }
        let mode = match f[1] {
            "lockstep" => Mode::Lockstep,
            "rollback" => Mode::Rollback,
            other => return Err(format!("未知的 mode：「{other}」")),
        };
        Ok(StatsRow {
            time_s: num("time_s", f[0])?,
            mode,
            ping_ms: opt_num("ping_ms", f[2])?,
            fps: num("fps", f[3])?,
            rollbacks: opt_num("rollbacks", f[4])?,
            resim_depth_avg: opt_num("resim_depth_avg", f[5])?,
            resim_depth_max: opt_num("resim_depth_max", f[6])?,
            resim_ms_avg: opt_num("resim_ms_avg", f[7])?,
            stalls: num("stalls", f[8])?,
            stall_ms: num("stall_ms", f[9])?,
            frame_advantage: opt_num("frame_advantage", f[10])?,
            prediction_accuracy_pct: opt_num("prediction_accuracy_pct", f[11])?,
            send_bytes_per_s: num("send_bytes_per_s", f[12])?,
            recv_bytes_per_s: num("recv_bytes_per_s", f[13])?,
            audio_underruns: num("audio_underruns", f[14])?,
        })
    }

    /// [`METRICS`] 順序的數值（空欄位＝`None`）。
    pub fn values(&self) -> [Option<f64>; 13] {
        [
            self.ping_ms,
            Some(self.fps),
            self.rollbacks.map(f64::from),
            self.resim_depth_avg,
            self.resim_depth_max.map(f64::from),
            self.resim_ms_avg,
            Some(f64::from(self.stalls)),
            Some(self.stall_ms),
            self.frame_advantage,
            self.prediction_accuracy_pct,
            Some(f64::from(self.send_bytes_per_s)),
            Some(f64::from(self.recv_bytes_per_s)),
            Some(self.audio_underruns as f64),
        ]
    }
}

/// 累計值 → 每秒的值：吃每秒一次的 [`Stats`]（累計），吐出這一秒的 [`StatsRow`]。同時累積整場的摘要。
#[derive(Debug, Clone)]
pub struct StatsRecorder {
    prev_at: Duration,
    prev: Stats,
    prev_underruns: u64,
    started_at: Duration,
    ping_sum_ms: f64,
    ping_n: u32,
    ping_max_ms: f64,
}

impl StatsRecorder {
    /// `connected_at`：連線成功的時間（與之後餵進來的 `now` 同一個時間原點）。
    pub fn new(connected_at: Duration) -> Self {
        Self {
            prev_at: connected_at,
            prev: Stats::default(),
            prev_underruns: 0,
            started_at: connected_at,
            ping_sum_ms: 0.0,
            ping_n: 0,
            ping_max_ms: 0.0,
        }
    }

    /// 餵入一筆統計（`audio_underruns`：音訊 underrun 的**累計**次數）。
    pub fn push(&mut self, now: Duration, stats: &Stats, audio_underruns: u64) -> StatsRow {
        let dt = now.saturating_sub(self.prev_at).as_secs_f64().max(1e-9);
        let prev = &self.prev;
        let ping_ms = stats.rtt.map(|r| r.as_secs_f64() * 1000.0);
        if let Some(p) = ping_ms {
            self.ping_sum_ms += p;
            self.ping_n += 1;
            self.ping_max_ms = self.ping_max_ms.max(p);
        }
        let rb = stats.rollback.as_ref();
        let prev_rb = prev.rollback.unwrap_or_default();
        let rollbacks = rb.map(|r| r.rollbacks.saturating_sub(prev_rb.rollbacks));
        let resim_frames = rb.map(|r| r.resim_frames.saturating_sub(prev_rb.resim_frames));
        let timed = rb.map(|r| r.resims_timed.saturating_sub(prev_rb.resims_timed));
        let row = StatsRow {
            time_s: now.saturating_sub(self.started_at).as_secs_f64(),
            mode: stats.mode,
            ping_ms,
            fps: f64::from(stats.frame.saturating_sub(prev.frame)) / dt,
            rollbacks,
            resim_depth_avg: match (rollbacks, resim_frames) {
                (Some(n), Some(f)) if n > 0 => Some(f as f64 / f64::from(n)),
                _ => None,
            },
            resim_depth_max: rb.map(|r| r.window_max_depth),
            resim_ms_avg: match (rb, timed) {
                (Some(r), Some(n)) if n > 0 => Some(
                    r.resim_time_total
                        .saturating_sub(prev_rb.resim_time_total)
                        .as_secs_f64()
                        * 1000.0
                        / f64::from(n),
                ),
                _ => None,
            },
            stalls: stats.stalls.saturating_sub(prev.stalls),
            stall_ms: stats
                .stall_time
                .saturating_sub(prev.stall_time)
                .as_secs_f64()
                * 1000.0,
            frame_advantage: rb.map(|r| f64::from(r.frame_advantage)),
            prediction_accuracy_pct: rb.and_then(|r| {
                let total = r.prediction_correct + r.prediction_wrong;
                (total > 0).then(|| r.prediction_correct as f64 * 100.0 / total as f64)
            }),
            send_bytes_per_s: stats.send_bytes_per_sec,
            recv_bytes_per_s: stats.recv_bytes_per_sec,
            audio_underruns: audio_underruns.saturating_sub(self.prev_underruns),
        };
        self.prev_at = now;
        self.prev = *stats;
        self.prev_underruns = audio_underruns;
        row
    }

    /// 整場的摘要（`final_stats`：結束時的統計；`ended_at`：結束時間）。
    pub fn summary(
        &self,
        ended_at: Duration,
        final_stats: &Stats,
        audio_underruns: u64,
    ) -> MatchSummary {
        let rb = final_stats.rollback.as_ref();
        MatchSummary {
            mode: final_stats.mode,
            duration: ended_at.saturating_sub(self.started_at),
            frames: final_stats.frame,
            ping_avg_ms: (self.ping_n > 0).then(|| self.ping_sum_ms / f64::from(self.ping_n)),
            ping_max_ms: (self.ping_n > 0).then_some(self.ping_max_ms),
            rollbacks: rb.map(|r| r.rollbacks),
            max_depth: rb.map(|r| r.max_depth),
            prediction_accuracy: rb.and_then(|r| r.prediction_accuracy()),
            stalls: final_stats.stalls,
            stall_time: final_stats.stall_time,
            bytes_sent: final_stats.bytes_sent,
            bytes_received: final_stats.bytes_received,
            audio_underruns,
        }
    }
}

/// 一場連線的摘要（連線結束時顯示給使用者）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MatchSummary {
    pub mode: Mode,
    pub duration: Duration,
    /// 已確認（lockstep：已完成）的幀數。
    pub frames: u32,
    pub ping_avg_ms: Option<f64>,
    pub ping_max_ms: Option<f64>,
    pub rollbacks: Option<u32>,
    pub max_depth: Option<u32>,
    pub prediction_accuracy: Option<f32>,
    pub stalls: u32,
    pub stall_time: Duration,
    pub bytes_sent: u64,
    pub bytes_received: u64,
    pub audio_underruns: u64,
}

impl fmt::Display for MatchSummary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let secs = self.duration.as_secs();
        writeln!(f, "模式：{}", self.mode)?;
        writeln!(
            f,
            "時長：{}:{:02}（{:.1} 秒）｜總幀數：{}",
            secs / 60,
            secs % 60,
            self.duration.as_secs_f64(),
            self.frames
        )?;
        match (self.ping_avg_ms, self.ping_max_ms) {
            (Some(a), Some(m)) => writeln!(f, "ping：平均 {a:.1} ms／最大 {m:.1} ms")?,
            _ => writeln!(f, "ping：沒有量到")?,
        }
        if let (Some(n), Some(d)) = (self.rollbacks, self.max_depth) {
            let acc = self
                .prediction_accuracy
                .map_or("—".to_string(), |a| format!("{:.1}%", a * 100.0));
            writeln!(f, "rollback：{n} 次（最大重跑深度 {d}）｜預測準確率 {acc}")?;
        }
        writeln!(
            f,
            "stall：{} 次（{:.2} 秒）｜傳送 {} B／接收 {} B｜音訊 underrun {} 次",
            self.stalls,
            self.stall_time.as_secs_f64(),
            self.bytes_sent,
            self.bytes_received,
            self.audio_underruns
        )
    }
}

// ---- 讀回與摘要 ------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CsvError {
    #[error("第一行不是預期的標題列（欄位名稱或順序不同）")]
    BadHeader,
    #[error("第 {line} 行：{message}")]
    BadRow { line: usize, message: String },
}

/// 整份 CSV 文字（含標題列）→ 每秒的紀錄。空行略過。
pub fn parse_csv(text: &str) -> Result<Vec<StatsRow>, CsvError> {
    let mut lines = text.lines().enumerate();
    match lines.next() {
        Some((_, first)) if first.trim_start_matches('\u{feff}').trim() == CSV_HEADER => {}
        _ => return Err(CsvError::BadHeader),
    }
    let mut rows = Vec::new();
    for (i, line) in lines {
        if line.trim().is_empty() {
            continue;
        }
        rows.push(StatsRow::parse(line).map_err(|message| CsvError::BadRow {
            line: i + 1,
            message,
        })?);
    }
    Ok(rows)
}

/// 一個指標在整場的統計。
#[derive(Debug, Clone, PartialEq)]
pub struct MetricSummary {
    pub name: &'static str,
    pub unit: &'static str,
    /// 有值的秒數（空欄位不計）。
    pub samples: usize,
    pub avg: f64,
    pub min: f64,
    pub p50: f64,
    pub p95: f64,
    pub max: f64,
}

/// 最近排名法（nearest-rank）百分位數；`sorted` 必須已由小到大排序且非空。
pub fn percentile(sorted: &[f64], p: f64) -> f64 {
    let n = sorted.len();
    let rank = ((p / 100.0) * n as f64).ceil().clamp(1.0, n as f64) as usize;
    sorted[rank - 1]
}

/// 每個指標的平均、最小、p50、p95、最大（完全沒有值的指標不列出）。
pub fn summarize(rows: &[StatsRow]) -> Vec<MetricSummary> {
    let mut out = Vec::new();
    for (i, &(name, unit)) in METRICS.iter().enumerate() {
        let mut v: Vec<f64> = rows.iter().filter_map(|r| r.values()[i]).collect();
        if v.is_empty() {
            continue;
        }
        v.sort_by(f64::total_cmp);
        out.push(MetricSummary {
            name,
            unit,
            samples: v.len(),
            avg: v.iter().sum::<f64>() / v.len() as f64,
            min: v[0],
            p50: percentile(&v, 50.0),
            p95: percentile(&v, 95.0),
            max: v[v.len() - 1],
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rollback::RollbackStats;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    fn rb_stats(frame: u32, rollbacks: u32, resim_frames: u64) -> Stats {
        Stats {
            mode: Mode::Rollback,
            rtt: Some(ms(40)),
            frame,
            rollback: Some(RollbackStats {
                rollbacks,
                resim_frames,
                resim_time_total: ms(u64::from(rollbacks) * 2),
                resims_timed: rollbacks,
                window_max_depth: 3,
                prediction_correct: 9,
                prediction_wrong: 1,
                frame_advantage: 0.5,
                ..RollbackStats::default()
            }),
            send_bytes_per_sec: 1000,
            recv_bytes_per_sec: 900,
            ..Stats::default()
        }
    }

    #[test]
    fn rows_are_per_second_deltas_computed_on_a_virtual_clock() {
        let mut rec = StatsRecorder::new(ms(500));
        // 連線後 1 秒：60 幀、4 次 rollback（共重跑 10 幀）、2 次 stall、underrun 累計 1。
        let mut s1 = rb_stats(60, 4, 10);
        s1.stalls = 2;
        s1.stall_time = ms(30);
        let r1 = rec.push(ms(1500), &s1, 1);
        assert_eq!(r1.time_s, 1.0);
        assert_eq!(r1.fps, 60.0);
        assert_eq!(r1.rollbacks, Some(4));
        assert_eq!(r1.resim_depth_avg, Some(2.5));
        assert_eq!(r1.resim_depth_max, Some(3));
        assert_eq!(r1.resim_ms_avg, Some(2.0));
        assert_eq!((r1.stalls, r1.stall_ms), (2, 30.0));
        assert_eq!(r1.ping_ms, Some(40.0));
        assert_eq!(r1.prediction_accuracy_pct, Some(90.0));
        assert_eq!(r1.audio_underruns, 1);
        // 下一秒：只多 58 幀、1 次 rollback（深度 1）、沒有新的 stall、underrun 沒增加。
        let mut s2 = rb_stats(118, 5, 11);
        s2.stalls = 2;
        s2.stall_time = ms(30);
        let r2 = rec.push(ms(2500), &s2, 1);
        assert_eq!(r2.time_s, 2.0);
        assert_eq!(r2.fps, 58.0);
        assert_eq!(r2.rollbacks, Some(1));
        assert_eq!(r2.resim_depth_avg, Some(1.0));
        assert_eq!((r2.stalls, r2.stall_ms), (0, 0.0));
        assert_eq!(r2.audio_underruns, 0);
        // 第三秒：沒有 rollback → 深度與耗時是空的（不是 0，否則會拉低平均）。
        let r3 = rec.push(ms(3500), &rb_stats(178, 5, 11), 1);
        assert_eq!(r3.rollbacks, Some(0));
        assert_eq!((r3.resim_depth_avg, r3.resim_ms_avg), (None, None));
    }

    #[test]
    fn lockstep_rows_leave_the_rollback_columns_empty() {
        let mut rec = StatsRecorder::new(Duration::ZERO);
        let s = Stats {
            mode: Mode::Lockstep,
            frame: 30,
            rtt: None,
            ..Stats::default()
        };
        let row = rec.push(ms(1000), &s, 0);
        assert_eq!(row.fps, 30.0);
        assert_eq!(row.ping_ms, None);
        let line = row.to_csv_line();
        assert_eq!(line, "1.0,lockstep,,30.00,,,,,0,0.0,,,0,0,0");
        assert_eq!(StatsRow::parse(&line).unwrap().rollbacks, None);
    }

    #[test]
    fn csv_round_trips_and_the_header_matches_the_row_format() {
        let mut rec = StatsRecorder::new(Duration::ZERO);
        let mut text = format!("{CSV_HEADER}\n");
        let mut rows = Vec::new();
        for i in 1..=20u32 {
            let row = rec.push(
                Duration::from_secs(u64::from(i)),
                &rb_stats(58 * i, i * 3, u64::from(i) * 7),
                u64::from(i / 5),
            );
            text.push_str(&row.to_csv_line());
            text.push('\n');
            rows.push(row);
        }
        assert_eq!(CSV_HEADER.split(',').count(), 15);
        assert_eq!(CSV_HEADER.split(',').skip(2).count(), METRICS.len());
        let parsed = parse_csv(&text).unwrap();
        assert_eq!(parsed.len(), 20);
        // 寫出 → 解析 → 再寫出：位元組相同（固定小數位數，沒有漂移）。
        let again: String = std::iter::once(CSV_HEADER.to_string())
            .chain(parsed.iter().map(StatsRow::to_csv_line))
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        assert_eq!(again, text);
        for (a, b) in rows.iter().zip(&parsed) {
            assert_eq!(a.rollbacks, b.rollbacks);
            assert_eq!(a.audio_underruns, b.audio_underruns);
            assert!((a.fps - b.fps).abs() < 0.005);
        }
    }

    #[test]
    fn summary_uses_nearest_rank_percentiles_and_skips_empty_cells() {
        let mk = |ping: Option<f64>, fps: f64| StatsRow {
            time_s: 0.0,
            mode: Mode::Rollback,
            ping_ms: ping,
            fps,
            rollbacks: None,
            resim_depth_avg: None,
            resim_depth_max: None,
            resim_ms_avg: None,
            stalls: 0,
            stall_ms: 0.0,
            frame_advantage: None,
            prediction_accuracy_pct: None,
            send_bytes_per_s: 0,
            recv_bytes_per_s: 0,
            audio_underruns: 0,
        };
        // 100 筆 ping：1..=100 ms；另有 10 筆空的（不計）。
        let mut rows: Vec<StatsRow> = (1..=100).map(|i| mk(Some(f64::from(i)), 60.0)).collect();
        rows.extend((0..10).map(|_| mk(None, 60.0)));
        let sum = summarize(&rows);
        let ping = sum.iter().find(|m| m.name == "ping_ms").unwrap();
        assert_eq!(ping.samples, 100);
        assert_eq!(ping.avg, 50.5);
        assert_eq!((ping.min, ping.max), (1.0, 100.0));
        assert_eq!(ping.p50, 50.0);
        assert_eq!(ping.p95, 95.0);
        let fps = sum.iter().find(|m| m.name == "fps").unwrap();
        assert_eq!((fps.samples, fps.avg, fps.p95), (110, 60.0, 60.0));
        assert!(
            sum.iter().all(|m| m.name != "rollbacks"),
            "完全沒有值的指標不列出"
        );
        assert_eq!(percentile(&[7.0], 95.0), 7.0);
    }

    #[test]
    fn malformed_csv_is_reported_with_the_line_number_and_never_panics() {
        assert_eq!(parse_csv(""), Err(CsvError::BadHeader));
        assert_eq!(parse_csv("a,b,c\n"), Err(CsvError::BadHeader));
        let bad = format!("{CSV_HEADER}\n1.0,rollback,x\n");
        assert!(matches!(
            parse_csv(&bad),
            Err(CsvError::BadRow { line: 2, .. })
        ));
        let bad_num = format!("{CSV_HEADER}\n1.0,rollback,,abc,,,,,0,0.0,,,0,0,0\n");
        let err = parse_csv(&bad_num).unwrap_err().to_string();
        assert!(err.contains("fps"), "{err}");
        // 只有標題列＝合法的空紀錄。
        assert_eq!(parse_csv(&format!("{CSV_HEADER}\n\n")).unwrap(), vec![]);
    }

    #[test]
    fn match_summary_aggregates_ping_and_prints_readably() {
        let mut rec = StatsRecorder::new(Duration::ZERO);
        for (i, rtt) in [20u64, 40, 90].into_iter().enumerate() {
            let mut s = rb_stats(60 * (i as u32 + 1), 1, 2);
            s.rtt = Some(ms(rtt));
            rec.push(Duration::from_secs(i as u64 + 1), &s, 0);
        }
        let last = rb_stats(190, 7, 20);
        let sum = rec.summary(Duration::from_secs(3), &last, 2);
        assert_eq!(sum.ping_avg_ms, Some(50.0));
        assert_eq!(sum.ping_max_ms, Some(90.0));
        assert_eq!(
            (sum.frames, sum.rollbacks, sum.audio_underruns),
            (190, Some(7), 2)
        );
        let text = sum.to_string();
        assert!(
            text.contains("模式：rollback") && text.contains("總幀數：190"),
            "{text}"
        );
        assert!(
            text.contains("最大 90.0 ms") && text.contains("rollback：7 次"),
            "{text}"
        );
    }
}
