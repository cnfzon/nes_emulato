//! `nes-app`：eframe GUI 前端。
//!
//! 執行緒模型：
//! - **UI 執行緒**：`eframe::App`（見 `app.rs`），處理輸入、選單、畫面顯示。
//! - **Emu 執行緒**（見 `emu.rs`）：擁有唯一的 `Nes` 實例，以 60.0988 Hz
//!   固定步進推進模擬。
//!
//! 兩條執行緒間：`crossbeam-channel` 傳指令/事件（`commands.rs`），
//! `triple_buffer` 傳最新畫面、Debug 快照與 PPU 影像（不需要鎖，UI 執行緒讀取時
//! 不會擋到 emu 執行緒寫入下一幀）。

mod app;
mod audio;
mod commands;
mod debugger;
mod emu;
mod input;

use std::thread;

use app::NesApp;
use commands::EmuCommand;
use eframe::egui;
use nes_core::{DebugSnapshot, FrameBuffer, PpuViews};

/// Noto Sans CJK TC（OFL-1.1，見 `assets/fonts/OFL.txt`），作為中文字元的
/// fallback 字型，避免選單/Debugger 面板的中文顯示成方框。
const NOTO_SANS_CJK_TC: &[u8] = include_bytes!("../assets/fonts/NotoSansCJKtc-Regular.otf");

/// 把 CJK 字型加進 egui 的 proportional/monospace family，當作 fallback：
/// 拉丁字母/數字仍優先用 egui 預設字型，只有找不到對應字符（如中文）時才
/// 會落到這套字型。
fn install_cjk_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        "noto_sans_cjk_tc".to_owned(),
        egui::FontData::from_static(NOTO_SANS_CJK_TC).into(),
    );
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        fonts
            .families
            .entry(family)
            .or_default()
            .push("noto_sans_cjk_tc".to_owned());
    }
    ctx.set_fonts(fonts);
}

fn main() -> eframe::Result {
    env_logger::init();

    let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded::<EmuCommand>();
    let (event_tx, event_rx) = crossbeam_channel::unbounded::<commands::EmuEvent>();
    let (frame_input, frame_output) = triple_buffer::triple_buffer(&FrameBuffer::blank());
    let (debug_input, debug_output) = triple_buffer::triple_buffer::<Option<DebugSnapshot>>(&None);
    let (views_input, views_output) = triple_buffer::triple_buffer::<Option<PpuViews>>(&None);

    let emu_handle = thread::Builder::new()
        .name("nes-emu".to_string())
        .spawn(move || emu::run(cmd_rx, event_tx, frame_input, debug_input, views_input))
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
        Box::new(move |cc| {
            install_cjk_fonts(&cc.egui_ctx);
            Ok(Box::new(NesApp::new(
                cmd_tx,
                event_rx,
                frame_output,
                debug_output,
                views_output,
                emu_handle,
            )))
        }),
    )
}
