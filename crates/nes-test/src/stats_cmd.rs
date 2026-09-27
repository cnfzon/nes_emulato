//! `nes-test stats-summary`：讀取一個或多個連線統計 CSV（`nes-app` 每場連線寫出的每秒一筆紀錄，
//! 欄位與單位見 `nes_net::statslog`），輸出每個指標的平均、最小、p50、p95、最大值的 Markdown 表格，
//! 可以直接貼進期末報告。多個檔案（例如有線／Wi-Fi × lockstep／rollback 四場）合成**一張表**，
//! 每個檔案的每個指標一列。
//!
//! 百分位數用最近排名法（nearest-rank）；空的欄位（例如 lockstep 沒有 rollback 欄位、還沒量到的 ping）
//! 不計入樣本數。結束碼：成功 0、CSV 讀不了或格式不符 2。

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use nes_net::statslog::{METRICS, StatsRow, parse_csv, summarize};

/// 一個檔案的紀錄。
#[derive(Debug)]
pub struct Loaded {
    pub label: String,
    pub rows: Vec<StatsRow>,
}

fn label_of(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    )
}

/// 讀入並解析（錯誤訊息帶檔名與行號）。
pub fn load(path: &Path) -> Result<Loaded, String> {
    let text =
        std::fs::read_to_string(path).map_err(|e| format!("無法讀取 {}：{e}", path.display()))?;
    let rows = parse_csv(&text).map_err(|e| format!("{}：{e}", path.display()))?;
    Ok(Loaded {
        label: label_of(path),
        rows,
    })
}

/// 把 `--metrics a,b,c` 換成指標名稱清單；有不認得的名稱就回傳錯誤（列出所有合法的名稱）。
pub fn parse_metric_filter(text: &str) -> Result<Vec<String>, String> {
    let wanted: Vec<String> = text
        .split(',')
        .map(|m| m.trim().to_string())
        .filter(|m| !m.is_empty())
        .collect();
    for w in &wanted {
        if !METRICS.iter().any(|(name, _)| name == w) {
            let names: Vec<&str> = METRICS.iter().map(|(n, _)| *n).collect();
            return Err(format!(
                "未知的指標「{w}」。可用的指標：{}",
                names.join("、")
            ));
        }
    }
    Ok(wanted)
}

fn num(v: f64) -> String {
    if v.abs() >= 1000.0 {
        format!("{v:.0}")
    } else {
        format!("{v:.2}")
    }
}

/// 一張 Markdown 表：每個檔案的每個（有值的）指標一列。
pub fn render(files: &[Loaded], only: &[String]) -> String {
    let mut out = String::new();
    out.push_str("| 檔案 | 模式 | 秒數 | 指標 | 單位 | 樣本 | 平均 | 最小 | p50 | p95 | 最大 |\n");
    out.push_str("|---|---|---|---|---|---|---|---|---|---|---|\n");
    for f in files {
        let mode = f
            .rows
            .first()
            .map_or("—".to_string(), |r| r.mode.to_string());
        let seconds = f.rows.last().map_or(0.0, |r| r.time_s);
        let summary = summarize(&f.rows);
        let mut printed = false;
        for m in summary
            .iter()
            .filter(|m| only.is_empty() || only.iter().any(|o| o == m.name))
        {
            printed = true;
            out.push_str(&format!(
                "| {} | {mode} | {seconds:.0} | {} | {} | {} | {} | {} | {} | {} | {} |\n",
                f.label,
                m.name,
                m.unit,
                m.samples,
                num(m.avg),
                num(m.min),
                num(m.p50),
                num(m.p95),
                num(m.max),
            ));
        }
        if !printed {
            out.push_str(&format!(
                "| {} | {mode} | {seconds:.0} | （沒有資料） | | 0 | | | | | |\n",
                f.label
            ));
        }
    }
    out
}

pub fn run(paths: &[PathBuf], metrics: Option<&str>) -> ExitCode {
    let only = match metrics.map(parse_metric_filter).transpose() {
        Ok(o) => o.unwrap_or_default(),
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(2);
        }
    };
    let mut files = Vec::new();
    for path in paths {
        match load(path) {
            Ok(f) => files.push(f),
            Err(e) => {
                eprintln!("{e}");
                return ExitCode::from(2);
            }
        }
    }
    print!("{}", render(&files, &only));
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use nes_net::statslog::CSV_HEADER;
    use nes_net::{Mode, RollbackStats, Stats, StatsRecorder};

    /// 用虛擬時鐘合成一場連線的每秒統計，寫成 CSV 文字（與 `nes-app` 寫出的完全同一條路徑）。
    fn csv_of(mode: Mode, seconds: u32, ping_ms: u64, rollbacks_per_sec: u32) -> String {
        let mut rec = StatsRecorder::new(Duration::ZERO);
        let mut text = format!("{CSV_HEADER}\n");
        for i in 1..=seconds {
            let rollback = (mode == Mode::Rollback).then(|| RollbackStats {
                rollbacks: i * rollbacks_per_sec,
                resim_frames: u64::from(i * rollbacks_per_sec * 2),
                window_max_depth: 3,
                prediction_correct: 90,
                prediction_wrong: 10,
                ..RollbackStats::default()
            });
            let stats = Stats {
                mode,
                rtt: Some(Duration::from_millis(ping_ms + u64::from(i % 5))),
                frame: 60 * i,
                send_bytes_per_sec: 1800,
                recv_bytes_per_sec: 1700,
                rollback,
                ..Stats::default()
            };
            let row = rec.push(Duration::from_secs(u64::from(i)), &stats, 0);
            text.push_str(&row.to_csv_line());
            text.push('\n');
        }
        text
    }

    fn write_temp(tag: &str, content: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("nes-stats-{tag}-{}.csv", std::process::id()));
        std::fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn four_matches_become_one_table_with_avg_p50_p95_max() {
        // 有線／Wi-Fi × lockstep／rollback 的四場（合成資料）。
        let specs = [
            ("wired-lockstep", Mode::Lockstep, 2, 0),
            ("wired-rollback", Mode::Rollback, 2, 1),
            ("wifi-lockstep", Mode::Lockstep, 12, 0),
            ("wifi-rollback", Mode::Rollback, 12, 4),
        ];
        let paths: Vec<PathBuf> = specs
            .iter()
            .map(|&(tag, mode, ping, rb)| write_temp(tag, &csv_of(mode, 100, ping, rb)))
            .collect();
        let files: Vec<Loaded> = paths.iter().map(|p| load(p).unwrap()).collect();
        let table = render(&files, &[]);
        for p in &paths {
            std::fs::remove_file(p).ok();
        }

        // 標題列與 4 個檔案都在同一張表裡。
        assert!(table.starts_with("| 檔案 | 模式 |"), "{table}");
        assert!(table.contains("| 平均 | 最小 | p50 | p95 | 最大 |"));
        for (tag, mode, _, _) in specs {
            assert!(
                table.contains(tag) && table.contains(&format!("| {mode} |")),
                "{tag}\n{table}"
            );
        }
        // 有線 ping：2 + (i % 5) → 2..=6 ms，100 秒；p50/p95/最大可以手算。
        let row = table
            .lines()
            .find(|l| l.contains("wired-rollback") && l.contains("| ping_ms |"))
            .unwrap();
        let cells: Vec<&str> = row.split('|').map(str::trim).collect();
        // cells: ["", 檔案, 模式, 秒數, 指標, 單位, 樣本, 平均, 最小, p50, p95, 最大, ""]
        assert_eq!(cells[6], "100");
        assert_eq!(cells[7], "4.00", "{row}"); // 平均：(2+3+4+5+6)/5 = 4
        assert_eq!(
            (cells[8], cells[9], cells[10], cells[11]),
            ("2.00", "4.00", "6.00", "6.00"),
            "{row}"
        );
        // lockstep 沒有 rollback 欄位；rollback 有（wifi-rollback 每秒 4 次）。
        assert!(
            !table
                .lines()
                .any(|l| l.contains("wired-lockstep") && l.contains("| rollbacks |"))
        );
        let rb = table
            .lines()
            .find(|l| l.contains("wifi-rollback") && l.contains("| rollbacks |"))
            .unwrap();
        assert!(rb.contains("| 4.00 | 4.00 | 4.00 | 4.00 | 4.00 |"), "{rb}");
    }

    #[test]
    fn the_metric_filter_keeps_only_the_requested_rows_and_rejects_typos() {
        let path = write_temp("filter", &csv_of(Mode::Rollback, 10, 30, 2));
        let files = [load(&path).unwrap()];
        std::fs::remove_file(&path).ok();
        let only = parse_metric_filter("ping_ms, fps").unwrap();
        let table = render(&files, &only);
        assert_eq!(table.lines().count(), 2 + 2, "{table}");
        assert!(table.contains("| ping_ms |") && table.contains("| fps |"));
        let err = parse_metric_filter("ping").unwrap_err();
        assert!(
            err.contains("未知的指標") && err.contains("ping_ms"),
            "{err}"
        );
    }

    #[test]
    fn unreadable_and_malformed_files_are_reported_with_the_path() {
        let missing = std::env::temp_dir().join("nes-stats-missing-file.csv");
        assert!(
            load(&missing)
                .unwrap_err()
                .contains("nes-stats-missing-file.csv")
        );
        let bad = write_temp("bad", "not,a,stats,file\n1,2,3\n");
        let err = load(&bad).err().unwrap();
        std::fs::remove_file(&bad).ok();
        assert!(err.contains("標題列"), "{err}");
        let bad_row = write_temp("badrow", &format!("{CSV_HEADER}\n1.0,rollback,x\n"));
        let err = load(&bad_row).err().unwrap();
        std::fs::remove_file(&bad_row).ok();
        assert!(err.contains("第 2 行"), "{err}");
    }

    #[test]
    fn an_empty_csv_prints_a_placeholder_row_instead_of_crashing() {
        let path = write_temp("empty", &format!("{CSV_HEADER}\n"));
        let files = [load(&path).unwrap()];
        std::fs::remove_file(&path).ok();
        let table = render(&files, &[]);
        assert!(table.contains("（沒有資料）"), "{table}");
    }
}
