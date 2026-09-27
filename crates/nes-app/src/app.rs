//! UI 執行緒：`eframe::App` 實作。
//!
//! 這個 struct 不持有 `Nes`——它只透過 `cmd_tx` 送指令給 emu 執行緒、
//! 從 `event_rx` 收事件、從 `frame_output`（triple buffer）讀最新畫面。

use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use eframe::egui;
use nes_core::{Buttons, DebugSnapshot, PpuViews, ReplayMismatch, RomId, RomInfo};
use nes_net::Mode;

use crate::audio::AudioOutput;
use crate::commands::{
    EmuCommand, EmuEvent, NetEndKind, NetPhase, NetStatus, NetSummaryInfo, PlaybackSpeed,
    SessionStatus,
};
use crate::debugger::DebuggerUi;
use crate::input::{
    HOTKEY_LOAD_STATE, HOTKEY_SAVE_STATE, PLAYER1_KEYS, PLAYER2_KEYS, buttons_from_keys,
};
use crate::netui::{
    JOIN_TIMEOUT_SECS, NetHistory, RecentAddrs, SILENCE_WARN_AFTER, draw_line_chart, firewall_hint,
    firewall_hint_due, overlay_lines, parse_join_addr, parse_port,
};

/// 錄製／播放 replay 時停用的功能與原因（選單提示、Debugger 面板共用）。
const RESTRICTED_WHILE_REPLAY: &str = "錄製／播放 replay 中停用：讀取存檔（F9）、單步指令、Trace、載入別的 ROM。\
     replay 是「開機狀態 + 每幀輸入」，這些操作會讓模擬離開「從開機狀態依輸入序列執行」的軌道，\
     錄出來的 replay 之後就無法重播。暫停與「單步一幀」仍可用（單步的幀會被錄進 replay）。";

/// Netplay 期間停用的功能與原因（選單提示、Debugger 面板共用）。
const RESTRICTED_WHILE_NETPLAY: &str = "Netplay 中停用：讀取存檔（F9）、單步指令、Trace、載入別的 ROM、暫停、錄製／播放 replay。\
     原因：這些操作只發生在你這一端，會讓雙方的模擬分歧（同步暫停與讀檔留待之後評估）。\
     Reset 可用：它是輸入的一部分，任一方按下，雙方會在同一幀重置。\
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

/// 連線大廳的分頁。
#[derive(Clone, Copy, PartialEq, Eq)]
enum LobbyTab {
    Host,
    Join,
}

/// 區網 IPv4 位址的說明（顯示在大廳；限制見 `nes_net::transport::local_lan_ipv4`）。
const LAN_IP_NOTE: &str = "這是作業系統路由到外網時會使用的那張網卡的位址（不會送出任何封包）。\
     多張網卡（有線＋Wi-Fi、虛擬機／WSL）或 VPN 開著時，可能不是對方所在網段的那個：\
     不對時請在命令提示字元執行 ipconfig，看對方能連到的那張網卡的「IPv4 位址」。";

/// 上一場 Netplay 的結果（彈出視窗顯示原因、本場摘要與自動存下的檔案）。
struct NetResult {
    kind: NetEndKind,
    message: String,
    files: Vec<PathBuf>,
    /// 連上過才有：時長、總幀數、主要統計、replay 與統計 CSV 的路徑。
    summary: Option<NetSummaryInfo>,
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
    /// 連線大廳是否開著、目前在哪個分頁。
    lobby_open: bool,
    lobby_tab: LobbyTab,
    /// 本機區網 IPv4（開大廳時偵測）。
    lan_ip: Option<Ipv4Addr>,
    /// 最近連線過的位址（只存在記憶體，設定檔留到 Phase 5）。
    recent: RecentAddrs,
    /// 開始等待對手／握手的時間（算「已等待 n 秒」與 10 秒的防火牆提示）。
    wait_since: Option<Instant>,
    /// 握手階段失敗的原因（被拒絕、逾時、無法綁定 port）：顯示在大廳裡，直到下一次嘗試。
    lobby_error: Option<String>,
    /// 目前正在嘗試加入的位址；連線成功時記進最近連線清單。
    joining: Option<SocketAddr>,
    /// 統計疊加層（F3）。
    show_overlay: bool,
    /// 疊加層的折線資料（最近 10 秒的 ping 與每秒 rollback 次數）。
    history: NetHistory,
    net_port_text: String,
    net_addr_text: String,
    net_form_error: Option<String>,
    /// 房主選的連線模式（加入者跟隨房主）。
    net_mode: Mode,
    /// lockstep：房主決定的共用 input delay（0–8）。rollback：這一方的本地 input delay（0–4）。
    net_input_delay: u8,
    /// rollback 的預測視窗 K（1–32）。
    net_window: u32,
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
            lobby_open: false,
            lobby_tab: LobbyTab::Host,
            lan_ip: None,
            recent: RecentAddrs::default(),
            wait_since: None,
            lobby_error: None,
            joining: None,
            show_overlay: false,
            history: NetHistory::default(),
            net_port_text: DEFAULT_NET_PORT.to_string(),
            net_addr_text: String::new(),
            net_form_error: None,
            net_mode: Mode::Rollback,
            net_input_delay: nes_net::rollback::DEFAULT_INPUT_DELAY,
            net_window: nes_net::rollback::DEFAULT_WINDOW,
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

    /// 收到 emu 執行緒的 Netplay 狀態：記折線、處理階段轉換（計時、關大廳、記最近連線）。
    fn on_net_status(&mut self, status: NetStatus) {
        let prev = self.net.phase;
        let now_connected = matches!(status.phase, NetPhase::Connected { .. });
        let was_connected = matches!(prev, NetPhase::Connected { .. });
        if now_connected && !was_connected {
            // 連上了：大廳完成任務，記下這個位址，折線從頭開始。
            self.wait_since = None;
            self.lobby_open = false;
            self.lobby_error = None;
            self.history.clear();
            if let Some(addr) = self.joining.take() {
                self.recent.remember(addr);
            }
        }
        if prev == NetPhase::Idle
            && matches!(
                status.phase,
                NetPhase::Waiting { .. } | NetPhase::Connecting { .. }
            )
        {
            self.wait_since = Some(Instant::now());
            self.lobby_error = None;
        }
        if status.phase == NetPhase::Idle {
            self.wait_since = None;
        }
        self.history.record(&status);
        self.net = status;
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
                EmuEvent::Net(status) => {
                    self.on_net_status(status);
                }
                EmuEvent::NetEnded {
                    kind,
                    message,
                    files,
                    summary,
                } => {
                    self.net = NetStatus::default();
                    self.wait_since = None;
                    self.joining = None;
                    self.history.clear();
                    if summary.is_some() {
                        // 連上過：一律彈出摘要視窗（時長、總幀數、主要統計、replay 與統計 CSV 的路徑）。
                        // 對方正常離開會立即到這裡；網路中斷則要等 5 秒逾時。
                        match kind {
                            NetEndKind::Normal => {
                                self.last_error = None;
                                self.last_info = Some(message.clone());
                            }
                            NetEndKind::Error | NetEndKind::Desync => {
                                self.last_error = Some(message.clone());
                            }
                        }
                        self.net_result = Some(NetResult {
                            kind,
                            message,
                            files,
                            summary,
                        });
                    } else if kind != NetEndKind::Normal {
                        // 握手階段就失敗（被拒絕、逾時、無法綁定 port）：原因顯示在大廳裡。
                        self.last_error = Some(message.clone());
                        self.lobby_error = Some(message);
                        self.lobby_open = true;
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

    /// 開啟連線大廳（順便重新偵測本機區網位址）。
    fn open_lobby(&mut self) {
        self.lobby_open = true;
        self.lan_ip = nes_net::transport::local_lan_ipv4();
        self.net_form_error = None;
    }

    /// Netplay 選單：連線大廳、取消／中斷連線、統計疊加層。設定與輸入都在大廳裡。
    fn netplay_menu(&mut self, ui: &mut egui::Ui) {
        ui.menu_button("Netplay", |ui| {
            let idle = self.net.phase == NetPhase::Idle;
            if ui
                .button("連線大廳…")
                .on_hover_text("建立房間或加入房間：顯示本機區網 IP、最近連線過的位址、握手狀態與取消")
                .clicked()
            {
                self.open_lobby();
                ui.close();
            }
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
            ui.separator();
            ui.checkbox(&mut self.show_overlay, "統計疊加層（F3）")
                .on_hover_text("半透明地覆蓋在遊戲畫面上：ping、rollback、預測準確率、stall、頻寬……與最近 10 秒的折線。不影響模擬");
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
                mode,
            } => {
                let s = &self.net.stats;
                let ping = s.rtt.map_or("—".to_string(), |r| {
                    format!("{:.0} ms", r.as_secs_f64() * 1000.0)
                });
                let head = format!(
                    "[Netplay {mode}] 已連線｜你是玩家 {}｜ping {ping}｜input delay {input_delay}",
                    player + 1
                );
                let stall = format!(
                    "stall {} 次（{:.1} 秒）",
                    s.stalls,
                    s.stall_time.as_secs_f64()
                );
                match &s.rollback {
                    // rollback：狀態列只放最重要的幾項（完整的疊加層留到 4d）。
                    Some(rb) => format!(
                        "{head}｜rollback {:.1} 次/秒（深度 平均 {:.1}／最大 {}）｜幀差 {:+.1}｜預測準確率 {}｜{stall}",
                        rb.rollbacks_per_sec,
                        rb.avg_depth,
                        rb.max_depth,
                        rb.frame_advantage,
                        rb.prediction_accuracy()
                            .map_or("—".to_string(), |a| format!("{:.0}%", a * 100.0)),
                    ),
                    None => format!(
                        "{head}｜{stall}｜↑{} ↓{} B/s",
                        s.send_bytes_per_sec, s.recv_bytes_per_sec,
                    ),
                }
            }
        };
        ui.separator();
        let silent = matches!(self.net.phase, NetPhase::Connected { .. })
            && self.net.stats.silent_for >= SILENCE_WARN_AFTER;
        if silent {
            ui.colored_label(
                egui::Color32::LIGHT_RED,
                format!(
                    "{text}｜⚠ {:.1} 秒沒收到對方封包",
                    self.net.stats.silent_for.as_secs_f64()
                ),
            );
        } else {
            ui.colored_label(egui::Color32::LIGHT_BLUE, text);
        }
    }

    /// 連線大廳（Phase 4d，取代 4b 的最小化選單）：本機 IP、建立房間、加入房間（含最近連線）、
    /// 握手狀態與取消、防火牆提示。
    fn lobby_window(&mut self, ctx: &egui::Context) {
        if !self.lobby_open {
            return;
        }
        let mut open = true;
        egui::Window::new("Netplay 連線大廳")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(440.0)
            .show(ctx, |ui| self.lobby_contents(ui));
        if !open {
            self.lobby_open = false;
        }
    }

    fn lobby_contents(&mut self, ui: &mut egui::Ui) {
        let idle = self.net.phase == NetPhase::Idle;
        let has_rom = self.rom_info.is_some();

        ui.horizontal(|ui| {
            ui.label("你的區網 IPv4：");
            match self.lan_ip {
                Some(ip) => {
                    ui.strong(ip.to_string());
                    if ui.small_button("複製").clicked() {
                        ui.ctx().copy_text(ip.to_string());
                    }
                }
                None => {
                    ui.colored_label(egui::Color32::YELLOW, "偵測不到（沒有網路？）");
                }
            }
            if ui.small_button("重新偵測").clicked() {
                self.lan_ip = nes_net::transport::local_lan_ipv4();
            }
        });
        ui.weak(LAN_IP_NOTE);
        ui.separator();

        if !has_rom {
            ui.colored_label(
                egui::Color32::YELLOW,
                "請先載入 ROM（File → Open ROM…）：雙方必須載入同一份 ROM 檔案。",
            );
        }
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.lobby_tab, LobbyTab::Host, "建立房間");
            ui.selectable_value(&mut self.lobby_tab, LobbyTab::Join, "加入房間");
        });
        ui.add_enabled_ui(idle && has_rom, |ui| match self.lobby_tab {
            LobbyTab::Host => self.host_form(ui),
            LobbyTab::Join => self.join_form(ui),
        });
        ui.separator();
        self.lobby_status(ui);
    }

    /// 模式、input delay、預測視窗 K（兩個分頁共用；模式只有房主能選）。
    fn mode_settings(&mut self, ui: &mut egui::Ui, host: bool) {
        if host {
            ui.horizontal(|ui| {
                ui.label("模式");
                for (mode, label, tip) in [
                    (
                        Mode::Rollback,
                        "rollback",
                        "本地輸入立即套用，對方輸入先預測；預測錯了就還原並重跑。高延遲下手感好、幀率穩定。",
                    ),
                    (
                        Mode::Lockstep,
                        "lockstep",
                        "雙方輸入到齊才推進。沒有預測與重跑，但延遲高時幀率會下降。",
                    ),
                ] {
                    if ui
                        .radio(self.net_mode == mode, label)
                        .on_hover_text(tip)
                        .clicked()
                        && self.net_mode != mode
                    {
                        self.net_mode = mode;
                        // 兩種模式的預設與上限不同。
                        self.net_input_delay = match mode {
                            Mode::Rollback => nes_net::rollback::DEFAULT_INPUT_DELAY,
                            Mode::Lockstep => nes_net::DEFAULT_INPUT_DELAY,
                        };
                    }
                }
            });
        }
        let max = match (host, self.net_mode) {
            (true, Mode::Lockstep) => nes_net::MAX_INPUT_DELAY,
            (true, Mode::Rollback) => nes_net::rollback::MAX_INPUT_DELAY,
            // 加入者不知道房主選哪個模式：用較大的上限，rollback 時 session 會截斷成 4。
            (false, _) => nes_net::MAX_INPUT_DELAY,
        };
        self.net_input_delay = self.net_input_delay.min(max);
        ui.horizontal(|ui| {
            ui.label("Input delay（幀）");
            ui.add(egui::DragValue::new(&mut self.net_input_delay).range(0..=max))
                .on_hover_text(
                    "本地按鍵套用在 N 幀之後（1 幀約 16.6 ms）。lockstep：越大越不容易 stall（房主決定，雙方共用）；\
                     rollback：越大越不容易預測失誤，但操作越延遲（各自決定，0–4）。",
                );
        });
        let k_enabled = !host || self.net_mode == Mode::Rollback;
        ui.horizontal(|ui| {
            ui.label("預測視窗 K（幀，rollback）");
            ui.add_enabled(
                k_enabled,
                egui::DragValue::new(&mut self.net_window)
                    .range(1..=nes_net::rollback::MAX_WINDOW),
            )
            .on_hover_text(
                "目前幀最多領先「雙方輸入都已確認的幀」K 幀，之後暫停等待（退化成 lockstep 的等待）。\
                 K 越大越能吸收網路延遲，但重跑越深。各自的本地設定，雙方不必一致。",
            );
        });
    }

    fn host_form(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label("監聽 port（UDP）");
            ui.add(egui::TextEdit::singleline(&mut self.net_port_text).desired_width(70.0));
        });
        if let (Some(ip), Ok(port)) = (self.lan_ip, parse_port(&self.net_port_text)) {
            ui.horizontal(|ui| {
                ui.label("請對方連到：");
                ui.strong(format!("{ip}:{port}"));
                if ui.small_button("複製").clicked() {
                    ui.ctx().copy_text(format!("{ip}:{port}"));
                }
            });
        }
        self.mode_settings(ui, true);
        ui.weak("模式由房主決定，加入者跟隨。連線成功時雙方都會重新開機。房間只容納一位對手。");
        if let Some(err) = &self.net_form_error {
            ui.colored_label(egui::Color32::LIGHT_RED, err);
        }
        if ui.button("建立房間").clicked() {
            match self.start_host() {
                Ok(()) => self.net_form_error = None,
                Err(e) => self.net_form_error = Some(e),
            }
        }
    }

    fn join_form(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label("房主的 IP:port");
            ui.add(
                egui::TextEdit::singleline(&mut self.net_addr_text)
                    .hint_text("192.168.1.10:7000")
                    .desired_width(180.0),
            );
        });
        if !self.recent.is_empty() {
            ui.label("最近連線過（點一下填入）：");
            ui.horizontal_wrapped(|ui| {
                let list: Vec<SocketAddr> = self.recent.iter().copied().collect();
                for addr in list {
                    if ui.small_button(addr.to_string()).clicked() {
                        self.net_addr_text = addr.to_string();
                    }
                }
                if ui.small_button("清除").clicked() {
                    self.recent.clear();
                }
            });
        }
        self.mode_settings(ui, false);
        ui.weak("你是玩家 2；雙方必須載入同一份 ROM，連線模式由房主決定（lockstep 時 input delay 用房主的）。");
        if let Some(err) = &self.net_form_error {
            ui.colored_label(egui::Color32::LIGHT_RED, err);
        }
        if ui.button("連線").clicked() {
            match self.start_join() {
                Ok(()) => self.net_form_error = None,
                Err(e) => self.net_form_error = Some(e),
            }
        }
    }

    /// 握手進行中的狀態（等待中、連線中、中斷中）、取消按鈕、防火牆提示，以及上一次失敗的原因。
    fn lobby_status(&mut self, ui: &mut egui::Ui) {
        let waited = self.wait_since.map_or(Duration::ZERO, |t| t.elapsed());
        let mut cancel = false;
        match self.net.phase {
            NetPhase::Idle => {
                if let Some(err) = self.lobby_error.clone() {
                    ui.colored_label(egui::Color32::LIGHT_RED, format!("上一次嘗試失敗：{err}"));
                    if ui.small_button("清除").clicked() {
                        self.lobby_error = None;
                    }
                } else {
                    ui.weak("尚未連線。");
                }
            }
            NetPhase::Waiting { port } => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(format!(
                        "等待對手連線（UDP port {port}）… 已等待 {} 秒",
                        waited.as_secs()
                    ));
                });
                if firewall_hint_due(waited) {
                    ui.colored_label(egui::Color32::YELLOW, firewall_hint(port));
                }
                cancel = ui.button("取消").clicked();
            }
            NetPhase::Connecting { addr } => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(format!(
                        "正在連線到 {addr}…（{} / {JOIN_TIMEOUT_SECS} 秒）",
                        waited.as_secs().min(JOIN_TIMEOUT_SECS)
                    ));
                });
                cancel = ui.button("取消").clicked();
            }
            NetPhase::Connected { .. } => {
                ui.label("已連線。");
            }
            NetPhase::Closing => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("中斷連線中…");
                });
            }
        }
        if cancel {
            let _ = self.cmd_tx.send(EmuCommand::NetDisconnect);
        }
    }

    /// 檢查輸入並送出 `NetHost`。
    fn start_host(&mut self) -> Result<(), String> {
        let port = parse_port(&self.net_port_text)?;
        self.lobby_error = None;
        self.joining = None;
        let _ = self.cmd_tx.send(EmuCommand::NetHost {
            port,
            mode: self.net_mode,
            input_delay: self.net_input_delay,
            window: self.net_window,
            replay_dir: netplay_replay_dir(),
        });
        Ok(())
    }

    /// 檢查輸入並送出 `NetJoin`。
    fn start_join(&mut self) -> Result<(), String> {
        let addr = parse_join_addr(&self.net_addr_text)?;
        self.lobby_error = None;
        self.joining = Some(addr);
        let _ = self.cmd_tx.send(EmuCommand::NetJoin {
            addr,
            input_delay: self.net_input_delay,
            window: self.net_window,
            replay_dir: netplay_replay_dir(),
        });
        Ok(())
    }

    /// Netplay 結束後的說明視窗：原因、本場摘要（時長、總幀數、主要統計）與自動存下的檔案路徑。
    /// desync 時特別醒目，並附上狀態檔與 replay 的路徑。
    fn net_result_window(&mut self, ctx: &egui::Context) {
        let Some(result) = &self.net_result else {
            return;
        };
        let mut close = false;
        let (title, color) = match result.kind {
            NetEndKind::Desync => ("Netplay：偵測到不同步（desync）", egui::Color32::LIGHT_RED),
            NetEndKind::Error => ("Netplay：連線中斷", egui::Color32::YELLOW),
            NetEndKind::Normal => ("Netplay：連線結束", egui::Color32::LIGHT_GREEN),
        };
        egui::Window::new(title)
            .collapsible(false)
            .resizable(false)
            .show(ctx, |ui| {
                ui.colored_label(color, &result.message);
                if let Some(info) = &result.summary {
                    ui.separator();
                    ui.strong(format!("本場摘要（你是玩家 {}）", info.player));
                    ui.monospace(info.summary.to_string());
                    for (label, path) in [("replay", &info.replay), ("統計 CSV", &info.stats_csv)]
                    {
                        if let Some(path) = path {
                            ui.label(format!("{label}："));
                            ui.add(egui::Label::new(path.display().to_string()).selectable(true));
                        }
                    }
                }
                let extra: Vec<&PathBuf> = result
                    .files
                    .iter()
                    .filter(|f| result.summary.as_ref().and_then(|s| s.replay.as_ref()) != Some(*f))
                    .collect();
                if !extra.is_empty() {
                    ui.separator();
                    ui.label("已自動存下：");
                    for path in extra {
                        ui.add(egui::Label::new(path.display().to_string()).selectable(true));
                    }
                }
                if result.kind == NetEndKind::Desync {
                    ui.weak(
                        "分析：用 nes-test replay verify <rom> <replay> 找出分歧的幀範圍；\
                         用 nes-test diff-state 比對雙方的 .state 檔。",
                    );
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

    /// 統計疊加層：半透明地覆蓋在遊戲畫面左上角（不接收輸入、不影響模擬）。
    fn draw_overlay(&self, ctx: &egui::Context, image_rect: egui::Rect) {
        egui::Area::new(egui::Id::new("net_overlay"))
            .order(egui::Order::Foreground)
            .fixed_pos(image_rect.left_top() + egui::vec2(8.0, 8.0))
            .interactable(false)
            .show(ctx, |ui| {
                egui::Frame::new()
                    .fill(egui::Color32::from_black_alpha(170))
                    .corner_radius(4.0)
                    .inner_margin(8.0)
                    .show(ui, |ui| {
                        for line in overlay_lines(&self.net) {
                            let color = if line.warn {
                                egui::Color32::LIGHT_RED
                            } else {
                                egui::Color32::WHITE
                            };
                            ui.label(
                                egui::RichText::new(line.text)
                                    .monospace()
                                    .size(11.0)
                                    .color(color),
                            );
                        }
                        if matches!(self.net.phase, NetPhase::Connected { .. }) {
                            let size = egui::vec2(220.0, 46.0);
                            draw_line_chart(
                                ui,
                                "ping",
                                "ms",
                                &self.history.ping_ms,
                                egui::Color32::LIGHT_GREEN,
                                size,
                            );
                            if self.net.stats.rollback.is_some() {
                                draw_line_chart(
                                    ui,
                                    "rollback/s",
                                    "次",
                                    &self.history.rollbacks_per_sec,
                                    egui::Color32::LIGHT_YELLOW,
                                    size,
                                );
                            }
                            ui.label(
                                egui::RichText::new("最近 10 秒｜F3 關閉")
                                    .monospace()
                                    .size(10.0)
                                    .color(egui::Color32::from_white_alpha(140)),
                            );
                        }
                    });
            });
    }

    /// desync 的醒目警告（疊在遊戲畫面上，直到使用者關閉結果視窗）：附上已存下的狀態檔與 replay 路徑。
    fn draw_desync_banner(&self, ctx: &egui::Context, image_rect: egui::Rect) {
        let Some(result) = self
            .net_result
            .as_ref()
            .filter(|r| r.kind == NetEndKind::Desync)
        else {
            return;
        };
        egui::Area::new(egui::Id::new("desync_banner"))
            .order(egui::Order::Foreground)
            .fixed_pos(image_rect.center_top() + egui::vec2(-image_rect.width() * 0.45, 8.0))
            .interactable(false)
            .show(ctx, |ui| {
                ui.set_max_width(image_rect.width() * 0.9);
                egui::Frame::new()
                    .fill(egui::Color32::from_rgba_unmultiplied(160, 0, 0, 220))
                    .corner_radius(4.0)
                    .inner_margin(8.0)
                    .show(ui, |ui| {
                        ui.label(
                            egui::RichText::new("⚠ DESYNC：雙方的模擬已不同步，連線已停止")
                                .strong()
                                .color(egui::Color32::WHITE),
                        );
                        ui.label(
                            egui::RichText::new(&result.message)
                                .size(11.0)
                                .color(egui::Color32::WHITE),
                        );
                        for path in &result.files {
                            ui.label(
                                egui::RichText::new(path.display().to_string())
                                    .monospace()
                                    .size(10.0)
                                    .color(egui::Color32::WHITE),
                            );
                        }
                    });
            });
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
                            self.rom_info.is_some() && !self.playing(),
                            egui::Button::new("Reset (soft reset)"),
                        )
                        .on_hover_text(
                            "soft reset：CPU/PPU/APU 重置，RAM 保留；錄製時會記進 replay（reset 是輸入的一部分）。Netplay 中任一方按下，雙方在同一幀重置",
                        )
                        .on_disabled_hover_text(
                            "需要載入 ROM；播放 replay 時 reset 來自 replay",
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

        if ctx.input(|i| i.key_pressed(egui::Key::F3)) {
            self.show_overlay = !self.show_overlay;
        }
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
            let image_rect = ui.centered_and_justified(|ui| ui.add(image)).inner.rect;
            if self.show_overlay {
                self.draw_overlay(&ctx, image_rect);
            }
            self.draw_desync_banner(&ctx, image_rect);
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

        self.lobby_window(&ctx);
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
