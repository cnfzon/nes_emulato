//! UI 執行緒：`eframe::App` 實作。
//!
//! 這個 struct 不持有 `Nes`——它只透過 `cmd_tx` 送指令給 emu 執行緒、
//! 從 `event_rx` 收事件、從 `frame_output`（triple buffer）讀最新畫面。

use std::thread::JoinHandle;

use crossbeam_channel::{Receiver, Sender};
use eframe::egui;
use nes_core::{Buttons, DebugSnapshot, PpuViews, RomInfo};

use crate::commands::{EmuCommand, EmuEvent};
use crate::debugger::DebuggerUi;
use crate::input::{
    HOTKEY_LOAD_STATE, HOTKEY_SAVE_STATE, PLAYER1_KEYS, PLAYER2_KEYS, buttons_from_keys,
};

pub struct NesApp {
    cmd_tx: Sender<EmuCommand>,
    event_rx: Receiver<EmuEvent>,
    frame_output: triple_buffer::Output<nes_core::FrameBuffer>,
    debug_output: triple_buffer::Output<Option<DebugSnapshot>>,
    emu_handle: Option<JoinHandle<()>>,
    debugger: DebuggerUi,

    texture: Option<egui::TextureHandle>,
    rom_info: Option<RomInfo>,
    show_debugger: bool,
    last_error: Option<String>,
    last_info: Option<String>,
    fps: f64,
    frame_count: u64,
    /// 兩位玩家最近一次送給 emu 執行緒的按鈕狀態。
    last_input: [Buttons; 2],
    quit_requested: bool,
    paused: bool,
    /// Debugger 面板「trace 到檔案」要記錄的指令數。
    trace_count: u32,
}

impl NesApp {
    pub fn new(
        cmd_tx: Sender<EmuCommand>,
        event_rx: Receiver<EmuEvent>,
        frame_output: triple_buffer::Output<nes_core::FrameBuffer>,
        debug_output: triple_buffer::Output<Option<DebugSnapshot>>,
        views_output: triple_buffer::Output<Option<PpuViews>>,
        emu_handle: JoinHandle<()>,
    ) -> Self {
        Self {
            cmd_tx,
            event_rx,
            frame_output,
            debug_output,
            emu_handle: Some(emu_handle),
            debugger: DebuggerUi::new(views_output),
            texture: None,
            rom_info: None,
            show_debugger: false,
            last_error: None,
            last_info: None,
            fps: 0.0,
            frame_count: 0,
            last_input: [Buttons::empty(); 2],
            quit_requested: false,
            paused: false,
            trace_count: 1000,
        }
    }

    fn toggle_pause(&mut self) {
        self.paused = !self.paused;
        let cmd = if self.paused {
            EmuCommand::Pause
        } else {
            EmuCommand::Resume
        };
        let _ = self.cmd_tx.send(cmd);
    }

    fn drain_events(&mut self) {
        for event in self.event_rx.try_iter() {
            match event {
                EmuEvent::RomLoaded(info) => {
                    self.rom_info = Some(info);
                    self.last_error = None;
                    self.last_info = None;
                }
                EmuEvent::Error(msg) => self.last_error = Some(msg),
                EmuEvent::FpsReport { fps } => self.fps = fps,
                EmuEvent::FrameAdvanced(frame) => self.frame_count = frame,
                EmuEvent::TraceWritten { path, lines } => {
                    self.last_info = Some(format!("已寫入 {lines} 行 trace 到 {}", path.display()));
                }
            }
        }
    }

    /// 讀鍵盤狀態，組成兩位玩家的按鈕狀態並送給 emu 執行緒（各自只在改變時送出，
    /// 避免每幀塞爆 channel）。
    ///
    /// 對應表與理由見 `input.rs`：玩家 1＝方向鍵、Z=B、X=A、Enter=Start、右 Shift=Select；
    /// 玩家 2＝WASD、F=B、G=A、T=Start、R=Select。
    fn poll_keyboard_input(&mut self, ctx: &egui::Context) {
        for (player, keys) in [&PLAYER1_KEYS, &PLAYER2_KEYS].into_iter().enumerate() {
            let buttons = ctx.input(|i| buttons_from_keys(keys, |key| i.key_down(key)));
            if buttons != self.last_input[player] {
                self.last_input[player] = buttons;
                let _ = self
                    .cmd_tx
                    .send(EmuCommand::SetInput(player as u8, buttons));
            }
        }
    }

    fn open_rom_dialog(&self) {
        if let Some(path) = rfd::FileDialog::new()
            .add_filter("NES ROM", &["nes"])
            .pick_file()
        {
            match std::fs::read(&path) {
                Ok(bytes) => {
                    let _ = self.cmd_tx.send(EmuCommand::LoadRom(bytes));
                }
                Err(e) => {
                    log::error!("讀取 ROM 檔案失敗: {e}");
                }
            }
        }
    }

    /// 選檔案並要求 emu 執行緒把接下來 `trace_count` 條指令的 trace 寫進去。
    fn trace_to_file_dialog(&self) {
        if let Some(path) = rfd::FileDialog::new()
            .add_filter("trace 文字檔", &["log", "txt"])
            .set_file_name("trace.log")
            .save_file()
        {
            let _ = self.cmd_tx.send(EmuCommand::TraceToFile {
                count: self.trace_count,
                path,
            });
        }
    }

    fn debugger_controls(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let pause_label = if self.paused { "繼續" } else { "暫停" };
            if ui.button(pause_label).clicked() {
                self.toggle_pause();
            }
        });
        // 單步與 trace 會讓 CPU 停在幀中間，只允許在暫停時使用。
        ui.add_enabled_ui(self.paused, |ui| {
            ui.horizontal(|ui| {
                if ui.button("單步指令").clicked() {
                    let _ = self.cmd_tx.send(EmuCommand::StepInstruction);
                }
                if ui.button("單步一幀").clicked() {
                    let _ = self.cmd_tx.send(EmuCommand::StepFrame);
                }
            });
            ui.horizontal(|ui| {
                ui.add(
                    egui::DragValue::new(&mut self.trace_count)
                        .range(1..=1_000_000)
                        .speed(10),
                );
                ui.label("條指令");
                if ui.button("Trace 到檔案…").clicked() {
                    self.trace_to_file_dialog();
                }
            });
        });
        if !self.paused {
            ui.weak("暫停後才能單步 / trace");
        }
    }

    fn ensure_texture(&mut self, ctx: &egui::Context) -> &egui::TextureHandle {
        let fb = self.frame_output.read();
        let image = egui::ColorImage::from_rgba_unmultiplied(
            [nes_core::frame::WIDTH, nes_core::frame::HEIGHT],
            fb.as_bytes(),
        );
        match &mut self.texture {
            Some(tex) => {
                tex.set(image, egui::TextureOptions::NEAREST);
            }
            None => {
                self.texture =
                    Some(ctx.load_texture("nes-screen", image, egui::TextureOptions::NEAREST));
            }
        }
        self.texture.as_ref().unwrap()
    }
}

impl eframe::App for NesApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();

        self.drain_events();
        self.poll_keyboard_input(&ctx);

        // Debugger 開著時，整個畫面只讀這一份快照：狀態列的幀數與面板的 cycle
        // 數出自同一次快照，才會一致。
        let snapshot: Option<DebugSnapshot> = if self.show_debugger {
            self.debug_output.read().clone()
        } else {
            None
        };
        self.debugger
            .sync_views(&ctx, self.show_debugger, &self.cmd_tx);

        egui::Panel::top("menu_bar").show(ui, |ui| {
            egui::MenuBar::new().ui(ui, |ui| {
                ui.menu_button("File", |ui| {
                    if ui.button("Open ROM...").clicked() {
                        self.open_rom_dialog();
                        ui.close();
                    }
                    if ui.button("Quit").clicked() {
                        self.quit_requested = true;
                        ui.close();
                    }
                });
                ui.menu_button("View", |ui| {
                    let response = ui.checkbox(&mut self.show_debugger, "Debugger");
                    if response.changed() {
                        let _ = self
                            .cmd_tx
                            .send(EmuCommand::SetDebugEnabled(self.show_debugger));
                    }
                });
                ui.menu_button("Emulation", |ui| {
                    let pause_label = if self.paused { "Resume" } else { "Pause" };
                    if ui.button(pause_label).clicked() {
                        self.toggle_pause();
                        ui.close();
                    }
                    if ui.button("Save State (F5)").clicked() {
                        let _ = self.cmd_tx.send(EmuCommand::SaveState);
                        ui.close();
                    }
                    if ui.button("Load State (F9)").clicked() {
                        let _ = self.cmd_tx.send(EmuCommand::LoadState);
                        ui.close();
                    }
                });
            });
        });

        ctx.input(|i| {
            if i.key_pressed(HOTKEY_SAVE_STATE) {
                let _ = self.cmd_tx.send(EmuCommand::SaveState);
            }
            if i.key_pressed(HOTKEY_LOAD_STATE) {
                let _ = self.cmd_tx.send(EmuCommand::LoadState);
            }
        });

        egui::Panel::bottom("status_bar").show(ui, |ui| {
            ui.horizontal(|ui| {
                match &self.rom_info {
                    Some(info) => {
                        ui.label(format!(
                            "Mapper {} | PRG {}x16KB | CHR {}x8KB",
                            info.mapper_id, info.prg_rom_banks, info.chr_rom_banks
                        ));
                    }
                    None => {
                        ui.label("尚未載入 ROM");
                    }
                }
                ui.separator();
                ui.label(format!("FPS: {:.1}", self.fps));
                ui.separator();
                let frame = snapshot
                    .as_ref()
                    .map_or(self.frame_count, |s| s.frame_count);
                ui.label(format!("Frame: {frame}"));
                if let Some(err) = &self.last_error {
                    ui.separator();
                    ui.colored_label(egui::Color32::RED, err);
                }
                if let Some(info) = &self.last_info {
                    ui.separator();
                    ui.label(info);
                }
            });
        });

        if self.show_debugger {
            egui::Panel::right("debugger")
                .default_size(360.0)
                .show(ui, |ui| {
                    ui.heading("Debugger");
                    self.debugger_controls(ui);
                    ui.separator();
                    self.debugger.show(ui, snapshot.as_ref());
                });
        }

        egui::CentralPanel::default().show(ui, |ui| {
            let available = ui.available_size();
            let scale = (available.x / nes_core::frame::WIDTH as f32)
                .min(available.y / nes_core::frame::HEIGHT as f32)
                .floor()
                .max(1.0);
            let size = egui::vec2(
                nes_core::frame::WIDTH as f32 * scale,
                nes_core::frame::HEIGHT as f32 * scale,
            );

            let texture = self.ensure_texture(&ctx);
            let image = egui::Image::new(texture).fit_to_exact_size(size);
            ui.centered_and_justified(|ui| ui.add(image));
        });

        if self.quit_requested {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
        }

        // 只要視窗還開著就持續要求重繪，讓畫面能跟上 emu 執行緒推進的新幀。
        ctx.request_repaint();
    }

    fn on_exit(&mut self) {
        let _ = self.cmd_tx.send(EmuCommand::Quit);
        if let Some(handle) = self.emu_handle.take() {
            let _ = handle.join();
        }
    }
}
