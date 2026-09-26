//! `Nes::copy_state_from`（rollback 的快照複製）的測試。規格見 `docs/architecture.md` §20.1。

use crate::dual::fingerprint_trace;
use crate::test_support::{
    ApuProbe, apu_probe_rom, cnrom_test_rom, dummy_read_probe_rom, input_probe_rom, mmc1_churn_rom,
    rendering_rom, uxrom_test_rom,
};
use crate::{Buttons, FrameInput, Nes};

fn script(i: u64) -> FrameInput {
    let p1 = Buttons::from_bits_truncate((i / 3).wrapping_mul(37) as u8);
    let p2 = Buttons::from_bits_truncate((i / 2).wrapping_mul(91).wrapping_add(5) as u8);
    let input = FrameInput::new(p1, p2);
    if i == 90 { input.with_reset() } else { input }
}

fn roms() -> Vec<(&'static str, Vec<u8>)> {
    vec![
        ("input_probe", input_probe_rom()),
        ("rendering", rendering_rom()),
        ("mmc1", mmc1_churn_rom()),
        ("uxrom", uxrom_test_rom()),
        ("cnrom", cnrom_test_rom()),
        ("dummy_read", dummy_read_probe_rom()),
        ("apu", apu_probe_rom(ApuProbe::DEFAULT)),
    ]
}

fn run(nes: &mut Nes, from: u64, to: u64) {
    for i in from..to {
        nes.run_frame(script(i));
    }
}

/// 兩台走過**不同歷史**的實例，複製之後存檔位元組（涵蓋所有序列化欄位）與指紋完全相同——
/// 任何漏掉的狀態欄位都會讓存檔位元組不同。
#[test]
fn copy_state_from_makes_every_serialized_field_identical() {
    for (name, rom) in roms() {
        let mut src = Nes::from_rom(&rom).unwrap();
        run(&mut src, 0, 137);
        let mut dst = Nes::from_rom(&rom).unwrap();
        // 走到完全不同的地方（不同的輸入、不同的幀數）。
        for i in 0..53 {
            dst.run_frame(FrameInput::new(Buttons::all(), Buttons::A));
            let _ = i;
        }
        assert_ne!(
            src.save_state(),
            dst.save_state(),
            "{name}：前提：兩者要不同"
        );
        assert!(dst.copy_state_from(&src));
        assert_eq!(dst.save_state(), src.save_state(), "{name}");
        assert_eq!(
            dst.behavior_fingerprint(),
            src.behavior_fingerprint(),
            "{name}"
        );
        assert_eq!(dst.frame_count(), src.frame_count(), "{name}");
    }
}

/// 複製之後兩台接下來吃同樣的輸入，逐幀指紋相同（狀態真的完整，不只是快照當下看起來一樣）。
#[test]
fn copied_instance_continues_identically() {
    for (name, rom) in roms() {
        let mut src = Nes::from_rom(&rom).unwrap();
        run(&mut src, 0, 70);
        let mut dst = Nes::from_rom(&rom).unwrap();
        run(&mut dst, 500, 511);
        assert!(dst.copy_state_from(&src));
        let a = fingerprint_trace(&mut src, (70..200).map(script));
        let b = fingerprint_trace(&mut dst, (70..200).map(script));
        assert_eq!(a, b, "{name}");
    }
}

/// 輸出（畫面、音訊、輸出設定）不被複製：目的端維持自己的畫面、輸出開關與取樣率。
#[test]
fn copy_state_from_leaves_the_outputs_alone() {
    let rom = rendering_rom();
    let mut src = Nes::from_rom(&rom).unwrap();
    run(&mut src, 0, 30);
    let mut dst = Nes::from_rom(&rom).unwrap();
    run(&mut dst, 0, 12);
    dst.set_audio_sample_rate(22_050.0);
    dst.set_audio_channel_mask(0b0000_0101);
    let picture = dst.frame_buffer().hash64();
    assert_ne!(picture, src.frame_buffer().hash64(), "前提：兩張畫面不同");
    let mut before = Vec::new();
    let mut probe = dst.clone();
    probe.drain_audio(&mut before);
    assert!(!before.is_empty(), "前提：目的端有尚未取走的取樣");

    assert!(dst.copy_state_from(&src));
    assert_eq!(dst.frame_buffer().hash64(), picture, "畫面沒有被動到");
    assert_eq!(dst.audio_sample_rate(), 22_050.0);
    assert_eq!(dst.audio_channel_mask(), 0b0000_0101);
    let mut after = Vec::new();
    dst.drain_audio(&mut after);
    assert_eq!(after, before, "尚未取走的音訊取樣沒有被動到");

    dst.set_output_enabled(false);
    assert!(dst.copy_state_from(&src));
    assert!(!dst.output_enabled(), "輸出開關沒有被動到");
    assert_eq!(dst.behavior_fingerprint(), src.behavior_fingerprint());
}

/// 不同的 ROM：拒絕，什麼都不改。
#[test]
fn copy_state_from_refuses_a_different_rom() {
    let mut a = Nes::from_rom(&input_probe_rom()).unwrap();
    let mut b = Nes::from_rom(&rendering_rom()).unwrap();
    run(&mut a, 0, 5);
    run(&mut b, 0, 9);
    let before = b.save_state();
    assert!(!b.copy_state_from(&a));
    assert_eq!(b.save_state(), before);
}

/// 規格第 1 節的測試：在第 F 幀存快照、繼續跑、還原、以相同輸入重跑（重跑幀關閉輸出，
/// 最後一幀開啟），行為指紋與「從未還原」的版本逐幀相同。快照是 `copy_state_from` 進一個
/// 預先配置好的槽（沒有序列化、沒有重新配置）。
#[test]
fn snapshot_restore_and_replay_matches_the_never_restored_run() {
    const TOTAL: u64 = 160;
    for (name, rom) in roms() {
        let mut straight = Nes::from_rom(&rom).unwrap();
        straight.set_output_enabled(false);
        let baseline = fingerprint_trace(&mut straight, (0..TOTAL).map(script));

        for save_at in [0u64, 1, 37, 90, 91, 120] {
            let mut nes = Nes::from_rom(&rom).unwrap();
            let mut slot = nes.clone();
            run(&mut nes, 0, save_at);
            assert!(slot.copy_state_from(&nes)); // 在第 F 幀存快照
            // 繼續跑（用錯誤的輸入，模擬預測失誤）。
            for _ in 0..25 {
                nes.run_frame(FrameInput::new(Buttons::all(), Buttons::all()));
            }
            assert!(nes.copy_state_from(&slot)); // 還原
            nes.set_output_enabled(false);
            let mut redone = fingerprint_trace(&mut nes, (save_at..TOTAL - 1).map(script));
            nes.set_output_enabled(true); // 最後一幀開輸出
            redone.extend(fingerprint_trace(&mut nes, (TOTAL - 1..TOTAL).map(script)));
            assert_eq!(
                redone,
                baseline[save_at as usize..],
                "{name} 在第 {save_at} 幀存快照"
            );
        }
    }
}
