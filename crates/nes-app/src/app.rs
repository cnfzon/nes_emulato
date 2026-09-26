//! UI 執行緒：`eframe::App` 實作。
//!
//! 這個 struct 不持有 `Nes`——它只透過 `cmd_tx` 送指令給 emu 執行緒、
//! 從 `event_rx` 收事件、從 `frame_output`（triple buffer）讀最新畫面。

use std::net::SocketAddr;
use std::path::PathBuf;
use std::thread::JoinHandle;

use crossbeam_channel::{Receiver, Sender};
use eframe::egui;
use nes_core::{Buttons, DebugSnapshot, PpuViews, ReplayMismatch, RomId, RomInfo};

use crate::audio::AudioOutput;
use crate::commands::{
    EmuCommand, EmuEvent, NetEndKind, NetPhase, NetStatus, PlaybackSpeed, SessionStatus,
};
use crate::debugger::DebuggerUi;
use crate::input::{
    HOTKEY_LOAD_STATE, HOTKEY_SAVE_STATE, PLAYER1_KEYS, PLAYER2_KEYS, buttons_from_keys,
};

/// 錄製／播放 replay 時停用的功能與原因（選單提示、Debugger 面板共用）。
const RESTRICTED_WHILE_REPLAY: &str = "錄製／播放 replay 中停用：讀取存檔（F9）、單步指令、Trace、載入別的 ROM。\
     replay 是「開機狀態 + 每幀輸入」，這些操作會讓模擬離開「從開機狀態依輸入序列執行」的軌道，\
     錄出來的 replay 之後就無法重播。暫停與「單步一幀」仍可用（單步的幀會被錄進 replay）。";

/// Netplay 期間停用的功能與原因（選單提示、Debugger 面板共用）。
const RESTRICTED_WHILE_NETPLAY: &str = "Netplay 中停用：讀取存檔（F9）、單步指令、Trace、載入別的 ROM、暫停、Reset、錄製／播放 replay。\
     原因：這些操作只發生在你這一端，會讓雙方的模擬分歧（同步暫停與讀檔留待之後評估）。\
     存檔仍可用：F5 存到記憶體、File → Save State to File 存成檔案，方便除錯。";

/// 預設的 Netplay 監聽 port。
const DEFAULT_NET_PORT: &str = "7000";

/// Netplay 自動存下的 replay 與 desync 狀態檔的固定資料夾：執行檔旁的 `netplay_replays/`。
fn netplay_replay_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("netplay_replays")))
        .unwrap_or_else(|| PathBuf::from("netplay_replays"))
}

/// Netplay 選單開啟的輸入視窗。
#[derive(Clone, Copy, PartialEq, Eq)]
enum NetDialog {
    Host,
    Join,
}

/// 上一場 Netplay 的結果（彈出視窗顯示原因與自動存下的檔案）。
struct NetResult {
    kind: NetEndKind,
    message: String,
    files: Vec<PathBuf>,
}

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

    /// Netplay 的階段與統計（來自 emu 執行緒）。
    net: NetStatus,
    net_dialog: Option<NetDialog>,
    net_port_text: String,
    net_addr_text: String,
    net_form_error: Option<String>,
    /// 房主決定的 input delay（0–8）；加入者使用房主的值。
    net_input_delay: u8,
    net_result: Option<NetResult>,
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
            net: NetStatus::default(),
            net_dialog: None,
            net_port_text: DEFAULT_NET_PORT.to_string(),
            net_addr_text: String::new(),
            net_form_error: None,
            net_input_delay: nes_net::DEFAULT_INPUT_DELAY,
            net_result: None,
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

    /// Netplay 進行中（含等待對手、連線中、中斷中）。
    fn netplaying(&self) -> bool {
        self.net.phase != NetPhase::Idle
    }

    /// 錄製、播放或 Netplay 中：會破壞「從開機狀態依輸入序列執行」（或讓 Netplay 雙方分歧）的功能都停用。
    fn busy(&self) -> bool {
        self.recording() || self.playing() || self.netplaying()
    }

    /// 目前停用功能的原因（提示文字）。
    fn restriction_text(&self) -> &'static str {
        if self.netplaying() {
            RESTRICTED_WHILE_NETPLAY
        } else {
            RESTRICTED_WHILE_REPLAY
        }
    }

    fn apply_volume(&self) {
        self.audio.shared.set_volume(self.volume, self.muted);
    }

    fn toggle_pause(&mut self) {
        if self.netplaying() {
            return; // Netplay 期間停用暫停（UI 已 disabled，這裡是保險）
        }
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
                EmuEvent::Net(status) => self.net = status,
                EmuEvent::NetEnded {
                    kind,
                    message,
                    files,
                } => {
                    self.net = NetStatus::default();
                    self.net_dialog = None;
                    match kind {
                        NetEndKind::Normal => {
                            self.last_error = None;
                            let saved = files
                                .iter()
                                .map(|p| p.display().to_string())
                                .collect::<Vec<_>>()
                                .join("、");
                            self.last_info = Some(if saved.is_empty() {
                                message
                            } else {
                                format!("{message}。replay：{saved}")
                            });
                        }
                        NetEndKind::Error | NetEndKind::Desync => {
                            self.last_error = Some(message.clone());
                            self.net_result = Some(NetResult {
                                kind,
                                message,
                                files,
                            });
                        }
                    }
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
        // 播放 replay 時輸入來自 replay：忽略鍵盤（emu 執行緒也會忽略 `SetInput`）。
        if self.playing() {
            return;
        }
        // Netplay：兩台電腦的本地鍵盤都用玩家 1 的按鍵配置，由 session 對應到被分配的玩家位置；
        // 玩家 2 的按鍵（WASD…）不作用。
        let maps: &[&crate::input::KeyMap] = if self.netplaying() {
            &[&PLAYER1_KEYS]
        } else {
            &[&PLAYER1_KEYS, &PLAYER2_KEYS]
        };
        for (player, keys) in maps.iter().enumerate() {
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
            if ui
                .add_enabled(!self.netplaying(), egui::Button::new(pause_label))
                .on_disabled_hover_text(RESTRICTED_WHILE_NETPLAY)
                .clicked()
            {
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
            ui.colored_label(egui::Color32::YELLOW, self.restriction_text());
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

    /// Netplay 選單：建立房間、加入、input delay、取消／中斷連線。
    fn netplay_menu(&mut self, ui: &mut egui::Ui) {
        ui.menu_button("Netplay", |ui| {
            let idle = self.net.phase == NetPhase::Idle;
            let can_start = idle && self.rom_info.is_some() && !self.busy();
            let why_not = "需要先載入 ROM，且不能在錄製／播放 replay／Netplay 中";
            if ui
                .add_enabled(can_start, egui::Button::new("建立房間…"))
                .on_hover_text(
                    "你當房主（玩家 1）：在指定的 port 等對手連上。連線成功時雙方都會重新開機。",
                )
                .on_disabled_hover_text(why_not)
                .clicked()
            {
                self.net_dialog = Some(NetDialog::Host);
                self.net_form_error = None;
                ui.close();
            }
            if ui
                .add_enabled(can_start, egui::Button::new("加入…"))
                .on_hover_text(
                    "加入別人的房間（你是玩家 2）：輸入房主的 IP:port。雙方必須載入同一份 ROM。",
                )
                .on_disabled_hover_text(why_not)
                .clicked()
            {
                self.net_dialog = Some(NetDialog::Join);
                self.net_form_error = None;
                ui.close();
            }
            ui.horizontal(|ui| {
                ui.label("Input delay（幀）");
                ui.add_enabled(
                    idle,
                    egui::DragValue::new(&mut self.net_input_delay)
                        .range(0..=nes_net::MAX_INPUT_DELAY),
                )
                .on_hover_text(
                    "本地按鍵套用在 N 幀之後。越大越不容易 stall，但操作越延遲（2 幀約 33 ms）。",
                );
            });
            ui.weak("只有房主的設定有效；加入者使用房主決定的值");
            ui.separator();
            let cancel_label = match self.net.phase {
                NetPhase::Idle | NetPhase::Waiting { .. } | NetPhase::Connecting { .. } => "取消",
                NetPhase::Connected { .. } | NetPhase::Closing => "中斷連線",
            };
            if ui
                .add_enabled(
                    !idle && self.net.phase != NetPhase::Closing,
                    egui::Button::new(cancel_label),
                )
                .clicked()
            {
                let _ = self.cmd_tx.send(EmuCommand::NetDisconnect);
                ui.close();
            }
            if !idle {
                ui.separator();
                ui.colored_label(egui::Color32::YELLOW, RESTRICTED_WHILE_NETPLAY);
            }
        });
    }

    /// 狀態列的 Netplay 資訊：連線狀態、ping、input delay、stall 次數。
    fn net_label(&self, ui: &mut egui::Ui) {
        let text = match self.net.phase {
            NetPhase::Idle => return,
            NetPhase::Waiting { port } => format!("[Netplay] 等待對手連線（UDP port {port}）…"),
            NetPhase::Connecting { addr } => format!("[Netplay] 正在連線到 {addr}…"),
            NetPhase::Closing => "[Netplay] 中斷連線中…".to_string(),
            NetPhase::Connected {
                player,
                input_delay,
            } => {
                let s = &self.net.stats;
                let ping = s.rtt.map_or("—".to_string(), |r| {
                    format!("{:.0} ms", r.as_secs_f64() * 1000.0)
                });
                format!(
                    "[Netplay] 已連線｜你是玩家 {}｜ping {ping}｜input delay {input_delay}｜stall {} 次（{:.1} 秒）｜↑{} ↓{} B/s",
                    player + 1,
                    s.stalls,
                    s.stall_time.as_secs_f64(),
                    s.send_bytes_per_sec,
                    s.recv_bytes_per_sec,
                )
            }
        };
        ui.separator();
        ui.colored_label(egui::Color32::LIGHT_BLUE, text);
    }

    /// 建立房間／加入的輸入視窗。
    fn net_dialog_window(&mut self, ctx: &egui::Context) {
        let Some(dialog) = self.net_dialog else {
            return;
        };
        let mut close = false;
        let title = match dialog {
            NetDialog::Host => "Netplay：建立房間",
            NetDialog::Join => "Netplay：加入房間",
        };
        egui::Window::new(title)
            .collapsible(false)
            .resizable(false)
            .show(ctx, |ui| {
                match dialog {
                    NetDialog::Host => {
                        ui.horizontal(|ui| {
                            ui.label("監聽 port（UDP）");
                            ui.add(
                                egui::TextEdit::singleline(&mut self.net_port_text)
                                    .desired_width(70.0),
                            );
                        });
                        ui.label(format!("Input delay：{} 幀（Netplay 選單可調整）", self.net_input_delay));
                        ui.weak(
                            "把你的區網 IP（命令提示字元執行 ipconfig，看「IPv4 位址」）與這個 port 告訴對方。\
                             Windows 防火牆第一次跳出提示時，請允許存取（私人網路）。",
                        );
                    }
                    NetDialog::Join => {
                        ui.horizontal(|ui| {
                            ui.label("房主的 IP:port");
                            ui.add(
                                egui::TextEdit::singleline(&mut self.net_addr_text)
                                    .hint_text("192.168.1.10:7000")
                                    .desired_width(180.0),
                            );
                        });
                        ui.weak("你是玩家 2；雙方必須載入同一份 ROM，input delay 由房主決定。");
                    }
                }
                if let Some(err) = &self.net_form_error {
                    ui.colored_label(egui::Color32::LIGHT_RED, err);
                }
                ui.horizontal(|ui| {
                    let go = match dialog {
                        NetDialog::Host => "建立房間",
                        NetDialog::Join => "連線",
                    };
                    if ui.button(go).clicked() {
                        match self.start_netplay(dialog) {
                            Ok(()) => close = true,
                            Err(e) => self.net_form_error = Some(e),
                        }
                    }
                    if ui.button("取消").clicked() {
                        close = true;
                    }
                });
            });
        if close {
            self.net_dialog = None;
        }
    }

    /// 檢查輸入並送出 `NetHost`／`NetJoin`。
    fn start_netplay(&mut self, dialog: NetDialog) -> Result<(), String> {
        let replay_dir = netplay_replay_dir();
        match dialog {
            NetDialog::Host => {
                let port: u16 = self
                    .net_port_text
                    .trim()
                    .parse()
                    .map_err(|_| "port 必須是 0–65535 的整數（例如 7000）".to_string())?;
                let _ = self.cmd_tx.send(EmuCommand::NetHost {
                    port,
                    input_delay: self.net_input_delay,
                    replay_dir,
                });
            }
            NetDialog::Join => {
                let addr: SocketAddr = self.net_addr_text.trim().parse().map_err(|_| {
                    "格式必須是「IP:port」，例如 192.168.1.10:7000（要包含 port）".to_string()
                })?;
                let _ = self.cmd_tx.send(EmuCommand::NetJoin { addr, replay_dir });
            }
        }
        Ok(())
    }

    /// Netplay 結束後的說明視窗（錯誤與 desync；正常離開只在狀態列顯示）。
    fn net_result_window(&mut self, ctx: &egui::Context) {
        let Some(result) = &self.net_result else {
            return;
        };
        let mut close = false;
        let (title, color) = match result.kind {
            NetEndKind::Desync => ("Netplay：偵測到不同步（desync）", egui::Color32::LIGHT_RED),
            _ => ("Netplay 已結束", egui::Color32::YELLOW),
        };
        egui::Window::new(title)
            .collapsible(false)
            .resizable(false)
            .show(ctx, |ui| {
                ui.colored_label(color, &result.message);
                if !result.files.is_empty() {
                    ui.separator();
                    ui.label("已自動存下：");
                    for path in &result.files {
                        ui.add(egui::Label::new(path.display().to_string()).selectable(true));
                    }
                    if result.kind == NetEndKind::Desync {
                        ui.weak(
                            "分析：用 nes-test replay verify <rom> <replay> 找出分歧的幀範圍；\
                             用 nes-test diff-state 比對雙方的 .state 檔。",
                        );
                    }
                }
                ui.separator();
                ui.label("已回到單機模式。");
                if ui.button("關閉").clicked() {
                    close = true;
                }
            });
        if close {
            self.net_result = None;
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
                        .on_disabled_hover_text(self.restriction_text())
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
                    if ui
                        .add_enabled(!self.netplaying(), egui::Button::new(pause_label))
                        .on_disabled_hover_text(RESTRICTED_WHILE_NETPLAY)
                        .clicked()
                    {
                        self.toggle_pause();
                        ui.close();
                    }
                    if ui
                        .add_enabled(
                            self.rom_info.is_some() && !self.playing() && !self.netplaying(),
                            egui::Button::new("Reset (soft reset)"),
                        )
                        .on_hover_text(
                            "soft reset：CPU/PPU/APU 重置，RAM 保留；錄製時會記進 replay（reset 是輸入的一部分）",
                        )
                        .on_disabled_hover_text(
                            "需要載入 ROM；播放 replay 時 reset 來自 replay；Netplay 中停用（reset 不是同步輸入的一部分）",
                        )
                        .clicked()
                    {
                        let _ = self.cmd_tx.send(EmuCommand::Reset);
                        ui.close();
                    }
                    ui.separator();
                    if ui.button("Save State (F5)（記憶體）").clicked() {
                        let _ = self.cmd_tx.send(EmuCommand::SaveState);
                        ui.close();
                    }
                    if ui
                        .add_enabled(!self.busy(), egui::Button::new("Load State (F9)（記憶體）"))
                        .on_disabled_hover_text(self.restriction_text())
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
                        ui.colored_label(egui::Color32::YELLOW, self.restriction_text());
                    }
                });
                self.netplay_menu(ui);
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
                self.net_label(ui);
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

        self.net_dialog_window(&ctx);
        self.net_result_window(&ctx);

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
