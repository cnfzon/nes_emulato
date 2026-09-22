//! `nes-app`：eframe GUI 前端。
//!
//! 執行緒模型：
//! - **UI 執行緒**：`eframe::App`（見 `app.rs`），處理輸入、選單、畫面顯示。
//! - **Emu 執行緒**（見 `emu.rs`）：擁有唯一的 `Nes` 實例，以 60.0988 Hz
//!   固定步進推進模擬。
//!
//! 兩條執行緒間：`crossbeam-channel` 傳指令/事件（`commands.rs`），
//! `triple_buffer` 傳最新畫面（不需要鎖，UI 執行緒讀取時不會擋到 emu 執行緒
//! 寫入下一幀）。

mod app;
mod audio;
mod commands;
mod emu;

use std::thread;

use app::NesApp;
use commands::EmuCommand;
use nes_core::FrameBuffer;

fn main() -> eframe::Result {
    env_logger::init();

    let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded::<EmuCommand>();
    let (event_tx, event_rx) = crossbeam_channel::unbounded::<commands::EmuEvent>();
    let (frame_input, frame_output) = triple_buffer::triple_buffer(&FrameBuffer::blank());

    let emu_handle = thread::Builder::new()
        .name("nes-emu".to_string())
        .spawn(move || emu::run(cmd_rx, event_tx, frame_input))
        .expect("failed to spawn emu thread");

    let native_options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([960.0, 720.0])
            .with_title("NES Netplay"),
        ..Default::default()
    };

    eframe::run_native(
        "NES Netplay",
        native_options,
        Box::new(move |_cc| {
            Ok(Box::new(NesApp::new(
                cmd_tx,
                event_rx,
                frame_output,
                emu_handle,
            )))
        }),
    )
}
