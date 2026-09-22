//! UI 執行緒：`eframe::App` 實作。
//!
//! 這個 struct 不持有 `Nes`——它只透過 `cmd_tx` 送指令給 emu 執行緒、
//! 從 `event_rx` 收事件、從 `frame_output`（triple buffer）讀最新畫面。

use std::thread::JoinHandle;

use crossbeam_channel::{Receiver, Sender};
use eframe::egui;
use nes_core::{Buttons, DebugSnapshot, RomInfo};

use crate::commands::{EmuCommand, EmuEvent};

pub struct NesApp {
    cmd_tx: Sender<EmuCommand>,
    event_rx: Receiver<EmuEvent>,
    frame_output: triple_buffer::Output<nes_core::FrameBuffer>,
    debug_output: triple_buffer::Output<Option<DebugSnapshot>>,
    emu_handle: Option<JoinHandle<()>>,

    texture: Option<egui::TextureHandle>,
    rom_info: Option<RomInfo>,
    show_debugger: bool,
    last_error: Option<String>,
    fps: f64,
    frame_count: u64,
    last_input: Buttons,
    quit_requested: bool,
    paused: bool,
}

impl NesApp {
    pub fn new(
        cmd_tx: Sender<EmuCommand>,
        event_rx: Receiver<EmuEvent>,
        frame_output: triple_buffer::Output<nes_core::FrameBuffer>,
        debug_output: triple_buffer::Output<Option<DebugSnapshot>>,
        emu_handle: JoinHandle<()>,
    ) -> Self {
        Self {
            cmd_tx,
            event_rx,
            frame_output,
            debug_output,
            emu_handle: Some(emu_handle),
            texture: None,
            rom_info: None,
            show_debugger: false,
            last_error: None,
            fps: 0.0,
            frame_count: 0,
            last_input: Buttons::empty(),
            quit_requested: false,
            paused: false,
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
                }
                EmuEvent::Error(msg) => self.last_error = Some(msg),
                EmuEvent::FpsReport { fps, frame } => {
                    self.fps = fps;
                    self.frame_count = frame;
                }
            }
        }
    }

    /// 讀鍵盤狀態，組成玩家 1 的按鍵狀態並送給 emu 執行緒（只在改變時送出，
    /// 避免每幀塞爆 channel）。
    ///
    /// 對應：方向鍵、Z=B、X=A、Enter=Start、Right Shift=Select。
    fn poll_keyboard_input(&mut self, ctx: &egui::Context) {
        let buttons = ctx.input(|i| {
            let mut b = Buttons::empty();
            b.set(Buttons::UP, i.key_down(egui::Key::ArrowUp));
            b.set(Buttons::DOWN, i.key_down(egui::Key::ArrowDown));
            b.set(Buttons::LEFT, i.key_down(egui::Key::ArrowLeft));
            b.set(Buttons::RIGHT, i.key_down(egui::Key::ArrowRight));
            b.set(Buttons::B, i.key_down(egui::Key::Z));
            b.set(Buttons::A, i.key_down(egui::Key::X));
            b.set(Buttons::START, i.key_down(egui::Key::Enter));
            b.set(Buttons::SELECT, i.key_down(egui::Key::ShiftRight));
            b
        });

        if buttons != self.last_input {
            self.last_input = buttons;
            let _ = self.cmd_tx.send(EmuCommand::SetInput(0, buttons));
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
            if i.key_pressed(egui::Key::F5) {
                let _ = self.cmd_tx.send(EmuCommand::SaveState);
            }
            if i.key_pressed(egui::Key::F9) {
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
                ui.label(format!("Frame: {}", self.frame_count));
                if let Some(err) = &self.last_error {
                    ui.separator();
                    ui.colored_label(egui::Color32::RED, err);
                }
            });
        });

        if self.show_debugger {
            egui::Panel::right("debugger").show(ui, |ui| {
                ui.heading("Debugger");
                match self.debug_output.read() {
                    Some(snap) => {
                        ui.label(format!("PC: {:#06X}", snap.cpu_pc));
                        ui.label(format!(
                            "A: {:#04X}  X: {:#04X}  Y: {:#04X}",
                            snap.cpu_a, snap.cpu_x, snap.cpu_y
                        ));
                        ui.label(format!(
                            "SP: {:#04X}  Status: {:#04X}",
                            snap.cpu_sp, snap.cpu_status
                        ));
                        ui.label(format!("CPU cycles: {}", snap.cpu_cycles));
                        ui.label(format!("下一條指令: {}", snap.cpu_disassembly));
                        if snap.cpu_jammed {
                            ui.colored_label(egui::Color32::RED, "CPU JAMMED");
                        }
                        ui.separator();
                        ui.label(format!(
                            "PPU scanline: {}  cycle: {}",
                            snap.ppu_scanline, snap.ppu_cycle
                        ));
                        ui.label(format!("PPU frame: {}", snap.ppu_frame));
                        ui.separator();
                        ui.label(format!("APU frame counter: {}", snap.apu_frame_counter));
                    }
                    None => {
                        ui.label("尚未取得資料");
                    }
                }
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
