//! Lockstep session：雙方在第 N 幀的輸入都到齊之後，才推進到第 N 幀。
//!
//! # 這個模組做什麼、不做什麼
//!
//! 它**不擁有 `Nes`、不讀系統時間、不做 I/O**。呼叫端（`nes-app` 的 emu 執行緒、`nes-test netsim`、
//! 測試）每個節拍做這幾件事：
//!
//! ```text
//! session.poll(now, &mut transport);            // 收封包、送封包、重送、逾時、握手
//! while let Some(event) = session.poll_event() { … }
//! if session.local_input_wanted() {
//!     session.add_local_input(keyboard);        // 這一幀「取樣」的按鍵（會套用在 D 幀之後）
//! }
//! if let Some(input) = session.next_ready_frame(now) {   // 雙方輸入到齊才回傳
//!     nes.run_frame(input);
//!     session.frame_done(|| nes.behavior_fingerprint());  // 每 60 幀交換一次指紋
//! }                                             // None：這個節拍不推進（也不阻塞）
//! ```
//!
//! 所有與時間有關的函式（`poll`、`next_ready_frame`、`disconnect`）都由呼叫端傳入 `now`
//! （自任意原點起算的 `Duration`），所以測試可以用虛擬時鐘：不必真的等待，結果完全可重現。
//!
//! # 為什麼是這個介面（給 4c 的 rollback 沿用）
//!
//! - `poll(now, transport)`／事件佇列／統計／握手／逾時／Ack 與冗餘傳送與**推進方式無關**，rollback 沿用。
//! - lockstep 與 rollback 只差在「什麼時候可以推進」：lockstep 是 [`LockstepSession::next_ready_frame`]
//!   （對方輸入沒到就回 `None`）；rollback 會改成「缺的輸入用預測補上、事後收到真的輸入再讀檔重跑」。
//!   輸入的送收（`add_local_input` ＋ Input／Ack 封包）與指紋檢查（`frame_done`）兩者相同。
//! - 用「呼叫端主動 poll」而不是 callback／執行緒，是為了讓 session 保持純邏輯、可用虛擬時鐘
//!   完整測試，也與 `nes-core` 的「外部驅動」哲學一致（`docs/architecture.md` §4）。
//!
//! # 輸入延遲（input delay）D
//!
//! 本地在「第 k 次取樣」取得的按鍵，套用在第 `k + D` 幀。前 D 幀（沒有任何取樣可用）雙方都用空輸入，
//! 這是協定約定的一部分（雙方都知道 D，因為 Host 在 `Accept` 決定它）。D 幀的延遲讓輸入有時間在
//! 網路上飛，網路單程延遲小於 D 幀時（D=2 ≈ 33 ms）幾乎不會 stall。
//!
//! # 冗餘傳送與重送
//!
//! 每個 `Input` 封包攜帶「所有尚未被對方 Ack 的本地輸入」（上限 [`crate::protocol::MAX_INPUTS_PER_PACKET`]）；
//! 收到 `Ack` 就丟棄已確認的部分。有新輸入時立刻送；沒有新輸入但仍有未確認的（例如 stall 中），每
//! [`RESEND_INTERVAL`] 重送一次。關閉冗餘（[`SessionConfig::redundancy`] ＝ `false`，只用於比較實驗）時，
//! 每個封包只帶一幀：新輸入送最新那一幀，逾時重送最舊的未確認幀——仍然正確，但掉一個包要等一次
//! 重送才補得回來，stall 會明顯增加。
//!
//! # 決定性
//!
//! session 的**輸出**（交給 `Nes::run_frame` 的 `FrameInput` 序列）只由雙方輸入決定，與封包的到達順序、
//! 時間、丟失、重複都無關：`next_ready_frame` 只在「第 f 幀雙方的輸入都已確定」時才回傳，而輸入一經
//! 收下就不會被覆蓋。這是 lockstep 正確性的核心，見 `tests/equivalence.rs`。

use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::net::SocketAddr;
use std::time::Duration;

use nes_core::{Buttons, CORE_BEHAVIOR_VERSION, FrameInput, RomId};

use crate::protocol::{
    DisconnectReason, MAX_INPUTS_PER_PACKET, Msg, PROTOCOL_VERSION, ProtocolError, RejectReason,
};
use crate::transport::{Datagram, Transport};

pub const DEFAULT_INPUT_DELAY: u8 = 2;
pub const MAX_INPUT_DELAY: u8 = 8;
/// 每隔幾個已完成的幀交換一次行為指紋。
pub const CHECKSUM_INTERVAL: u32 = 60;
/// 超過這麼久沒有收到對方的任何封包就視為斷線。
pub const PEER_TIMEOUT: Duration = Duration::from_secs(5);
/// Client 等 Accept／Reject 的時間上限。
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
pub const HELLO_INTERVAL: Duration = Duration::from_millis(250);
pub const PING_INTERVAL: Duration = Duration::from_millis(500);
/// 有未確認的輸入、又沒有新輸入可送時，重送的間隔。
pub const RESEND_INTERVAL: Duration = Duration::from_millis(50);
pub const STATS_INTERVAL: Duration = Duration::from_secs(1);
/// 主動中斷之後，等對方回覆自己的 `Disconnect`（帶幀數）的時間上限。
pub const CLOSING_GRACE: Duration = Duration::from_secs(1);
const CLOSING_RESEND: Duration = Duration::from_millis(100);
/// 對方的輸入最多可以領先「已連續收到的幀」多少幀（防止惡意封包讓記憶體無限成長）。
const MAX_REMOTE_AHEAD: u32 = 256;
/// 最多保留幾個尚未比對的指紋。
const MAX_PENDING_CHECKSUMS: usize = 32;
/// 主動中斷／Desync 時，`Disconnect` 送幾次（UDP 會掉包）。
const DISCONNECT_REPEATS: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Host,
    Client,
}

#[derive(Debug, Clone)]
pub struct SessionConfig {
    pub role: Role,
    pub rom_id: RomId,
    /// Host 決定；Client 的這個欄位會被 `Accept` 覆蓋。上限 [`MAX_INPUT_DELAY`]（超過會被截斷）。
    pub input_delay: u8,
    /// Host 提供（`nes-net` 不產生亂數／不讀系統時間）；Client 的這個欄位會被 `Accept` 覆蓋。
    pub session_id: u32,
    pub core_behavior_version: u16,
    pub protocol_version: u16,
    /// 冗餘傳送（見模組說明）。預設開啟；關閉只用於比較實驗。
    pub redundancy: bool,
    pub peer_timeout: Duration,
    /// 握手的時間上限；`None` ＝ 一直等（Host 等對手預設不逾時，由使用者取消）。
    pub handshake_timeout: Option<Duration>,
}

impl SessionConfig {
    pub fn host(rom_id: RomId, input_delay: u8, session_id: u32) -> Self {
        Self {
            role: Role::Host,
            rom_id,
            input_delay,
            session_id,
            core_behavior_version: CORE_BEHAVIOR_VERSION,
            protocol_version: PROTOCOL_VERSION,
            redundancy: true,
            peer_timeout: PEER_TIMEOUT,
            handshake_timeout: None,
        }
    }

    pub fn client(rom_id: RomId) -> Self {
        Self {
            role: Role::Client,
            rom_id,
            input_delay: DEFAULT_INPUT_DELAY,
            session_id: 0,
            core_behavior_version: CORE_BEHAVIOR_VERSION,
            protocol_version: PROTOCOL_VERSION,
            redundancy: true,
            peer_timeout: PEER_TIMEOUT,
            handshake_timeout: Some(HANDSHAKE_TIMEOUT),
        }
    }
}

/// session 結束的原因（本地視角，UI 直接顯示 `Display` 的文字）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndReason {
    /// Client：Host 拒絕連線。
    Rejected(RejectReason),
    /// Client：逾時都沒有收到 Accept／Reject。
    HandshakeTimeout,
    /// 連線中，超過 [`PEER_TIMEOUT`] 沒有收到任何封包。
    Timeout,
    PeerLeft,
    LocalLeft,
    /// 雙方的行為指紋在已完成 `frame` 幀時不同。
    Desync {
        frame: u32,
    },
}

impl fmt::Display for EndReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EndReason::Rejected(r) => write!(f, "連線被拒絕：{r}"),
            EndReason::HandshakeTimeout => write!(
                f,
                "連線逾時：對方沒有回應。請確認 IP 與 port 正確、房主已建立房間，且防火牆允許 UDP"
            ),
            EndReason::Timeout => write!(f, "連線中斷：超過 5 秒沒有收到對方的任何封包"),
            EndReason::PeerLeft => write!(f, "對方已中斷連線"),
            EndReason::LocalLeft => write!(f, "你已中斷連線"),
            EndReason::Desync { frame } => write!(
                f,
                "偵測到不同步（desync）：已完成 {frame} 幀時雙方的行為指紋不同，連線已停止"
            ),
        }
    }
}

/// 統計資料（`Event::Stats` 每秒一次，或隨時用 [`LockstepSession::stats`] 查詢）。
/// 位元組數是 UDP payload（不含 UDP／IP 標頭的 28 位元組／封包）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats {
    /// 平滑後的來回時間（Ping／Pong 量測）；還沒有量到就是 `None`。
    pub rtt: Option<Duration>,
    pub input_delay: u8,
    /// 已完成的幀數。
    pub frame: u32,
    /// 累計 stall 次數（一次連續等不到輸入算一次）與總時間。
    pub stalls: u32,
    pub stall_time: Duration,
    pub bytes_sent: u64,
    pub bytes_received: u64,
    pub packets_sent: u64,
    pub packets_received: u64,
    /// 最近一個統計視窗（約 1 秒）的每秒位元組數。
    pub send_bytes_per_sec: u32,
    pub recv_bytes_per_sec: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// 握手成功。雙方都必須從開機狀態開始（重新載入 ROM），第 0 幀對齊。
    Connected {
        player: u8,
        input_delay: u8,
    },
    /// 下一幀的輸入沒到齊，開始 stall（一次連續的 stall 只通知一次）。
    Stalled {
        frame: u32,
    },
    /// stall 結束；`waited` 是這一次等了多久。
    Resumed {
        frame: u32,
        waited: Duration,
    },
    /// 行為指紋不符。`local`／`remote` 是雙方在 `frame`（已完成的幀數）的指紋，
    /// 由對方通知而來的 Desync 可能不知道其中一個。這個事件之後 session 結束。
    Desync {
        frame: u32,
        local: Option<u64>,
        remote: Option<u64>,
    },
    /// session 結束（拒絕、逾時、對方離開、自己離開、desync）。`peer_frames` 是對方在 `Disconnect`
    /// 裡報告的已完成幀數（取兩者較小值存 replay，雙方的檔案才會完全相同）。
    Disconnected {
        reason: EndReason,
        peer_frames: Option<u32>,
    },
    Stats(Stats),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// Host：等對手。
    Waiting,
    /// Client：送 Hello 中。
    Connecting,
    Running,
    /// 主動中斷，等對方回覆幀數（最多 [`CLOSING_GRACE`]）。
    Closing,
    Ended,
}

#[derive(Debug, Clone, Copy)]
enum State {
    Listening,
    Connecting,
    Running,
    Closing {
        since: Duration,
        next_resend: Duration,
    },
    Ended,
}

#[derive(Debug)]
pub struct LockstepSession {
    cfg: SessionConfig,
    state: State,
    session_id: u32,
    local_player: u8,
    input_delay: u8,
    started_at: Option<Duration>,
    last_recv: Duration,
    next_hello: Duration,

    // 本地輸入：`local[i]` 是第 `local_base + i` 幀。前面已被對方確認、且已被我們自己用掉的會丟棄。
    local: VecDeque<Buttons>,
    local_base: u32,
    /// 對方已確認收到的幀數（第 `< local_acked_next` 幀都確認了）。
    local_acked_next: u32,
    // 對方的輸入（尚未被 `next_ready_frame` 用掉的）。
    remote: BTreeMap<u32, Buttons>,
    /// 從第 0 幀起連續收到到哪（第 `< remote_contig` 幀都收到了）。
    remote_contig: u32,
    /// 下一個要交給 `run_frame` 的幀。
    next_frame: u32,
    frames_done: u32,
    input_dirty: bool,
    last_send: Duration,
    last_resend: Duration,

    local_fps: BTreeMap<u32, u64>,
    remote_fps: BTreeMap<u32, u64>,

    next_ping: Duration,
    srtt_us: Option<u64>,
    stall_since: Option<Duration>,
    stalls: u32,
    stall_time: Duration,

    bytes_sent: u64,
    bytes_received: u64,
    packets_sent: u64,
    packets_received: u64,
    window_start: Duration,
    window_sent: u64,
    window_received: u64,
    rate_sent: u32,
    rate_received: u32,
    next_stats: Duration,

    outbox: Vec<Msg>,
    events: VecDeque<Event>,
    end_reason: Option<EndReason>,
    peer_frames: Option<u32>,
}

impl LockstepSession {
    pub fn new(cfg: SessionConfig) -> Self {
        let input_delay = cfg.input_delay.min(MAX_INPUT_DELAY);
        let (state, local_player) = match cfg.role {
            Role::Host => (State::Listening, 0),
            Role::Client => (State::Connecting, 1),
        };
        Self {
            session_id: cfg.session_id,
            state,
            local_player,
            input_delay,
            started_at: None,
            last_recv: Duration::ZERO,
            next_hello: Duration::ZERO,
            local: VecDeque::new(),
            local_base: 0,
            local_acked_next: 0,
            remote: BTreeMap::new(),
            remote_contig: 0,
            next_frame: 0,
            frames_done: 0,
            input_dirty: false,
            last_send: Duration::ZERO,
            last_resend: Duration::ZERO,
            local_fps: BTreeMap::new(),
            remote_fps: BTreeMap::new(),
            next_ping: Duration::ZERO,
            srtt_us: None,
            stall_since: None,
            stalls: 0,
            stall_time: Duration::ZERO,
            bytes_sent: 0,
            bytes_received: 0,
            packets_sent: 0,
            packets_received: 0,
            window_start: Duration::ZERO,
            window_sent: 0,
            window_received: 0,
            rate_sent: 0,
            rate_received: 0,
            next_stats: Duration::ZERO,
            outbox: Vec::new(),
            events: VecDeque::new(),
            end_reason: None,
            peer_frames: None,
            cfg,
        }
    }

    // ---- 查詢 -----------------------------------------------------------------

    pub fn status(&self) -> Status {
        match self.state {
            State::Listening => Status::Waiting,
            State::Connecting => Status::Connecting,
            State::Running => Status::Running,
            State::Closing { .. } => Status::Closing,
            State::Ended => Status::Ended,
        }
    }

    pub fn is_running(&self) -> bool {
        matches!(self.state, State::Running)
    }

    pub fn is_ended(&self) -> bool {
        matches!(self.state, State::Ended)
    }

    /// 本地玩家的位置（0＝玩家 1、1＝玩家 2）。Host 永遠是 0，Client 由 `Accept` 指派。
    pub fn local_player(&self) -> u8 {
        self.local_player
    }

    pub fn input_delay(&self) -> u8 {
        self.input_delay
    }

    pub fn session_id(&self) -> u32 {
        self.session_id
    }

    /// 已交給呼叫端並回報完成（[`Self::frame_done`]）的幀數。
    pub fn frames_completed(&self) -> u32 {
        self.frames_done
    }

    pub fn end_reason(&self) -> Option<EndReason> {
        self.end_reason
    }

    /// 對方在 `Disconnect` 裡報告的已完成幀數。
    pub fn peer_frames(&self) -> Option<u32> {
        self.peer_frames
    }

    pub fn stats(&self) -> Stats {
        Stats {
            rtt: self.srtt_us.map(Duration::from_micros),
            input_delay: self.input_delay,
            frame: self.frames_done,
            stalls: self.stalls,
            stall_time: self.stall_time,
            bytes_sent: self.bytes_sent,
            bytes_received: self.bytes_received,
            packets_sent: self.packets_sent,
            packets_received: self.packets_received,
            send_bytes_per_sec: self.rate_sent,
            recv_bytes_per_sec: self.rate_received,
        }
    }

    pub fn poll_event(&mut self) -> Option<Event> {
        self.events.pop_front()
    }

    // ---- 輸入 -----------------------------------------------------------------

    fn local_next(&self) -> u32 {
        self.local_base + self.local.len() as u32
    }

    /// 現在該取樣本地輸入了嗎？每推進一幀恰好一次（前 D 幀是預先填好的空輸入）。
    pub fn local_input_wanted(&self) -> bool {
        self.is_running() && self.local_next() <= self.next_frame + u32::from(self.input_delay)
    }

    /// 加入這一次取樣的本地按鍵（套用在 `D` 幀之後）。只有 [`Self::local_input_wanted`] 為 `true`
    /// 時才接受（否則忽略並回傳 `false`），所以本地輸入永遠只會領先 `D + 1` 幀，不會因為 stall
    /// 而無限堆積。
    pub fn add_local_input(&mut self, buttons: Buttons) -> bool {
        if !self.local_input_wanted() {
            return false;
        }
        self.local.push_back(buttons);
        self.input_dirty = true;
        true
    }

    /// 雙方在 `next_frame` 的輸入都到齊時回傳這一幀的 [`FrameInput`]（`reset` 永遠是 `false`：
    /// netplay 期間 reset 不是輸入的一部分，UI 停用它）；否則回傳 `None`——**不阻塞**，
    /// 呼叫端下個節拍再試。第一次等不到時開始計算一次 stall，等到了才結束。
    pub fn next_ready_frame(&mut self, now: Duration) -> Option<FrameInput> {
        if !self.is_running() {
            return None;
        }
        let f = self.next_frame;
        let local = self.local_input(f);
        let remote = self.remote.get(&f).copied();
        let (Some(local), Some(remote)) = (local, remote) else {
            // 只有「本地輸入已經有、就缺對方的」才是網路造成的 stall。
            if local.is_some() && self.stall_since.is_none() {
                self.stall_since = Some(now);
                self.stalls += 1;
                self.events.push_back(Event::Stalled { frame: f });
            }
            return None;
        };
        if let Some(since) = self.stall_since.take() {
            let waited = now.saturating_sub(since);
            self.stall_time += waited;
            self.events.push_back(Event::Resumed { frame: f, waited });
        }
        self.remote.remove(&f);
        self.next_frame += 1;
        self.prune_local();
        let (p1, p2) = if self.local_player == 0 {
            (local, remote)
        } else {
            (remote, local)
        };
        Some(FrameInput::new(p1, p2))
    }

    fn local_input(&self, frame: u32) -> Option<Buttons> {
        let idx = frame.checked_sub(self.local_base)?;
        self.local.get(idx as usize).copied()
    }

    fn prune_local(&mut self) {
        let keep_from = self.local_acked_next.min(self.next_frame);
        while self.local_base < keep_from && self.local.pop_front().is_some() {
            self.local_base += 1;
        }
    }

    /// 呼叫端剛用 `next_ready_frame` 的輸入跑完一幀之後呼叫。每 [`CHECKSUM_INTERVAL`] 幀才會呼叫
    /// `fingerprint`（`|| nes.behavior_fingerprint()`），交換並比對雙方的指紋。
    pub fn frame_done(&mut self, fingerprint: impl FnOnce() -> u64) {
        self.frames_done += 1;
        let n = self.frames_done;
        if !self.is_running() || !n.is_multiple_of(CHECKSUM_INTERVAL) {
            return;
        }
        let fp = fingerprint();
        self.local_fps.insert(n, fp);
        trim_oldest(&mut self.local_fps);
        self.outbox.push(Msg::Checksum {
            session_id: self.session_id,
            frame: n,
            fingerprint: fp,
        });
        if let Some(remote) = self.remote_fps.remove(&n) {
            self.compare_checksum(n, fp, remote);
        }
    }

    fn compare_checksum(&mut self, frame: u32, local: u64, remote: u64) {
        self.local_fps.remove(&frame);
        if local == remote {
            return;
        }
        self.events.push_back(Event::Desync {
            frame,
            local: Some(local),
            remote: Some(remote),
        });
        for _ in 0..DISCONNECT_REPEATS {
            self.outbox.push(Msg::Disconnect {
                session_id: self.session_id,
                reason: DisconnectReason::Desync { frame },
                frames_completed: self.frames_done,
            });
        }
        self.end(EndReason::Desync { frame });
    }

    // ---- 主動中斷 -------------------------------------------------------------

    /// 使用者中斷連線（或取消等待）。連線中：送出 `Disconnect`（帶自己已完成的幀數），
    /// 進入 `Closing`，等對方回覆它的幀數（最多 [`CLOSING_GRACE`]）再結束；呼叫端要繼續 `poll`。
    pub fn disconnect(&mut self, now: Duration) {
        match self.state {
            State::Running => {
                for _ in 0..DISCONNECT_REPEATS {
                    self.outbox.push(Msg::Disconnect {
                        session_id: self.session_id,
                        reason: DisconnectReason::Left,
                        frames_completed: self.frames_done,
                    });
                }
                self.state = State::Closing {
                    since: now,
                    next_resend: now + CLOSING_RESEND,
                };
            }
            State::Listening | State::Connecting => self.end(EndReason::LocalLeft),
            State::Closing { .. } | State::Ended => {}
        }
    }

    fn end(&mut self, reason: EndReason) {
        if self.is_ended() {
            return;
        }
        self.state = State::Ended;
        self.end_reason = Some(reason);
        self.events.push_back(Event::Disconnected {
            reason,
            peer_frames: self.peer_frames,
        });
    }

    // ---- poll -----------------------------------------------------------------

    /// 收封包、處理、送封包、重送、逾時、握手、統計。每個節拍呼叫一次（越頻繁越即時；可以每毫秒）。
    pub fn poll<T: Transport + ?Sized>(&mut self, now: Duration, transport: &mut T) {
        let started = *self.started_at.get_or_insert(now);
        for datagram in transport.recv(now) {
            self.on_datagram(now, datagram, transport);
        }
        self.flush_outbox(now, transport);

        match self.state {
            State::Connecting => {
                if let Some(limit) = self.cfg.handshake_timeout
                    && now.saturating_sub(started) >= limit
                {
                    self.end(EndReason::HandshakeTimeout);
                } else if now >= self.next_hello {
                    self.next_hello = now + HELLO_INTERVAL;
                    let hello = Msg::Hello {
                        protocol_version: self.cfg.protocol_version,
                        core_behavior_version: self.cfg.core_behavior_version,
                        rom_id: self.cfg.rom_id,
                    };
                    self.send(now, transport, None, &hello);
                }
            }
            State::Listening => {
                if let Some(limit) = self.cfg.handshake_timeout
                    && now.saturating_sub(started) >= limit
                {
                    self.end(EndReason::HandshakeTimeout);
                }
            }
            State::Running => self.poll_running(now, transport),
            State::Closing { since, next_resend } => {
                if now.saturating_sub(since) >= CLOSING_GRACE {
                    self.end(EndReason::LocalLeft);
                } else if now >= next_resend {
                    self.state = State::Closing {
                        since,
                        next_resend: now + CLOSING_RESEND,
                    };
                    let msg = Msg::Disconnect {
                        session_id: self.session_id,
                        reason: DisconnectReason::Left,
                        frames_completed: self.frames_done,
                    };
                    self.send(now, transport, None, &msg);
                }
            }
            State::Ended => {}
        }
        self.flush_outbox(now, transport);
    }

    fn poll_running<T: Transport + ?Sized>(&mut self, now: Duration, transport: &mut T) {
        if now.saturating_sub(self.last_recv) > self.cfg.peer_timeout {
            self.end(EndReason::Timeout);
            return;
        }
        self.send_inputs_if_due(now, transport);
        if now >= self.next_ping {
            self.next_ping = now + PING_INTERVAL;
            let ping = Msg::Ping {
                session_id: self.session_id,
                timestamp_us: now.as_micros() as u64,
            };
            self.send(now, transport, None, &ping);
        }
        if now >= self.next_stats {
            let elapsed = now
                .saturating_sub(self.window_start)
                .as_secs_f64()
                .max(1e-9);
            self.rate_sent = (self.window_sent as f64 / elapsed) as u32;
            self.rate_received = (self.window_received as f64 / elapsed) as u32;
            self.window_start = now;
            self.window_sent = 0;
            self.window_received = 0;
            self.next_stats = now + STATS_INTERVAL;
            self.events.push_back(Event::Stats(self.stats()));
        }
    }

    fn unacked(&self) -> u32 {
        self.local_next().saturating_sub(self.local_acked_next)
    }

    fn send_inputs_if_due<T: Transport + ?Sized>(&mut self, now: Duration, transport: &mut T) {
        let unacked = self.unacked();
        if unacked == 0 {
            self.input_dirty = false;
            return;
        }
        if self.cfg.redundancy {
            // 冗餘：每個封包帶「所有」未確認的輸入（從最舊的起，上限 N 幀）。
            if self.input_dirty || now.saturating_sub(self.last_send) >= RESEND_INTERVAL {
                self.send_input_range(now, transport, self.local_acked_next, unacked);
                self.input_dirty = false;
            }
        } else {
            // 無冗餘（實驗用）：新輸入送最新那一幀；逾時重送最舊的未確認幀。
            if self.input_dirty {
                self.send_input_range(now, transport, self.local_next() - 1, 1);
                self.input_dirty = false;
            }
            if now.saturating_sub(self.last_resend) >= RESEND_INTERVAL {
                self.send_input_range(now, transport, self.local_acked_next, 1);
                self.last_resend = now;
            }
        }
    }

    fn send_input_range<T: Transport + ?Sized>(
        &mut self,
        now: Duration,
        transport: &mut T,
        start: u32,
        count: u32,
    ) {
        let count = (count as usize).min(MAX_INPUTS_PER_PACKET);
        let Some(offset) = start.checked_sub(self.local_base) else {
            return;
        };
        let inputs: Vec<Buttons> = self
            .local
            .iter()
            .skip(offset as usize)
            .take(count)
            .copied()
            .collect();
        if inputs.is_empty() {
            return;
        }
        self.last_send = now;
        let msg = Msg::Input {
            session_id: self.session_id,
            start_frame: start,
            inputs,
        };
        self.send(now, transport, None, &msg);
    }

    fn flush_outbox<T: Transport + ?Sized>(&mut self, now: Duration, transport: &mut T) {
        for msg in std::mem::take(&mut self.outbox) {
            self.send(now, transport, None, &msg);
        }
    }

    /// 編碼並送出。`to` 有值（Host 握手時對端尚未鎖定）就送給那個位址。
    fn send<T: Transport + ?Sized>(
        &mut self,
        now: Duration,
        transport: &mut T,
        to: Option<SocketAddr>,
        msg: &Msg,
    ) {
        let Ok(bytes) = msg.encode() else {
            log::error!("編碼失敗（不應發生）：{msg:?}");
            return;
        };
        self.bytes_sent += bytes.len() as u64;
        self.window_sent += bytes.len() as u64;
        self.packets_sent += 1;
        match to {
            Some(addr) => transport.send_to(now, addr, &bytes),
            None => transport.send(now, &bytes),
        }
    }

    // ---- 收封包 ---------------------------------------------------------------

    fn on_datagram<T: Transport + ?Sized>(
        &mut self,
        now: Duration,
        datagram: Datagram,
        transport: &mut T,
    ) {
        if self.is_ended() {
            return;
        }
        let msg = match Msg::decode(&datagram.data) {
            Ok(msg) => msg,
            Err(ProtocolError::UnsupportedVersion(theirs)) => {
                // 對方用不同版本的協定打招呼：Host 還在等人時，明確告訴它為什麼不行。
                if matches!(self.state, State::Listening) && !datagram.stranger {
                    let reject = Msg::Reject {
                        reason: RejectReason::ProtocolVersion {
                            host: self.cfg.protocol_version,
                            client: theirs,
                        },
                    };
                    self.send(now, transport, datagram.from, &reject);
                }
                return;
            }
            Err(_) => return, // 雜訊、截斷、超過大小：丟棄
        };

        if datagram.stranger {
            // 已經有對手了：別的位址來敲門，只回覆「房間已滿」，其餘一律忽略。
            if matches!(msg, Msg::Hello { .. })
                && matches!(self.state, State::Running | State::Closing { .. })
            {
                let reject = Msg::Reject {
                    reason: RejectReason::RoomFull,
                };
                self.send(now, transport, datagram.from, &reject);
            }
            return;
        }
        self.bytes_received += datagram.data.len() as u64;
        self.window_received += datagram.data.len() as u64;
        self.packets_received += 1;

        match msg {
            Msg::Hello {
                protocol_version,
                core_behavior_version,
                rom_id,
            } => self.on_hello(
                now,
                transport,
                datagram.from,
                protocol_version,
                core_behavior_version,
                rom_id,
            ),
            Msg::Accept {
                session_id,
                input_delay,
                player,
            } => self.on_accept(now, session_id, input_delay, player),
            Msg::Reject { reason } => {
                if matches!(self.state, State::Connecting) {
                    self.end(EndReason::Rejected(reason));
                }
            }
            other => {
                // session_id 不符的封包直接丟棄。
                if matches!(self.state, State::Running | State::Closing { .. })
                    && other.session_id() == Some(self.session_id)
                {
                    self.last_recv = now;
                    self.on_session_msg(now, transport, other);
                }
            }
        }
    }

    fn on_hello<T: Transport + ?Sized>(
        &mut self,
        now: Duration,
        transport: &mut T,
        from: Option<SocketAddr>,
        protocol_version: u16,
        core_behavior_version: u16,
        rom_id: RomId,
    ) {
        if self.cfg.role != Role::Host {
            return;
        }
        match self.state {
            State::Listening => {
                let reason = if protocol_version != self.cfg.protocol_version {
                    Some(RejectReason::ProtocolVersion {
                        host: self.cfg.protocol_version,
                        client: protocol_version,
                    })
                } else if core_behavior_version != self.cfg.core_behavior_version {
                    Some(RejectReason::CoreVersion {
                        host: self.cfg.core_behavior_version,
                        client: core_behavior_version,
                    })
                } else if rom_id != self.cfg.rom_id {
                    Some(RejectReason::RomMismatch {
                        host: self.cfg.rom_id,
                        client: rom_id,
                    })
                } else {
                    None
                };
                if let Some(reason) = reason {
                    // 拒絕之後仍然等別人：不鎖定這個位址。
                    self.send(now, transport, from, &Msg::Reject { reason });
                    return;
                }
                if let Some(addr) = from {
                    transport.set_peer(addr);
                }
                self.begin_running(now, 0);
                self.send_accept(now, transport, from);
            }
            // Accept 掉了，Client 又送 Hello：重送同一份 Accept（冪等）。
            State::Running => self.send_accept(now, transport, from),
            _ => {}
        }
    }

    fn send_accept<T: Transport + ?Sized>(
        &mut self,
        now: Duration,
        transport: &mut T,
        to: Option<SocketAddr>,
    ) {
        let accept = Msg::Accept {
            session_id: self.session_id,
            input_delay: self.input_delay,
            player: 1,
        };
        self.send(now, transport, to, &accept);
    }

    fn on_accept(&mut self, now: Duration, session_id: u32, input_delay: u8, player: u8) {
        if !matches!(self.state, State::Connecting) || player > 1 || input_delay > MAX_INPUT_DELAY {
            return;
        }
        self.session_id = session_id;
        self.input_delay = input_delay;
        self.begin_running(now, player);
    }

    /// 握手完成：進入 Running。前 `D` 幀的本地輸入是空的（協定約定，雙方都知道 D）。
    fn begin_running(&mut self, now: Duration, player: u8) {
        self.state = State::Running;
        self.local_player = player;
        self.last_recv = now;
        self.next_ping = now;
        self.window_start = now;
        self.next_stats = now + STATS_INTERVAL;
        self.local.extend(std::iter::repeat_n(
            Buttons::empty(),
            usize::from(self.input_delay),
        ));
        // 預填的輸入也要送給對方（對方要靠封包才知道我方前 D 幀是空的），連上就立刻送。
        self.input_dirty = !self.local.is_empty();
        self.events.push_back(Event::Connected {
            player,
            input_delay: self.input_delay,
        });
    }

    fn on_session_msg<T: Transport + ?Sized>(
        &mut self,
        now: Duration,
        transport: &mut T,
        msg: Msg,
    ) {
        // `Closing`：只關心對方的 Disconnect（帶它的幀數）。
        if matches!(self.state, State::Closing { .. }) {
            if let Msg::Disconnect {
                frames_completed, ..
            } = msg
            {
                self.peer_frames = Some(frames_completed);
                self.end(EndReason::LocalLeft);
            }
            return;
        }
        match msg {
            Msg::Input {
                start_frame,
                inputs,
                ..
            } => self.on_input(now, transport, start_frame, &inputs),
            Msg::Ack { frame, .. } => {
                let acked = frame.saturating_add(1).min(self.local_next());
                self.local_acked_next = self.local_acked_next.max(acked);
                self.prune_local();
            }
            Msg::Checksum {
                frame, fingerprint, ..
            } => match self.local_fps.remove(&frame) {
                Some(local) => self.compare_checksum(frame, local, fingerprint),
                None => {
                    if frame > self.frames_done {
                        self.remote_fps.insert(frame, fingerprint);
                        trim_oldest(&mut self.remote_fps);
                    }
                }
            },
            Msg::Ping { timestamp_us, .. } => {
                let pong = Msg::Pong {
                    session_id: self.session_id,
                    timestamp_us,
                };
                self.send(now, transport, None, &pong);
            }
            Msg::Pong { timestamp_us, .. } => {
                let sample = (now.as_micros() as u64).saturating_sub(timestamp_us);
                self.srtt_us = Some(match self.srtt_us {
                    None => sample,
                    Some(s) => (s * 7 + sample) / 8,
                });
            }
            Msg::Disconnect {
                reason,
                frames_completed,
                ..
            } => {
                self.peer_frames = Some(frames_completed);
                match reason {
                    DisconnectReason::Left => {
                        // 回覆自己的幀數，對方才能算出雙方共同的長度。
                        let reply = Msg::Disconnect {
                            session_id: self.session_id,
                            reason: DisconnectReason::Left,
                            frames_completed: self.frames_done,
                        };
                        for _ in 0..DISCONNECT_REPEATS {
                            self.send(now, transport, None, &reply);
                        }
                        self.end(EndReason::PeerLeft);
                    }
                    DisconnectReason::Desync { frame } => {
                        self.events.push_back(Event::Desync {
                            frame,
                            local: self.local_fps.get(&frame).copied(),
                            remote: None,
                        });
                        self.end(EndReason::Desync { frame });
                    }
                }
            }
            Msg::Hello { .. } | Msg::Accept { .. } | Msg::Reject { .. } => {}
        }
    }

    fn on_input<T: Transport + ?Sized>(
        &mut self,
        now: Duration,
        transport: &mut T,
        start_frame: u32,
        inputs: &[Buttons],
    ) {
        for (i, &buttons) in inputs.iter().enumerate() {
            let Some(frame) = start_frame.checked_add(i as u32) else {
                break;
            };
            // 已經連續收到的：重複，忽略。太遠的：不收（有上限，防止記憶體無限成長）。
            if frame < self.remote_contig {
                continue;
            }
            if frame - self.remote_contig >= MAX_REMOTE_AHEAD {
                break;
            }
            self.remote.entry(frame).or_insert(buttons);
        }
        while self.remote.contains_key(&self.remote_contig) {
            self.remote_contig += 1;
        }
        // 每收到一個 Input 就回 Ack（重複的封包也回：Ack 可能掉了）。
        if let Some(last) = self.remote_contig.checked_sub(1) {
            let ack = Msg::Ack {
                session_id: self.session_id,
                frame: last,
            };
            self.send(now, transport, None, &ack);
        }
    }
}

/// 只保留最新的 [`MAX_PENDING_CHECKSUMS`] 個。
fn trim_oldest(map: &mut BTreeMap<u32, u64>) {
    while map.len() > MAX_PENDING_CHECKSUMS {
        map.pop_first();
    }
}
