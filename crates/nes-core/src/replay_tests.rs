//! replay（錄製／播放／驗證）與雙實例工具的測試。規格見 `docs/architecture.md` §18.3–§18.5。

use crate::dual::{Divergence, DualRunner, fingerprint_trace, first_divergence, run_both};
use crate::replay::{
    Checkpoint, DEFAULT_CHECKPOINT_INTERVAL, InputRun, REPLAY_FORMAT_VERSION, REPLAY_MAGIC, Replay,
    ReplayError, ReplayPlayer, ReplayRecorder, verify,
};
use crate::test_support::{input_probe_rom, rendering_rom};
use crate::{Buttons, CORE_BEHAVIOR_VERSION, FrameInput, Nes, RomId};

/// 固定腳本：第 `i` 幀（0 起算）的輸入。玩家 1 輪流按 A、A+右、左+B、放開；玩家 2 週期性地
/// 按 上+B 與 Start；第 150 幀（0 起算）按一次 reset。腳本**從不**按 Select，
/// 竄改測試用 Select 當作「一定不同」的標記。
fn script(i: u64) -> FrameInput {
    let p1 = [
        Buttons::A,
        Buttons::A | Buttons::RIGHT,
        Buttons::LEFT | Buttons::B,
        Buttons::empty(),
    ][((i / 7) % 4) as usize];
    let p2 = if i % 11 < 4 {
        Buttons::UP | Buttons::B
    } else if i.is_multiple_of(13) {
        Buttons::START
    } else {
        Buttons::empty()
    };
    let input = FrameInput::new(p1, p2);
    if i == 150 { input.with_reset() } else { input }
}

fn script_inputs(frames: u64) -> Vec<FrameInput> {
    (0..frames).map(script).collect()
}

/// 從開機錄製 `frames` 幀的腳本。
fn record(rom: &[u8], frames: u64, interval: u16) -> Replay {
    let mut nes = Nes::from_rom(rom).unwrap();
    let mut recorder = ReplayRecorder::new(&nes, interval).unwrap();
    for input in script_inputs(frames) {
        nes.run_frame(input);
        recorder.record_frame(input, &nes).unwrap();
    }
    recorder.finish(&nes).unwrap()
}

fn xxh3(bytes: &[u8]) -> u64 {
    xxhash_rust::xxh3::xxh3_64(bytes)
}

// ---- 錄製與播放 -----------------------------------------------------------------

#[test]
fn record_then_verify_round_trips_including_the_reset() {
    let rom = input_probe_rom();
    let replay = record(&rom, 200, DEFAULT_CHECKPOINT_INTERVAL);

    assert_eq!(replay.core_behavior_version, CORE_BEHAVIOR_VERSION);
    assert_eq!(replay.rom_id, RomId::of_file(&rom));
    assert_eq!(replay.total_frames, 200);
    // 第 0 幀（開機）、每 60 幀、最後一幀。
    let frames: Vec<u32> = replay.checkpoints.iter().map(|c| c.frame).collect();
    assert_eq!(frames, [0, 60, 120, 180, 200]);
    assert_eq!(replay.inputs().collect::<Vec<_>>(), script_inputs(200));
    // RLE 真的有壓縮（腳本有長串相同的輸入）。
    assert!((replay.runs.len() as u32) < replay.total_frames);

    let bytes = replay.encode();
    assert_eq!(Replay::decode(&bytes).unwrap(), replay);

    let report = verify(&rom, &Replay::decode(&bytes).unwrap()).unwrap();
    assert_eq!(report.frames, 200);
    assert_eq!(report.checkpoints_verified, 5);
}

/// 黃金 replay：CI 上可執行（合成 ROM）。錄一段固定腳本（雙人操作 + 一次 reset），把「檢查點指紋」
/// 與「編碼後位元組的雜湊」釘在這裡。指紋變了＝模擬行為變了（依 §15 遞增 `CORE_BEHAVIOR_VERSION`）；
/// 位元組雜湊變了但指紋沒變＝replay 的位元組佈局變了（要遞增 `REPLAY_FORMAT_VERSION`）。
#[test]
fn golden_replay_pins_checkpoints_and_encoded_bytes() {
    const CHECKPOINTS: [(u32, u64); 6] = [
        (0, 0xeb1946a9f28d9243),
        (60, 0x987ae65be604ac06),
        (120, 0x4f22639c605381f1),
        (180, 0x5cb3ce1909ccb96f),
        (240, 0xda0633072ed1ef21),
        (300, 0xaa87895b92db46d4),
    ];
    const ENCODED_LEN: usize = 901;
    const ENCODED_XXH3: u64 = 0xcc265e4713e6d767;

    let rom = input_probe_rom();
    let replay = record(&rom, 300, DEFAULT_CHECKPOINT_INTERVAL);
    let actual: Vec<(u32, u64)> = replay
        .checkpoints
        .iter()
        .map(|c| (c.frame, c.fingerprint))
        .collect();
    let bytes = replay.encode();
    assert_eq!(
        (actual.as_slice(), bytes.len(), xxh3(&bytes)),
        (&CHECKPOINTS[..], ENCODED_LEN, ENCODED_XXH3),
        "golden replay 改變了：見測試上方的說明"
    );
    // 固定腳本錄出來的 replay 一定能被驗證通過。
    assert!(verify(&rom, &replay).is_ok());
}

#[test]
fn recording_only_starts_from_the_power_on_state() {
    let rom = input_probe_rom();
    let mut nes = Nes::from_rom(&rom).unwrap();
    assert!(ReplayRecorder::new(&nes, 60).is_ok());
    assert_eq!(
        ReplayRecorder::new(&nes, 0).unwrap_err(),
        ReplayError::BadInterval
    );
    nes.run_frame(FrameInput::NONE);
    assert_eq!(
        ReplayRecorder::new(&nes, 60).unwrap_err(),
        ReplayError::NotPowerOn
    );
    // 單步除錯也會破壞前提。
    let mut stepped = Nes::from_rom(&rom).unwrap();
    stepped.step_instruction();
    assert_eq!(
        ReplayRecorder::new(&stepped, 60).unwrap_err(),
        ReplayError::NotPowerOn
    );
}

/// 有跑但沒記錄的幀（例如錄製中偷偷讀檔、多跑一幀）會被幀數對不上偵測到。
#[test]
fn recorder_detects_frames_that_were_not_recorded() {
    let mut nes = Nes::from_rom(&input_probe_rom()).unwrap();
    let mut recorder = ReplayRecorder::new(&nes, 60).unwrap();
    nes.run_frame(FrameInput::NONE);
    recorder.record_frame(FrameInput::NONE, &nes).unwrap();
    nes.run_frame(FrameInput::NONE); // 沒有記錄這一幀
    nes.run_frame(FrameInput::NONE);
    assert_eq!(
        recorder.record_frame(FrameInput::NONE, &nes).unwrap_err(),
        ReplayError::OutOfSync {
            recorded: 1,
            nes_frames: 3
        }
    );
    assert!(matches!(
        recorder.finish(&nes),
        Err(ReplayError::OutOfSync { .. })
    ));
}

#[test]
fn empty_recording_is_valid_and_finish_adds_the_final_checkpoint() {
    let rom = input_probe_rom();
    let empty = record(&rom, 0, 60);
    assert_eq!(empty.total_frames, 0);
    assert_eq!(empty.checkpoints.len(), 1);
    assert_eq!(empty.checkpoints[0].frame, 0);
    assert_eq!(Replay::decode(&empty.encode()).unwrap(), empty);
    assert_eq!(verify(&rom, &empty).unwrap().frames, 0);

    // 剛好是間隔的整數倍時，不會重複最後一個檢查點。
    let exact = record(&rom, 120, 60);
    let frames: Vec<u32> = exact.checkpoints.iter().map(|c| c.frame).collect();
    assert_eq!(frames, [0, 60, 120]);
    // 間隔 1：每一幀都有檢查點。
    let dense = record(&rom, 10, 1);
    assert_eq!(dense.checkpoints.len(), 11);
    assert!(verify(&rom, &dense).is_ok());
}

/// 播放時輸出開或關都得到同樣的結果（驗證模式關閉輸出只是為了加速）。
#[test]
fn playback_with_output_on_and_off_verifies_the_same() {
    let rom = rendering_rom();
    let replay = record(&rom, 90, 30);
    for output in [true, false] {
        let mut nes = Nes::from_rom(&rom).unwrap();
        nes.set_output_enabled(output);
        let mut player = ReplayPlayer::new(replay.clone(), &nes).unwrap();
        while player.step(&mut nes).unwrap() {}
        assert!(player.is_finished());
        assert_eq!(player.verified_checkpoints(), player.checkpoint_count());
        assert_eq!(player.step(&mut nes), Ok(false), "播完之後不再前進");
    }
}

// ---- 拒絕播放 -------------------------------------------------------------------

#[test]
fn playback_is_refused_when_the_core_version_or_the_rom_differs() {
    let rom = input_probe_rom();
    let replay = record(&rom, 30, 10);

    let mut old = replay.clone();
    old.core_behavior_version = CORE_BEHAVIOR_VERSION + 1;
    let err = verify(&rom, &old).unwrap_err();
    assert_eq!(
        err,
        ReplayError::CoreVersionMismatch {
            replay: CORE_BEHAVIOR_VERSION + 1,
            current: CORE_BEHAVIOR_VERSION
        }
    );
    let text = err.to_string();
    assert!(
        text.contains("核心行為版本") && text.contains("拒絕播放"),
        "{text}"
    );

    let other_rom = rendering_rom();
    let err = verify(&other_rom, &replay).unwrap_err();
    assert!(matches!(err, ReplayError::RomMismatch { .. }));
    let text = err.to_string();
    assert!(
        text.contains("另一份 ROM")
            && text.contains(&replay.rom_id.short())
            && text.contains(&RomId::of_file(&other_rom).short()),
        "{text}"
    );

    // 不是開機狀態的 Nes 也拒絕（例如已經跑過幀）。
    let mut nes = Nes::from_rom(&rom).unwrap();
    nes.run_frame(FrameInput::NONE);
    assert_eq!(
        ReplayPlayer::new(replay, &nes).unwrap_err(),
        ReplayError::NotPowerOn
    );
}

// ---- 分歧範圍 -------------------------------------------------------------------

/// 竄改某一幀的輸入之後驗證：回報的分歧範圍必須包含被竄改的那一幀，且不符的檢查點是
/// 「該幀（含）之後第一個檢查點」。`k` 是第 k 幀（1 起算）。
fn tampered_verify(rom: &[u8], replay: &Replay, k: u32, tamper: impl Fn(&mut FrameInput)) {
    let mut inputs: Vec<FrameInput> = replay.inputs().collect();
    tamper(&mut inputs[k as usize - 1]);
    let mut bad = replay.clone();
    bad.runs = Replay::compress(inputs);

    let ReplayError::Mismatch(m) = verify(rom, &bad).unwrap_err() else {
        panic!("第 {k} 幀被竄改，verify 卻沒有回報不符");
    };
    let range = m.suspect_frames();
    assert!(
        range.contains(&k),
        "第 {k} 幀被竄改，但回報的範圍 {range:?}（{m}）不含它"
    );
    let first_cp_at_or_after = replay
        .checkpoints
        .iter()
        .map(|c| c.frame)
        .find(|&f| f >= k)
        .unwrap();
    assert_eq!(m.frame, first_cp_at_or_after, "第 {k} 幀");
    assert_eq!(m.last_good_frame, Some(*range.start() - 1));
    assert_ne!(m.expected, m.actual);
}

#[test]
fn a_tampered_input_is_reported_within_the_suspect_range() {
    let rom = input_probe_rom();
    let replay = record(&rom, 300, 60);
    // 跨過各種位置：第 2 幀（第一個會被程式讀到的輸入）、檢查點前後、reset 那一幀、最後一幀。
    for k in [2u32, 30, 59, 60, 61, 120, 149, 151, 200, 299, 300] {
        tampered_verify(&rom, &replay, k, |i| i.p1 ^= Buttons::SELECT);
    }
    // 玩家 2 的輸入。
    tampered_verify(&rom, &replay, 77, |i| i.p2 ^= Buttons::SELECT);
    // 拿掉那一次 reset（第 151 幀，1 起算）。
    tampered_verify(&rom, &replay, 151, |i| i.reset = false);
    // 多加一次 reset。
    tampered_verify(&rom, &replay, 33, |i| i.reset = true);
}

/// 用密集的檢查點（間隔 1）時，分歧範圍縮到單一幀。
#[test]
fn with_dense_checkpoints_the_range_narrows_to_one_frame() {
    let rom = input_probe_rom();
    let replay = record(&rom, 100, 1);
    let mut inputs: Vec<FrameInput> = replay.inputs().collect();
    inputs[41].p1 ^= Buttons::SELECT; // 第 42 幀
    let mut bad = replay.clone();
    bad.runs = Replay::compress(inputs);
    let ReplayError::Mismatch(m) = verify(&rom, &bad).unwrap_err() else {
        panic!()
    };
    assert_eq!((m.frame, m.last_good_frame), (42, Some(41)));
    assert_eq!(m.suspect_frames(), 42..=42);
}

/// 竄改檢查點本身（模擬「核心行為變了」）：第一個不符的檢查點與範圍。
#[test]
fn a_wrong_checkpoint_fingerprint_is_the_first_mismatch() {
    let rom = input_probe_rom();
    let mut replay = record(&rom, 300, 60);
    replay.checkpoints[3].fingerprint ^= 1; // 第 180 幀
    let ReplayError::Mismatch(m) = verify(&rom, &replay).unwrap_err() else {
        panic!()
    };
    assert_eq!((m.frame, m.last_good_frame), (180, Some(120)));
    assert_eq!(m.suspect_frames(), 121..=180);
    let text = m.to_string();
    assert!(
        text.contains("第 180 幀") && text.contains("121–180"),
        "{text}"
    );

    // 連開機狀態的檢查點都不符。
    let mut replay = record(&rom, 30, 10);
    replay.checkpoints[0].fingerprint ^= 1;
    let ReplayError::Mismatch(m) = verify(&rom, &replay).unwrap_err() else {
        panic!()
    };
    assert_eq!(
        (m.frame, m.last_good_frame, m.suspect_frames()),
        (0, None, 0..=0)
    );
    assert!(m.to_string().contains("開機狀態"));
}

// ---- 解碼器 ---------------------------------------------------------------------

fn xorshift(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

/// 解碼器對任意位元組都不 panic，且接受的位元組一定是規範形式（decode 後 encode 得到同樣的位元組）。
fn decode_checked(bytes: &[u8]) -> bool {
    match Replay::decode(bytes) {
        Ok(replay) => {
            assert_eq!(replay.encode(), bytes, "接受的位元組必須是規範形式");
            true
        }
        Err(_) => false,
    }
}

#[test]
fn decoder_never_panics_on_random_truncated_or_mutated_bytes() {
    let mut state = 0x9E37_79B9_7F4A_7C15u64;
    let valid = record(&input_probe_rom(), 200, 25).encode();
    assert!(decode_checked(&valid));

    // 1. 完全隨機的位元組（一半有正確的 magic 與版本前綴，才走得進深處）。
    for round in 0..20_000 {
        let len = (xorshift(&mut state) % 160) as usize;
        let mut bytes: Vec<u8> = (0..len).map(|_| xorshift(&mut state) as u8).collect();
        if round % 2 == 0 && len >= 6 {
            bytes[..4].copy_from_slice(&REPLAY_MAGIC);
            bytes[4..6].copy_from_slice(&REPLAY_FORMAT_VERSION.to_le_bytes());
        }
        decode_checked(&bytes);
    }

    // 2. 有效 replay 的每一個截斷長度：一定被拒絕（不能默默接受不完整的檔案）。
    for len in 0..valid.len() {
        assert!(
            !decode_checked(&valid[..len]),
            "截斷到 {len} 位元組卻被接受"
        );
    }

    // 3. 有效 replay 的每個位置改成隨機值（多次），以及一次改多個位置。
    for offset in 0..valid.len() {
        for _ in 0..8 {
            let mut bytes = valid.clone();
            bytes[offset] = xorshift(&mut state) as u8;
            decode_checked(&bytes);
        }
    }
    for _ in 0..5_000 {
        let mut bytes = valid.clone();
        for _ in 0..(1 + xorshift(&mut state) % 6) {
            let i = (xorshift(&mut state) % bytes.len() as u64) as usize;
            bytes[i] = xorshift(&mut state) as u8;
        }
        decode_checked(&bytes);
    }

    // 4. 尾端多出的位元組。
    let mut extra = valid.clone();
    extra.push(0);
    assert_eq!(
        Replay::decode(&extra).unwrap_err(),
        ReplayError::TrailingBytes { extra: 1 }
    );
}

/// 標頭宣稱「幾十億個輸入段／檢查點」時，不能因此配置巨量記憶體或 panic：長度不夠就直接拒絕。
#[test]
fn decoder_rejects_absurd_counts_without_allocating() {
    let valid = record(&input_probe_rom(), 20, 10).encode();
    // 輸入段數在 offset 30。
    let mut bytes = valid.clone();
    bytes[30..34].copy_from_slice(&u32::MAX.to_le_bytes());
    assert_eq!(
        Replay::decode(&bytes).unwrap_err(),
        ReplayError::Truncated {
            what: "輸入串流"
        }
    );
    // 檢查點數在輸入串流之後。
    let replay = Replay::decode(&valid).unwrap();
    let count_offset = 34 + replay.runs.len() * 7;
    let mut bytes = valid;
    bytes[count_offset..count_offset + 4].copy_from_slice(&u32::MAX.to_le_bytes());
    assert_eq!(
        Replay::decode(&bytes).unwrap_err(),
        ReplayError::Truncated { what: "檢查點" }
    );
}

#[test]
fn decoder_reports_each_kind_of_malformed_file() {
    let base = Replay {
        core_behavior_version: CORE_BEHAVIOR_VERSION,
        rom_id: RomId::of_file(b"x"),
        total_frames: 3,
        checkpoint_interval: 60,
        runs: vec![
            InputRun {
                input: FrameInput::NONE,
                count: 2,
            },
            InputRun {
                input: FrameInput::NONE.with_reset(),
                count: 1,
            },
        ],
        checkpoints: vec![
            Checkpoint {
                frame: 0,
                fingerprint: 1,
            },
            Checkpoint {
                frame: 3,
                fingerprint: 2,
            },
        ],
    };
    let bytes = base.encode();
    assert_eq!(Replay::decode(&bytes).unwrap(), base);
    // 版面：標頭 34 位元組；第一段在 34..41、第二段在 41..48。
    let with = |f: &dyn Fn(&mut Vec<u8>)| {
        let mut b = bytes.clone();
        f(&mut b);
        Replay::decode(&b).unwrap_err()
    };

    assert_eq!(with(&|b| b[0] = b'X'), ReplayError::BadMagic);
    assert_eq!(
        with(&|b| b[4..6].copy_from_slice(&9u16.to_le_bytes())),
        ReplayError::UnsupportedFormat {
            found: 9,
            supported: REPLAY_FORMAT_VERSION
        }
    );
    assert_eq!(
        with(&|b| b[28..30].copy_from_slice(&0u16.to_le_bytes())),
        ReplayError::BadInterval
    );
    assert_eq!(
        with(&|b| b[36] = 0x02),
        ReplayError::BadFlags {
            index: 0,
            flags: 0x02
        }
    );
    assert_eq!(
        with(&|b| b[37..41].copy_from_slice(&0u32.to_le_bytes())),
        ReplayError::ZeroRun { index: 0 }
    );
    assert_eq!(
        with(&|b| b[24..28].copy_from_slice(&5u32.to_le_bytes())),
        ReplayError::FrameCountMismatch { sum: 3, total: 5 }
    );
    // 檢查點：幀號超過總幀數／沒有遞增。第一個檢查點在 52..64。
    assert_eq!(
        with(&|b| b[52..56].copy_from_slice(&4u32.to_le_bytes())),
        ReplayError::BadCheckpoint { index: 0, frame: 4 }
    );
    assert_eq!(
        with(&|b| b[64..68].copy_from_slice(&0u32.to_le_bytes())),
        ReplayError::BadCheckpoint { index: 1, frame: 0 }
    );
    assert_eq!(
        Replay::decode(&[]).unwrap_err(),
        ReplayError::Truncated { what: "標頭" }
    );
}

/// 位元組佈局的釘住值：標頭各欄位的位置與 little-endian。
#[test]
fn encoding_layout_is_as_documented() {
    let replay = Replay {
        core_behavior_version: 0x0203,
        rom_id: RomId::from_bytes([0xA0; 16]),
        total_frames: 0x0000_0102,
        checkpoint_interval: 0x0040,
        runs: vec![InputRun {
            input: FrameInput::new(Buttons::A, Buttons::RIGHT).with_reset(),
            count: 0x0000_0102,
        }],
        checkpoints: vec![Checkpoint {
            frame: 0x0000_0102,
            fingerprint: 0x0807_0605_0403_0201,
        }],
    };
    let mut expected = Vec::new();
    expected.extend_from_slice(b"NESR");
    expected.extend_from_slice(&[0x01, 0x00]); // 格式版本 1
    expected.extend_from_slice(&[0x03, 0x02]); // 核心行為版本
    expected.extend_from_slice(&[0xA0; 16]); // rom_id
    expected.extend_from_slice(&[0x02, 0x01, 0x00, 0x00]); // 總幀數
    expected.extend_from_slice(&[0x40, 0x00]); // 檢查點間隔
    expected.extend_from_slice(&[0x01, 0x00, 0x00, 0x00]); // 輸入段數
    expected.extend_from_slice(&[0x01, 0x80, 0x01, 0x02, 0x01, 0x00, 0x00]); // p1=A, p2=RIGHT, reset, ×0x102
    expected.extend_from_slice(&[0x01, 0x00, 0x00, 0x00]); // 檢查點數
    expected.extend_from_slice(&[0x02, 0x01, 0x00, 0x00]);
    expected.extend_from_slice(&[0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08]);
    assert_eq!(replay.encode(), expected);
    assert_eq!(Replay::decode(&expected).unwrap(), replay);
}

// ---- 雙實例工具 -----------------------------------------------------------------

#[test]
fn two_instances_with_the_same_inputs_never_diverge() {
    for rom in [input_probe_rom(), rendering_rom()] {
        assert_eq!(run_both(&rom, &script_inputs(200)).unwrap(), None);
    }
}

/// 兩個實例吃不同的輸入：第一個不同的幀就是輸入開始不同的那一幀，且回報固定不變。
#[test]
fn dual_runner_reports_the_first_divergent_frame() {
    let mut runner = DualRunner::new(&input_probe_rom()).unwrap();
    for (i, input) in script_inputs(60).into_iter().enumerate() {
        let mut other = input;
        if i == 16 {
            other.p2 |= Buttons::SELECT; // 第 17 幀
        }
        let result = runner.step(input, other);
        if i < 16 {
            assert_eq!(result, None);
        } else {
            let d = result.expect("第 17 幀起有分歧");
            assert_eq!(d.frame, 17);
            assert_ne!(d.a, d.b);
        }
    }
    assert_eq!(runner.frames(), 60);
    assert_eq!(runner.first_divergence().unwrap().frame, 17);
}

/// rollback 重跑幀會關閉輸出：兩個實例一個開一個關（還逐幀切換），指紋仍然一致。
#[test]
fn output_switch_never_causes_a_divergence() {
    for rom in [input_probe_rom(), rendering_rom()] {
        let mut runner = DualRunner::new(&rom).unwrap();
        for (i, input) in script_inputs(150).into_iter().enumerate() {
            runner.set_output(i % 3 != 0, i % 7 == 0);
            assert_eq!(runner.step(input, input), None, "第 {} 幀", i + 1);
        }
    }
}

/// 兩端各自獨立跑、事後比對（4b／4c 的用法）：離線重播的逐幀指紋與錄製時的檢查點一致；
/// 兩份軌跡一樣長、一致 → 無分歧；動一個位置 → 回報那一幀。
#[test]
fn independent_traces_can_be_compared_afterwards() {
    let rom = input_probe_rom();
    let replay = record(&rom, 200, 25);

    let mut end_a = Nes::from_rom(&rom).unwrap();
    let mut end_b = Nes::from_rom(&rom).unwrap();
    end_b.set_output_enabled(false);
    let trace_a = fingerprint_trace(&mut end_a, replay.inputs());
    let trace_b = fingerprint_trace(&mut end_b, replay.inputs());
    assert_eq!(trace_a.len(), 200);
    assert_eq!(first_divergence(&trace_a, &trace_b), None);
    // 逐幀軌跡在檢查點的幀上等於 replay 裡記錄的指紋。
    for cp in replay.checkpoints.iter().filter(|c| c.frame > 0) {
        assert_eq!(
            trace_a[cp.frame as usize - 1],
            cp.fingerprint,
            "檢查點 {}",
            cp.frame
        );
    }

    let mut wrong = trace_b.clone();
    wrong[99] ^= 1;
    assert_eq!(
        first_divergence(&trace_a, &wrong),
        Some(Divergence {
            frame: 100,
            a: trace_a[99],
            b: wrong[99]
        })
    );
    // 長度不同但前綴相同：只比對共同的部分。
    assert_eq!(first_divergence(&trace_a, &trace_a[..50]), None);
}

/// rollback 性質（4c 的預演）：在第 k 幀存檔、往前跑、讀檔、關閉輸出用同樣的輸入重跑，
/// 逐幀指紋與不中斷的軌跡一致。
#[test]
fn rollback_resimulation_matches_the_uninterrupted_trace() {
    let rom = input_probe_rom();
    let inputs = script_inputs(200);
    let mut straight = Nes::from_rom(&rom).unwrap();
    let baseline = fingerprint_trace(&mut straight, inputs.iter().copied());

    let mut nes = Nes::from_rom(&rom).unwrap();
    fingerprint_trace(&mut nes, inputs[..120].iter().copied());
    let saved = nes.save_state();
    fingerprint_trace(&mut nes, inputs[120..160].iter().copied()); // 預測錯誤的幀
    nes.load_state(&saved).unwrap();
    nes.set_output_enabled(false);
    let redone = fingerprint_trace(&mut nes, inputs[120..].iter().copied());
    assert_eq!(redone, baseline[120..]);
}
