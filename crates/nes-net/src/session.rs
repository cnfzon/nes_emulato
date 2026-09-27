//! 連線 session：握手、輸入交換、統計、逾時與中斷，加上兩種推進模式——
//! **lockstep**（Phase 4b）：雙方在第 N 幀的輸入都到齊之後，才推進到第 N 幀；
//! **rollback**（Phase 4c）：本地輸入立即套用、對方輸入先預測，真實輸入到達且不同時還原並重跑
//! （排程邏輯在 [`crate::rollback`]，這裡只負責網路與握手）。模式由 Host 在 `Accept` 決定，Client 跟隨。
//!
//! 下面的說明以 lockstep 為主（`next_ready_frame`／`frame_done`）；rollback 的介面是
//! [`Session::advance`]（回傳請求清單，見 `rollback.rs` 與 `snapshot.rs`）。握手、Ack／重送、Ping、統計、
//! 中斷與逾時兩種模式完全共用。
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
//!     session.add_local_input(keyboard);        // 這一幀「取樣」的按鍵（會套用在 D 幀之後）；可以帶 reset 旗標
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
//! - lockstep 與 rollback 只差在「什麼時候可以推進」：lockstep 是 [`Session::next_ready_frame`]
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

use nes_core::{CORE_BEHAVIOR_VERSION, FrameInput, RomId};

use crate::protocol::{
    ConfirmedFingerprint, DisconnectReason, LEGACY_PROTOCOL_VERSION, MAX_INPUTS_PER_PACKET, Mode,
    Msg, PROTOCOL_VERSION, PlayerInput, ProtocolError, RejectReason,
};
use crate::rollback::{
    self, ConfirmedFrame, Plan, REMOTE_HISTORY, RemoteInput, RollbackConfig, RollbackPlanner,
    RollbackStats, Sabotage,
};
use crate::transport::{Datagram, Transport};

/// lockstep 的預設輸入延遲（雙方共用，Host 決定）。rollback 的本地輸入延遲預設 1、可設 0–4
/// （見 [`rollback::DEFAULT_INPUT_DELAY`]、[`rollback::MAX_INPUT_DELAY`]）。
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
/// 一次 `poll` 最多處理幾個收到的封包（其餘丟棄並計入 `packets_ignored`）：封包洪流下 CPU 與記憶體都有上限。
pub const MAX_DATAGRAMS_PER_POLL: usize = 1024;
/// 事件佇列的上限（呼叫端沒有取走時）。滿了先丟最舊的 `Stats`／`Stalled`／`Resumed`；
/// `Connected`／`Desync`／`Disconnected` 一定保留（各最多一個）。
pub const MAX_QUEUED_EVENTS: usize = 128;
/// 對方輸入的 `sender_frame` 最多可以領先本地目前幀多少幀，否則視為語意錯誤（合法的對方最多領先
/// 「預測視窗＋輸入延遲」約 40 幀）。
const MAX_SENDER_LEAD: u32 = 256;
/// 送出後還沒被對方 Ack 的本地輸入上限（約 10 秒）。對方一直送封包卻從不確認時，本地輸入不能無限堆積。
pub const MAX_UNACKED_INPUTS: u32 = 600;
/// 對「別的位址」或被拒絕的 Hello 的回覆速率上限（每秒幾個）：不讓 session 成為放大器，也不被 Hello 洪流拖垮。
const MAX_REPLIES_PER_SEC: u32 = 20;
/// 握手完成之後，Host 還容許「與原本相同的 Hello」（Accept 掉了、Client 重送）的時間。
const HELLO_GRACE: Duration = HANDSHAKE_TIMEOUT;
/// `Pong` 帶回的往返時間超過這個值視為不可信（不更新 RTT）。
const MAX_PLAUSIBLE_RTT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Host,
    Client,
}

#[derive(Debug, Clone)]
pub struct SessionConfig {
    pub role: Role,
    pub rom_id: RomId,
    /// **lockstep**：雙方共用的輸入延遲，Host 決定，Client 的這個欄位會被 `Accept` 覆蓋（上限
    /// [`MAX_INPUT_DELAY`]，超過會被截斷）。**rollback**：這一方自己的本地輸入延遲（上限
    /// [`rollback::MAX_INPUT_DELAY`]），雙方各自決定、不必一致。
    pub input_delay: u8,
    /// 連線模式。Host 決定；Client 的這個欄位會被 `Accept` 覆蓋。
    pub mode: Mode,
    /// rollback 的預測視窗 K（本地設定，雙方不必一致）。
    pub window: u32,
    /// rollback 的時間同步（預設開；關閉只用於比較實驗）。
    pub time_sync: bool,
    /// 破壞性測試開關（**只給測試用**）。
    pub sabotage: Sabotage,
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
    /// Host。預設 lockstep（Phase 4b 的行為）；用 [`Self::with_mode`] 改成 rollback。
    pub fn host(rom_id: RomId, input_delay: u8, session_id: u32) -> Self {
        Self {
            role: Role::Host,
            rom_id,
            input_delay,
            mode: Mode::Lockstep,
            window: rollback::DEFAULT_WINDOW,
            time_sync: true,
            sabotage: Sabotage::None,
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
            input_delay: rollback::DEFAULT_INPUT_DELAY,
            mode: Mode::Lockstep,
            window: rollback::DEFAULT_WINDOW,
            time_sync: true,
            sabotage: Sabotage::None,
            session_id: 0,
            core_behavior_version: CORE_BEHAVIOR_VERSION,
            protocol_version: PROTOCOL_VERSION,
            redundancy: true,
            peer_timeout: PEER_TIMEOUT,
            handshake_timeout: Some(HANDSHAKE_TIMEOUT),
        }
    }
}

impl SessionConfig {
    pub fn with_mode(mut self, mode: Mode) -> Self {
        self.mode = mode;
        self
    }

    pub fn with_input_delay(mut self, input_delay: u8) -> Self {
        self.input_delay = input_delay;
        self
    }

    pub fn with_window(mut self, window: u32) -> Self {
        self.window = window;
        self
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
    /// 對方送來格式正確但語意錯誤的封包（Phase 4d），連線被中止。
    ProtocolViolation(Violation),
}

/// 語意層級的協定違規（封包解得開，但內容不可能來自遵守協定的對方）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Violation {
    /// 輸入的幀號領先「連續收到的幀」太遠（或幀號溢位）。合法的對方只會從「未被 Ack 的最舊一幀」起送。
    InputFrameTooFar { frame: u32 },
    /// 一個 `Input` 封包攜帶超過 [`MAX_INPUTS_PER_PACKET`] 幀。
    TooManyInputs { count: usize },
    /// 同一幀收到與先前**不同**的輸入（輸入一經送出就不能改變）。
    ConflictingInput { frame: u32 },
    /// `Input.sender_frame` 遠在本地目前幀之後。
    SenderFrameTooFar { sender_frame: u32 },
    /// `Ack` 確認了本地根本沒送過的幀。
    AckBeyondSent { frame: u32 },
    /// 握手完成之後又收到不該有的握手訊息（`what`：Hello／Accept）——內容與原本不同、超過寬限期或角色不對。
    HandshakeAfterRunning { what: &'static str },
    /// 對方持續送封包，卻超過 [`MAX_UNACKED_INPUTS`] 幀沒有確認我們的輸入。
    NotAcking,
}

impl fmt::Display for Violation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Violation::InputFrameTooFar { frame } => {
                write!(f, "輸入的幀號 {frame} 遠超出合理範圍")
            }
            Violation::TooManyInputs { count } => write!(
                f,
                "一個封包攜帶 {count} 幀輸入，超過上限 {MAX_INPUTS_PER_PACKET}"
            ),
            Violation::ConflictingInput { frame } => {
                write!(f, "第 {frame} 幀收到與先前不同的輸入")
            }
            Violation::SenderFrameTooFar { sender_frame } => {
                write!(f, "對方宣稱的幀號 {sender_frame} 遠超出合理範圍")
            }
            Violation::AckBeyondSent { frame } => {
                write!(f, "對方確認了我們從未送出的第 {frame} 幀")
            }
            Violation::HandshakeAfterRunning { what } => {
                write!(f, "握手完成後又收到不合理的 {what}")
            }
            Violation::NotAcking => write!(
                f,
                "對方持續送封包卻超過 {MAX_UNACKED_INPUTS} 幀沒有確認我們的輸入"
            ),
        }
    }
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
            EndReason::ProtocolViolation(v) => {
                write!(f, "連線已中止：對方送來不合協定的封包（{v}）")
            }
        }
    }
}

/// 統計資料（`Event::Stats` 每秒一次，或隨時用 [`Session::stats`] 查詢）。
/// 位元組數是 UDP payload（不含 UDP／IP 標頭的 28 位元組／封包）。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Stats {
    pub mode: Mode,
    /// 平滑後的來回時間（Ping／Pong 量測）；還沒有量到就是 `None`。
    pub rtt: Option<Duration>,
    /// 這一方實際使用的（本地）輸入延遲。
    pub input_delay: u8,
    /// 已完成的幀數（rollback：已確認幀）。
    pub frame: u32,
    /// 累計 stall 次數（一次連續等不到輸入算一次）與總時間。lockstep：等對方的輸入；
    /// rollback：預測視窗已滿、暫停推進。
    pub stalls: u32,
    pub stall_time: Duration,
    /// rollback 專屬統計（lockstep 為 `None`）。
    pub rollback: Option<RollbackStats>,
    pub bytes_sent: u64,
    pub bytes_received: u64,
    pub packets_sent: u64,
    pub packets_received: u64,
    /// 最近一個統計視窗（約 1 秒）的每秒位元組數。
    pub send_bytes_per_sec: u32,
    pub recv_bytes_per_sec: u32,
    /// 被忽略的封包數：無法解碼、來自別的位址、過時的重複輸入、超過每次 `poll` 上限、被限速的回覆。
    pub packets_ignored: u64,
    /// 距離上一次收到對方（session_id 相符的）封包過了多久；還沒連上時是零。UI 用它在斷線逾時之前提早警告。
    pub silent_for: Duration,
}

// `Stats` 比其他事件大很多，但事件每秒才一個（不在熱路徑上），裝箱只會讓每個呼叫端的比對多一層 `*`。
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// 握手成功。雙方都必須從開機狀態開始（重新載入 ROM），第 0 幀對齊。
    Connected {
        player: u8,
        /// 這一方實際使用的（本地）輸入延遲。
        input_delay: u8,
        mode: Mode,
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
pub struct Session {
    cfg: SessionConfig,
    state: State,
    session_id: u32,
    local_player: u8,
    input_delay: u8,
    mode: Mode,
    /// rollback 模式（握手完成後）的排程器。
    rb: Option<RollbackPlanner>,
    /// 上一次統計時的累計 rollback 次數（算每秒次數用）。
    rollbacks_at_last_stats: u32,
    rollbacks_per_sec: f32,
    /// 最近一個統計視窗內最深的一次重跑。
    window_max_depth: u32,
    started_at: Option<Duration>,
    last_recv: Duration,
    next_hello: Duration,

    // 本地輸入：`local[i]` 是第 `local_base + i` 幀。前面已被對方確認、且已被我們自己用掉的會丟棄。
    local: VecDeque<PlayerInput>,
    local_base: u32,
    /// 對方已確認收到的幀數（第 `< local_acked_next` 幀都確認了）。
    local_acked_next: u32,
    // 對方的輸入（尚未被 `next_ready_frame` 用掉的）。
    remote: BTreeMap<u32, PlayerInput>,
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

    /// 最近一次 `poll` 的時間（算 `silent_for`）。
    last_now: Duration,
    packets_ignored: u64,
    /// Host：對手的 Hello 內容與握手完成的時間（之後只容許相同內容的重送）。
    peer_hello: Option<(u16, u16, RomId)>,
    running_since: Duration,
    /// Client：房主的 Accept 內容（之後只容許相同內容的重複 Accept）。
    accepted: Option<(u32, u8, u8, Mode)>,
    /// 回覆速率限制的視窗。
    reply_window_start: Duration,
    replies_in_window: u32,
}

/// 各個內部佇列／緩衝區目前的大小（診斷與封包洪流測試用：全部都有固定上限）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BufferSizes {
    pub local_inputs: usize,
    pub remote_inputs: usize,
    pub local_checksums: usize,
    pub remote_checksums: usize,
    pub events: usize,
    pub outbox: usize,
    /// rollback 規劃器：對方輸入、已確認的指紋、對方待比對的指紋、預測中的幀。
    pub planner_remote_inputs: usize,
    pub planner_fingerprints: usize,
    pub planner_remote_fingerprints: usize,
    pub planner_frames: usize,
}

impl Session {
    pub fn new(cfg: SessionConfig) -> Self {
        let mode = cfg.mode;
        let input_delay = local_delay_for(mode, cfg.input_delay);
        let (state, local_player) = match cfg.role {
            Role::Host => (State::Listening, 0),
            Role::Client => (State::Connecting, 1),
        };
        Self {
            session_id: cfg.session_id,
            state,
            local_player,
            input_delay,
            mode,
            rb: None,
            rollbacks_at_last_stats: 0,
            rollbacks_per_sec: 0.0,
            window_max_depth: 0,
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
            last_now: Duration::ZERO,
            packets_ignored: 0,
            peer_hello: None,
            running_since: Duration::ZERO,
            accepted: None,
            reply_window_start: Duration::ZERO,
            replies_in_window: 0,
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

    /// 這一方實際使用的（本地）輸入延遲。
    pub fn input_delay(&self) -> u8 {
        self.input_delay
    }

    /// 連線模式。Host 從設定得知；Client 要握手完成（`Connected`）之後才是 Host 決定的那個。
    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn session_id(&self) -> u32 {
        self.session_id
    }

    /// 已完成的幀數。lockstep：已交給呼叫端並回報完成（[`Self::frame_done`]）的幀數；
    /// rollback：**已確認幀**（雙方輸入都是真實的、不會再被改變的幀數；存 replay 與 `Disconnect` 用這個）。
    pub fn frames_completed(&self) -> u32 {
        self.rb
            .as_ref()
            .map_or(self.frames_done, RollbackPlanner::confirmed_frame)
    }

    pub fn end_reason(&self) -> Option<EndReason> {
        self.end_reason
    }

    /// 對方在 `Disconnect` 裡報告的已完成幀數。
    pub fn peer_frames(&self) -> Option<u32> {
        self.peer_frames
    }

    pub fn stats(&self) -> Stats {
        let rollback = self.rb.as_ref().map(|rb| RollbackStats {
            rollbacks_per_sec: self.rollbacks_per_sec,
            window_max_depth: self.window_max_depth,
            ..rb.stats()
        });
        Stats {
            mode: self.mode,
            rtt: self.srtt_us.map(Duration::from_micros),
            input_delay: self.input_delay,
            frame: self.frames_completed(),
            stalls: rollback.map_or(self.stalls, |r| r.stalls),
            stall_time: rollback.map_or(self.stall_time, |r| r.stall_time),
            rollback,
            bytes_sent: self.bytes_sent,
            bytes_received: self.bytes_received,
            packets_sent: self.packets_sent,
            packets_received: self.packets_received,
            send_bytes_per_sec: self.rate_sent,
            recv_bytes_per_sec: self.rate_received,
            packets_ignored: self.packets_ignored,
            silent_for: if self.is_running() {
                self.last_now.saturating_sub(self.last_recv)
            } else {
                Duration::ZERO
            },
        }
    }

    /// 各個內部佇列目前的大小（診斷與封包洪流測試用）。
    pub fn buffer_sizes(&self) -> BufferSizes {
        let planner = self.rb.as_ref().map(RollbackPlanner::buffer_sizes);
        BufferSizes {
            local_inputs: self.local.len(),
            remote_inputs: self.remote.len(),
            local_checksums: self.local_fps.len(),
            remote_checksums: self.remote_fps.len(),
            events: self.events.len(),
            outbox: self.outbox.len(),
            planner_remote_inputs: planner.map_or(0, |p| p.remote_inputs),
            planner_fingerprints: planner.map_or(0, |p| p.fingerprints),
            planner_remote_fingerprints: planner.map_or(0, |p| p.remote_fingerprints),
            planner_frames: planner.map_or(0, |p| p.frames),
        }
    }

    /// 事件入佇列（有上限，見 [`MAX_QUEUED_EVENTS`]）。
    fn push_event(&mut self, event: Event) {
        let droppable = |e: &Event| {
            matches!(
                e,
                Event::Stats(_) | Event::Stalled { .. } | Event::Resumed { .. }
            )
        };
        if self.events.len() >= MAX_QUEUED_EVENTS {
            if let Some(i) = self.events.iter().position(droppable) {
                self.events.remove(i);
            } else if droppable(&event) {
                return;
            }
        }
        self.events.push_back(event);
    }

    pub fn poll_event(&mut self) -> Option<Event> {
        self.events.pop_front()
    }

    // ---- 輸入 -----------------------------------------------------------------

    fn local_next(&self) -> u32 {
        self.local_base + self.local.len() as u32
    }

    /// 現在該取樣本地輸入了嗎？每推進一幀恰好一次（前 D 幀是預先填好的空輸入）。
    /// **只有 lockstep 用**（rollback 的取樣由 [`Self::advance`] 內部決定）。
    pub fn local_input_wanted(&self) -> bool {
        self.rb.is_none()
            && self.is_running()
            && self.local_next() <= self.next_frame + u32::from(self.input_delay)
    }

    /// 加入這一次取樣的本地按鍵（套用在 `D` 幀之後）。只有 [`Self::local_input_wanted`] 為 `true`
    /// 時才接受（否則忽略並回傳 `false`），所以本地輸入永遠只會領先 `D + 1` 幀，不會因為 stall
    /// 而無限堆積。
    pub fn add_local_input(&mut self, input: impl Into<PlayerInput>) -> bool {
        if !self.local_input_wanted() {
            return false;
        }
        self.local.push_back(input.into());
        self.input_dirty = true;
        true
    }

    /// 雙方在 `next_frame` 的輸入都到齊時回傳這一幀的 [`FrameInput`]（`reset` 是雙方 reset 旗標的 OR，
    /// 所以任一方按 Reset，雙方在同一幀 soft reset）；否則回傳 `None`——**不阻塞**，
    /// 呼叫端下個節拍再試。第一次等不到時開始計算一次 stall，等到了才結束。
    pub fn next_ready_frame(&mut self, now: Duration) -> Option<FrameInput> {
        if !self.is_running() || self.rb.is_some() {
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
                self.push_event(Event::Stalled { frame: f });
            }
            return None;
        };
        if let Some(since) = self.stall_since.take() {
            let waited = now.saturating_sub(since);
            self.stall_time += waited;
            self.push_event(Event::Resumed { frame: f, waited });
        }
        self.next_frame += 1;
        // 已用掉的對方輸入多留 REMOTE_HISTORY 幀：晚到的重複封包可以比對（同一幀不同輸入＝違規）。
        while let Some((&oldest, _)) = self.remote.first_key_value()
            && oldest.saturating_add(REMOTE_HISTORY) < self.next_frame
        {
            self.remote.pop_first();
        }
        self.prune_local();
        Some(PlayerInput::merge(local, remote, self.local_player))
    }

    fn local_input(&self, frame: u32) -> Option<PlayerInput> {
        let idx = frame.checked_sub(self.local_base)?;
        self.local.get(idx as usize).copied()
    }

    fn prune_local(&mut self) {
        // lockstep 還要用 `local[next_frame..]`；rollback 有自己的一份（規劃器），只留尚未被 Ack 的（重送用）。
        let keep_from = if self.rb.is_some() {
            self.local_acked_next
        } else {
            self.local_acked_next.min(self.next_frame)
        };
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
        if local != remote {
            self.raise_desync(frame, local, remote);
        }
    }

    /// 偵測到雙方的行為指紋不同：事件、通知對方、結束。
    fn raise_desync(&mut self, frame: u32, local: u64, remote: u64) {
        self.push_event(Event::Desync {
            frame,
            local: Some(local),
            remote: Some(remote),
        });
        for _ in 0..DISCONNECT_REPEATS {
            self.outbox.push(Msg::Disconnect {
                session_id: self.session_id,
                reason: DisconnectReason::Desync { frame },
                frames_completed: self.frames_completed(),
            });
        }
        self.end(EndReason::Desync { frame });
    }

    /// 對方送來語意錯誤的封包：通知對方（`Disconnect`）、以 [`EndReason::ProtocolViolation`] 結束。
    /// 絕不 panic、不讓狀態被改壞（違規的輸入在到這裡之前已經被拒絕，沒有套用）。
    fn violate(&mut self, violation: Violation) {
        if self.is_ended() {
            return;
        }
        log::warn!("協定違規，中止連線：{violation}");
        for _ in 0..DISCONNECT_REPEATS {
            self.outbox.push(Msg::Disconnect {
                session_id: self.session_id,
                reason: DisconnectReason::Left,
                frames_completed: self.frames_completed(),
            });
        }
        self.end(EndReason::ProtocolViolation(violation));
    }

    /// 對「別的位址」與被拒絕的 Hello 的回覆速率限制。
    fn reply_allowed(&mut self, now: Duration) -> bool {
        if now.saturating_sub(self.reply_window_start) >= Duration::from_secs(1) {
            self.reply_window_start = now;
            self.replies_in_window = 0;
        }
        if self.replies_in_window >= MAX_REPLIES_PER_SEC {
            self.packets_ignored += 1;
            return false;
        }
        self.replies_in_window += 1;
        true
    }

    // ---- rollback ---------------------------------------------------------------

    /// **rollback 模式**：每個幀節拍呼叫一次，回傳這個節拍要在模擬器上執行的請求清單（見 [`crate::rollback`]）。
    /// `keyboard` 是這個節拍的本地按鍵（可帶 reset 旗標），只有真的推進新的一幀時才被取樣。
    /// 不在 rollback 模式、或還沒連上，回傳 [`Outcome::Idle`](crate::rollback::Outcome::Idle)。
    ///
    /// 呼叫端依序執行 `plan.requests`（[`crate::snapshot::execute`]），每個 `SaveState` 之後呼叫
    /// [`Self::state_saved`]，最後呼叫 [`Self::drain_confirmed`]。
    pub fn advance(&mut self, now: Duration, keyboard: impl Into<PlayerInput>) -> Plan {
        if !self.is_running() {
            return Plan::idle();
        }
        let Some(rb) = self.rb.as_mut() else {
            return Plan::idle();
        };
        let plan = rb.advance(now, keyboard.into());
        if let Some(sampled) = plan.sampled {
            // 送給對方的本地輸入序列（與規劃器裡的那份相同）。
            self.local.push_back(sampled);
            self.input_dirty = true;
            if self.unacked() > MAX_UNACKED_INPUTS {
                self.violate(Violation::NotAcking);
            }
        }
        plan
    }

    /// rollback：呼叫端執行完一個 `SaveState { frame }` 之後回報該狀態的行為指紋。
    pub fn state_saved(&mut self, frame: u32, fingerprint: u64) {
        if let Some(rb) = &mut self.rb {
            rb.state_saved(frame, fingerprint);
        }
        self.check_rollback_desync();
    }

    /// rollback：取走新確認的幀（最終的雙方輸入與指紋），用來記錄 replay。
    pub fn drain_confirmed(&mut self) -> Vec<ConfirmedFrame> {
        self.rb
            .as_mut()
            .map(RollbackPlanner::drain_confirmed)
            .unwrap_or_default()
    }

    /// rollback：呼叫端量測一次重跑（還原＋重跑整段請求）的耗時後回報（統計用）。
    pub fn record_resim(&mut self, duration: Duration) {
        if let Some(rb) = &mut self.rb {
            rb.record_resim(duration);
        }
    }

    /// rollback 的規劃器（唯讀，測試與診斷用）。
    pub fn planner(&self) -> Option<&RollbackPlanner> {
        self.rb.as_ref()
    }

    /// 比對對方送來的指紋（只比對本地已確認的幀）；不符就結束。
    fn check_rollback_desync(&mut self) {
        if self.is_ended() {
            return;
        }
        if let Some(d) = self.rb.as_mut().and_then(RollbackPlanner::poll_desync) {
            self.raise_desync(d.frame, d.local, d.remote);
        }
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
                        frames_completed: self.frames_completed(),
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
        self.push_event(Event::Disconnected {
            reason,
            peer_frames: self.peer_frames,
        });
    }

    // ---- poll -----------------------------------------------------------------

    /// 收封包、處理、送封包、重送、逾時、握手、統計。每個節拍呼叫一次（越頻繁越即時；可以每毫秒）。
    pub fn poll<T: Transport + ?Sized>(&mut self, now: Duration, transport: &mut T) {
        let started = *self.started_at.get_or_insert(now);
        self.last_now = now;
        for (i, datagram) in transport.recv(now).into_iter().enumerate() {
            if i >= MAX_DATAGRAMS_PER_POLL {
                // 封包洪流：這一輪只處理前 N 個，其餘丟棄（UDP 本來就不保證送達）。
                self.packets_ignored += 1;
                continue;
            }
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
                        frames_completed: self.frames_completed(),
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
        self.check_rollback_desync();
        if self.is_ended() {
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
            if let Some(rb) = self.rb.as_mut() {
                self.window_max_depth = rb.take_window_max_depth();
                let total = rb.stats().rollbacks;
                self.rollbacks_per_sec =
                    (total - self.rollbacks_at_last_stats) as f32 / elapsed as f32;
                self.rollbacks_at_last_stats = total;
            }
            self.window_start = now;
            self.window_sent = 0;
            self.window_received = 0;
            self.next_stats = now + STATS_INTERVAL;
            self.push_event(Event::Stats(self.stats()));
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
        let inputs: Vec<PlayerInput> = self
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
        // 時間同步與指紋資訊（rollback）：每個 Input 封包都帶，所以丟包只會延後、不會跳過。
        let (sender_frame, frame_advantage, confirmed) = match &self.rb {
            Some(rb) => (
                rb.current_frame(),
                rb.advantage_to_send(),
                rb.confirmed_fingerprint(),
            ),
            None => (self.next_frame, 0, None),
        };
        let msg = Msg::Input {
            session_id: self.session_id,
            start_frame: start,
            inputs,
            sender_frame,
            frame_advantage,
            confirmed,
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
        self.send_with_version(now, transport, to, msg, PROTOCOL_VERSION);
    }

    /// 同 [`Self::send`]，但標頭寫指定的協定版本（只用來讓舊版 Client 讀懂 `Reject`）。
    fn send_with_version<T: Transport + ?Sized>(
        &mut self,
        now: Duration,
        transport: &mut T,
        to: Option<SocketAddr>,
        msg: &Msg,
        header_version: u16,
    ) {
        let Ok(bytes) = msg.encode_with_version(header_version) else {
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
        let msg = match Msg::decode_lenient(&datagram.data) {
            Ok(msg) => msg,
            Err(ProtocolError::UnsupportedVersion(theirs)) => {
                let ours = self.cfg.protocol_version;
                match self.state {
                    // 對方用不同版本的協定打招呼：Host 還在等人時，明確告訴它為什麼不行。
                    // 對 v1（Phase 4b）的 Client 用 v1 的標頭回覆：`Reject` 的本體佈局兩版相同，舊版程式才解得出並顯示原因。
                    State::Listening if !datagram.stranger => {
                        if !self.reply_allowed(now) {
                            return;
                        }
                        let reject = Msg::Reject {
                            reason: RejectReason::ProtocolVersion {
                                host: ours,
                                client: theirs,
                            },
                        };
                        let header = if theirs == LEGACY_PROTOCOL_VERSION {
                            theirs
                        } else {
                            ours
                        };
                        self.send_with_version(now, transport, datagram.from, &reject, header);
                    }
                    // Client：房主回了一個「別的版本」的（Reject）封包（例如 v1 的房主拒絕 v2 的我們，
                    // 用它自己的標頭回覆）。任何版本的 NESN 封包都代表對方的協定與我們不同。
                    State::Connecting if !datagram.stranger => {
                        self.end(EndReason::Rejected(RejectReason::ProtocolVersion {
                            host: theirs,
                            client: ours,
                        }));
                    }
                    _ => {}
                }
                return;
            }
            Err(_) => {
                // 雜訊、截斷、超過大小：丟棄
                self.packets_ignored += 1;
                return;
            }
        };

        if datagram.stranger {
            // 已經有對手了：別的位址來敲門，只回覆「房間已滿」，其餘一律忽略。
            self.packets_ignored += 1;
            if matches!(msg, Msg::Hello { .. })
                && matches!(self.state, State::Running | State::Closing { .. })
                && self.reply_allowed(now)
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
                mode,
            } => self.on_accept(now, session_id, input_delay, player, mode),
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
            // Client 不該收到 Hello：握手完成後才收到＝違規；握手前忽略。
            if matches!(self.state, State::Running) {
                self.violate(Violation::HandshakeAfterRunning { what: "Hello" });
            }
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
                    // 拒絕之後仍然等別人：不鎖定這個位址（回覆有速率限制）。
                    if self.reply_allowed(now) {
                        self.send(now, transport, from, &Msg::Reject { reason });
                    }
                    return;
                }
                self.peer_hello = Some((protocol_version, core_behavior_version, rom_id));
                if let Some(addr) = from {
                    transport.set_peer(addr);
                }
                self.begin_running(now, 0, self.cfg.mode, self.cfg.input_delay);
                self.send_accept(now, transport, from);
            }
            // Accept 掉了，Client 又送 Hello：寬限期內、內容與原本相同才重送同一份 Accept（冪等）。
            // 內容不同（換了 ROM／版本？）或超過寬限期＝握手完成後不該再出現的 Hello，違規。
            State::Running => {
                let same =
                    self.peer_hello == Some((protocol_version, core_behavior_version, rom_id));
                if same && now.saturating_sub(self.running_since) <= HELLO_GRACE {
                    self.send_accept(now, transport, from);
                } else {
                    self.violate(Violation::HandshakeAfterRunning { what: "Hello" });
                }
            }
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
            mode: self.mode,
        };
        self.send(now, transport, to, &accept);
    }

    fn on_accept(
        &mut self,
        now: Duration,
        session_id: u32,
        input_delay: u8,
        player: u8,
        mode: Mode,
    ) {
        match self.state {
            State::Running => {
                // 房主每收到一個 Hello 就回一份 Accept，所以 Client 連上之後還會收到「內容相同」的重複 Accept
                // （Hello 與 Accept 在網路上重疊）：忽略。內容不同、或自己是 Host 卻收到 Accept＝違規。
                if self.cfg.role == Role::Client
                    && self.accepted == Some((session_id, input_delay, player, mode))
                {
                    self.packets_ignored += 1;
                } else {
                    self.violate(Violation::HandshakeAfterRunning { what: "Accept" });
                }
                return;
            }
            State::Connecting => {}
            _ => return,
        }
        if player > 1 || input_delay > MAX_INPUT_DELAY {
            return;
        }
        self.accepted = Some((session_id, input_delay, player, mode));
        self.session_id = session_id;
        // lockstep：雙方共用 Host 決定的 D。rollback：各自決定本地輸入延遲（用自己的設定）。
        let delay = match mode {
            Mode::Lockstep => input_delay,
            Mode::Rollback => self.cfg.input_delay,
        };
        self.begin_running(now, player, mode, delay);
    }

    /// 握手完成：進入 Running。前 `D` 幀的本地輸入是空的（lockstep：協定約定，雙方都知道 D；
    /// rollback：空輸入也照常送給對方，對方不必猜）。
    fn begin_running(&mut self, now: Duration, player: u8, mode: Mode, input_delay: u8) {
        self.state = State::Running;
        self.running_since = now;
        self.mode = mode;
        self.input_delay = local_delay_for(mode, input_delay);
        if mode == Mode::Rollback {
            self.rb = Some(RollbackPlanner::new(RollbackConfig {
                window: self.cfg.window,
                input_delay: self.input_delay,
                local_player: player,
                time_sync: self.cfg.time_sync,
                sabotage: self.cfg.sabotage,
            }));
        }
        self.local_player = player;
        self.last_recv = now;
        self.next_ping = now;
        self.window_start = now;
        self.next_stats = now + STATS_INTERVAL;
        self.local.extend(std::iter::repeat_n(
            PlayerInput::NONE,
            usize::from(self.input_delay),
        ));
        // 預填的輸入也要送給對方（對方要靠封包才知道我方前 D 幀是空的），連上就立刻送。
        self.input_dirty = !self.local.is_empty();
        self.push_event(Event::Connected {
            player,
            input_delay: self.input_delay,
            mode,
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
                sender_frame,
                frame_advantage,
                confirmed,
                ..
            } => self.on_input(
                now,
                transport,
                start_frame,
                &inputs,
                (sender_frame, frame_advantage, confirmed),
            ),
            Msg::Ack { frame, .. } => {
                // 合法的對方只會確認我們送過的幀。
                if frame >= self.local_next() {
                    self.violate(Violation::AckBeyondSent { frame });
                    return;
                }
                self.local_acked_next = self.local_acked_next.max(frame + 1);
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
                // 我們只會送出「過去的」時間戳：來自未來、或大得離譜的往返時間不可信，不更新 RTT。
                let now_us = now.as_micros() as u64;
                let sample = now_us.saturating_sub(timestamp_us);
                if timestamp_us > now_us || u128::from(sample) > MAX_PLAUSIBLE_RTT.as_micros() {
                    self.packets_ignored += 1;
                    return;
                }
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
                            frames_completed: self.frames_completed(),
                        };
                        for _ in 0..DISCONNECT_REPEATS {
                            self.send(now, transport, None, &reply);
                        }
                        self.end(EndReason::PeerLeft);
                    }
                    DisconnectReason::Desync { frame } => {
                        self.push_event(Event::Desync {
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

    /// lockstep 的對方輸入表（`remote`：含已用掉但仍保留歷史的幀）。語意同 [`RollbackPlanner::on_remote_input`]。
    fn lockstep_remote_input(&mut self, frame: u32, input: PlayerInput) -> RemoteInput {
        if frame >= self.remote_contig && frame - self.remote_contig >= MAX_REMOTE_AHEAD {
            return RemoteInput::TooFar;
        }
        if let Some(&held) = self.remote.get(&frame) {
            return if held == input {
                RemoteInput::Duplicate
            } else {
                RemoteInput::Conflict {
                    held,
                    received: input,
                }
            };
        }
        if frame < self.remote_contig {
            return RemoteInput::Stale;
        }
        self.remote.insert(frame, input);
        while self.remote.contains_key(&self.remote_contig) {
            self.remote_contig += 1;
        }
        RemoteInput::Accepted
    }

    fn on_input<T: Transport + ?Sized>(
        &mut self,
        now: Duration,
        transport: &mut T,
        start_frame: u32,
        inputs: &[PlayerInput],
        sync: (u32, i8, Option<ConfirmedFingerprint>),
    ) {
        // ---- 語意檢查（違規的輸入不會被套用）----
        if inputs.len() > MAX_INPUTS_PER_PACKET {
            self.violate(Violation::TooManyInputs {
                count: inputs.len(),
            });
            return;
        }
        let our_frame = self
            .rb
            .as_ref()
            .map_or(self.next_frame, RollbackPlanner::current_frame);
        if sync.0 > our_frame.saturating_add(MAX_SENDER_LEAD) {
            self.violate(Violation::SenderFrameTooFar {
                sender_frame: sync.0,
            });
            return;
        }
        for (i, &input) in inputs.iter().enumerate() {
            let Some(frame) = start_frame.checked_add(i as u32) else {
                self.violate(Violation::InputFrameTooFar { frame: start_frame });
                return;
            };
            let verdict = match self.rb.as_mut() {
                Some(rb) => rb.on_remote_input(frame, input),
                None => self.lockstep_remote_input(frame, input),
            };
            match verdict {
                RemoteInput::Accepted | RemoteInput::Duplicate => {}
                RemoteInput::Stale => self.packets_ignored += 1,
                RemoteInput::TooFar => {
                    self.violate(Violation::InputFrameTooFar { frame });
                    return;
                }
                RemoteInput::Conflict { .. } => {
                    self.violate(Violation::ConflictingInput { frame });
                    return;
                }
            }
        }
        if let Some(rb) = self.rb.as_mut() {
            // rollback：時間同步與指紋。
            let rtt = self.srtt_us.map(Duration::from_micros);
            let (sender_frame, advantage, confirmed) = sync;
            rb.on_remote_sync(sender_frame, advantage, rtt);
            if let Some(fp) = confirmed {
                rb.on_remote_fingerprint(fp);
            }
            self.remote_contig = rb.remote_contiguous();
            self.check_rollback_desync();
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

/// 各模式允許的本地輸入延遲上限。
fn local_delay_for(mode: Mode, requested: u8) -> u8 {
    match mode {
        Mode::Lockstep => requested.min(MAX_INPUT_DELAY),
        Mode::Rollback => requested.min(rollback::MAX_INPUT_DELAY),
    }
}

/// 只保留最新的 [`MAX_PENDING_CHECKSUMS`] 個。
fn trim_oldest(map: &mut BTreeMap<u32, u64>) {
    while map.len() > MAX_PENDING_CHECKSUMS {
        map.pop_first();
    }
}
