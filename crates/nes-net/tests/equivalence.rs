//! Phase 4b 最重要的測試：**等價性**。
//!
//! lockstep 連線的正確性，歸約到 Phase 4a 已驗證的 replay 正確性：
//!
//! 1. 兩端的行為指紋逐幀相同；
//! 2. 而且等於「把雙方輸入合併成一份 replay，離線重播」的結果——這份標準答案由
//!    [`expected_log`] 從雙方腳本與 input delay **獨立**算出，完全不經過網路與 session；
//! 3. 兩端各自記錄的 replay（每幀一個檢查點）與離線的 replay 位元組完全相同，
//!    並且能通過 `replay::verify`。
//!
//! 網路由虛擬時鐘模擬（`SimulatedTransport`）：不 sleep、不讀系統時間，結果只由種子決定。
//! 每種網路條件跑 20 組不同的種子（各 3600 幀，約 60 秒的遊戲時間）。
//!
//! 慢的部分是 `Nes` 的模擬本身（每組兩個實例 × 3600 幀），所以各組種子分散到多個執行緒。

use std::sync::OnceLock;
use std::time::Duration;

use nes_core::replay::verify;
use nes_core::test_support::input_probe_rom;
use nes_net::NetworkConfig;
use nes_net::matchlog::MatchLog;
use nes_net::sim::{MatchConfig, MatchReport, Tamper, expected_log, run_match};

const FRAMES: u32 = 3600;
const SEEDS: u64 = 20;

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

fn rom() -> &'static [u8] {
    static ROM: OnceLock<Vec<u8>> = OnceLock::new();
    ROM.get_or_init(input_probe_rom)
}

fn expected(input_delay: u8) -> &'static MatchLog {
    static LOGS: [OnceLock<MatchLog>; 9] = [const { OnceLock::new() }; 9];
    LOGS[usize::from(input_delay)].get_or_init(|| {
        expected_log(
            rom(),
            MatchConfig::default().script_seed,
            input_delay,
            FRAMES,
        )
        .unwrap()
    })
}

pub fn condition_ideal() -> NetworkConfig {
    NetworkConfig::IDEAL
}

pub fn condition_lossy() -> NetworkConfig {
    NetworkConfig {
        loss: 0.10,
        delay: ms(100),
        jitter: ms(30),
        duplicate: 0.0,
    }
}

pub fn condition_terrible() -> NetworkConfig {
    NetworkConfig {
        loss: 0.30,
        delay: ms(200),
        jitter: ms(80),
        duplicate: 0.05,
    }
}

/// 對 `seeds` 個種子各跑一場，分散到多個執行緒（結果依種子排序，與執行緒數量無關）。
fn run_seeds(base: &MatchConfig, seeds: u64) -> Vec<MatchReport> {
    let expected = expected(base.input_delay);
    let threads = std::thread::available_parallelism()
        .map_or(4, |n| n.get())
        .min(8);
    let mut reports: Vec<(u64, MatchReport)> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..threads as u64)
            .map(|t| {
                scope.spawn(move || {
                    (0..seeds)
                        .filter(|s| s % threads as u64 == t)
                        .map(|seed| {
                            let cfg = MatchConfig {
                                seed: 1000 + seed,
                                ..base.clone()
                            };
                            (seed, run_match(rom(), &cfg, expected).unwrap())
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().unwrap())
            .collect()
    });
    reports.sort_by_key(|(seed, _)| *seed);
    reports.into_iter().map(|(_, r)| r).collect()
}

fn summarize(name: &str, reports: &[MatchReport]) -> String {
    let n = reports.len() as f64;
    let stalls = reports
        .iter()
        .map(|r| f64::from(r.total_stalls()) / 2.0)
        .sum::<f64>()
        / n;
    let stall_ms = reports
        .iter()
        .flat_map(|r| &r.ends)
        .map(|e| e.stats.stall_time.as_secs_f64() * 1000.0)
        .sum::<f64>()
        / (n * 2.0);
    let elapsed = reports
        .iter()
        .map(|r| r.virtual_elapsed.as_secs_f64())
        .sum::<f64>()
        / n;
    let bw = |f: fn(&nes_net::Stats) -> u64| {
        reports
            .iter()
            .flat_map(|r| {
                r.ends
                    .iter()
                    .map(|e| f(&e.stats) as f64 / r.virtual_elapsed.as_secs_f64())
            })
            .sum::<f64>()
            / (n * 2.0)
    };
    format!(
        "{name}: 等價 {}/{}，平均每端 stall {stalls:.1} 次 / {stall_ms:.0} ms，耗時 {elapsed:.1} 秒（虛擬），頻寬 送 {:.0} B/s 收 {:.0} B/s",
        reports.iter().filter(|r| r.equivalent()).count(),
        reports.len(),
        bw(|s| s.bytes_sent),
        bw(|s| s.bytes_received),
    )
}

fn assert_all_equivalent(name: &str, base: &MatchConfig) -> Vec<MatchReport> {
    let reports = run_seeds(base, SEEDS);
    println!("{}", summarize(name, &reports));
    for (seed, r) in reports.iter().enumerate() {
        assert!(!r.timed_out, "{name} 種子 {seed}：逾時");
        for (i, e) in r.ends.iter().enumerate() {
            assert_eq!(
                e.frames, FRAMES,
                "{name} 種子 {seed} 端點 {i} 沒跑完；事件 {:?}",
                e.events
            );
        }
        assert_eq!(
            r.a_vs_b, None,
            "{name} 種子 {seed}：兩端指紋在第 {:?} 幀分歧",
            r.a_vs_b
        );
        assert_eq!(
            r.vs_expected,
            [None, None],
            "{name} 種子 {seed}：與離線重播不同"
        );
        assert_eq!(r.desync_events(), 0, "{name} 種子 {seed}");
        assert!(r.equivalent());
    }
    reports
}

/// 兩端各自的 replay（每幀一個檢查點）＝離線 replay（位元組完全相同），且離線 replay 通過 verify。
fn assert_replays_identical(reports: &[MatchReport], input_delay: u8) {
    let offline = expected(input_delay).to_replay(FRAMES, 1);
    verify(rom(), &offline).expect("離線 replay 必須通過 verify");
    let offline_bytes = offline.encode();
    for (seed, r) in reports.iter().enumerate() {
        for (i, e) in r.ends.iter().enumerate() {
            assert_eq!(
                e.log.to_replay(FRAMES, 1).encode(),
                offline_bytes,
                "種子 {seed} 端點 {i} 的 replay 與離線 replay 不同"
            );
        }
    }
}

fn base(network: NetworkConfig) -> MatchConfig {
    MatchConfig {
        frames: FRAMES,
        network,
        checkpoint_interval: 1,
        ..MatchConfig::default()
    }
}

#[test]
fn ideal_network_is_equivalent_to_the_offline_replay() {
    let reports = assert_all_equivalent("理想網路", &base(condition_ideal()));
    assert_replays_identical(&reports, 2);
    // 理想網路 + D=2：除了開機時等第一個封包之外不該 stall。
    for r in &reports {
        for e in &r.ends {
            assert!(
                e.stats.stalls <= 1,
                "理想網路 stall 了 {} 次",
                e.stats.stalls
            );
        }
    }
}

#[test]
fn lossy_100ms_30ms_jitter_is_equivalent_to_the_offline_replay() {
    let reports = assert_all_equivalent("10% 丟包、100ms、抖動 30ms", &base(condition_lossy()));
    assert_replays_identical(&reports, 2);
}

#[test]
fn terrible_network_is_equivalent_to_the_offline_replay() {
    let reports = assert_all_equivalent(
        "30% 丟包、200ms、抖動 80ms、5% 重複",
        &base(condition_terrible()),
    );
    assert_replays_identical(&reports, 2);
}

#[test]
fn every_input_delay_from_0_to_8_is_equivalent() {
    // 只跑 600 幀 × 一個種子（D 不同，標準答案也不同）。
    for delay in 0..=8u8 {
        let frames = 600;
        let expected = expected_log(rom(), 0x5EED, delay, frames).unwrap();
        let cfg = MatchConfig {
            frames,
            input_delay: delay,
            network: condition_lossy(),
            checkpoint_interval: 1,
            ..MatchConfig::default()
        };
        let r = run_match(rom(), &cfg, &expected).unwrap();
        assert!(r.equivalent(), "input delay {delay}：{:?}", r.vs_expected);
    }
}

/// 破壞性測試 1：接收端（A）把對方（B）的輸入套用到錯誤的幀（差 1）→ 等價性必須被打破，
/// 而且 session 自己的指紋交換也要抓到它（Desync 事件）。
#[test]
fn off_by_one_frame_application_is_detected() {
    let cfg = MatchConfig {
        tamper: Tamper::AppliesRemoteInputOneFrameLate,
        ..base(condition_ideal())
    };
    let r = run_match(rom(), &cfg, expected(2)).unwrap();
    assert!(!r.equivalent(), "差 1 幀的錯誤必須讓等價性測試失敗");
    assert!(
        r.a_vs_b.is_some() || r.vs_expected.iter().any(Option::is_some),
        "指紋必須分歧"
    );
    assert!(
        r.desync_events() >= 1,
        "session 的指紋交換必須偵測到 desync"
    );
    let first = r.vs_expected[0].expect("A 與離線標準答案分歧");
    println!(
        "差 1 幀：A 與離線標準答案第 {first} 幀起分歧；A 端事件 {:?}",
        r.ends[0]
            .events
            .iter()
            .filter(|e| matches!(
                e,
                nes_net::Event::Desync { .. } | nes_net::Event::Disconnected { .. }
            ))
            .collect::<Vec<_>>()
    );
}

/// 破壞性測試 2：關閉冗餘傳送。仍然正確（靠 Ack ＋ 逾時重送），但 stall 增加。
///
/// 在規格指定的兩種網路條件下，stall 的主因是「延遲遠大於 input delay」（lockstep 本質，
/// 冗餘與否都一樣多），所以差距只有 1.x 倍；在「延遲小、丟包是主因」的條件下，差距才會非常明顯
/// （冗餘讓下一個封包自然補上掉的幀；沒有冗餘就得等一次逾時重送）。三種都量測並印出對照，
/// 判定條件：一律仍然正確、stall 時間不減少；丟包主導的條件必須增加到 2 倍以上。
#[test]
fn disabling_redundancy_stays_correct_but_stalls_much_more() {
    let loss_dominated = NetworkConfig {
        loss: 0.20,
        delay: ms(15),
        jitter: ms(5),
        duplicate: 0.0,
    };
    for (name, network, factor) in [
        ("20% 丟包、15ms、抖動 5ms（丟包主導）", loss_dominated, 2.0),
        ("10% 丟包、100ms、抖動 30ms", condition_lossy(), 1.0),
        (
            "30% 丟包、200ms、抖動 80ms、5% 重複",
            condition_terrible(),
            1.0,
        ),
    ] {
        let with = run_seeds(&base(network), 4);
        let without = run_seeds(
            &MatchConfig {
                redundancy: false,
                ..base(network)
            },
            4,
        );
        println!("{}", summarize(&format!("{name}｜冗餘開"), &with));
        println!("{}", summarize(&format!("{name}｜冗餘關"), &without));
        assert!(
            without.iter().all(MatchReport::equivalent),
            "{name}：關閉冗餘後結果仍應正確"
        );
        let stall = |rs: &[MatchReport]| -> f64 {
            rs.iter()
                .flat_map(|r| &r.ends)
                .map(|e| e.stats.stall_time.as_secs_f64())
                .sum::<f64>()
                / (rs.len() * 2) as f64
        };
        assert!(
            stall(&without) > stall(&with) * factor,
            "{name}：關閉冗餘的 stall 時間 {:.2}s 應大於開啟的 {:.2}s 的 {factor} 倍",
            stall(&without),
            stall(&with)
        );
    }
}
