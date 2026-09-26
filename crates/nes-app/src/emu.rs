//! Emu 執行緒：擁有唯一的 `Nes` 實例，以固定頻率推進模擬。
//!
//! 計時策略：NES 的畫面更新頻率是 60.0988 Hz（不是剛好 60）。這裡用
//! sleep + 累積誤差（accumulator）補償的寫法——每次醒來就把經過的實際時間
//! 累加，只要累積夠一幀的時間預算就推進一幀，這樣長時間下來的平均幀率會
//! 準確收斂到目標值，不會因為 `sleep` 本身不精確而持續漂移。
//!
//! 音訊：每跑完一幀就把核心產生的取樣送進 `AudioShared` 的環形緩衝區（見 `audio.rs`），並依
//! 緩衝區的填充程度微調核心的輸出取樣率（動態速率控制，±0.5%）。**節拍仍由這裡的計時器掌控，
//! 不由音訊裝置驅動**；音訊裝置的時脈與這個計時器的長期漂移由動態速率控制吸收。
//!
//! # 錄製與播放 replay（Phase 4a）
//!
//! replay ＝「開機狀態 + 每幀的 [`FrameInput`]」（見 `nes-core` 的 `replay`），所以任何會讓 `Nes`
//! 離開「從開機狀態依輸入序列執行」這條軌道的操作，在錄製與播放期間都被拒絕：讀取記憶體中的存檔、
//! 單步一條指令、trace（單步指令）、載入別的 ROM。**單步一幀**仍可用（它就是一次 `run_frame`，
//! 會被記進 replay）；暫停可用。所有幀（計時器驅動或單步）都經過 [`Emu::run_one_frame`]，所以
//! 錄製一定記錄得到每一幀，reset 也只透過 `FrameInput` 傳遞。
//!
//! # Netplay（Phase 4b，lockstep）
//!
//! 模式：**Local**（`Session::Idle`／錄製／播放）與 **Netplay**（`Session::Netplay`）。Netplay 下 emu 執行緒
//! 仍以 60.0988 Hz 為節拍；**每次迴圈（約 1 ms）都 poll 一次 socket**（收封包、送重送、逾時），
//! 每個幀節拍再向 session 要「下一幀」：雙方輸入沒到齊就**不推進、也不阻塞**（UI 與音訊照常，音訊的空檔
//! 交給既有的動態速率控制），下個迴圈再試。輸入延遲、握手、重送、指紋檢查都在 `nes-net` 的
//! `LockstepSession`；這裡只負責把 session 的輸出（`FrameInput`）套用在 `Nes` 上，並把每一幀記進
//! `MatchLog`（雙方合併後的輸入 ＋ 每幀指紋），結束時存成標準的 replay。
//!
//! Netplay 期間停用讀取存檔、單步、trace、載入 ROM、暫停、reset、錄製／播放 replay（單方面做這些會讓
//! 雙方分歧；同步暫停留待之後評估）。存檔到檔案（F5、匯出）保留，方便除錯。
//! Desync 時自動存下目前的狀態檔與該場 replay。**`nes-net` 的邏輯不讀系統時間**：`now` 全由這裡的
//! `Instant` 提供。
//!
//! TODO：之後可以換成 `spin_sleep`（忙等 + 讓出時間片混合，減少 sleep 的
//! 系統排程抖動）。

use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crossbeam_channel::{Receiver, Sender};
use nes_core::replay::DEFAULT_CHECKPOINT_INTERVAL;
use nes_core::{
    Buttons, DebugSnapshot, FrameBuffer, FrameInput, Nes, PpuViews, Replay, ReplayError,
    ReplayMismatch, ReplayPlayer, ReplayRecorder,
};

use nes_net::matchlog::MatchLog;
use nes_net::{EndReason, Event as NetEvent, LockstepSession, SessionConfig, Status, UdpTransport};

use crate::audio::{AudioProducer, RateController};
use crate::commands::{
    EmuCommand, EmuEvent, NetEndKind, NetPhase, NetStatus, PlaybackSpeed, SessionStatus,
};

const TARGET_FPS: f64 = 60.0988;

/// 執行中每隔幾幀更新一次 PPU 影像（pattern table / nametable）。它們要畫 6 張圖，
/// 而且人眼不需要 60Hz 更新；暫停時單步／讀檔之後則會立即更新。
const VIEWS_EVERY_N_FRAMES: u32 = 3;

/// 單次 `TraceToFile` 允許的最大指令數，避免手誤輸入超大數字讓 emu 執行緒
/// 卡在寫檔上（同步執行，期間不處理其他指令）。
const MAX_TRACE_INSTRUCTIONS: u32 = 1_000_000;

/// 「最快」播放時，每個時間片最多連續跑多久才回頭處理指令與發布狀態。
const MAX_SPEED_SLICE: Duration = Duration::from_millis(8);

/// 結束程式時，等對方回覆它已完成幀數的時間上限（Netplay）。
const QUIT_GRACE: Duration = Duration::from_millis(500);

/// 從目前位置起執行 `count` 條指令，把每條指令「執行前」的 trace 行寫入
/// `path`。呼叫端必須確保模擬已暫停（`Nes::step_instruction` 的限制）。
fn write_trace(nes: &mut Nes, count: u32, path: &Path) -> io::Result<()> {
    let mut out = BufWriter::new(File::create(path)?);
    for _ in 0..count {
        writeln!(out, "{}", nes.trace())?;
        nes.step_instruction();
    }
    out.flush()
}

const NOT_PAUSED_MSG: &str = "單步/trace 只能在暫停狀態下使用";

/// UI 要看的除錯資料的產生開關與輸出端。
struct DebugSinks {
    /// Debugger 面板是否開著——只在開著時才產生/傳送 `DebugSnapshot`（見
    /// `EmuCommand::SetDebugEnabled` 的文件說明理由）。
    enabled: bool,
    snapshot: triple_buffer::Input<Option<DebugSnapshot>>,
    /// PPU 影像的 pattern table 調色盤選擇；`None` = 不產生。
    views_palette: Option<u8>,
    views: triple_buffer::Input<Option<PpuViews>>,
    /// 距離上次產生 PPU 影像過了幾幀（只在執行中使用）。
    frames_since_views: u32,
}

impl DebugSinks {
    fn publish_views(&mut self, nes: &Nes) {
        if let Some(palette) = self.views_palette {
            self.views.write(Some(nes.debug_ppu_views(palette)));
            self.frames_since_views = 0;
        }
    }
}

/// 把目前狀態發布給 UI：Debugger 面板開著時寫入 `DebugSnapshot`，並且不論
/// 面板開不開都送出最新幀數（狀態列用）。`immediate` 為 `true`（單步、讀檔等）
/// 時 PPU 影像立即更新，否則依 [`VIEWS_EVERY_N_FRAMES`] 降頻。
fn publish_state(nes: &Nes, sinks: &mut DebugSinks, event_tx: &Sender<EmuEvent>, immediate: bool) {
    if sinks.enabled {
        sinks.snapshot.write(Some(nes.debug_snapshot()));
    }
    sinks.frames_since_views += 1;
    if immediate || sinks.frames_since_views >= VIEWS_EVERY_N_FRAMES {
        sinks.publish_views(nes);
    }
    let _ = event_tx.send(EmuEvent::FrameAdvanced(nes.frame_count()));
}

/// 音訊在 emu 執行緒這一側的狀態。
struct AudioSide {
    /// 環形緩衝區的生產者端（emu 執行緒獨占）。
    output: AudioProducer,
    rate: RateController,
    /// 聽得到的聲道（換 ROM 之後要重新套用）。
    channel_mask: u8,
    /// 核心吐出的取樣的暫存區（重複使用，避免每幀配置）。
    scratch: Vec<f32>,
}

impl AudioSide {
    fn new(output: AudioProducer) -> Self {
        let rate = RateController::new(output.target_fill());
        Self {
            output,
            rate,
            channel_mask: nes_core::apu::ALL_CHANNELS,
            scratch: Vec::with_capacity(4096),
        }
    }

    /// 新的 `Nes`（載入 ROM）：套用輸出取樣率與聲道遮罩，丟掉舊的取樣。
    fn configure(&mut self, nes: &mut Nes) {
        if self.output.is_active() {
            nes.set_audio_sample_rate(f64::from(self.output.device_rate()));
        }
        nes.set_audio_channel_mask(self.channel_mask);
        self.output.flush();
        self.rate.reset(self.output.target_fill());
    }

    /// 一幀跑完：把取樣送去播放，並微調下一幀的輸出取樣率。
    fn after_frame(&mut self, nes: &mut Nes) {
        self.scratch.clear();
        nes.drain_audio(&mut self.scratch);
        if !self.output.is_active() {
            return;
        }
        self.output.push_samples(&self.scratch);
        let device = f64::from(self.output.device_rate());
        let rate = self
            .rate
            .update(self.output.fill(), self.output.target_fill(), device);
        nes.set_audio_sample_rate(rate);
        self.output.set_current_rate(rate);
    }

    /// 暫停中（單步、讀檔……）產生的取樣不播放，直接丟掉。
    fn discard(&mut self, nes: &mut Nes) {
        self.scratch.clear();
        nes.drain_audio(&mut self.scratch);
    }
}

/// 目前的工作階段：一般執行、錄製、播放 replay、或 Netplay。
enum Session {
    Idle,
    Recording(ReplayRecorder),
    Replaying(ReplayPlayer),
    Netplay(Box<Netplay>),
}

/// Netplay（lockstep）工作階段。
struct Netplay {
    session: LockstepSession,
    transport: UdpTransport,
    /// session 的時間原點：`now = clock.elapsed()`。`nes-net` 不讀系統時間，這裡是唯一提供時間的地方。
    clock: Instant,
    /// 連線成功（重新開機）之後才有：雙方合併後的輸入與每幀指紋。
    log: Option<MatchLog>,
    replay_dir: PathBuf,
    /// 房主：實際監聽的 port；加入者：房主的位址。
    target: NetTarget,
    last_status: Status,
}

#[derive(Debug, Clone, Copy)]
enum NetTarget {
    Host { port: u16 },
    Join { addr: std::net::SocketAddr },
}

impl Netplay {
    fn phase(&self) -> NetPhase {
        match (self.session.status(), self.target) {
            (Status::Waiting | Status::Connecting, NetTarget::Host { port }) => {
                NetPhase::Waiting { port }
            }
            (Status::Waiting | Status::Connecting, NetTarget::Join { addr }) => {
                NetPhase::Connecting { addr }
            }
            (Status::Running, _) => NetPhase::Connected {
                player: self.session.local_player(),
                input_delay: self.session.input_delay(),
            },
            (Status::Closing, _) => NetPhase::Closing,
            (Status::Ended, _) => NetPhase::Idle,
        }
    }

    fn status(&self) -> NetStatus {
        NetStatus {
            phase: self.phase(),
            stats: self.session.stats(),
        }
    }
}

/// 開始 Netplay 的角色。
enum NetRole {
    Host { port: u16, input_delay: u8 },
    Join { addr: std::net::SocketAddr },
}

/// session_id：Host 每場不同即可（用系統時間與行程 id 攪出來；`nes-net` 不產生亂數）。
fn fresh_session_id() -> u32 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    (nanos ^ (nanos >> 32)) as u32 ^ std::process::id().rotate_left(16)
}

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn write_into(dir: &Path, name: &str, bytes: &[u8]) -> io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(name);
    std::fs::write(&path, bytes)?;
    Ok(std::fs::canonicalize(&path).unwrap_or(path))
}

/// 一幀跑完之後的結果。
enum FrameOutcome {
    Ran,
    /// replay 播完了，全部檢查點相符。
    ReplayDone,
    /// 檢查點不符。
    Mismatch(ReplayMismatch),
    /// 錄製時記錄失敗（幀數對不上、超過上限）。
    RecordFailed(ReplayError),
    /// Netplay：下一幀雙方的輸入還沒到齊，這個節拍不推進（不阻塞，下個迴圈再試）。
    Stalled,
}

/// 一個幀節拍的結果。
enum Tick {
    Ran,
    /// Netplay 的輸入沒到齊。
    Stalled,
    /// 工作階段結束（播完、不符、錄製失敗）。
    Ended,
}

/// Emu 執行緒的全部狀態。
struct Emu {
    event_tx: Sender<EmuEvent>,
    frame_out: triple_buffer::Input<FrameBuffer>,
    sinks: DebugSinks,
    audio: AudioSide,

    nes: Option<Nes>,
    /// 目前 ROM 的原始檔案內容：開始錄製／播放時要重新開機（`Nes::from_rom`）。
    rom_bytes: Option<Vec<u8>>,
    session: Session,
    speed: PlaybackSpeed,
    /// 使用者要求 reset：變成下一幀 `FrameInput` 的 `reset` 旗標。
    pending_reset: bool,
    input: [Buttons; 2],
    paused: bool,
    saved_state: Option<Vec<u8>>,
    /// 本輪指令處理是否改變了模擬狀態（載入 ROM/讀檔/單步）。改變了就要立刻重發快照與幀數
    /// ——暫停中 `run_frame` 不會再被呼叫，不補發的話 Debugger 面板會停在舊狀態。
    state_changed: bool,
    fps_frames: u64,
}

impl Emu {
    fn send(&self, event: EmuEvent) {
        let _ = self.event_tx.send(event);
    }

    fn error(&self, message: impl Into<String>) {
        self.send(EmuEvent::Error(message.into()));
    }

    /// 錄製、播放或 Netplay 中（不是一般執行）。
    fn busy(&self) -> bool {
        !matches!(self.session, Session::Idle)
    }

    fn netplaying(&self) -> bool {
        matches!(self.session, Session::Netplay(_))
    }

    /// 拒絕一個會破壞「從開機狀態依輸入序列執行」（或讓 Netplay 雙方分歧）的操作，並說明原因。
    fn refuse(&self, what: &str) {
        let (during, why) = match self.session {
            Session::Recording(_) => ("錄製中", "它會破壞「從開機狀態依輸入序列執行」的前提"),
            Session::Replaying(_) => (
                "播放 replay 中",
                "它會破壞「從開機狀態依輸入序列執行」的前提",
            ),
            Session::Netplay(_) => (
                "Netplay 中",
                "單方面這麼做會讓雙方的模擬分歧（同步暫停／讀檔留待之後評估）",
            ),
            Session::Idle => return,
        };
        self.error(format!("{during}不能{what}：{why}"));
    }

    fn session_status(&self) -> SessionStatus {
        match &self.session {
            Session::Idle => SessionStatus::Idle,
            Session::Recording(rec) => SessionStatus::Recording {
                frames: rec.frames(),
            },
            Session::Replaying(p) => SessionStatus::Playing {
                frame: p.frame(),
                total: p.total_frames(),
                verified: p.verified_checkpoints(),
                checkpoints: p.checkpoint_count(),
            },
            // Netplay 的狀態走 `EmuEvent::Net`，不佔用錄製／播放的狀態列。
            Session::Netplay(_) => SessionStatus::Idle,
        }
    }

    fn send_session(&self) {
        self.send(EmuEvent::Session(self.session_status()));
    }

    /// `notify`：通知 UI（emu 執行緒自己改變暫停狀態時）。
    fn set_paused(&mut self, paused: bool, notify: bool) {
        self.paused = paused;
        if !paused {
            self.audio.rate.reset(self.audio.output.target_fill());
        }
        self.audio.output.set_paused(paused);
        if notify {
            self.send(EmuEvent::Paused(paused));
        }
    }

    // ---- Netplay -----------------------------------------------------------------

    /// 通知 UI 目前的 Netplay 階段與統計。
    fn send_net_status(&self) {
        let status = match &self.session {
            Session::Netplay(rt) => rt.status(),
            _ => NetStatus::default(),
        };
        self.send(EmuEvent::Net(status));
    }

    fn net_failed(&self, message: String) {
        self.send(EmuEvent::NetEnded {
            kind: NetEndKind::Error,
            message,
            files: Vec::new(),
        });
    }

    fn start_netplay(&mut self, role: NetRole, replay_dir: PathBuf) {
        if self.busy() {
            self.refuse("開始 Netplay");
            return;
        }
        let Some(rom_id) = self.nes.as_ref().map(Nes::rom_id) else {
            self.net_failed("請先載入 ROM（雙方必須載入同一份 ROM 檔案）".into());
            return;
        };
        let (transport, cfg, target) = match role {
            NetRole::Host { port, input_delay } => {
                match UdpTransport::listen(([0, 0, 0, 0], port).into()) {
                    Ok(t) => {
                        let actual = t.local_addr().map_or(port, |a| a.port());
                        (
                            t,
                            SessionConfig::host(rom_id, input_delay, fresh_session_id()),
                            NetTarget::Host { port: actual },
                        )
                    }
                    Err(e) => {
                        return self.net_failed(format!(
                            "無法在 port {port} 建立房間：{e}（這個 port 可能已被別的程式使用）"
                        ));
                    }
                }
            }
            NetRole::Join { addr } => match UdpTransport::connect(addr) {
                Ok(t) => (t, SessionConfig::client(rom_id), NetTarget::Join { addr }),
                Err(e) => return self.net_failed(format!("無法建立網路連線：{e}")),
            },
        };
        if self.paused {
            self.set_paused(false, true);
        }
        self.pending_reset = false;
        let rt = Netplay {
            session: LockstepSession::new(cfg),
            transport,
            clock: Instant::now(),
            log: None,
            replay_dir,
            target,
            last_status: Status::Ended,
        };
        self.session = Session::Netplay(Box::new(rt));
        self.send_net_status();
    }

    /// 每次迴圈呼叫：驅動 session（收送封包、重送、逾時），處理它的事件。
    fn net_poll(&mut self) {
        let (events, status) = match &mut self.session {
            Session::Netplay(rt) => {
                let now = rt.clock.elapsed();
                rt.session.poll(now, &mut rt.transport);
                let events: Vec<NetEvent> =
                    std::iter::from_fn(|| rt.session.poll_event()).collect();
                (events, rt.session.status())
            }
            _ => return,
        };
        for event in events {
            match event {
                NetEvent::Connected { .. } => self.net_connected(),
                NetEvent::Stats(_) => self.send_net_status(),
                NetEvent::Disconnected {
                    reason,
                    peer_frames,
                } => self.finish_netplay(reason, peer_frames),
                // Desync 之後一定接著 Disconnected（在那裡存檔）；stall 統計走 Stats。
                NetEvent::Desync { .. } | NetEvent::Stalled { .. } | NetEvent::Resumed { .. } => {}
            }
        }
        let changed = match &mut self.session {
            Session::Netplay(rt) if rt.last_status != status => {
                rt.last_status = status;
                true
            }
            _ => false,
        };
        if changed {
            self.send_net_status();
        }
    }

    /// 握手成功：**雙方都重新開機**（第 0 幀對齊），開始記錄這一場。
    fn net_connected(&mut self) {
        let Some(rom) = self.rom_bytes.clone() else {
            return;
        };
        match self.boot(&rom) {
            Ok(nes) => {
                let log = MatchLog::new(&nes);
                self.nes = Some(nes);
                self.pending_reset = false;
                self.state_changed = true;
                if let Session::Netplay(rt) = &mut self.session {
                    rt.log = Some(log);
                }
            }
            Err(e) => {
                self.error(format!("Netplay 重新開機失敗: {e}"));
                if let Session::Netplay(rt) = &mut self.session {
                    let now = rt.clock.elapsed();
                    rt.session.disconnect(now);
                }
            }
        }
    }

    /// Netplay 結束：存下這一場的 replay（雙方合併的輸入；連線正常結束時雙方取共同完成的幀數，
    /// 兩份檔案位元組完全相同），desync 時再存目前的狀態檔；回到單機模式並通知 UI 原因與檔案路徑。
    fn finish_netplay(&mut self, reason: EndReason, peer_frames: Option<u32>) {
        let Session::Netplay(rt) = std::mem::replace(&mut self.session, Session::Idle) else {
            return;
        };
        let rt = *rt;
        let player = rt.session.local_player() + 1;
        let stamp = unix_seconds();
        let rom = self
            .nes
            .as_ref()
            .map(|n| n.rom_id().short())
            .unwrap_or_default();
        let mut files = Vec::new();
        let mut notes = Vec::new();

        if let Some(log) = rt.log.as_ref().filter(|l| l.frames() > 0) {
            let own = log.frames();
            let frames = peer_frames.map_or(own, |p| p.min(own));
            let replay = log.to_replay(frames, DEFAULT_CHECKPOINT_INTERVAL);
            let name = format!("netplay-{stamp}-p{player}-{rom}.replay");
            match write_into(&rt.replay_dir, &name, &replay.encode()) {
                Ok(path) => files.push(path),
                Err(e) => notes.push(format!("replay 存檔失敗：{e}")),
            }
        }
        if let EndReason::Desync { frame } = reason
            && rt.log.is_some()
            && let Some(nes) = &self.nes
        {
            let name = format!("netplay-{stamp}-p{player}-{rom}-desync-frame{frame}.state");
            match write_into(&rt.replay_dir, &name, &nes.save_state()) {
                Ok(path) => files.push(path),
                Err(e) => notes.push(format!("狀態檔存檔失敗：{e}")),
            }
        }

        let kind = match reason {
            EndReason::LocalLeft | EndReason::PeerLeft => NetEndKind::Normal,
            EndReason::Desync { .. } => NetEndKind::Desync,
            EndReason::Rejected(_) | EndReason::HandshakeTimeout | EndReason::Timeout => {
                NetEndKind::Error
            }
        };
        let mut message = reason.to_string();
        for note in notes {
            message.push('；');
            message.push_str(&note);
        }
        self.send(EmuEvent::NetEnded {
            kind,
            message,
            files,
        });
        self.send_net_status();
        // 回到單機模式：手上的 `Nes` 保持原狀繼續跑（單機模式沒有任何限制）。
        self.state_changed = true;
    }

    /// 換一台剛開機的 `Nes`（載入 ROM、開始錄製／播放）。
    fn boot(&mut self, rom: &[u8]) -> Result<Nes, String> {
        let mut nes = Nes::from_rom(rom).map_err(|e| e.to_string())?;
        self.audio.configure(&mut nes);
        Ok(nes)
    }

    /// 處理一個指令；回傳 `true` 代表要結束執行緒。
    fn handle(&mut self, cmd: EmuCommand) -> bool {
        match cmd {
            EmuCommand::LoadRom(bytes) => {
                if self.busy() {
                    self.refuse("載入別的 ROM");
                    return false;
                }
                match self.boot(&bytes) {
                    Ok(new_nes) => {
                        let info = new_nes.rom_info().clone();
                        let id = new_nes.rom_id();
                        self.nes = Some(new_nes);
                        self.rom_bytes = Some(bytes);
                        self.pending_reset = false;
                        self.state_changed = true;
                        self.send(EmuEvent::RomLoaded(info, id));
                    }
                    Err(e) => self.error(format!("無法載入 ROM: {e}")),
                }
            }
            EmuCommand::SetInput(player, buttons) => {
                // 播放 replay 時輸入來自 replay，忽略鍵盤。Netplay 只有本地那一個搖桿
                //（玩家 1 的按鍵配置，由 session 對應到被分配的玩家位置）。
                let allowed = match self.session {
                    Session::Replaying(_) => false,
                    Session::Netplay(_) => player == 0,
                    _ => true,
                };
                if allowed && let Some(slot) = self.input.get_mut(player as usize) {
                    *slot = buttons;
                }
            }
            EmuCommand::Pause => {
                if self.netplaying() {
                    self.refuse("暫停");
                } else {
                    self.set_paused(true, false);
                }
            }
            EmuCommand::Resume => self.set_paused(false, false),
            EmuCommand::Reset => {
                if matches!(self.session, Session::Replaying(_)) {
                    self.refuse("按 reset（reset 也是 replay 輸入的一部分）");
                } else if self.netplaying() {
                    self.refuse("按 reset（reset 不是 Netplay 輸入的一部分）");
                } else if self.nes.is_some() {
                    self.pending_reset = true;
                }
            }
            EmuCommand::SetAudioChannelMask(mask) => {
                self.audio.channel_mask = mask;
                if let Some(n) = &mut self.nes {
                    n.set_audio_channel_mask(mask);
                }
            }
            EmuCommand::SetDebugEnabled(enabled) => {
                self.sinks.enabled = enabled;
                // 剛打開面板時立刻送一份目前狀態，不必等到下一幀
                // 才有資料（尤其是暫停中時，run_frame 不會再被呼叫）。
                if enabled && let Some(n) = &self.nes {
                    self.sinks.snapshot.write(Some(n.debug_snapshot()));
                }
            }
            EmuCommand::SetDebugViews(palette) => {
                self.sinks.views_palette = palette;
                // 同上：立刻產生一次，不必等 N 幀（暫停中也才看得到）。
                if let Some(n) = &self.nes {
                    self.sinks.publish_views(n);
                }
            }
            EmuCommand::SaveState => {
                if let Some(n) = &self.nes {
                    self.saved_state = Some(n.save_state());
                }
            }
            EmuCommand::ExportState => {
                if let Some(n) = &self.nes {
                    self.send(EmuEvent::StateExported(n.save_state()));
                }
            }
            EmuCommand::LoadState => {
                if self.busy() {
                    self.refuse("讀取存檔");
                } else if let (Some(n), Some(state)) = (&mut self.nes, &self.saved_state) {
                    match n.load_state(state) {
                        Ok(()) => self.state_changed = true,
                        Err(e) => self.error(e.to_string()),
                    }
                }
            }
            EmuCommand::StepInstruction => {
                if self.busy() {
                    self.refuse("單步執行指令");
                } else if !self.paused {
                    self.error(NOT_PAUSED_MSG);
                } else if let Some(n) = &mut self.nes {
                    n.step_instruction();
                    // 顯示「畫到一半」的畫面，可以看到掃描線逐步畫出來。
                    self.frame_out.write(n.frame_buffer().clone());
                    self.state_changed = true;
                }
            }
            EmuCommand::StepFrame => {
                if !self.paused {
                    self.error(NOT_PAUSED_MSG);
                } else if let Some(outcome) = self.run_one_frame() {
                    self.finish_frame(outcome, true);
                    self.state_changed = true;
                }
            }
            EmuCommand::TraceToFile { count, path } => {
                if self.busy() {
                    self.refuse("trace（單步執行指令）");
                } else if !self.paused {
                    self.error(NOT_PAUSED_MSG);
                } else if let Some(n) = &mut self.nes {
                    let count = count.min(MAX_TRACE_INSTRUCTIONS);
                    let result = write_trace(n, count, &path);
                    // 不論成功與否，模擬狀態可能已經前進了。
                    self.state_changed = true;
                    let event = match result {
                        Ok(()) => EmuEvent::TraceWritten { path, lines: count },
                        Err(e) => EmuEvent::Error(format!("寫入 trace 失敗: {e}")),
                    };
                    self.send(event);
                }
            }
            EmuCommand::StartRecording => self.start_recording(),
            EmuCommand::StopRecording => self.stop_recording(),
            EmuCommand::StartReplay(bytes) => self.start_replay(&bytes),
            EmuCommand::StopReplay => {
                if matches!(self.session, Session::Replaying(_)) {
                    self.session = Session::Idle;
                    if let Some(n) = &mut self.nes {
                        n.set_output_enabled(true);
                    }
                    self.send_session();
                }
            }
            EmuCommand::SetPlaybackSpeed(speed) => {
                self.speed = speed;
                self.audio.rate.reset(self.audio.output.target_fill());
            }
            EmuCommand::NetHost {
                port,
                input_delay,
                replay_dir,
            } => self.start_netplay(NetRole::Host { port, input_delay }, replay_dir),
            EmuCommand::NetJoin { addr, replay_dir } => {
                self.start_netplay(NetRole::Join { addr }, replay_dir);
            }
            EmuCommand::NetDisconnect => {
                if let Session::Netplay(rt) = &mut self.session {
                    let now = rt.clock.elapsed();
                    rt.session.disconnect(now);
                }
            }
            EmuCommand::Quit => {
                // 讓對方立刻知道（而不是等 5 秒逾時），並把這場存成 replay。和「中斷連線」一樣先交換
                // 雙方完成的幀數（最多等 `QUIT_GRACE`），兩份 replay 才會位元組相同。
                let peer_frames = match &mut self.session {
                    Session::Netplay(rt) => {
                        let deadline = Instant::now() + QUIT_GRACE;
                        rt.session.disconnect(rt.clock.elapsed());
                        loop {
                            rt.session.poll(rt.clock.elapsed(), &mut rt.transport);
                            if rt.session.is_ended() || Instant::now() >= deadline {
                                break;
                            }
                            thread::sleep(Duration::from_millis(1));
                        }
                        Some(rt.session.peer_frames())
                    }
                    _ => None,
                };
                if let Some(peer_frames) = peer_frames {
                    self.finish_netplay(EndReason::LocalLeft, peer_frames);
                }
                return true;
            }
            #[cfg(test)]
            EmuCommand::Barrier(_) => unreachable!("Barrier 由 run 迴圈處理"),
        }
        false
    }

    fn start_recording(&mut self) {
        if self.busy() {
            self.refuse("開始新的錄製");
            return;
        }
        let Some(rom) = self.rom_bytes.clone() else {
            self.error("尚未載入 ROM，無法錄製");
            return;
        };
        // 重新開機：replay 的起點必須是 `Nes::from_rom` 的開機狀態。
        let fresh = match self.boot(&rom) {
            Ok(n) => n,
            Err(e) => return self.error(format!("重新開機失敗: {e}")),
        };
        match ReplayRecorder::new(&fresh, DEFAULT_CHECKPOINT_INTERVAL) {
            Ok(recorder) => {
                self.nes = Some(fresh);
                self.session = Session::Recording(recorder);
                self.pending_reset = false;
                self.state_changed = true;
                self.send_session();
            }
            Err(e) => self.error(format!("無法開始錄製: {e}")),
        }
    }

    fn stop_recording(&mut self) {
        if !matches!(self.session, Session::Recording(_)) {
            return;
        }
        let Session::Recording(recorder) = std::mem::replace(&mut self.session, Session::Idle)
        else {
            return;
        };
        if let Some(n) = &self.nes {
            match recorder.finish(n) {
                Ok(replay) => self.send(EmuEvent::RecordingFinished {
                    frames: replay.total_frames,
                    bytes: replay.encode(),
                }),
                Err(e) => self.error(format!("結束錄製失敗: {e}")),
            }
        }
        self.send_session();
    }

    fn start_replay(&mut self, bytes: &[u8]) {
        if self.busy() {
            self.refuse("開始播放另一份 replay");
            return;
        }
        let Some(rom) = self.rom_bytes.clone() else {
            self.error("尚未載入 ROM：請先載入 replay 對應的 ROM");
            return;
        };
        let replay = match Replay::decode(bytes) {
            Ok(r) => r,
            Err(e) => return self.error(format!("replay 檔案無效: {e}")),
        };
        let mut fresh = match self.boot(&rom) {
            Ok(n) => n,
            Err(e) => return self.error(format!("重新開機失敗: {e}")),
        };
        // 版本或 rom_id 不符時在這裡拒絕，訊息可讀（見 `ReplayError`）。
        match ReplayPlayer::new(replay, &fresh) {
            Ok(player) => {
                fresh.set_output_enabled(true);
                self.nes = Some(fresh);
                self.session = Session::Replaying(player);
                self.pending_reset = false;
                self.state_changed = true;
                self.send_session();
            }
            Err(e) => self.error(format!("無法播放 replay：{e}")),
        }
    }

    /// 推進一幀（計時器驅動與單步共用）：一般執行、錄製與播放 replay 都經過這裡，所以錄製
    /// 一定記得到每一幀。沒有 `Nes`（尚未載入 ROM）回傳 `None`。
    fn run_one_frame(&mut self) -> Option<FrameOutcome> {
        let nes = self.nes.as_mut()?;
        if let Session::Netplay(rt) = &mut self.session {
            // 還沒連上（等對手／握手中）：不推進任何幀。
            if !rt.session.is_running() || rt.log.is_none() {
                return None;
            }
            let now = rt.clock.elapsed();
            // 本地取樣：玩家 1 的按鍵配置，由 session 對應到被分配的玩家位置（每推進一幀取樣一次）。
            if rt.session.local_input_wanted() {
                rt.session.add_local_input(self.input[0]);
            }
            return Some(match rt.session.next_ready_frame(now) {
                Some(input) => {
                    nes.run_frame(input);
                    if let Some(log) = &mut rt.log {
                        log.record(input, nes);
                    }
                    rt.session.frame_done(|| nes.behavior_fingerprint());
                    FrameOutcome::Ran
                }
                // 雙方輸入沒到齊：不推進、不阻塞。
                None => FrameOutcome::Stalled,
            });
        }
        Some(match &mut self.session {
            Session::Replaying(player) => match player.step(nes) {
                Ok(true) if !player.is_finished() => FrameOutcome::Ran,
                Ok(_) => FrameOutcome::ReplayDone,
                Err(m) => FrameOutcome::Mismatch(m),
            },
            session => {
                let input = FrameInput {
                    p1: self.input[0],
                    p2: self.input[1],
                    reset: std::mem::take(&mut self.pending_reset),
                };
                nes.run_frame(input);
                match session {
                    Session::Recording(recorder) => match recorder.record_frame(input, nes) {
                        Ok(()) => FrameOutcome::Ran,
                        Err(e) => FrameOutcome::RecordFailed(e),
                    },
                    _ => FrameOutcome::Ran,
                }
            }
        })
    }

    /// 一幀跑完之後：發布畫面、處理 replay 的結束／不符（自動暫停並通知 UI）、錄製失敗。
    /// 回傳 `false` 代表這個工作階段已結束（呼叫端應停止連續推進）。
    fn finish_frame(&mut self, outcome: FrameOutcome, write_frame: bool) -> bool {
        if write_frame && let Some(n) = &self.nes {
            self.frame_out.write(n.frame_buffer().clone());
        }
        match outcome {
            FrameOutcome::Ran => {
                if matches!(self.session, Session::Recording(_) | Session::Replaying(_)) {
                    self.send_session();
                }
                true
            }
            FrameOutcome::Stalled => true,
            FrameOutcome::ReplayDone => {
                if let Session::Replaying(player) =
                    std::mem::replace(&mut self.session, Session::Idle)
                {
                    self.send(EmuEvent::Session(SessionStatus::Finished {
                        total: player.total_frames(),
                        checkpoints: player.verified_checkpoints(),
                    }));
                }
                self.end_replay_and_pause();
                false
            }
            FrameOutcome::Mismatch(m) => {
                self.session = Session::Idle;
                self.send(EmuEvent::Session(SessionStatus::Mismatch(m)));
                self.end_replay_and_pause();
                false
            }
            FrameOutcome::RecordFailed(e) => {
                self.session = Session::Idle;
                self.error(format!("錄製中止：{e}"));
                self.send_session();
                false
            }
        }
    }

    /// replay 結束（完成或不符）：恢復輸出、暫停，讓使用者看得到最後的畫面與訊息。
    fn end_replay_and_pause(&mut self) {
        if let Some(n) = &mut self.nes {
            n.set_output_enabled(true);
        }
        self.set_paused(true, true);
    }

    /// 這一幀的取樣：播放 replay 的兩倍速與最快靜音（丟掉），其餘正常送去播放。
    fn audio_after_frame(&mut self) {
        let quiet =
            matches!(self.session, Session::Replaying(_)) && self.speed != PlaybackSpeed::X1;
        if let Some(n) = &mut self.nes {
            if quiet {
                self.audio.discard(n);
            } else {
                self.audio.after_frame(n);
            }
        }
    }

    fn publish(&mut self, immediate: bool) {
        if let Some(n) = &self.nes {
            publish_state(n, &mut self.sinks, &self.event_tx, immediate);
        }
    }

    /// 計時器驅動的一幀。
    fn tick_frame(&mut self) -> Tick {
        let Some(outcome) = self.run_one_frame() else {
            return Tick::Ran;
        };
        if matches!(outcome, FrameOutcome::Stalled) {
            // Netplay 的輸入沒到齊：沒有新畫面、沒有新取樣，也不算 FPS。
            return Tick::Stalled;
        }
        self.audio_after_frame();
        let keep = self.finish_frame(outcome, true);
        self.fps_frames += 1;
        self.publish(false);
        if keep { Tick::Ran } else { Tick::Ended }
    }

    /// 「最快」播放：在一個時間片內盡可能連續跑，只有第一幀有輸出（畫面與音訊都關閉可以省下大部分
    /// 時間；輸出不影響行為指紋，所以不影響驗證結果）。
    fn run_max_speed_slice(&mut self) {
        let started = Instant::now();
        let mut ran = 0u64;
        loop {
            let render = ran == 0;
            if let Some(n) = &mut self.nes {
                n.set_output_enabled(render);
            }
            let Some(outcome) = self.run_one_frame() else {
                break;
            };
            ran += 1;
            let keep = self.finish_frame(outcome, render);
            if !keep || started.elapsed() >= MAX_SPEED_SLICE {
                break;
            }
        }
        if let Some(n) = &mut self.nes {
            n.set_output_enabled(true);
            self.audio.discard(n);
        }
        self.fps_frames += ran;
        self.publish(false);
    }

    /// 一幀的時間預算：播放 replay 時依播放速度縮短。
    fn frame_period(&self, base: Duration) -> Duration {
        match (&self.session, self.speed) {
            (Session::Replaying(_), PlaybackSpeed::X2) => base / 2,
            _ => base,
        }
    }

    fn max_speed_playback(&self) -> bool {
        matches!(self.session, Session::Replaying(_)) && self.speed == PlaybackSpeed::Max
    }
}

pub fn run(
    cmd_rx: Receiver<EmuCommand>,
    event_tx: Sender<EmuEvent>,
    frame_input: triple_buffer::Input<FrameBuffer>,
    debug_input: triple_buffer::Input<Option<DebugSnapshot>>,
    views_input: triple_buffer::Input<Option<PpuViews>>,
    audio: AudioProducer,
) {
    let mut emu = Emu {
        event_tx,
        frame_out: frame_input,
        sinks: DebugSinks {
            enabled: false,
            snapshot: debug_input,
            views_palette: None,
            views: views_input,
            frames_since_views: 0,
        },
        audio: AudioSide::new(audio),
        nes: None,
        rom_bytes: None,
        session: Session::Idle,
        speed: PlaybackSpeed::X1,
        pending_reset: false,
        input: [Buttons::empty(); 2],
        paused: false,
        saved_state: None,
        state_changed: false,
        fps_frames: 0,
    };
    let frame_duration = Duration::from_secs_f64(1.0 / TARGET_FPS);

    let mut accumulator = Duration::ZERO;
    let mut last_tick = Instant::now();
    let mut fps_window_start = Instant::now();

    // 測試用屏障（`EmuCommand::Barrier`）：本輪結果發布之後才回覆。
    #[cfg(test)]
    let mut barriers: Vec<Sender<()>> = Vec::new();

    'outer: loop {
        emu.state_changed = false;

        for cmd in cmd_rx.try_iter() {
            #[cfg(test)]
            if let EmuCommand::Barrier(ack) = cmd {
                barriers.push(ack);
                continue;
            }
            if emu.handle(cmd) {
                break 'outer;
            }
        }

        // Netplay：每次迴圈都 poll 一次 socket（收封包、重送、逾時、事件）。
        emu.net_poll();

        if emu.state_changed {
            emu.publish(true);
        }
        // 暫停中單步、讀檔產生的音訊不播放（暫停時要完全靜音）。
        if emu.paused
            && let Some(n) = &mut emu.nes
        {
            emu.audio.discard(n);
        }
        #[cfg(test)]
        for ack in barriers.drain(..) {
            let _ = ack.send(());
        }

        let now = Instant::now();
        accumulator += now.duration_since(last_tick);
        last_tick = now;

        if emu.max_speed_playback() && !emu.paused && emu.nes.is_some() {
            accumulator = Duration::ZERO;
            emu.run_max_speed_slice();
        } else {
            let period = emu.frame_period(frame_duration);
            while accumulator >= period {
                accumulator -= period;
                if emu.paused || emu.nes.is_none() {
                    continue;
                }
                match emu.tick_frame() {
                    Tick::Ran => {}
                    Tick::Ended => {
                        // 工作階段結束（播完、不符、錄製失敗）：丟掉累積的時間，不要補幀。
                        accumulator = Duration::ZERO;
                        break;
                    }
                    Tick::Stalled => {
                        // Netplay 等對方的輸入：下個迴圈（約 1 ms 後）立刻再試，
                        // 累積的時間最多留一幀，恢復之後不狂追。
                        accumulator = period;
                        break;
                    }
                }
            }
        }

        if fps_window_start.elapsed() >= Duration::from_secs(1) {
            let fps = emu.fps_frames as f64 / fps_window_start.elapsed().as_secs_f64();
            emu.send(EmuEvent::FpsReport { fps });
            emu.fps_frames = 0;
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

    /// 測試裡等待事件／屏障的「卡死保護」上限：只用來在 emu 執行緒真的卡死時讓測試失敗而不是
    /// 永遠掛著，**不是**時序假設——正常情況下每個等待都是事件驅動、立刻返回。
    const HANG_GUARD: Duration = Duration::from_secs(120);

    /// 啟動 emu 執行緒的測試工具：通道、triple buffer 的讀端、執行緒 handle。
    struct Spawned {
        cmd_tx: Sender<EmuCommand>,
        event_rx: Receiver<EmuEvent>,
        debug_output: triple_buffer::Output<Option<DebugSnapshot>>,
        views_output: triple_buffer::Output<Option<PpuViews>>,
        handle: thread::JoinHandle<()>,
    }

    impl Spawned {
        fn start(audio: AudioProducer) -> Self {
            let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded::<EmuCommand>();
            let (event_tx, event_rx) = crossbeam_channel::unbounded::<EmuEvent>();
            let (frame_input, _frame_output) = triple_buffer::triple_buffer(&FrameBuffer::blank());
            let (debug_input, debug_output) =
                triple_buffer::triple_buffer::<Option<DebugSnapshot>>(&None);
            let (views_input, views_output) =
                triple_buffer::triple_buffer::<Option<PpuViews>>(&None);
            let handle = thread::spawn(move || {
                run(
                    cmd_rx,
                    event_tx,
                    frame_input,
                    debug_input,
                    views_input,
                    audio,
                )
            });
            Self {
                cmd_tx,
                event_rx,
                debug_output,
                views_output,
                handle,
            }
        }

        /// 屏障：等 emu 執行緒處理完先前送出的所有指令（含發布的快照、事件、音訊）。
        fn barrier(&self) {
            let (ack_tx, ack_rx) = crossbeam_channel::bounded::<()>(1);
            self.cmd_tx.send(EmuCommand::Barrier(ack_tx)).unwrap();
            ack_rx
                .recv_timeout(HANG_GUARD)
                .expect("emu 執行緒沒有回應屏障");
        }

        /// 阻塞等下一個 `RomLoaded`（途中其他事件略過）。
        fn wait_rom_loaded(&self) {
            loop {
                match self.event_rx.recv_timeout(HANG_GUARD) {
                    Ok(EmuEvent::RomLoaded(..)) => return,
                    Ok(_) => {}
                    Err(e) => panic!("等不到 RomLoaded：{e}"),
                }
            }
        }

        /// 阻塞等到至少有 `n` 幀完成（`FrameAdvanced(f)`，`f >= n`）。emu 執行緒的幀是真實時間
        /// 驅動的，但這裡只等「進度」，不假設多快。
        fn wait_frames(&self, n: u64) {
            loop {
                match self.event_rx.recv_timeout(HANG_GUARD) {
                    Ok(EmuEvent::FrameAdvanced(f)) if f >= n => return,
                    Ok(_) => {}
                    Err(e) => panic!("等不到第 {n} 幀：{e}"),
                }
            }
        }

        /// 阻塞等「現在起」再完成 `k` 幀：先丟掉已經在佇列裡的舊事件，記下最新的幀數，再等
        /// 到幀數增加 `k`。用在「先改了設定（屏障確認生效），再看之後的幀」的情境。
        fn wait_new_frames(&self, k: u64) {
            let mut latest = 0;
            for event in self.event_rx.try_iter() {
                if let EmuEvent::FrameAdvanced(f) = event {
                    latest = latest.max(f);
                }
            }
            self.wait_frames(latest + k);
        }

        fn quit(self) {
            self.cmd_tx.send(EmuCommand::Quit).unwrap();
            self.handle.join().unwrap();
        }
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
        let mut emu = Spawned::start(crate::audio::disabled_audio());
        emu.cmd_tx.send(EmuCommand::LoadRom(test_rom())).unwrap();
        emu.wait_rom_loaded();
        emu.cmd_tx.send(EmuCommand::SetDebugEnabled(true)).unwrap();
        // 屏障：確認面板已啟用（負載下，幀可能在這個指令被處理之前就已經跑了好幾幀）。
        emu.barrier();

        // 之後再完成 2 幀：`publish_state` 先寫快照、再送 `FrameAdvanced`，所以收到事件時快照
        // 已經是那一幀的內容。等的是進度（事件），不是牆鐘時間。
        emu.wait_new_frames(2);
        let snap = emu
            .debug_output
            .read()
            .clone()
            .expect("執行中且面板開著，必須有快照");

        assert_ne!(snap, DebugSnapshot::default());
        assert!(snap.cpu_cycles > 0);
        assert!(snap.frame_count >= 2, "快照的幀數 {}", snap.frame_count);
        // 註：`test_rom` 全是 `$AA`，PC 會一路跑過 `$FFFF` 進入 RAM 的 `BRK`，所以 SP 不是固定值
        //（舊版測試只讀到第 0 幀的快照，才碰巧是 `$FD`）。這裡只檢查「真的在前進」。
        assert!(snap.ppu_frame >= 1, "PPU 已經跑過至少一幀");
        emu.quit();
    }

    /// 啟動 emu 執行緒、載入測試 ROM、暫停並打開 debug 快照。
    struct Harness {
        cmd_tx: Sender<EmuCommand>,
        event_rx: Receiver<EmuEvent>,
        debug_output: triple_buffer::Output<Option<DebugSnapshot>>,
        views_output: triple_buffer::Output<Option<PpuViews>>,
        handle: thread::JoinHandle<()>,
    }

    impl Harness {
        fn start_paused() -> Self {
            let emu = Spawned::start(crate::audio::disabled_audio());
            emu.cmd_tx.send(EmuCommand::LoadRom(test_rom())).unwrap();
            emu.cmd_tx.send(EmuCommand::Pause).unwrap();
            emu.cmd_tx.send(EmuCommand::SetDebugEnabled(true)).unwrap();
            let mut h = Self {
                cmd_tx: emu.cmd_tx,
                event_rx: emu.event_rx,
                debug_output: emu.debug_output,
                views_output: emu.views_output,
                handle: emu.handle,
            };
            h.settle();
            h
        }

        /// 等 emu 執行緒處理完已送出的所有指令（屏障；暫停中沒有新幀，狀態會靜止）。
        /// 不依賴時間：命令依序處理，屏障的回覆在本輪結果發布之後才送出。
        fn settle(&mut self) {
            let (ack_tx, ack_rx) = crossbeam_channel::bounded::<()>(1);
            self.cmd_tx.send(EmuCommand::Barrier(ack_tx)).unwrap();
            ack_rx
                .recv_timeout(HANG_GUARD)
                .expect("emu 執行緒沒有回應屏障");
        }

        fn snapshot(&mut self) -> DebugSnapshot {
            self.debug_output.read().clone().expect("尚未收到快照")
        }

        fn events(&self) -> Vec<EmuEvent> {
            self.event_rx.try_iter().collect()
        }

        fn quit(self) {
            self.cmd_tx.send(EmuCommand::Quit).unwrap();
            self.handle.join().unwrap();
        }
    }

    /// 暫停中單步一條指令後，快照必須「立刻」更新（不需要任何一幀被執行）。
    #[test]
    fn step_instruction_while_paused_updates_snapshot() {
        let mut h = Harness::start_paused();
        let before = h.snapshot();

        h.cmd_tx.send(EmuCommand::StepInstruction).unwrap();
        h.settle();
        let after = h.snapshot();

        assert!(after.cpu_cycles > before.cpu_cycles);
        assert_ne!(after.cpu_pc, before.cpu_pc);
        assert_eq!(after.frame_count, before.frame_count, "單步指令不算一幀");
        h.quit();
    }

    /// 暫停中單步一幀：幀數 +1，且狀態列吃的 `FrameAdvanced` 事件與 Debugger
    /// 快照的 `frame_count` 一致（第 5 項：兩者不得再有取樣落差）。
    #[test]
    fn step_frame_keeps_frame_event_and_snapshot_consistent() {
        let mut h = Harness::start_paused();
        let before = h.snapshot();
        h.events();

        h.cmd_tx.send(EmuCommand::StepFrame).unwrap();
        h.cmd_tx.send(EmuCommand::StepFrame).unwrap();
        h.settle();
        let after = h.snapshot();

        assert_eq!(after.frame_count, before.frame_count + 2);
        let last_event_frame = h
            .events()
            .into_iter()
            .filter_map(|e| match e {
                EmuEvent::FrameAdvanced(f) => Some(f),
                _ => None,
            })
            .next_back();
        assert_eq!(last_event_frame, Some(after.frame_count));
        h.quit();
    }

    #[test]
    fn step_commands_are_rejected_while_running() {
        let emu = Spawned::start(crate::audio::disabled_audio());
        emu.cmd_tx.send(EmuCommand::LoadRom(test_rom())).unwrap();
        emu.cmd_tx.send(EmuCommand::StepInstruction).unwrap();
        emu.cmd_tx.send(EmuCommand::StepFrame).unwrap();
        emu.barrier();
        let errors = emu
            .event_rx
            .try_iter()
            .filter(|e| matches!(e, EmuEvent::Error(_)))
            .count();
        emu.quit();
        assert_eq!(errors, 2);
    }

    #[test]
    fn trace_to_file_writes_n_lines_and_advances_state() {
        let mut h = Harness::start_paused();
        let before = h.snapshot();
        let path = std::env::temp_dir().join(format!("nes_trace_test_{}.log", std::process::id()));

        h.cmd_tx
            .send(EmuCommand::TraceToFile {
                count: 25,
                path: path.clone(),
            })
            .unwrap();
        h.settle();
        let after = h.snapshot();

        let text = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(text.lines().count(), 25);
        assert!(
            text.lines()
                .next()
                .unwrap()
                .starts_with(&format!("{:04X}", before.cpu_pc))
        );
        assert!(after.cpu_cycles > before.cpu_cycles);
        assert!(
            h.events()
                .iter()
                .any(|e| matches!(e, EmuEvent::TraceWritten { lines: 25, .. }))
        );
        h.quit();
    }

    /// PPU 影像預設不產生（面板/分頁沒開時零成本）；`SetDebugViews(Some)` 之後
    /// 立即產生一份，`None` 之後不再更新。
    #[test]
    fn ppu_views_are_only_produced_while_requested() {
        let mut h = Harness::start_paused();
        assert!(h.views_output.read().is_none(), "沒要求就不該產生");

        h.cmd_tx.send(EmuCommand::SetDebugViews(Some(2))).unwrap();
        h.settle();
        let views = h.views_output.read().clone().expect("要求後應立即產生");
        assert_eq!(views.pattern_tables[0].width, 128);
        assert_eq!(views.nametables[3].height, 240);

        // 關掉之後，單步不會再更新影像。
        h.cmd_tx.send(EmuCommand::SetDebugViews(None)).unwrap();
        h.settle();
        h.views_output.read(); // 清掉「有新資料」狀態
        h.cmd_tx.send(EmuCommand::StepFrame).unwrap();
        h.settle();
        assert!(!h.views_output.update(), "關閉後不該有新影像");
        h.quit();
    }

    /// 暫停中單步一幀時，PPU 影像跟著立即更新（不必等降頻的 N 幀）。
    #[test]
    fn ppu_views_update_immediately_when_stepping_while_paused() {
        let mut h = Harness::start_paused();
        h.cmd_tx.send(EmuCommand::SetDebugViews(Some(0))).unwrap();
        h.settle();
        h.views_output.read();

        h.cmd_tx.send(EmuCommand::StepInstruction).unwrap();
        h.settle();
        assert!(h.views_output.update(), "單步之後應立即有新影像");
        h.quit();
    }

    /// 執行中：每幀的音訊取樣被送進環形緩衝區；沒有消費者時緩衝區會被填到上限、多出來的
    /// 丟掉並計數；緩衝區偏滿時動態速率控制把輸出取樣率調低（但不超過 −0.5%）。
    #[test]
    fn emu_thread_feeds_the_audio_ring_and_applies_rate_control() {
        let (audio, producer, _consumer) = crate::audio::audio_channel(48_000);
        let emu = Spawned::start(producer);
        emu.cmd_tx.send(EmuCommand::LoadRom(test_rom())).unwrap();
        emu.wait_rom_loaded();

        // 每幀約 800 個取樣、上限 7200：跑滿 30 幀一定會開始丟取樣。等的是「幀數」（進度），
        // 不是牆鐘時間；每幀的音訊在 `FrameAdvanced` 之前就已經送進緩衝區。
        emu.wait_frames(30);

        let cap = audio.target_fill() * 3;
        assert!(audio.fill() > audio.target_fill(), "取樣有送進緩衝區");
        assert!(audio.fill() <= cap, "不超過延遲上限");
        assert!(audio.dropped() > 0, "沒人消費 → 超過上限的取樣被丟掉");
        let rate = audio.current_rate();
        assert!(
            (48_000.0 * 0.995 - 1e-6..48_000.0).contains(&rate),
            "速率 {rate}"
        );
        emu.quit();
    }

    /// 暫停中單步一幀不會有聲音：取樣被丟掉，不進緩衝區；繼續之後才又有取樣。
    #[test]
    fn paused_stepping_does_not_feed_the_audio_ring() {
        let (audio, producer, _consumer) = crate::audio::audio_channel(48_000);
        let emu = Spawned::start(producer);
        emu.cmd_tx.send(EmuCommand::LoadRom(test_rom())).unwrap();
        emu.cmd_tx.send(EmuCommand::Pause).unwrap();
        emu.barrier();
        let before = audio.fill();

        for _ in 0..5 {
            emu.cmd_tx.send(EmuCommand::StepFrame).unwrap();
        }
        emu.barrier();
        assert_eq!(audio.fill(), before, "暫停時單步的音訊不進緩衝區");

        emu.cmd_tx.send(EmuCommand::Resume).unwrap();
        emu.wait_frames(8); // 單步 5 幀之後，繼續跑到第 8 幀
        assert!(audio.fill() > before, "繼續之後又有取樣");
        emu.quit();
    }

    // ---- Phase 4a：錄製、播放、reset、被停用的功能 ---------------------------------------

    use nes_core::test_support::{input_probe_rom, rendering_rom};
    use nes_core::{Replay, ReplayRecorder};

    /// 載入「對輸入與 reset 敏感」的合成 ROM 並暫停（之後全部用 `StepFrame` 驅動，不依賴時間）。
    fn start_probe_paused() -> Spawned {
        let emu = Spawned::start(crate::audio::disabled_audio());
        emu.cmd_tx
            .send(EmuCommand::LoadRom(input_probe_rom()))
            .unwrap();
        emu.cmd_tx.send(EmuCommand::Pause).unwrap();
        emu.barrier();
        emu.event_rx.try_iter().for_each(drop);
        emu
    }

    fn errors(events: &[EmuEvent]) -> Vec<String> {
        events
            .iter()
            .filter_map(|e| match e {
                EmuEvent::Error(m) => Some(m.clone()),
                _ => None,
            })
            .collect()
    }

    fn sessions(events: &[EmuEvent]) -> Vec<SessionStatus> {
        events
            .iter()
            .filter_map(|e| match e {
                EmuEvent::Session(s) => Some(*s),
                _ => None,
            })
            .collect()
    }

    fn step(emu: &Spawned, n: usize) {
        for _ in 0..n {
            emu.cmd_tx.send(EmuCommand::StepFrame).unwrap();
        }
    }

    /// 錄一段：P1 按 A 10 幀、reset、再 10 幀、P2 按 上+B 20 幀。回傳 replay 位元組。
    fn record_script(emu: &Spawned) -> Vec<u8> {
        let tx = &emu.cmd_tx;
        tx.send(EmuCommand::StartRecording).unwrap();
        tx.send(EmuCommand::SetInput(0, Buttons::A)).unwrap();
        step(emu, 10);
        tx.send(EmuCommand::Reset).unwrap();
        step(emu, 10);
        tx.send(EmuCommand::SetInput(1, Buttons::UP | Buttons::B))
            .unwrap();
        step(emu, 20);
        tx.send(EmuCommand::StopRecording).unwrap();
        emu.barrier();
        emu.event_rx
            .try_iter()
            .find_map(|e| match e {
                EmuEvent::RecordingFinished { bytes, frames } => {
                    assert_eq!(frames, 40);
                    Some(bytes)
                }
                _ => None,
            })
            .expect("StopRecording 之後要收到 RecordingFinished")
    }

    /// 錄製（含 reset）→ 停止 → 播放：全部檢查點相符、播完自動暫停、播放期間鍵盤輸入被忽略、
    /// reset 真的作用在模擬上。
    #[test]
    fn recording_and_replaying_round_trip_through_the_emu_thread() {
        let emu = start_probe_paused();
        emu.cmd_tx.send(EmuCommand::StartRecording).unwrap();
        emu.barrier();
        let started = emu.event_rx.try_iter().collect::<Vec<_>>();
        assert_eq!(
            sessions(&started).last(),
            Some(&SessionStatus::Recording { frames: 0 })
        );
        emu.cmd_tx.send(EmuCommand::StopRecording).unwrap(); // 重來，用完整腳本
        emu.barrier();
        emu.event_rx.try_iter().for_each(drop);

        let bytes = record_script(&emu);
        let replay = Replay::decode(&bytes).unwrap();
        assert_eq!(replay.total_frames, 40);
        assert_eq!(replay.rom_id, nes_core::RomId::of_file(&input_probe_rom()));
        let inputs: Vec<_> = replay.inputs().collect();
        assert_eq!(inputs[0].p1, Buttons::A);
        assert!(
            !inputs[9].reset && inputs[10].reset && !inputs[11].reset,
            "reset 在 Reset 指令之後的第一幀"
        );
        assert_eq!(inputs[39].p2, Buttons::UP | Buttons::B);
        assert_eq!(inputs.iter().filter(|i| i.reset).count(), 1);

        // 播放（暫停中用 StepFrame 逐幀）。鍵盤輸入必須被忽略：否則檢查點會不符。
        emu.cmd_tx.send(EmuCommand::StartReplay(bytes)).unwrap();
        emu.cmd_tx
            .send(EmuCommand::SetInput(0, Buttons::all()))
            .unwrap();
        emu.barrier();
        let events = emu.event_rx.try_iter().collect::<Vec<_>>();
        assert!(errors(&events).is_empty(), "{:?}", errors(&events));
        assert_eq!(
            sessions(&events).last(),
            Some(&SessionStatus::Playing {
                frame: 0,
                total: 40,
                verified: 1,
                checkpoints: 2
            })
        );
        step(&emu, 40);
        emu.cmd_tx.send(EmuCommand::ExportState).unwrap();
        emu.barrier();
        let events = emu.event_rx.try_iter().collect::<Vec<_>>();
        assert!(errors(&events).is_empty(), "{:?}", errors(&events));
        assert_eq!(
            sessions(&events).last(),
            Some(&SessionStatus::Finished {
                total: 40,
                checkpoints: 2
            })
        );
        assert!(events.iter().any(|e| matches!(e, EmuEvent::Paused(true))));

        // reset 真的作用了：程式的開機次數（$10）是 2；幀計數 $11 沒有被清掉（RAM 保留）。
        let state = events
            .iter()
            .find_map(|e| match e {
                EmuEvent::StateExported(b) => Some(b.clone()),
                _ => None,
            })
            .unwrap();
        let end: Nes = nes_core::state::decode(&state).unwrap();
        assert_eq!(end.peek(0x0010), 2);
        assert!(end.peek(0x0011) >= 38);
        assert_eq!(end.frame_count(), 40);
        emu.quit();
    }

    /// 播放中竄改某一幀的輸入 → 檢查點不符：自動暫停，回報的範圍包含被竄改的那一幀。
    #[test]
    fn replay_mismatch_pauses_and_reports_the_suspect_frames() {
        let emu = start_probe_paused();
        let bytes = record_script(&emu);
        let mut replay = Replay::decode(&bytes).unwrap();
        let mut inputs: Vec<_> = replay.inputs().collect();
        inputs[24].p1 ^= Buttons::SELECT; // 第 25 幀
        replay.runs = Replay::compress(inputs);

        emu.cmd_tx
            .send(EmuCommand::StartReplay(replay.encode()))
            .unwrap();
        step(&emu, 40);
        emu.barrier();
        let events = emu.event_rx.try_iter().collect::<Vec<_>>();
        let mismatch = sessions(&events)
            .into_iter()
            .find_map(|s| match s {
                SessionStatus::Mismatch(m) => Some(m),
                _ => None,
            })
            .expect("竄改過的 replay 必須回報檢查點不符");
        assert!(mismatch.suspect_frames().contains(&25), "{mismatch}");
        assert!(events.iter().any(|e| matches!(e, EmuEvent::Paused(true))));
        emu.quit();
    }

    /// 錄製中：讀取存檔（F9）、單步指令、trace、載入別的 ROM、重複開始錄製、播放 replay 全部被拒絕，
    /// 訊息說明原因；被拒絕的操作不改變模擬（錄下來的幀數只等於單步的幀數）；單步一幀與存檔仍可用。
    #[test]
    fn restricted_operations_are_refused_while_recording() {
        let emu = start_probe_paused();
        let tx = &emu.cmd_tx;
        tx.send(EmuCommand::StartRecording).unwrap();
        tx.send(EmuCommand::SaveState).unwrap(); // 存檔本身沒問題
        step(&emu, 3);
        tx.send(EmuCommand::LoadState).unwrap(); // F9
        tx.send(EmuCommand::StepInstruction).unwrap();
        let trace_path =
            std::env::temp_dir().join(format!("nes_rec_block_{}.log", std::process::id()));
        tx.send(EmuCommand::TraceToFile {
            count: 5,
            path: trace_path.clone(),
        })
        .unwrap();
        tx.send(EmuCommand::LoadRom(rendering_rom())).unwrap();
        tx.send(EmuCommand::StartRecording).unwrap();
        tx.send(EmuCommand::StartReplay(vec![1, 2, 3])).unwrap();
        step(&emu, 2);
        tx.send(EmuCommand::StopRecording).unwrap();
        emu.barrier();

        let events = emu.event_rx.try_iter().collect::<Vec<_>>();
        let errs = errors(&events);
        assert_eq!(errs.len(), 6, "{errs:#?}");
        assert!(errs.iter().all(|m| m.contains("錄製中")), "{errs:#?}");
        assert!(
            errs[0].contains("讀取存檔") && errs[0].contains("開機狀態"),
            "{}",
            errs[0]
        );
        assert!(!trace_path.exists(), "被拒絕的 trace 不得寫檔");
        let bytes = events
            .iter()
            .find_map(|e| match e {
                EmuEvent::RecordingFinished { bytes, frames } => {
                    assert_eq!(*frames, 5, "3 + 2 個單步的幀，被拒絕的操作沒有多跑任何東西");
                    Some(bytes.clone())
                }
                _ => None,
            })
            .expect("錄製結束");
        // 這份錄製仍然可以完整重播（沒有被破壞）。
        let replay = Replay::decode(&bytes).unwrap();
        assert!(nes_core::replay::verify(&input_probe_rom(), &replay).is_ok());

        // 錄製結束後這些操作恢復可用。
        tx.send(EmuCommand::LoadState).unwrap();
        tx.send(EmuCommand::StepInstruction).unwrap();
        emu.barrier();
        assert!(errors(&emu.event_rx.try_iter().collect::<Vec<_>>()).is_empty());
        emu.quit();
    }

    /// 播放中：同樣的操作被拒絕（訊息說「播放 replay 中」）。
    #[test]
    fn restricted_operations_are_refused_while_replaying() {
        let emu = start_probe_paused();
        let bytes = record_script(&emu);
        let tx = &emu.cmd_tx;
        tx.send(EmuCommand::StartReplay(bytes)).unwrap();
        tx.send(EmuCommand::LoadState).unwrap();
        tx.send(EmuCommand::StepInstruction).unwrap();
        tx.send(EmuCommand::Reset).unwrap();
        tx.send(EmuCommand::LoadRom(rendering_rom())).unwrap();
        tx.send(EmuCommand::StartRecording).unwrap();
        emu.barrier();
        let errs = errors(&emu.event_rx.try_iter().collect::<Vec<_>>());
        assert_eq!(errs.len(), 5, "{errs:#?}");
        assert!(
            errs.iter().all(|m| m.contains("播放 replay 中")),
            "{errs:#?}"
        );
        emu.quit();
    }

    /// 錄製時的 ROM 與載入的 ROM 不同 → 拒絕播放，訊息可讀（rom_id 前 16 字元），並維持一般狀態。
    #[test]
    fn replay_of_another_rom_is_refused_with_a_readable_message() {
        let emu = start_probe_paused();
        let bytes = record_script(&emu);
        emu.cmd_tx
            .send(EmuCommand::LoadRom(rendering_rom()))
            .unwrap();
        emu.cmd_tx.send(EmuCommand::StartReplay(bytes)).unwrap();
        emu.barrier();
        let events = emu.event_rx.try_iter().collect::<Vec<_>>();
        let errs = errors(&events);
        assert_eq!(errs.len(), 1, "{errs:#?}");
        assert!(errs[0].contains("另一份 ROM"), "{}", errs[0]);
        assert!(errs[0].contains(&nes_core::RomId::of_file(&input_probe_rom()).short()));
        assert!(errs[0].contains(&nes_core::RomId::of_file(&rendering_rom()).short()));
        assert!(
            sessions(&events)
                .iter()
                .all(|s| matches!(s, SessionStatus::Idle)),
            "沒有進入播放"
        );

        // 壞檔案也是可讀的錯誤，不 panic。
        emu.cmd_tx
            .send(EmuCommand::StartReplay(b"not a replay".to_vec()))
            .unwrap();
        emu.barrier();
        let errs = errors(&emu.event_rx.try_iter().collect::<Vec<_>>());
        assert!(errs[0].contains("replay 檔案無效"), "{}", errs[0]);
        emu.quit();
    }

    /// 一般執行時的 Reset 也是透過 `FrameInput`：暫停中先排隊，下一幀才生效（程式的開機次數 +1）。
    #[test]
    fn reset_is_applied_through_the_next_frame_input() {
        let emu = start_probe_paused();
        let boot_count = |emu: &Spawned| {
            emu.cmd_tx.send(EmuCommand::ExportState).unwrap();
            emu.barrier();
            let state = emu
                .event_rx
                .try_iter()
                .find_map(|e| match e {
                    EmuEvent::StateExported(b) => Some(b),
                    _ => None,
                })
                .unwrap();
            let nes: Nes = nes_core::state::decode(&state).unwrap();
            (nes.peek(0x0010), nes.frame_count())
        };
        step(&emu, 15);
        assert_eq!(boot_count(&emu), (1, 15));
        emu.cmd_tx.send(EmuCommand::Reset).unwrap();
        assert_eq!(boot_count(&emu), (1, 15), "reset 排隊中，還沒有作用");
        step(&emu, 1);
        assert_eq!(boot_count(&emu), (2, 16), "下一幀開始前 soft reset");
        step(&emu, 4);
        assert_eq!(boot_count(&emu), (2, 20), "只 reset 一次");
        emu.quit();
    }

    /// 沒有暫停：「最快」播放在時間片內連續推進，很快播完並自動暫停，且全部檢查點相符
    /// （關閉輸出不影響驗證）。等的是事件，不是牆鐘時間。
    #[test]
    fn max_speed_playback_verifies_the_whole_replay_and_pauses_at_the_end() {
        // 直接用 nes-core 錄一份 900 幀的腳本（含 reset）。
        let rom = input_probe_rom();
        let mut nes = Nes::from_rom(&rom).unwrap();
        let mut recorder = ReplayRecorder::new(&nes, 60).unwrap();
        for i in 0..900u32 {
            let mut input = nes_core::FrameInput::new(
                Buttons::from_bits_truncate((i / 5) as u8),
                Buttons::from_bits_truncate((i / 3) as u8),
            );
            input.reset = i == 400;
            nes.run_frame(input);
            recorder.record_frame(input, &nes).unwrap();
        }
        let bytes = recorder.finish(&nes).unwrap().encode();

        let emu = Spawned::start(crate::audio::disabled_audio());
        emu.cmd_tx.send(EmuCommand::LoadRom(rom)).unwrap();
        emu.cmd_tx
            .send(EmuCommand::SetPlaybackSpeed(PlaybackSpeed::Max))
            .unwrap();
        emu.cmd_tx.send(EmuCommand::StartReplay(bytes)).unwrap();
        let finished = loop {
            match emu.event_rx.recv_timeout(HANG_GUARD) {
                Ok(EmuEvent::Session(SessionStatus::Finished { total, checkpoints })) => {
                    break (total, checkpoints);
                }
                Ok(EmuEvent::Session(SessionStatus::Mismatch(m))) => panic!("檢查點不符：{m}"),
                Ok(EmuEvent::Error(e)) => panic!("{e}"),
                Ok(_) => {}
                Err(e) => panic!("等不到播放完成：{e}"),
            }
        };
        assert_eq!(
            finished,
            (900, 16),
            "900 幀：第 0、每 60 幀、最後一幀（900 是 60 的倍數）"
        );
        emu.barrier();
        emu.quit();
    }

    /// 兩倍速：仍然逐幀驗證（只是時間預算減半）。
    #[test]
    fn double_speed_playback_also_verifies_every_checkpoint() {
        let rom = input_probe_rom();
        let mut nes = Nes::from_rom(&rom).unwrap();
        let mut recorder = ReplayRecorder::new(&nes, 20).unwrap();
        for i in 0..60u32 {
            let input = nes_core::FrameInput::new(Buttons::from_bits_truncate(i as u8), Buttons::A);
            nes.run_frame(input);
            recorder.record_frame(input, &nes).unwrap();
        }
        let bytes = recorder.finish(&nes).unwrap().encode();

        let emu = Spawned::start(crate::audio::disabled_audio());
        emu.cmd_tx.send(EmuCommand::LoadRom(rom)).unwrap();
        emu.cmd_tx
            .send(EmuCommand::SetPlaybackSpeed(PlaybackSpeed::X2))
            .unwrap();
        emu.cmd_tx.send(EmuCommand::StartReplay(bytes)).unwrap();
        loop {
            match emu.event_rx.recv_timeout(HANG_GUARD) {
                Ok(EmuEvent::Session(SessionStatus::Finished { total, checkpoints })) => {
                    assert_eq!((total, checkpoints), (60, 4));
                    break;
                }
                Ok(EmuEvent::Session(SessionStatus::Mismatch(m))) => panic!("檢查點不符：{m}"),
                Ok(_) => {}
                Err(e) => panic!("等不到播放完成：{e}"),
            }
        }
        emu.quit();
    }

    /// 停止播放回到一般執行（保留目前的 Nes）；停止之後鍵盤輸入重新生效。
    #[test]
    fn stopping_a_replay_returns_to_normal_execution() {
        let emu = start_probe_paused();
        let bytes = record_script(&emu);
        emu.cmd_tx.send(EmuCommand::StartReplay(bytes)).unwrap();
        step(&emu, 5);
        emu.cmd_tx.send(EmuCommand::StopReplay).unwrap();
        emu.barrier();
        let events = emu.event_rx.try_iter().collect::<Vec<_>>();
        assert_eq!(sessions(&events).last(), Some(&SessionStatus::Idle));
        // 之後可以正常使用被停用的功能。
        emu.cmd_tx.send(EmuCommand::StepInstruction).unwrap();
        emu.barrier();
        assert!(errors(&emu.event_rx.try_iter().collect::<Vec<_>>()).is_empty());
        emu.quit();
    }

    // ---- Phase 4b：Netplay（兩個 emu 執行緒經由 127.0.0.1 的真實 UDP）--------------------------
    //
    // 這些測試用真實時間（emu 執行緒本來就以 60 Hz 的實時計時器推進），但全部是事件驅動的等待
    // （等 `Net`／`NetEnded`／`FrameAdvanced` 事件），`HANG_GUARD` 只是卡死保護，不是時序假設。

    use crate::commands::{NetEndKind, NetPhase};
    use nes_core::replay::verify;

    /// 阻塞等到 `pick` 從事件中挑出一個值（途中其他事件略過）。
    fn wait_for<T>(emu: &Spawned, what: &str, mut pick: impl FnMut(&EmuEvent) -> Option<T>) -> T {
        loop {
            match emu.event_rx.recv_timeout(HANG_GUARD) {
                Ok(event) => {
                    if let Some(v) = pick(&event) {
                        return v;
                    }
                }
                Err(e) => panic!("等不到{what}：{e}"),
            }
        }
    }

    fn wait_phase(emu: &Spawned, what: &str, want: impl Fn(&NetPhase) -> bool) -> NetPhase {
        wait_for(emu, what, |e| match e {
            EmuEvent::Net(s) if want(&s.phase) => Some(s.phase),
            _ => None,
        })
    }

    fn wait_net_ended(emu: &Spawned) -> (NetEndKind, String, Vec<std::path::PathBuf>) {
        wait_for(emu, "NetEnded", |e| match e {
            EmuEvent::NetEnded {
                kind,
                message,
                files,
            } => Some((*kind, message.clone(), files.clone())),
            _ => None,
        })
    }

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        std::env::temp_dir().join(format!("nes-netplay-{tag}-{}-{nanos}", std::process::id()))
    }

    fn start_with_rom(rom: Vec<u8>) -> Spawned {
        let emu = Spawned::start(crate::audio::disabled_audio());
        emu.cmd_tx.send(EmuCommand::LoadRom(rom)).unwrap();
        emu.wait_rom_loaded();
        emu
    }

    /// 房主建立房間，回傳實際監聽的 port。
    fn host_room(emu: &Spawned, dir: &std::path::Path) -> u16 {
        emu.cmd_tx
            .send(EmuCommand::NetHost {
                port: 0,
                input_delay: 2,
                replay_dir: dir.to_path_buf(),
            })
            .unwrap();
        match wait_phase(emu, "房主進入等待", |p| {
            matches!(p, NetPhase::Waiting { .. })
        }) {
            NetPhase::Waiting { port } => port,
            _ => unreachable!(),
        }
    }

    fn join_room(emu: &Spawned, port: u16, dir: &std::path::Path) {
        emu.cmd_tx
            .send(EmuCommand::NetJoin {
                addr: ([127, 0, 0, 1], port).into(),
                replay_dir: dir.to_path_buf(),
            })
            .unwrap();
    }

    #[test]
    fn two_emu_threads_play_a_match_and_save_identical_replays() {
        let dir = temp_dir("match");
        let rom = input_probe_rom();
        let a = start_with_rom(rom.clone());
        let b = start_with_rom(rom.clone());
        let port = host_room(&a, &dir);
        join_room(&b, port, &dir);

        // 連線：房主是玩家 1、加入者是玩家 2，input delay 由房主決定。
        let pa = wait_phase(&a, "房主連線", |p| {
            matches!(p, NetPhase::Connected { .. })
        });
        let pb = wait_phase(&b, "加入者連線", |p| {
            matches!(p, NetPhase::Connected { .. })
        });
        assert_eq!(
            pa,
            NetPhase::Connected {
                player: 0,
                input_delay: 2
            }
        );
        assert_eq!(
            pb,
            NetPhase::Connected {
                player: 1,
                input_delay: 2
            }
        );

        // 兩端的本地鍵盤都用「玩家 1」的槽位（SetInput(0, …)）；玩家 2 的槽位在 Netplay 中被忽略。
        a.cmd_tx.send(EmuCommand::SetInput(0, Buttons::A)).unwrap();
        a.cmd_tx
            .send(EmuCommand::SetInput(1, Buttons::all()))
            .unwrap();
        b.cmd_tx
            .send(EmuCommand::SetInput(0, Buttons::UP | Buttons::B))
            .unwrap();
        // 連線成功時重新開機：幀數從 0 重新算；等雙方都跑過 200 幀。
        a.wait_frames(200);
        b.wait_frames(200);

        a.cmd_tx.send(EmuCommand::NetDisconnect).unwrap();
        let (ka, msg_a, files_a) = wait_net_ended(&a);
        let (kb, msg_b, files_b) = wait_net_ended(&b);
        assert_eq!(ka, NetEndKind::Normal, "{msg_a}");
        assert_eq!(kb, NetEndKind::Normal, "{msg_b}");
        assert!(msg_b.contains("對方已中斷連線"), "{msg_b}");
        assert_eq!((files_a.len(), files_b.len()), (1, 1), "{msg_a} / {msg_b}");

        // 兩份 replay 位元組完全相同（雙方取共同完成的幀數），且通過 verify。
        let bytes_a = std::fs::read(&files_a[0]).unwrap();
        let bytes_b = std::fs::read(&files_b[0]).unwrap();
        assert_eq!(bytes_a, bytes_b, "雙方的 replay 必須完全相同");
        let replay = Replay::decode(&bytes_a).unwrap();
        assert!(replay.total_frames >= 200, "{} 幀", replay.total_frames);
        verify(&rom, &replay).expect("replay 必須通過 verify");
        // 合併後的輸入：玩家 1 是房主的 A、玩家 2 是加入者的 上+B（前 D 幀是空的）；
        // 房主試圖用玩家 2 的槽位送 all() 被忽略。
        let inputs: Vec<_> = replay.inputs().collect();
        assert_eq!(inputs[0].p1, Buttons::empty());
        let last = inputs[inputs.len() - 1];
        assert_eq!(last.p1, Buttons::A);
        assert_eq!(last.p2, Buttons::UP | Buttons::B);
        assert!(inputs.iter().all(|i| !i.reset));

        // 回到單機模式：Netplay 的限制解除（可以暫停），而且沒有殘留的錯誤。
        a.cmd_tx.send(EmuCommand::Pause).unwrap();
        a.barrier();
        assert!(errors(&a.event_rx.try_iter().collect::<Vec<_>>()).is_empty());
        a.quit();
        b.quit();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn joining_with_a_different_rom_is_rejected_with_the_reason_and_returns_to_local_mode() {
        let dir = temp_dir("reject");
        let a = start_with_rom(input_probe_rom());
        let b = start_with_rom(rendering_rom());
        let port = host_room(&a, &dir);
        join_room(&b, port, &dir);

        let (kind, message, files) = wait_net_ended(&b);
        assert_eq!(kind, NetEndKind::Error);
        assert!(
            message.contains("拒絕") && message.contains("ROM 不同"),
            "{message}"
        );
        assert!(files.is_empty(), "沒有連上，不存 replay");
        // 房主仍然在等（沒有被壞 Client 搞掉），也可以取消。
        a.cmd_tx.send(EmuCommand::NetDisconnect).unwrap();
        let (ka, _, _) = wait_net_ended(&a);
        assert_eq!(ka, NetEndKind::Normal);
        // 加入者回到單機模式，emu 執行緒仍然響應。
        b.cmd_tx.send(EmuCommand::Pause).unwrap();
        b.barrier();
        a.quit();
        b.quit();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn restricted_operations_are_refused_during_netplay_and_nothing_advances() {
        let dir = temp_dir("restricted");
        let emu = start_with_rom(input_probe_rom());
        emu.cmd_tx.send(EmuCommand::SaveState).unwrap();
        host_room(&emu, &dir); // 房主等待中：Netplay 工作階段已建立
        emu.event_rx.try_iter().for_each(drop);

        for cmd in [
            EmuCommand::LoadState,
            EmuCommand::Pause,
            EmuCommand::Reset,
            EmuCommand::LoadRom(input_probe_rom()),
            EmuCommand::StepInstruction,
            EmuCommand::StartRecording,
            EmuCommand::StartReplay(vec![]),
            EmuCommand::TraceToFile {
                count: 1,
                path: dir.join("trace.log"),
            },
        ] {
            emu.cmd_tx.send(cmd).unwrap();
        }
        emu.barrier();
        let events = emu.event_rx.try_iter().collect::<Vec<_>>();
        let errs = errors(&events);
        assert!(
            errs.len() >= 7,
            "每個被停用的操作都要被拒絕並說明：{errs:?}"
        );
        assert!(
            errs.iter()
                .all(|m| m.contains("Netplay 中") && m.contains("分歧")),
            "{errs:?}"
        );
        // 存檔到檔案（唯讀）仍然可用。
        emu.cmd_tx.send(EmuCommand::ExportState).unwrap();
        emu.barrier();
        assert!(
            emu.event_rx
                .try_iter()
                .any(|e| matches!(e, EmuEvent::StateExported(_)))
        );
        emu.cmd_tx.send(EmuCommand::NetDisconnect).unwrap();
        wait_net_ended(&emu);
        emu.quit();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn hosting_on_a_port_that_is_in_use_returns_to_local_mode_with_a_readable_error() {
        let taken = std::net::UdpSocket::bind("0.0.0.0:0").unwrap();
        let port = taken.local_addr().unwrap().port();
        let emu = start_with_rom(input_probe_rom());
        emu.cmd_tx
            .send(EmuCommand::NetHost {
                port,
                input_delay: 2,
                replay_dir: temp_dir("inuse"),
            })
            .unwrap();
        let (kind, message, _) = wait_net_ended(&emu);
        assert_eq!(kind, NetEndKind::Error);
        assert!(message.contains(&format!("port {port}")), "{message}");
        emu.cmd_tx.send(EmuCommand::Pause).unwrap();
        emu.barrier(); // 沒有 Netplay 工作階段：暫停不會被拒絕
        assert!(errors(&emu.event_rx.try_iter().collect::<Vec<_>>()).is_empty());
        emu.quit();
    }

    /// 直接關閉程式（`Quit`）：對方立刻知道（不必等逾時），兩邊仍然存下位元組相同的 replay。
    #[test]
    fn closing_one_program_ends_the_other_side_at_once_with_identical_replays() {
        let dir = temp_dir("quit");
        let rom = input_probe_rom();
        let a = start_with_rom(rom.clone());
        let b = start_with_rom(rom.clone());
        let port = host_room(&a, &dir);
        join_room(&b, port, &dir);
        wait_phase(&a, "房主連線", |p| {
            matches!(p, NetPhase::Connected { .. })
        });
        wait_phase(&b, "加入者連線", |p| {
            matches!(p, NetPhase::Connected { .. })
        });
        a.cmd_tx
            .send(EmuCommand::SetInput(0, Buttons::RIGHT | Buttons::A))
            .unwrap();
        b.cmd_tx
            .send(EmuCommand::SetInput(0, Buttons::LEFT))
            .unwrap();
        a.wait_frames(150);
        b.wait_frames(150);

        // 加入者關閉程式。
        b.cmd_tx.send(EmuCommand::Quit).unwrap();
        let (kb, msg_b, files_b) = wait_net_ended(&b);
        b.handle.join().unwrap();
        let (ka, msg_a, files_a) = wait_net_ended(&a);
        assert_eq!(kb, NetEndKind::Normal, "{msg_b}");
        assert_eq!(ka, NetEndKind::Normal, "{msg_a}");
        assert!(msg_a.contains("對方已中斷連線"), "{msg_a}");
        assert_eq!((files_a.len(), files_b.len()), (1, 1));
        let (bytes_a, bytes_b) = (
            std::fs::read(&files_a[0]).unwrap(),
            std::fs::read(&files_b[0]).unwrap(),
        );
        assert_eq!(bytes_a, bytes_b, "雙方的 replay 必須完全相同");
        verify(&rom, &Replay::decode(&bytes_a).unwrap()).expect("必須通過 verify");
        a.quit();
        std::fs::remove_dir_all(&dir).ok();
    }
}
