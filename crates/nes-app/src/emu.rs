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
use nes_core::{Buttons, FrameBuffer, Nes};

use crate::commands::{EmuCommand, EmuEvent};

const TARGET_FPS: f64 = 60.0988;

pub fn run(
    cmd_rx: Receiver<EmuCommand>,
    event_tx: Sender<EmuEvent>,
    mut frame_input: triple_buffer::Input<FrameBuffer>,
) {
    let frame_duration = Duration::from_secs_f64(1.0 / TARGET_FPS);

    let mut nes: Option<Nes> = None;
    let mut current_input = [Buttons::empty(); 2];
    let mut paused = false;
    let mut saved_state: Option<Vec<u8>> = None;

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
