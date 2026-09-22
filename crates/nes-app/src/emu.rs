//! Emu 執行緒：擁有唯一的 `Nes` 實例，以固定頻率推進模擬。
//!
//! 計時策略：NES 的畫面更新頻率是 60.0988 Hz（不是剛好 60）。這裡用
//! sleep + 累積誤差（accumulator）補償的寫法——每次醒來就把經過的實際時間
//! 累加，只要累積夠一幀的時間預算就推進一幀，這樣長時間下來的平均幀率會
//! 準確收斂到目標值，不會因為 `sleep` 本身不精確而持續漂移。
//!
//! TODO：之後可以換成 `spin_sleep`（忙等 + 讓出時間片混合，減少 sleep 的
//! 系統排程抖動）或改由音訊裝置的 callback 驅動節奏（音訊硬體的時脈通常
//! 比作業系統計時器更穩定）。

use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use nes_core::{Buttons, DebugSnapshot, FrameBuffer, Nes};

use crate::commands::{EmuCommand, EmuEvent};

const TARGET_FPS: f64 = 60.0988;

pub fn run(
    cmd_rx: Receiver<EmuCommand>,
    event_tx: Sender<EmuEvent>,
    mut frame_input: triple_buffer::Input<FrameBuffer>,
    mut debug_input: triple_buffer::Input<Option<DebugSnapshot>>,
) {
    let frame_duration = Duration::from_secs_f64(1.0 / TARGET_FPS);

    let mut nes: Option<Nes> = None;
    let mut current_input = [Buttons::empty(); 2];
    let mut paused = false;
    let mut saved_state: Option<Vec<u8>> = None;
    // Debugger 面板是否開著——只在開著時才產生/傳送 `DebugSnapshot`（見
    // `EmuCommand::SetDebugEnabled` 的文件說明理由）。
    let mut debug_enabled = false;

    let mut accumulator = Duration::ZERO;
    let mut last_tick = Instant::now();

    let mut fps_window_start = Instant::now();
    let mut fps_window_frames: u64 = 0;

    'outer: loop {
        for cmd in cmd_rx.try_iter() {
            match cmd {
                EmuCommand::LoadRom(bytes) => match Nes::from_rom(&bytes) {
                    Ok(new_nes) => {
                        let info = new_nes.rom_info().clone();
                        if debug_enabled {
                            debug_input.write(Some(new_nes.debug_snapshot()));
                        }
                        nes = Some(new_nes);
                        let _ = event_tx.send(EmuEvent::RomLoaded(info));
                    }
                    Err(e) => {
                        let _ = event_tx.send(EmuEvent::Error(e.to_string()));
                    }
                },
                EmuCommand::SetInput(player, buttons) => {
                    if let Some(slot) = current_input.get_mut(player as usize) {
                        *slot = buttons;
                    }
                }
                EmuCommand::Pause => paused = true,
                EmuCommand::Resume => paused = false,
                EmuCommand::SetDebugEnabled(enabled) => {
                    debug_enabled = enabled;
                    // 剛打開面板時立刻送一份目前狀態，不必等到下一幀
                    // 才有資料（尤其是暫停中時，run_frame 不會再被呼叫）。
                    if debug_enabled && let Some(n) = &nes {
                        debug_input.write(Some(n.debug_snapshot()));
                    }
                }
                EmuCommand::SaveState => {
                    if let Some(n) = &nes {
                        saved_state = Some(n.save_state());
                    }
                }
                EmuCommand::LoadState => {
                    if let (Some(n), Some(state)) = (&mut nes, &saved_state)
                        && let Err(e) = n.load_state(state)
                    {
                        let _ = event_tx.send(EmuEvent::Error(e.to_string()));
                    }
                }
                EmuCommand::Quit => break 'outer,
            }
        }

        let now = Instant::now();
        accumulator += now.duration_since(last_tick);
        last_tick = now;

        while accumulator >= frame_duration {
            accumulator -= frame_duration;
            if !paused && let Some(n) = &mut nes {
                let fb = n.run_frame(current_input).clone();
                frame_input.write(fb);
                fps_window_frames += 1;
                if debug_enabled {
                    debug_input.write(Some(n.debug_snapshot()));
                }
            }
        }

        if fps_window_start.elapsed() >= Duration::from_secs(1) {
            let fps = fps_window_frames as f64 / fps_window_start.elapsed().as_secs_f64();
            let frame = nes.as_ref().map_or(0, |n| n.frame_count());
            let _ = event_tx.send(EmuEvent::FpsReport { fps, frame });
            fps_window_frames = 0;
            fps_window_start = Instant::now();
        }

        // 每毫秒醒來檢查一次指令 / 是否該推進下一幀，避免忙等吃滿一個核心。
        thread::sleep(Duration::from_millis(1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEADER_SIZE: usize = 16;
    const PRG_BANK_SIZE: usize = 16 * 1024;
    const CHR_BANK_SIZE: usize = 8 * 1024;

    /// 一份最小可用的 NROM 測試 ROM：1x16KB PRG + 1x8KB CHR。
    fn test_rom() -> Vec<u8> {
        let mut bytes = vec![0u8; HEADER_SIZE];
        bytes[0..4].copy_from_slice(b"NES\x1A");
        bytes[4] = 1; // PRG banks
        bytes[5] = 1; // CHR banks
        bytes.extend(vec![0xAAu8; PRG_BANK_SIZE]);
        bytes.extend(vec![0xBBu8; CHR_BANK_SIZE]);
        bytes
    }

    /// 不需要 GUI 的迴歸測試：啟動真正的 emu 執行緒、載入 ROM、打開
    /// debug 開關、跑數幀，確認收到的 `DebugSnapshot` 不是「沒資料」也不是
    /// `DebugSnapshot::default()`，欄位看起來合理，最後正常關閉執行緒。
    ///
    /// 這是為了防止「Debugger 面板顯示假的全 0 資料」這個 bug 再次發生：
    /// 當年的 bug 沒有被任何測試抓到，就是因為所有測試都只測
    /// `nes-core`，沒有人測過 emu 執行緒到 UI 這段真正的資料傳輸路徑。
    #[test]
    fn emu_thread_streams_real_debug_snapshots_while_running() {
        let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded::<EmuCommand>();
        let (event_tx, event_rx) = crossbeam_channel::unbounded::<EmuEvent>();
        let (frame_input, _frame_output) = triple_buffer::triple_buffer(&FrameBuffer::blank());
        let (debug_input, mut debug_output) =
            triple_buffer::triple_buffer::<Option<DebugSnapshot>>(&None);

        let handle = thread::spawn(move || run(cmd_rx, event_tx, frame_input, debug_input));

        cmd_tx.send(EmuCommand::LoadRom(test_rom())).unwrap();
        match event_rx.recv_timeout(Duration::from_secs(2)) {
            Ok(EmuEvent::RomLoaded(_)) => {}
            other => panic!("expected RomLoaded, got {other:?}"),
        }

        cmd_tx.send(EmuCommand::SetDebugEnabled(true)).unwrap();

        // Poll 直到看到「跑過至少一幀」的快照，或逾時失敗——emu 執行緒的
        // 節奏是真實時間驅動的，不是決定性的，所以用 poll 而不是固定次數。
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut observed: Option<DebugSnapshot> = None;
        while Instant::now() < deadline {
            if let Some(snap) = debug_output.read().clone()
                && snap.cpu_cycles > 0
            {
                observed = Some(snap);
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }

        let snap = observed.expect("沒有在逾時前收到有進度的 DebugSnapshot");
        assert_ne!(snap, DebugSnapshot::default());
        assert!(snap.cpu_cycles > 0);
        assert_eq!(snap.cpu_sp, 0xFD, "test_rom 的內容不會動到堆疊指標");

        cmd_tx.send(EmuCommand::Quit).unwrap();
        handle.join().unwrap();
    }
}
