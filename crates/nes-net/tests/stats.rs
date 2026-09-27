//! Phase 4d 第 6d 項：統計的計算，全部用虛擬時鐘（由模擬網路的條件決定「正確答案」）。
//!
//! 模擬網路的單程延遲、丟包率是已知的，所以 ping、每秒 rollback 次數、預測準確率、stall、頻寬、幀率
//! 都有可以獨立推算的預期值。統計事件（`Event::Stats`）每個虛擬秒一筆，餵給 [`StatsRecorder`] 就是
//! 連線中寫出的 CSV；再讀回、摘要，就是 `nes-test stats-summary` 做的事（往返測試）。

use std::sync::OnceLock;
use std::time::Duration;

use nes_core::test_support::input_probe_rom;
use nes_net::sim::{EndReport, MatchConfig, MatchReport, run_match};
use nes_net::statslog::{CSV_HEADER, StatsRow, parse_csv, summarize};
use nes_net::{EndReason, Mode, NetworkConfig, StatsRecorder};

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

fn rom() -> &'static [u8] {
    static ROM: OnceLock<Vec<u8>> = OnceLock::new();
    ROM.get_or_init(input_probe_rom)
}

fn play(cfg: &MatchConfig) -> MatchReport {
    let expected = cfg.expected(rom()).unwrap();
    run_match(rom(), cfg, &expected).unwrap()
}

fn net(loss: f64, delay_ms: u64, jitter_ms: u64) -> NetworkConfig {
    NetworkConfig {
        loss,
        delay: ms(delay_ms),
        jitter: ms(jitter_ms),
        duplicate: 0.0,
    }
}

/// 一端的統計事件 → CSV 的每秒紀錄。
fn rows_of(end: &EndReport) -> Vec<StatsRow> {
    let mut rec = StatsRecorder::new(end.connected_at.expect("必須連上過"));
    end.stats_log
        .iter()
        .map(|(now, stats)| rec.push(*now, stats, 0))
        .collect()
}

#[test]
fn ping_is_twice_the_one_way_delay_in_both_modes() {
    for (mode, delay) in [(Mode::Lockstep, 25u64), (Mode::Rollback, 50)] {
        let cfg = MatchConfig {
            frames: 900,
            mode,
            network: net(0.0, delay, 0),
            input_delay: if mode == Mode::Rollback { 2 } else { 4 },
            ..MatchConfig::default()
        };
        let r = play(&cfg);
        assert!(r.equivalent(), "{mode}");
        for (i, end) in r.ends.iter().enumerate() {
            let rtt = end.stats.rtt.expect("跑了 15 秒必須量得到 ping");
            let want = ms(2 * delay);
            assert!(
                rtt >= want && rtt <= want + ms(15),
                "{mode} 端點 {i}：RTT {rtt:?}，模擬網路的往返是 {want:?}"
            );
            // 每一秒的紀錄也都是這個值（平滑後）。
            let rows = rows_of(end);
            let last = rows.last().unwrap().ping_ms.unwrap();
            assert!((last - want.as_secs_f64() * 1000.0).abs() < 15.0, "{last}");
        }
    }
}

#[test]
fn rollback_rows_add_up_to_the_final_counters_and_are_consistent() {
    // 每次取樣都換按鍵（最壞情況的預測失誤）＋ 10% 丟包、單程 100 ms。
    let cfg = MatchConfig {
        frames: 900,
        mode: Mode::Rollback,
        network: net(0.10, 100, 30),
        input_delay: 1,
        input_hold: 1,
        ..MatchConfig::default()
    };
    let r = play(&cfg);
    assert!(r.equivalent());
    for (i, end) in r.ends.iter().enumerate() {
        let rows = rows_of(end);
        assert!(rows.len() >= 10, "端點 {i}：只有 {} 筆", rows.len());
        let last = end.stats_log.last().unwrap().1;
        let rb = last.rollback.unwrap();
        // 每秒的「新增」加起來，正好是最後一筆統計的累計值（沒有漏算、沒有重複）。
        let sum_rollbacks: u32 = rows.iter().filter_map(|r| r.rollbacks).sum();
        assert_eq!(sum_rollbacks, rb.rollbacks, "端點 {i}：rollback 次數");
        assert!(rb.rollbacks > 0, "這個條件下一定有 rollback");
        let sum_stalls: u32 = rows.iter().map(|r| r.stalls).sum();
        assert_eq!(sum_stalls, last.stalls, "端點 {i}：stall 次數");
        let stall_ms: f64 = rows.iter().map(|r| r.stall_ms).sum();
        let want = last.stall_time.as_secs_f64() * 1000.0;
        assert!(
            (stall_ms - want).abs() < 1.0,
            "端點 {i}：{stall_ms} vs {want}"
        );
        // 重跑深度：不超過預測視窗 K（8），最大值出現在某一秒；平均不超過同一秒的最大值。
        let max_depth = rows.iter().filter_map(|r| r.resim_depth_max).max().unwrap();
        assert!(
            (1..=8).contains(&max_depth),
            "端點 {i}：最大重跑深度 {max_depth}"
        );
        assert_eq!(
            max_depth, rb.max_depth,
            "端點 {i}：每秒最大值的最大值＝整場最大值"
        );
        for row in &rows {
            if let (Some(avg), Some(max)) = (row.resim_depth_avg, row.resim_depth_max) {
                assert!(avg >= 1.0 && avg <= f64::from(max), "{row:?}");
            }
            if row.rollbacks == Some(0) {
                assert_eq!(
                    row.resim_depth_avg, None,
                    "沒有 rollback 的一秒，深度是空的"
                );
            }
        }
        // 預測準確率：有預測錯誤（丟包＋每次取樣都換按鍵），但不是全錯。
        let acc = rows.last().unwrap().prediction_accuracy_pct.unwrap();
        assert!(acc > 0.0 && acc < 100.0, "端點 {i}：預測準確率 {acc}");
        let want = rb.prediction_accuracy().unwrap() * 100.0;
        assert!((acc as f32 - want).abs() < 0.1, "{acc} vs {want}");
        // ping ≈ 2 × 100 ms（±抖動與丟包造成的平滑誤差）。
        let ping = rows.last().unwrap().ping_ms.unwrap();
        assert!((150.0..=260.0).contains(&ping), "端點 {i}：ping {ping}");
        // 頻寬（UDP payload）有值，量級與 4b／4c 的實測（約 1–2 KB/s）相符。
        let send: f64 = rows
            .iter()
            .map(|r| f64::from(r.send_bytes_per_s))
            .sum::<f64>()
            / rows.len() as f64;
        assert!(
            (300.0..=6000.0).contains(&send),
            "端點 {i}：平均送出 {send} B/s"
        );
        // 幀率：確認幀的推進速度，有 rollback 時仍接近 60（確認幀落後但速率一樣），不是 0 也不會 > 65。
        let mean_fps: f64 = rows.iter().map(|r| r.fps).sum::<f64>() / rows.len() as f64;
        assert!(
            (50.0..=65.0).contains(&mean_fps),
            "端點 {i}：平均 fps {mean_fps}"
        );
    }
}

#[test]
fn an_ideal_network_has_sixty_fps_no_rollbacks_and_no_stalls() {
    for mode in [Mode::Lockstep, Mode::Rollback] {
        let cfg = MatchConfig {
            frames: 900,
            mode,
            input_delay: 2,
            ..MatchConfig::default()
        };
        let r = play(&cfg);
        assert!(r.equivalent(), "{mode}");
        for end in &r.ends {
            let rows = rows_of(end);
            // 第一秒可能有開機時的等待；之後每一秒都是標準幀率（60.0988）。
            for row in rows.iter().skip(1) {
                assert!((row.fps - 60.1).abs() < 2.0, "{mode}：{row:?}");
                assert_eq!(row.stalls, 0, "{mode}：{row:?}");
            }
            if mode == Mode::Rollback {
                // 理想網路（零延遲）：預測永遠正確（沒有輸入變化造成的錯誤）或至少很少 rollback。
                let total: u32 = rows.iter().filter_map(|r| r.rollbacks).sum();
                assert!(total <= 20, "理想網路的 rollback 次數 {total}");
            } else {
                assert!(
                    rows.iter().all(|r| r.rollbacks.is_none()),
                    "lockstep 沒有 rollback 欄位"
                );
            }
        }
    }
}

#[test]
fn silence_is_reported_growing_before_the_five_second_timeout() {
    let cfg = MatchConfig {
        frames: 1800,
        mode: Mode::Rollback,
        input_delay: 1,
        network: net(0.0, 20, 0),
        blackout_at: Some(Duration::from_secs(4)),
        ..MatchConfig::default()
    };
    let r = play(&cfg);
    for end in &r.ends {
        assert_eq!(end.end_reason, Some(EndReason::Timeout));
        let log = &end.stats_log;
        // 斷網之前：沒有沉默；斷網之後：每秒一筆的 `silent_for` 逐秒增加，超過 5 秒就結束。
        let before: Vec<_> = log.iter().filter(|(t, _)| *t < ms(3900)).collect();
        assert!(!before.is_empty());
        assert!(
            before.iter().all(|(_, s)| s.silent_for < ms(200)),
            "{before:?}"
        );
        let after: Vec<Duration> = log
            .iter()
            .filter(|(t, _)| *t > ms(4200))
            .map(|(_, s)| s.silent_for)
            .collect();
        assert!(after.len() >= 3, "{after:?}");
        assert!(
            after.windows(2).all(|w| w[1] > w[0]),
            "沉默時間逐秒增加：{after:?}"
        );
        assert!(
            *after.last().unwrap() >= Duration::from_secs(3)
                && *after.last().unwrap() <= Duration::from_secs(5),
            "{after:?}"
        );
    }
}

#[test]
fn a_whole_matchs_csv_round_trips_through_parse_and_summarize() {
    let cfg = MatchConfig {
        frames: 1200,
        mode: Mode::Rollback,
        network: net(0.10, 60, 20),
        input_delay: 1,
        input_hold: 2,
        ..MatchConfig::default()
    };
    let r = play(&cfg);
    let rows = rows_of(&r.ends[0]);
    let mut text = format!("{CSV_HEADER}\n");
    for row in &rows {
        text.push_str(&row.to_csv_line());
        text.push('\n');
    }
    let parsed = parse_csv(&text).expect("自己寫出的 CSV 必須讀得回來");
    assert_eq!(parsed.len(), rows.len());
    let (a, b) = (summarize(&rows), summarize(&parsed));
    assert_eq!(a.len(), b.len());
    for (x, y) in a.iter().zip(&b) {
        assert_eq!((x.name, x.samples), (y.name, y.samples));
        // 寫出時固定小數位數，所以只有四捨五入的差。
        let tol = 0.01 * x.max.abs().max(1.0);
        for (p, q) in [
            (x.avg, y.avg),
            (x.min, y.min),
            (x.p50, y.p50),
            (x.p95, y.p95),
            (x.max, y.max),
        ] {
            assert!((p - q).abs() <= tol, "{}：{p} vs {q}", x.name);
        }
    }
    let ping = b.iter().find(|m| m.name == "ping_ms").unwrap();
    assert!(
        ping.p50 > 100.0 && ping.p95 >= ping.p50 && ping.max >= ping.p95,
        "{ping:?}"
    );
}
