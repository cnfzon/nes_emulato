//! Phase 4c 最重要的測試：**rollback 的等價性**（規劃：`docs/architecture.md` §20）。
//!
//! rollback 的正確性歸約到 Phase 4a 已驗證的 replay 正確性：
//!
//! 1. 所有「已確認」的幀，兩端的行為指紋逐幀相同；
//! 2. 而且等於「用最終確認的雙方輸入離線重播」的結果——標準答案 `expected_log_with` 只由雙方腳本、各自的 input delay 與
//!    ROM 算出，**完全不經過網路、預測與還原**；
//! 3. 兩端各自記錄的 replay（每幀一個檢查點）與離線 replay 位元組完全相同，並且能通過 `replay::verify`；
//! 4. 沒有任何 Desync 事件（**包括丟包、延遲、預測失誤時的假 desync**）。
//!
//! 網路由虛擬時鐘模擬（不 sleep、不讀系統時間，結果只由種子決定）。每種網路條件跑 20 組不同的種子（各 3600 幀）。
//!
//! 也在這裡：最壞情況的預測失誤、時鐘偏差的收斂、對方停止傳送輸入、破壞性測試。

use std::sync::OnceLock;
use std::time::Duration;

use nes_core::replay::verify;
use nes_core::test_support::input_probe_rom;
use nes_net::sim::{AdvantageSample, MatchConfig, MatchReport, Tamper, run_match};
use nes_net::{Event, Mode, NetworkConfig, Sabotage};

/// 每場的幀數。rollback 每個幀節拍最多重跑 K 幀，模擬成本是 lockstep 的數倍，所以這裡用 900 幀（15 秒）×
/// 20 組種子；3600 幀的版本由 `nes-test netsim`（release、平行）跑並記錄在文件裡。
const FRAMES: u32 = 900;
const SEEDS: u64 = 20;

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

fn rom() -> &'static [u8] {
    static ROM: OnceLock<Vec<u8>> = OnceLock::new();
    ROM.get_or_init(input_probe_rom)
}

fn ideal() -> NetworkConfig {
    NetworkConfig::IDEAL
}

/// 與 4b 相同的三種網路條件。
fn lossy() -> NetworkConfig {
    NetworkConfig {
        loss: 0.10,
        delay: ms(100),
        jitter: ms(30),
        duplicate: 0.0,
    }
}

fn terrible() -> NetworkConfig {
    NetworkConfig {
        loss: 0.30,
        delay: ms(200),
        jitter: ms(80),
        duplicate: 0.05,
    }
}

fn base(network: NetworkConfig) -> MatchConfig {
    MatchConfig {
        frames: FRAMES,
        network,
        mode: Mode::Rollback,
        input_delay: 1,
        window: 8,
        checkpoint_interval: 1,
        ..MatchConfig::default()
    }
}

/// 對 `seeds` 個種子各跑一場，分散到多個執行緒（結果依種子排序，與執行緒數量無關）。
fn run_seeds(base: &MatchConfig, seeds: u64) -> Vec<MatchReport> {
    let expected = base.expected(rom()).unwrap();
    let threads = std::thread::available_parallelism()
        .map_or(4, |n| n.get())
        .min(8);
    let mut reports: Vec<(u64, MatchReport)> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..threads as u64)
            .map(|t| {
                let expected = &expected;
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

fn mean(reports: &[MatchReport], f: impl Fn(&nes_net::sim::EndReport) -> f64) -> f64 {
    let n = reports.len() * 2;
    reports.iter().flat_map(|r| &r.ends).map(f).sum::<f64>() / n as f64
}

fn summarize(name: &str, reports: &[MatchReport]) -> String {
    let equivalent = reports.iter().filter(|r| r.equivalent()).count();
    let rb = |f: fn(&nes_net::RollbackStats) -> f64| mean(reports, |e| e.rollback().map_or(0.0, f));
    let elapsed = reports
        .iter()
        .map(|r| r.virtual_elapsed.as_secs_f64())
        .sum::<f64>()
        / reports.len() as f64;
    format!(
        "{name}: 等價 {equivalent}/{}，每端 rollback {:.0} 次（平均深度 {:.2}、最大 {:.0}），預測準確率 {:.1}%，stall {:.1} 次／{:.0} ms，時間同步放慢 {:.1} 次，耗時 {elapsed:.1} 秒（虛擬）",
        reports.len(),
        rb(|s| f64::from(s.rollbacks)),
        rb(|s| f64::from(s.avg_depth)),
        mean(reports, |e| e
            .rollback()
            .map_or(0.0, |s| f64::from(s.max_depth))),
        100.0 * rb(|s| f64::from(s.prediction_accuracy().unwrap_or(1.0))),
        mean(reports, |e| f64::from(e.stats.stalls)),
        mean(reports, |e| e.stats.stall_time.as_secs_f64() * 1000.0),
        rb(|s| f64::from(s.holds)),
    )
}

/// 全部等價、沒有任何 desync（假的也不行）、沒有內部錯誤。
fn assert_all_equivalent(name: &str, base: &MatchConfig, seeds: u64) -> Vec<MatchReport> {
    let reports = run_seeds(base, seeds);
    println!("{}", summarize(name, &reports));
    for (seed, r) in reports.iter().enumerate() {
        assert!(!r.timed_out, "{name} 種子 {seed}：逾時");
        for (i, e) in r.ends.iter().enumerate() {
            assert!(
                e.frames >= FRAMES,
                "{name} 種子 {seed} 端點 {i} 只確認到 {} 幀；事件 {:?}",
                e.frames,
                e.events
            );
            assert_eq!(e.exec_error, None, "{name} 種子 {seed} 端點 {i}");
        }
        assert_eq!(r.desync_events(), 0, "{name} 種子 {seed}：假的 desync");
        assert_eq!(r.a_vs_b, None, "{name} 種子 {seed}：兩端指紋分歧");
        assert_eq!(
            r.vs_expected,
            [None, None],
            "{name} 種子 {seed}：與離線重播不同"
        );
        assert!(r.equivalent());
    }
    reports
}

/// 兩端各自的 replay（每幀一個檢查點）＝離線 replay（位元組完全相同），且離線 replay 通過 verify。
fn assert_replays_identical(base: &MatchConfig, reports: &[MatchReport]) {
    let offline = base.expected(rom()).unwrap().to_replay(FRAMES, 1);
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

// ---- 10b：三種網路條件 × 20 組種子 ------------------------------------------------------

#[test]
fn rollback_ideal_network_is_equivalent_to_the_offline_replay() {
    let cfg = base(ideal());
    let reports = assert_all_equivalent("rollback 理想網路", &cfg, SEEDS);
    assert_replays_identical(&cfg, &reports);
    // 理想網路：沒有預測失誤、也不該 stall。
    for r in &reports {
        for e in &r.ends {
            assert_eq!(e.rollback().unwrap().rollbacks, 0, "理想網路不該 rollback");
        }
    }
}

#[test]
fn rollback_lossy_100ms_30ms_jitter_is_equivalent_to_the_offline_replay() {
    let cfg = base(lossy());
    let reports = assert_all_equivalent("rollback 10% 丟包、100ms、抖動 30ms", &cfg, SEEDS);
    assert_replays_identical(&cfg, &reports);
    for r in &reports {
        assert!(
            r.ends.iter().all(|e| e.rollback().unwrap().rollbacks > 0),
            "這個條件下一定會 rollback（否則測試沒有測到 rollback）"
        );
    }
}

#[test]
fn rollback_terrible_network_is_equivalent_to_the_offline_replay() {
    let cfg = base(terrible());
    let reports =
        assert_all_equivalent("rollback 30% 丟包、200ms、抖動 80ms、5% 重複", &cfg, SEEDS);
    assert_replays_identical(&cfg, &reports);
}

// ---- 10c：最壞情況——輸入每 2–3 幀就改變（大量預測失誤），含 Reset -------------------------

#[test]
fn rollback_worst_case_inputs_changing_every_2_to_3_frames_stay_equivalent() {
    for hold in [2u32, 3] {
        for (name, network) in [("100ms 丟包", lossy()), ("200ms 丟包", terrible())] {
            let cfg = MatchConfig {
                input_hold: hold,
                resets: true,
                ..base(network)
            };
            let reports = assert_all_equivalent(
                &format!("最壞情況（每 {hold} 幀換一次輸入、含 Reset）{name}"),
                &cfg,
                8,
            );
            for r in &reports {
                let s = r.ends[0].rollback().unwrap();
                assert!(s.prediction_wrong > 0, "最壞情況必須有大量預測失誤");
            }
            assert_replays_identical(&cfg, &reports);
        }
    }
    // 每一幀都換（比規格更壞）。
    let cfg = MatchConfig {
        input_hold: 1,
        resets: true,
        ..base(lossy())
    };
    assert_all_equivalent("最壞情況（每幀換輸入、含 Reset）", &cfg, 4);
}

/// 各自不同的本地輸入延遲（rollback 不要求雙方一致）與不同的預測視窗。
#[test]
fn rollback_with_different_input_delays_and_windows_is_equivalent() {
    for (delays, window) in [([0u8, 4], 8u32), ([4, 0], 3), ([2, 3], 1), ([1, 1], 16)] {
        let cfg = MatchConfig {
            frames: 900,
            delays: Some(delays),
            window,
            ..base(lossy())
        };
        let expected = cfg.expected(rom()).unwrap();
        let r = run_match(rom(), &cfg, &expected).unwrap();
        assert!(
            r.equivalent(),
            "delays {delays:?} K={window}：{:?} {:?}",
            r.vs_expected,
            r.ends.iter().map(|e| e.events.clone()).collect::<Vec<_>>()
        );
        assert_eq!(r.desync_events(), 0);
    }
}

/// 也涵蓋「輸出開啟」的路徑（模擬 GUI）：重跑的幀關閉輸出、新的一幀開啟，結果仍然等價。
#[test]
fn rollback_with_rendering_output_enabled_is_equivalent() {
    use nes_core::Nes;
    use nes_net::sim::{Endpoint, NTSC_FPS};
    use nes_net::{InMemoryTransport, Session, SessionConfig, SimulatedTransport};
    let rom = rom();
    let rom_id = nes_core::RomId::of_file(rom);
    let (a, b) = InMemoryTransport::pair();
    let net = lossy();
    let frames = 600;
    let mk = |cfg: SessionConfig, t, seed| {
        Endpoint::new(
            Session::new(cfg),
            SimulatedTransport::new(t, net, seed),
            rom,
            0x5EED,
            frames,
        )
        .with_headless(false)
    };
    let mut ea = mk(
        SessionConfig::host(rom_id, 1, 7).with_mode(Mode::Rollback),
        a,
        1,
    );
    let mut eb = mk(SessionConfig::client(rom_id).with_input_delay(1), b, 2);
    let mut now = Duration::ZERO;
    while !(ea.done() && eb.done()) {
        ea.tick(now, true);
        eb.tick(now, true);
        now += nes_net::sim::STEP;
        assert!(now < Duration::from_secs(600), "逾時（{NTSC_FPS}）");
    }
    let expected = MatchConfig {
        frames,
        input_delay: 1,
        mode: Mode::Rollback,
        ..MatchConfig::default()
    }
    .expected(rom)
    .unwrap();
    for e in [&ea, &eb] {
        assert_eq!(e.exec_error, None);
        assert_eq!(
            &e.log.as_ref().unwrap().fingerprints()[..=frames as usize],
            expected.fingerprints()
        );
        assert!(e.session.stats().rollback.unwrap().rollbacks > 0);
    }
    // 輸出開啟的路徑真的有畫東西。
    let _ = Nes::from_rom(rom).unwrap();
    assert!(ea.nes.as_ref().unwrap().output_enabled());
}

// ---- 10d：時鐘偏差的收斂 -----------------------------------------------------------------

fn skew_config(time_sync: bool) -> MatchConfig {
    MatchConfig {
        frames: 3600,     // 規格：3600 幀
        clock_skew: 0.01, // B 的幀時鐘比 A 快 1%
        time_sync,
        // 單程 40 ms（RTT 80 ms）、輕微抖動與丟包：接近真實的網際網路。
        network: NetworkConfig {
            loss: 0.01,
            delay: ms(40),
            jitter: ms(8),
            duplicate: 0.0,
        },
        ..base(ideal())
    }
}

fn describe(advantage: &[AdvantageSample]) -> String {
    advantage
        .iter()
        .step_by(4)
        .map(|s| format!("{}s:{:+}", s.second, s.a_minus_b))
        .collect::<Vec<_>>()
        .join(" ")
}

/// 兩端使用速度不同的虛擬時鐘（B 快 1%），跑 3600 幀：幀數優勢收斂在小範圍內，rollback 次數有界。
/// 對照組：關閉時間同步，差距會持續擴大到預測視窗的上限（然後靠 stall 硬撐）。
#[test]
fn clock_skew_of_one_percent_converges_with_time_sync_and_drifts_without() {
    let synced = run_match(
        rom(),
        &skew_config(true),
        &skew_config(true).expected(rom()).unwrap(),
    )
    .unwrap();
    let unsynced = run_match(
        rom(),
        &skew_config(false),
        &skew_config(false).expected(rom()).unwrap(),
    )
    .unwrap();
    assert!(synced.equivalent(), "時間同步開啟：仍然等價");
    assert!(
        unsynced.equivalent(),
        "時間同步關閉：仍然等價（只是慢、stall 多）"
    );
    assert_eq!(synced.desync_events() + unsynced.desync_events(), 0);

    println!("時鐘偏差 1%（B 快），每 4 秒一筆 A−B 的目前幀差：");
    println!("  時間同步開：{}", describe(&synced.advantage));
    println!("  時間同步關：{}", describe(&unsynced.advantage));
    let stat = |r: &MatchReport, i: usize| *r.ends[i].rollback().unwrap();
    for (name, r) in [("開", &synced), ("關", &unsynced)] {
        let (a, b) = (stat(r, 0), stat(r, 1));
        println!(
            "  時間同步{name}：A rollback {} 次（放慢 {}、stall {} 次／{:.0} ms），B rollback {} 次（放慢 {}、stall {} 次／{:.0} ms）",
            a.rollbacks,
            a.holds,
            a.stalls,
            a.stall_time.as_secs_f64() * 1000.0,
            b.rollbacks,
            b.holds,
            b.stalls,
            b.stall_time.as_secs_f64() * 1000.0,
        );
    }

    // 收斂：暖機 10 秒之後，兩端目前幀的差維持在小範圍內（預測視窗 8 之內，而且明顯小於不同步時的漂移）。
    let settled: Vec<i32> = synced
        .advantage
        .iter()
        .filter(|s| s.second >= 10)
        .map(|s| s.a_minus_b)
        .collect();
    assert!(settled.len() > 40, "取樣數 {}", settled.len());
    let worst = settled.iter().map(|d| d.abs()).max().unwrap();
    assert!(worst <= 4, "時間同步開啟：收斂後幀差最大 {worst}（應 ≤ 4）");

    // B 快 1%：不同步時 A−B 一路往負的方向漂（B 領先）。
    let drift = unsynced.advantage.last().unwrap().a_minus_b;
    assert!(
        drift.abs() > worst,
        "不同步時幀差 {drift} 應該比同步時的 {worst} 大得多"
    );

    // rollback 次數有界：同步時只有領先的那一端（B）被放慢，兩端的 rollback 次數都遠小於「每個輸入變化都 rollback」的上限。
    let input_changes = 3600 / 3;
    for i in 0..2 {
        let s = stat(&synced, i);
        assert!(
            s.rollbacks <= input_changes + 10,
            "端點 {i} rollback {} 次超過輸入變化次數 {input_changes}",
            s.rollbacks
        );
    }
    // 放慢確實發生在快的那一端（B），而且次數約等於偏差累積的幀數（3600 幀 × 1% ≈ 36）。
    let holds_b = stat(&synced, 1).holds;
    assert!(
        (20..=60).contains(&holds_b),
        "B 應該被放慢約 36 次，實際 {holds_b}"
    );
    assert_eq!(stat(&synced, 0).holds, 0, "落後的 A 不該被放慢");
}

// ---- 10e：對方停止傳送輸入超過 K 幀：本地停止推進而不是出錯；恢復後正常繼續 ----------------

#[test]
fn a_silent_peer_stalls_the_local_side_within_the_window_and_play_resumes_afterwards() {
    use nes_core::RomId;
    use nes_net::sim::{Endpoint, TamperTransport};
    use nes_net::{InMemoryTransport, Session, SessionConfig, SimulatedTransport};

    let rom = rom();
    let rom_id = RomId::of_file(rom);
    let frames = 1200;
    let window = 6;
    let (ma, mb) = InMemoryTransport::pair();
    let net = ideal();
    type T = SimulatedTransport<TamperTransport<InMemoryTransport>>;
    let mk = |cfg: SessionConfig, m, seed| {
        let t: T = SimulatedTransport::new(TamperTransport::new(m, Tamper::None), net, seed);
        Endpoint::new(Session::new(cfg), t, rom, 0x5EED, frames).with_window(window)
    };
    let mut a = mk(
        SessionConfig::host(rom_id, 1, 3)
            .with_mode(Mode::Rollback)
            .with_window(window),
        ma,
        1,
    );
    let mut b = mk(
        SessionConfig::client(rom_id)
            .with_input_delay(1)
            .with_window(window),
        mb,
        2,
    );

    let mut now = Duration::ZERO;
    let blackout = (Duration::from_secs(4), Duration::from_secs(7)); // 3 秒（< 5 秒的斷線逾時）
    let mut max_ahead = 0;
    let mut cur_at_blackout_end = (0, 0);
    let mut stalled_during_blackout = false;
    while !(a.done() && b.done()) {
        if now == blackout.0 {
            a.transport.set_config(net.blackout());
            b.transport.set_config(net.blackout());
        }
        if now == blackout.1 {
            a.transport.set_config(net);
            b.transport.set_config(net);
        }
        a.tick(now, true);
        b.tick(now, true);
        for e in [&a, &b] {
            if let Some(p) = e.session.planner() {
                max_ahead = max_ahead.max(p.current_frame() - p.confirmed_frame());
            }
        }
        if now == blackout.1 - ms(1) {
            let pa = a.session.planner().unwrap();
            let pb = b.session.planner().unwrap();
            cur_at_blackout_end = (pa.current_frame(), pb.current_frame());
            // 靜默 3 秒（180 幀）之後，本地停在「已確認幀 + K」，沒有繼續往前預測。
            stalled_during_blackout = pa.current_frame() - pa.confirmed_frame() == window
                && pb.current_frame() - pb.confirmed_frame() == window;
            assert!(pa.stats().stalls >= 1 && pb.stats().stalls >= 1);
        }
        now += nes_net::sim::STEP;
        assert!(now < Duration::from_secs(300), "恢復之後沒有跑完");
    }
    assert!(
        stalled_during_blackout,
        "靜默期間必須停在 K 幀的預測視窗內：{cur_at_blackout_end:?}"
    );
    assert!(
        max_ahead <= window,
        "目前幀領先已確認幀 {max_ahead} 幀，超過預測視窗 {window}"
    );
    for e in [&a, &b] {
        assert_eq!(e.exec_error, None, "停止推進時不能出錯（快照範圍）");
        assert_eq!(e.session.end_reason(), None, "3 秒靜默不該中斷連線");
    }
    // 恢復後結果仍然等價。
    let expected = MatchConfig {
        frames,
        input_delay: 1,
        mode: Mode::Rollback,
        ..MatchConfig::default()
    }
    .expected(rom)
    .unwrap();
    for e in [&a, &b] {
        assert_eq!(
            &e.log.as_ref().unwrap().fingerprints()[..=frames as usize],
            expected.fingerprints()
        );
        assert!(
            !e.events.iter().any(|ev| matches!(ev, Event::Desync { .. })),
            "恢復之後不能有 desync"
        );
    }
    println!(
        "靜默 3 秒：兩端都停在確認幀 + {window}（目前幀 {cur_at_blackout_end:?}），恢復後跑完 {frames} 幀且等價"
    );
}

// ---- 10g：破壞性測試（每一項都必須被抓到）-------------------------------------------------

fn sabotaged(sabotage: Sabotage, network: NetworkConfig) -> MatchReport {
    let cfg = MatchConfig {
        frames: 1200,
        sabotage,
        // 讓預測失誤頻繁發生。
        input_hold: 2,
        max_virtual_time: Some(Duration::from_secs(400)),
        ..base(network)
    };
    run_match(rom(), &cfg, &cfg.expected(rom()).unwrap()).unwrap()
}

/// 還原到 F+1 而不是 F（差 1）：等價性測試必須失敗，session 自己的指紋交換也必須抓到。
#[test]
fn restoring_to_f_plus_1_instead_of_f_is_caught() {
    let r = sabotaged(Sabotage::LoadOneFrameLate, lossy());
    assert!(!r.equivalent(), "差 1 幀的還原必須讓等價性測試失敗");
    assert!(
        r.a_vs_b.is_some() || r.vs_expected.iter().any(Option::is_some),
        "指紋必須分歧"
    );
    assert!(
        r.desync_events() >= 1,
        "session 的指紋交換必須偵測到 desync"
    );
    println!(
        "還原到 F+1：等價性失敗（A 與離線在第 {:?} 幀分歧、兩端在第 {:?} 幀分歧），desync 事件 {} 個：{:?}",
        r.vs_expected[0],
        r.a_vs_b,
        r.desync_events(),
        r.ends[0]
            .events
            .iter()
            .filter(|e| matches!(e, Event::Desync { .. }))
            .collect::<Vec<_>>()
    );
}

/// 偵測到預測錯誤時不執行還原：必須被抓到。
#[test]
fn detecting_a_misprediction_without_rolling_back_is_caught() {
    let r = sabotaged(Sabotage::SkipRollback, lossy());
    assert!(!r.equivalent(), "不還原必須讓等價性測試失敗");
    assert!(
        r.desync_events() >= 1,
        "session 的指紋交換必須偵測到 desync"
    );
    println!(
        "不執行還原：等價性失敗（A 與離線在第 {:?} 幀分歧），desync 事件 {} 個",
        r.vs_expected[0],
        r.desync_events()
    );
}

/// 對用預測輸入模擬出來的幀計算指紋：在有延遲、丟包的網路下會產生**假的 desync**
/// （模擬本身是正確的），被「不得出現任何假 desync」的斷言（[`assert_all_equivalent`]）抓到。
#[test]
fn fingerprinting_predicted_frames_produces_false_desyncs_that_the_no_desync_check_catches() {
    let r = sabotaged(Sabotage::PublishUnconfirmedFingerprint, lossy());
    assert!(
        r.desync_events() >= 1,
        "對預測幀算指紋，在丟包網路下必須產生（假的）desync"
    );
    assert!(!r.equivalent(), "連線被誤判為 desync 而中止，沒有跑完");
    // 對照：同樣的網路與腳本，正確的實作沒有任何 desync（這正是 `assert_all_equivalent` 檢查的）。
    let ok = sabotaged(Sabotage::None, lossy());
    assert_eq!(ok.desync_events(), 0);
    assert!(ok.equivalent());
    println!(
        "對預測幀算指紋：假 desync {} 個（在第 {} 幀）；正確實作 0 個",
        r.desync_events(),
        r.ends
            .iter()
            .flat_map(|e| &e.events)
            .find_map(|e| match e {
                Event::Desync { frame, .. } => Some(*frame),
                _ => None,
            })
            .unwrap_or(0)
    );
}

/// 過去的 4b 測試在 rollback 下同樣要有：接收端把對方的輸入套用到錯誤的幀（差 1）→ 必須被抓到。
#[test]
fn applying_the_remote_input_one_frame_late_is_caught_in_rollback_mode() {
    let cfg = MatchConfig {
        frames: 900,
        tamper: Tamper::AppliesRemoteInputOneFrameLate,
        max_virtual_time: Some(Duration::from_secs(200)),
        ..base(lossy())
    };
    let r = run_match(rom(), &cfg, &cfg.expected(rom()).unwrap()).unwrap();
    assert!(!r.equivalent());
    assert!(r.desync_events() >= 1);
}

// ---- 單一節拍的 AdvanceFrame 請求數永遠不超過 K（佐證 `rollback-bench` 的「一個節拍合計 K 幀」模型）----

/// 規劃器只在 `cur − C < K` 時推進新的一幀，而重跑的範圍是 `[F, cur)` ⊆ `[C, cur)`：
/// 所以一個節拍最壞是「重跑 K−1 幀＋新的一幀」，或視窗已滿時「重跑 K 幀、沒有新的一幀」，合計 ≤ K。
/// 這裡在各種網路條件、預測失誤頻率、K、input delay、時鐘偏差下實測這個上限，
/// 並且要求上限**確實被碰到**（在壓力條件下等於 K），證明模型是緊的而不是空泛的。
#[test]
fn a_single_tick_never_requests_more_than_k_advance_frames() {
    let mut reached = std::collections::BTreeMap::new();
    for network in [ideal(), lossy(), terrible()] {
        for hold in [1u32, 3] {
            for window in [1u32, 2, 8, 16] {
                for (delay, skew) in [(0u8, 0.0), (4, -0.01)] {
                    let cfg = MatchConfig {
                        frames: 240,
                        input_hold: hold,
                        window,
                        input_delay: delay,
                        clock_skew: skew,
                        resets: true,
                        seed: 7 + u64::from(hold) * 31 + u64::from(window),
                        ..base(network)
                    };
                    let r = run_match(rom(), &cfg, &cfg.expected(rom()).unwrap()).unwrap();
                    assert!(
                        r.equivalent(),
                        "K={window} hold={hold} D={delay} 丟包 {} 不等價：逾時 {}、幀數 {:?}、與離線 {:?}、兩端 {:?}、desync {}、事件 {:?}",
                        network.loss,
                        r.timed_out,
                        r.ends.iter().map(|e| e.frames).collect::<Vec<_>>(),
                        r.vs_expected,
                        r.a_vs_b,
                        r.desync_events(),
                        r.ends.iter().map(|e| e.end_reason).collect::<Vec<_>>()
                    );
                    for (i, e) in r.ends.iter().enumerate() {
                        assert!(
                            e.max_advances_per_plan <= window as usize,
                            "端點 {i}：單一節拍請求了 {} 個 AdvanceFrame，超過 K={window}（hold={hold}、D={delay}、丟包 {}）",
                            e.max_advances_per_plan,
                            network.loss
                        );
                        let entry = reached.entry(window).or_insert(0usize);
                        *entry = (*entry).max(e.max_advances_per_plan);
                    }
                }
            }
        }
    }
    println!("各 K 實測到的單一節拍最大 AdvanceFrame 數：{reached:?}");
    for (window, max) in &reached {
        // 在丟包＋高延遲＋每幀換輸入的壓力下，一定有節拍把視窗用滿。
        assert_eq!(
            max,
            &(*window as usize),
            "K={window} 的上限沒有被碰到（模型不緊）：實測最大 {max}"
        );
    }
}
