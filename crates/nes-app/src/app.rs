//! UI 執行緒：`eframe::App` 實作。
//!
//! 這個 struct 不持有 `Nes`——它只透過 `cmd_tx` 送指令給 emu 執行緒、
//! 從 `event_rx` 收事件、從 `frame_output`（triple buffer）讀最新畫面。

use std::thread::JoinHandle;

use crossbeam_channel::{Receiver, Sender};
use eframe::egui;
use nes_core::{Buttons, DebugSnapshot, PpuViews, ReplayMismatch, RomId, RomInfo};

use crate::audio::AudioOutput;
use crate::commands::{EmuCommand, EmuEvent, PlaybackSpeed, SessionStatus};
use crate::debugger::DebuggerUi;
use crate::input::{
    HOTKEY_LOAD_STATE, HOTKEY_SAVE_STATE, PLAYER1_KEYS, PLAYER2_KEYS, buttons_from_keys,
};

/// 錄製／播放 replay 時停用的功能與原因（選單提示、Debugger 面板共用）。
const RESTRICTED_WHILE_REPLAY: &str = "錄製／播放 replay 中停用：讀取存檔（F9）、單步指令、Trace、載入別的 ROM。\
     replay 是「開機狀態 + 每幀輸入」，這些操作會讓模擬離開「從開機狀態依輸入序列執行」的軌道，\
     錄出來的 replay 之後就無法重播。暫停與「單步一幀」仍可用（單步的幀會被錄進 replay）。";

/// NES 的畫面更新率（NTSC），只用來把幀數換算成秒數顯示。
const NTSC_FPS: f64 = 60.0988;

pub struct NesApp {
    cmd_tx: Sender<EmuCommand>,
    event_rx: Receiver<EmuEvent>,
    frame_output: triple_buffer::Output<nes_core::FrameBuffer>,
    debug_output: triple_buffer::Output<Option<DebugSnapshot>>,
    emu_handle: Option<JoinHandle<()>>,
    debugger: DebuggerUi,

    texture: Option<egui::TextureHandle>,
    rom_info: Option<RomInfo>,
    /// 目前 ROM 的識別碼（整個檔案的 xxh3-128），狀態列顯示前 16 個十六進位字元。
    rom_id: Option<RomId>,
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

    /// 音訊輸出（持有 cpal 的串流與共享狀態）。
    audio: AudioOutput,
    /// 主音量（0.0–1.0）與靜音。
    volume: f32,
    muted: bool,
    /// 聽得到的 APU 聲道（`nes_core::apu::CHANNEL_*` 位元）。
    channel_mask: u8,

    /// 錄製／播放的狀態（來自 emu 執行緒）。
    session: SessionStatus,
    playback_speed: PlaybackSpeed,
    /// 錄製結束但還沒有存成檔案的 replay（使用者取消了存檔對話框時保留，可從 File 選單再存）。
    unsaved_recording: Option<Vec<u8>>,
    /// 檢查點不符時彈出的視窗內容。
    mismatch_window: Option<ReplayMismatch>,
}

impl NesApp {
    pub fn new(
        cmd_tx: Sender<EmuCommand>,
        event_rx: Receiver<EmuEvent>,
        frame_output: triple_buffer::Output<nes_core::FrameBuffer>,
        debug_output: triple_buffer::Output<Option<DebugSnapshot>>,
        views_output: triple_buffer::Output<Option<PpuViews>>,
        emu_handle: JoinHandle<()>,
        audio: AudioOutput,
    ) -> Self {
        let volume = 0.7;
        audio.shared.set_volume(volume, false);
        Self {
            cmd_tx,
            event_rx,
            frame_output,
            debug_output,
            emu_handle: Some(emu_handle),
            debugger: DebuggerUi::new(views_output),
            texture: None,
            rom_info: None,
            rom_id: None,
            show_debugger: false,
            last_error: None,
            last_info: None,
            fps: 0.0,
            frame_count: 0,
            last_input: [Buttons::empty(); 2],
            quit_requested: false,
            paused: false,
            trace_count: 1000,
            audio,
            volume,
            muted: false,
            channel_mask: nes_core::apu::ALL_CHANNELS,
            session: SessionStatus::Idle,
            playback_speed: PlaybackSpeed::X1,
            unsaved_recording: None,
            mismatch_window: None,
        }
    }

    /// 正在錄製。
    fn recording(&self) -> bool {
        matches!(self.session, SessionStatus::Recording { .. })
    }

    /// 正在播放 replay（輸入來自 replay，鍵盤被忽略）。
    fn playing(&self) -> bool {
        matches!(self.session, SessionStatus::Playing { .. })
    }

    /// 錄製或播放中：會破壞「從開機狀態依輸入序列執行」的功能都停用。
    fn busy(&self) -> bool {
        self.recording() || self.playing()
    }

    fn apply_volume(&self) {
        self.audio.shared.set_volume(self.volume, self.muted);
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
        // 先收集再處理：處理某些事件會開檔案對話框（需要 &mut self）。
        let events: Vec<EmuEvent> = self.event_rx.try_iter().collect();
        for event in events {
            match event {
                EmuEvent::RomLoaded(info, id) => {
                    self.rom_info = Some(info);
                    self.rom_id = Some(id);
                    self.last_error = None;
                    self.last_info = None;
                }
                EmuEvent::Error(msg) => self.last_error = Some(msg),
                EmuEvent::FpsReport { fps } => self.fps = fps,
                EmuEvent::FrameAdvanced(frame) => self.frame_count = frame,
                EmuEvent::TraceWritten { path, lines } => {
                    self.last_info = Some(format!("已寫入 {lines} 行 trace 到 {}", path.display()));
                }
                EmuEvent::Session(status) => {
                    if let SessionStatus::Mismatch(m) = status {
                        self.mismatch_window = Some(m);
                    }
                    self.session = status;
                }
                EmuEvent::Paused(paused) => self.paused = paused,
                EmuEvent::RecordingFinished { bytes, frames } => {
                    self.last_info = Some(format!("錄製結束：{frames} 幀"));
                    self.unsaved_recording = Some(bytes);
                    self.save_recording_dialog();
                }
                EmuEvent::StateExported(bytes) => self.save_state_file_dialog(&bytes),
            }
        }
    }

    /// 讀鍵盤狀態，組成兩位玩家的按鈕狀態並送給 emu 執行緒（各自只在改變時送出，
    /// 避免每幀塞爆 channel）。
    ///
    /// 對應表與理由見 `input.rs`：玩家 1＝方向鍵、Z=B、X=A、Enter=Start、右 Shift=Select；
    /// 玩家 2＝WASD、F=B、G=A、T=Start、R=Select。
    fn poll_keyboard_input(&mut self, ctx: &egui::Context) {
        // 播放 replay 時輸入來自 replay：忽略鍵盤（emu 執行緒也會忽略 `SetInput`）。
        if self.playing() {
            return;
        }
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

    /// 把剛錄好的 replay 存成檔案（rfd 對話框）。取消時保留在記憶體，可從 File 選單再存。
    fn save_recording_dialog(&mut self) {
        let Some(bytes) = self.unsaved_recording.take() else {
            return;
        };
        let picked = rfd::FileDialog::new()
            .add_filter("NES replay", &["replay"])
            .set_file_name("recording.replay")
            .save_file();
        let Some(path) = picked else {
            self.unsaved_recording = Some(bytes);
            self.last_info = Some("尚未儲存 replay（可用 File → Save Recording As… 再存）".into());
            return;
        };
        match std::fs::write(&path, &bytes) {
            Ok(()) => {
                self.last_error = None;
                self.last_info = Some(format!(
                    "已儲存 replay：{}（{} 位元組）",
                    path.display(),
                    bytes.len()
                ));
            }
            Err(e) => {
                self.last_error = Some(format!("儲存 replay 失敗: {e}"));
                self.unsaved_recording = Some(bytes);
            }
        }
    }

    fn open_replay_dialog(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter("NES replay", &["replay"])
            .pick_file()
        else {
            return;
        };
        match std::fs::read(&path) {
            Ok(bytes) => {
                self.mismatch_window = None;
                let _ = self.cmd_tx.send(EmuCommand::StartReplay(bytes));
            }
            Err(e) => self.last_error = Some(format!("讀取 replay 失敗: {e}")),
        }
    }

    /// 把目前狀態的存檔存成檔案（供 `nes-test diff-state` 比對）。
    fn save_state_file_dialog(&mut self, bytes: &[u8]) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter("NES save state", &["state"])
            .set_file_name("snapshot.state")
            .save_file()
        else {
            return;
        };
        match std::fs::write(&path, bytes) {
            Ok(()) => {
                self.last_error = None;
                self.last_info = Some(format!("已儲存存檔：{}", path.display()));
            }
            Err(e) => self.last_error = Some(format!("儲存存檔失敗: {e}")),
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
        // 單步指令與 trace 會讓 CPU 停在幀中間，只允許在暫停時使用；錄製／播放 replay 時更完全停用
        // （會破壞「從開機狀態依輸入序列執行」）。「單步一幀」就是一次 `run_frame`，錄製時會被記錄。
        let busy = self.busy();
        ui.horizontal(|ui| {
            ui.add_enabled_ui(self.paused && !busy, |ui| {
                if ui.button("單步指令").clicked() {
                    let _ = self.cmd_tx.send(EmuCommand::StepInstruction);
                }
            });
            ui.add_enabled_ui(self.paused, |ui| {
                if ui.button("單步一幀").clicked() {
                    let _ = self.cmd_tx.send(EmuCommand::StepFrame);
                }
            });
        });
        ui.add_enabled_ui(self.paused && !busy, |ui| {
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
        if busy {
            ui.colored_label(egui::Color32::YELLOW, RESTRICTED_WHILE_REPLAY);
        } else if !self.paused {
            ui.weak("暫停後才能單步 / trace");
        }
    }

    /// 狀態列的音訊資訊：緩衝區填充量與累計 underrun 次數（沒有裝置時顯示提示）。
    fn audio_status_label(&self, ui: &mut egui::Ui) {
        let shared = &self.audio.shared;
        if !shared.is_active() {
            ui.colored_label(egui::Color32::YELLOW, "音訊：無裝置（無聲）");
            return;
        }
        let fill_ms = shared.fill_ms();
        ui.label(format!(
            "音訊：緩衝 {fill_ms:.0} ms | underrun {}",
            shared.underruns()
        ));
    }

    /// 狀態列的錄製／播放狀態。用文字標記（不用符號字元，避免字型缺字顯示成方框）。
    fn session_label(&self, ui: &mut egui::Ui) {
        match self.session {
            SessionStatus::Idle => {}
            SessionStatus::Recording { frames } => {
                ui.separator();
                ui.colored_label(
                    egui::Color32::LIGHT_RED,
                    format!(
                        "[錄製中] 第 {frames} 幀（{:.1} 秒）",
                        f64::from(frames) / NTSC_FPS
                    ),
                );
            }
            SessionStatus::Playing {
                frame,
                total,
                verified,
                checkpoints,
            } => {
                ui.separator();
                let speed = match self.playback_speed {
                    PlaybackSpeed::X1 => "1x",
                    PlaybackSpeed::X2 => "2x",
                    PlaybackSpeed::Max => "最快",
                };
                ui.colored_label(
                    egui::Color32::LIGHT_GREEN,
                    format!(
                        "[播放中 {speed}] 第 {frame}/{total} 幀｜檢查點 {verified}/{checkpoints} 已驗證相符｜鍵盤輸入已停用"
                    ),
                );
            }
            SessionStatus::Finished { total, checkpoints } => {
                ui.separator();
                ui.colored_label(
                    egui::Color32::LIGHT_GREEN,
                    format!("[播放完成] {total} 幀，{checkpoints} 個檢查點全數相符"),
                );
            }
            SessionStatus::Mismatch(m) => {
                ui.separator();
                let range = m.suspect_frames();
                ui.colored_label(
                    egui::Color32::LIGHT_RED,
                    format!(
                        "[檢查點不符] 第 {} 幀；分歧發生在第 {}–{} 幀之間",
                        m.frame,
                        range.start(),
                        range.end()
                    ),
                );
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
                    let busy = self.busy();
                    if ui
                        .add_enabled(!busy, egui::Button::new("Open ROM..."))
                        .on_disabled_hover_text(RESTRICTED_WHILE_REPLAY)
                        .clicked()
                    {
                        self.open_rom_dialog();
                        ui.close();
                    }
                    if ui
                        .add_enabled(
                            self.rom_info.is_some(),
                            egui::Button::new("Save State to File..."),
                        )
                        .on_hover_text("把目前狀態存成 .state 檔，可用 nes-test diff-state 逐欄位比對")
                        .clicked()
                    {
                        let _ = self.cmd_tx.send(EmuCommand::ExportState);
                        ui.close();
                    }
                    if ui
                        .add_enabled(
                            self.unsaved_recording.is_some(),
                            egui::Button::new("Save Recording As..."),
                        )
                        .on_disabled_hover_text("沒有尚未儲存的錄製")
                        .clicked()
                    {
                        self.save_recording_dialog();
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
                ui.menu_button("Audio", |ui| {
                    if ui.checkbox(&mut self.muted, "靜音").changed() {
                        self.apply_volume();
                    }
                    ui.horizontal(|ui| {
                        ui.label("音量");
                        if ui
                            .add(egui::Slider::new(&mut self.volume, 0.0..=1.0).show_value(true))
                            .changed()
                        {
                            self.apply_volume();
                        }
                    });
                    ui.separator();
                    match (&self.audio.device_name, &self.audio.notice) {
                        (_, Some(notice)) => {
                            ui.colored_label(egui::Color32::YELLOW, notice);
                        }
                        (Some(name), None) => {
                            ui.label(format!(
                                "裝置：{name}（{} Hz）",
                                self.audio.shared.device_rate()
                            ));
                        }
                        (None, None) => {
                            ui.label("裝置：（未知）");
                        }
                    }
                });
                ui.menu_button("Emulation", |ui| {
                    let pause_label = if self.paused { "Resume" } else { "Pause" };
                    if ui.button(pause_label).clicked() {
                        self.toggle_pause();
                        ui.close();
                    }
                    if ui
                        .add_enabled(
                            self.rom_info.is_some() && !self.playing(),
                            egui::Button::new("Reset (soft reset)"),
                        )
                        .on_hover_text(
                            "soft reset：CPU/PPU/APU 重置，RAM 保留；錄製時會記進 replay（reset 是輸入的一部分）",
                        )
                        .on_disabled_hover_text("需要載入 ROM；播放 replay 時 reset 來自 replay")
                        .clicked()
                    {
                        let _ = self.cmd_tx.send(EmuCommand::Reset);
                        ui.close();
                    }
                    ui.separator();
                    if ui.button("Save State (F5)").clicked() {
                        let _ = self.cmd_tx.send(EmuCommand::SaveState);
                        ui.close();
                    }
                    if ui
                        .add_enabled(!self.busy(), egui::Button::new("Load State (F9)"))
                        .on_disabled_hover_text(RESTRICTED_WHILE_REPLAY)
                        .clicked()
                    {
                        let _ = self.cmd_tx.send(EmuCommand::LoadState);
                        ui.close();
                    }
                });
                ui.menu_button("Replay", |ui| {
                    let has_rom = self.rom_info.is_some();
                    let busy = self.busy();
                    if ui
                        .add_enabled(
                            has_rom && !busy,
                            egui::Button::new("Start Recording (重新開機)"),
                        )
                        .on_hover_text(
                            "會先重新開機（power-on）再開始錄製：replay 只包含「開機狀態 + 每幀輸入」，\
                             不含存檔，所以一定要從開機狀態開始。錄製中停用讀取存檔（F9）與單步指令。",
                        )
                        .on_disabled_hover_text("需要先載入 ROM，且不能已在錄製／播放中")
                        .clicked()
                    {
                        let _ = self.cmd_tx.send(EmuCommand::StartRecording);
                        ui.close();
                    }
                    if ui
                        .add_enabled(
                            self.recording(),
                            egui::Button::new("Stop and Save Recording..."),
                        )
                        .clicked()
                    {
                        let _ = self.cmd_tx.send(EmuCommand::StopRecording);
                        ui.close();
                    }
                    ui.separator();
                    if ui
                        .add_enabled(has_rom && !busy, egui::Button::new("Play Replay..."))
                        .on_hover_text(
                            "會重新開機並依 replay 的輸入逐幀重播，同時驗證檢查點；ROM 或核心版本不符會拒絕。\
                             播放期間鍵盤輸入被忽略。",
                        )
                        .on_disabled_hover_text("需要先載入對應的 ROM，且不能已在錄製／播放中")
                        .clicked()
                    {
                        self.open_replay_dialog();
                        ui.close();
                    }
                    if ui
                        .add_enabled(self.playing(), egui::Button::new("Stop Replay"))
                        .clicked()
                    {
                        let _ = self.cmd_tx.send(EmuCommand::StopReplay);
                        ui.close();
                    }
                    ui.separator();
                    ui.label("播放速度");
                    for (speed, label) in [
                        (PlaybackSpeed::X1, "1x"),
                        (PlaybackSpeed::X2, "2x（靜音）"),
                        (PlaybackSpeed::Max, "最快（靜音）"),
                    ] {
                        if ui
                            .radio_value(&mut self.playback_speed, speed, label)
                            .changed()
                        {
                            let _ = self.cmd_tx.send(EmuCommand::SetPlaybackSpeed(speed));
                        }
                    }
                    if busy {
                        ui.separator();
                        ui.colored_label(egui::Color32::YELLOW, RESTRICTED_WHILE_REPLAY);
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
                if let Some(id) = &self.rom_id {
                    ui.separator();
                    ui.label(format!("ROM {}", id.short()))
                        .on_hover_text(format!("rom_id（整個檔案的 xxh3-128）：{id}"));
                }
                ui.separator();
                ui.label(format!("FPS: {:.1}", self.fps));
                ui.separator();
                let frame = snapshot
                    .as_ref()
                    .map_or(self.frame_count, |s| s.frame_count);
                ui.label(format!("Frame: {frame}"));
                ui.separator();
                self.audio_status_label(ui);
                self.session_label(ui);
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
                    self.debugger.show(
                        ui,
                        snapshot.as_ref(),
                        &self.audio.shared,
                        &mut self.channel_mask,
                        &self.cmd_tx,
                    );
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

        if let Some(m) = self.mismatch_window {
            let mut open = true;
            egui::Window::new("Replay 驗證失敗")
                .collapsible(false)
                .resizable(false)
                .open(&mut open)
                .show(&ctx, |ui| {
                    let range = m.suspect_frames();
                    ui.label(format!(
                        "第 {} 幀的檢查點與 replay 記錄的行為指紋不符。",
                        m.frame
                    ));
                    ui.label(format!(
                        "分歧發生在第 {}–{} 幀之間（上一個相符的檢查點：{}）。",
                        range.start(),
                        range.end(),
                        m.last_good_frame
                            .map_or("無".to_string(), |p| format!("第 {p} 幀"))
                    ));
                    ui.label(format!(
                        "預期指紋 {:#018x}\n實際指紋 {:#018x}",
                        m.expected, m.actual
                    ));
                    ui.weak("模擬已暫停在不符的那一幀。第 n 幀＝第 n 次 run_frame。");
                });
            if !open {
                self.mismatch_window = None;
            }
        }

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
