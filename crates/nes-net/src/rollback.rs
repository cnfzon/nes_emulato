//! Rollback 的規劃器（Phase 4c）：決定「接下來該對模擬器做什麼」，自己**不擁有 `Nes`、不做 I/O、不讀系統時間**。
//!
//! # 為什麼是「請求清單」
//!
//! 規劃器每個幀節拍回傳一份 [`Plan`]：一串 [`Request`]（`SaveState`、`LoadState`、`AdvanceFrame`），由呼叫端
//! （`nes-app` 的 emu 執行緒、`nes-test netsim`、測試）依序在自己的 `Nes` 上執行（見 [`crate::snapshot::execute`]）。
//! 這樣（1）規劃器可以**完全不需要 `Nes`** 單獨測試（測試裡用一個假的「狀態雜湊鏈」當模擬器）；（2）`nes-net` 維持
//! 「不依賴執行緒／計時器／GUI」，與 `nes-core` 的「外部驅動」哲學一致（`docs/architecture.md` §4）；（3）真正昂貴
//! 的工作（重跑幾幀）發生在 emu 執行緒，由它決定何時執行、量測耗時。
//!
//! # 幀號約定（與 replay、協定一致）
//!
//! - **輸入的幀號 `f`**（0 起算）＝第 `f + 1` 次 `run_frame` 使用的輸入。
//! - **狀態 `S_n`** ＝ 已完成 `n` 幀之後的模擬狀態（`S_0` ＝ 開機）。`AdvanceFrame { frame: f }` 把 `S_f` 推進成 `S_{f+1}`。
//! - **`SaveState { frame: n }`** ＝ 把「目前狀態」存成 `S_n`（緊接在產生 `S_n` 的 `AdvanceFrame` 之後，開機時是 `S_0`）；
//!   **`LoadState { frame: n }`** ＝ 還原成先前存的 `S_n`。
//!
//! # 兩個幀號
//!
//! - **目前幀 `cur`**：目前模擬到哪（狀態是 `S_cur`），可能包含用預測輸入模擬出來的幀。
//! - **已確認幀 `C`**：`C ≤ cur`，第 `0..C` 幀的雙方輸入都是真實的、而且模擬時用的就是真實輸入；`S_C` 不會再被改變，
//!   之前的快照可以丟棄。不變式：`cur − C ≤ K`（預測視窗），所以快照環形緩衝只需要 `K + 1` 個狀態（`S_C ..= S_cur`）。
//!
//! # 演算法（每個幀節拍一次 [`RollbackPlanner::advance`]）
//!
//! 1. **比對**：對 `[C, cur)` 每一幀，若當時是用預測輸入模擬的、而現在真實輸入到了：相同 → 預測正確；不同 → 預測錯誤。
//!    找出最早的預測錯誤幀 `F`。
//! 2. **還原並重跑**：有 `F` 就產生 `LoadState(F)`，接著對 `F..cur` 每一幀 `AdvanceFrame`（**關閉輸出**）＋`SaveState`，
//!    輸入用「現在知道的」：真實輸入已到就用真實的，否則重新預測。
//! 3. **確認**：`C` 前進到第一個「真實輸入還沒到」的幀（或 `cur`）。
//! 4. **推進新的一幀**：若 `cur − C ≥ K` → 暫停（[`Outcome::Stalled`]，退化成 lockstep 的等待）；若時間同步要求放慢
//!    → [`Outcome::Held`]（這個節拍不推進，呼叫端仍消耗掉一幀的時間）；否則取樣本地輸入、用（預測的）對方輸入
//!    `AdvanceFrame`（**開啟輸出**，只有這一幀開啟）＋`SaveState`。
//!
//! **預測**：對方在第 `f` 幀的輸入＝「最後一個已確認（連續收到）的輸入」的按鍵；**reset 旗標永遠預測為 `false`**
//! （不預測會讓雙方重置的事件）。
//!
//! # 如何追蹤「每個快照是否完全由已確認輸入產生」（desync 偵測）
//!
//! 每次 `SaveState { frame: n }` 執行之後，呼叫端回報該狀態的行為指紋（[`RollbackPlanner::state_saved`]），規劃器把它記在
//! `fps[n]`。**指紋只在 `n ≤ C`（該狀態已確認）時才會被送出或拿來比對**：`n ≤ C` 代表第 `0..n` 幀模擬時用的輸入都已經
//! 與真實輸入逐幀比對相同（預測正確，或已經還原重跑成真實輸入）。預測輸入產生、尚未確認的狀態（`n > C`）不會外流；
//! 預測錯誤時 `n > F` 的指紋全部作廢（`fps.retain(n ≤ F)`），重跑後重新回報。所以丟包、延遲、預測失誤都不會產生假的 desync
//! （測試：`nes-net/tests/rollback_equivalence.rs`，含「對預測幀算指紋」的破壞性測試，它**會**產生假 desync）。
//!
//! # 時間同步
//!
//! 雙方在每個 `Input` 封包交換「幀數優勢」＝ 本地目前幀 − 依 RTT 推估的對方目前幀（`對方封包標示的幀 + RTT/2 × 60.0988`）。
//! 對最近 [`SYNC_WINDOW`] 個樣本取平均，建議放慢的幀數 ＝ `(本地優勢 − 對方回報的優勢) / 2`（GGPO 的作法：雙方各自的估計有雜訊，
//! 取兩者之差的一半）。達到 [`HOLD_THRESHOLD`] 幀就「多等一幀」（[`Outcome::Held`]），並把視窗內的樣本依這一幀修正
//! （本地優勢 −1、對方優勢 +1），避免視窗還沒更新時連續過度放慢；兩次放慢之間至少隔 [`HOLD_COOLDOWN`] 幀。
//! 放慢只影響時間，**不影響模擬結果**（結果只由輸入決定），所以浮點數只出現在這裡（`nes-core` 的模擬狀態仍然只用整數）。
//!
//! # 決定性與正確性
//!
//! 交給 `run_frame` 的每一幀輸入，最終一定是雙方的真實輸入（預測的都會在真實輸入到達時被還原重跑）；模擬結果因此只由雙方輸入決定，
//! 與封包到達的順序、時間、丟失、重複、預測是否失誤、重跑次數都無關。驗證見 `tests/rollback_equivalence.rs`。

use std::collections::{BTreeMap, VecDeque};
use std::time::Duration;

use nes_core::FrameInput;

use crate::protocol::{ConfirmedFingerprint, PlayerInput};

/// NTSC 的畫面更新率。
pub const NTSC_FPS: f64 = 60.0988;
/// 預設的預測視窗 K（目前幀最多領先已確認幀多少幀）。
pub const DEFAULT_WINDOW: u32 = 8;
pub const MAX_WINDOW: u32 = 32;
/// rollback 的本地輸入延遲：預設 1，可設定 0–4（每一方自己決定，不必一致）。
pub const DEFAULT_INPUT_DELAY: u8 = 1;
pub const MAX_INPUT_DELAY: u8 = 4;
/// 對方的輸入最多可以領先「已連續收到的幀」多少幀（防止惡意封包讓記憶體無限成長）。
const MAX_REMOTE_AHEAD: u32 = 256;
/// 已用掉（drain）的對方輸入多保留幾幀：晚到的重複封包可以拿來比對，「同一幀收到不同的輸入」就抓得到
/// （Phase 4d）。超過這個歷史的舊封包無法比對，只能忽略（安全，但不算偵測）。
pub const REMOTE_HISTORY: u32 = 256;
/// 已確認的指紋至少保留多少幀（比對對方稍晚才到的指紋）。
const FP_HISTORY: u32 = 128;
/// 最多保留幾個尚未比對的對方指紋。
const MAX_PENDING_REMOTE_FPS: usize = 64;
/// 時間同步：平均幾個樣本、至少幾個樣本才動作。
pub const SYNC_WINDOW: usize = 40;
const SYNC_MIN_SAMPLES: usize = 8;
/// 建議放慢達到幾幀才「多等一幀」。
pub const HOLD_THRESHOLD: f64 = 1.0;
/// 兩次放慢之間至少隔幾幀。
pub const HOLD_COOLDOWN: u32 = 4;

/// 故意做錯的破壞性測試開關（**只給測試用**；正式路徑永遠是 `None`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Sabotage {
    #[default]
    None,
    /// 還原到 `F + 1` 而不是 `F`（差 1）。
    LoadOneFrameLate,
    /// 偵測到預測錯誤，但不執行還原（當作沒事）。
    SkipRollback,
    /// 對用預測輸入模擬出來的幀（尚未確認）也送出／比對指紋。
    PublishUnconfirmedFingerprint,
}

#[derive(Debug, Clone, Copy)]
pub struct RollbackConfig {
    /// 預測視窗 K（1..=[`MAX_WINDOW`]）。
    pub window: u32,
    /// 本地輸入延遲（0..=[`MAX_INPUT_DELAY`]）。
    pub input_delay: u8,
    /// 本地玩家的位置（0＝玩家 1）。
    pub local_player: u8,
    /// 是否啟用時間同步（關閉只用於比較實驗）。
    pub time_sync: bool,
    pub sabotage: Sabotage,
}

impl RollbackConfig {
    pub fn new(local_player: u8) -> Self {
        Self {
            window: DEFAULT_WINDOW,
            input_delay: DEFAULT_INPUT_DELAY,
            local_player,
            time_sync: true,
            sabotage: Sabotage::None,
        }
    }

    fn normalized(mut self) -> Self {
        self.window = self.window.clamp(1, MAX_WINDOW);
        self.input_delay = self.input_delay.min(MAX_INPUT_DELAY);
        self.local_player = self.local_player.min(1);
        self
    }
}

/// 規劃器要求呼叫端在模擬器上執行的一步。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Request {
    /// 把目前狀態存成 `S_frame`（存完之後呼叫 [`RollbackPlanner::state_saved`] 回報行為指紋）。
    SaveState { frame: u32 },
    /// 還原成先前存的 `S_frame`。
    LoadState { frame: u32 },
    /// 用 `input` 推進一幀（`S_frame → S_{frame+1}`）。`output_enabled` 為 `false` 時不產生畫面與音訊
    /// （重跑的幀）；只有真正新推進的那一幀為 `true`。
    AdvanceFrame {
        frame: u32,
        input: FrameInput,
        output_enabled: bool,
    },
}

/// 這個節拍的結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// 推進了新的一幀（請求清單的最後是它的 `AdvanceFrame`＋`SaveState`，輸出已開啟）。
    Advanced,
    /// 預測視窗已滿（目前幀領先已確認幀 K 幀）：這個節拍不推進，呼叫端下個迴圈再試（退化成 lockstep 的等待）。
    Stalled,
    /// 時間同步要求放慢：這個節拍不推進，但呼叫端**消耗掉這一幀的時間**（不重試）。
    Held,
    /// 還沒開始（session 沒在執行）。
    Idle,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// 依序執行。即使沒有推進新的一幀，也可能有還原＋重跑（輸出全部關閉）。
    pub requests: Vec<Request>,
    pub outcome: Outcome,
    /// 這個節拍取樣了本地輸入（套用在 `cur + input_delay` 幀）：session 要把它加進送給對方的輸入序列。
    pub sampled: Option<PlayerInput>,
}

impl Plan {
    pub fn idle() -> Self {
        Self {
            requests: Vec::new(),
            outcome: Outcome::Idle,
            sampled: None,
        }
    }
}

/// 一個剛被確認的幀：最終的雙方輸入，以及該幀結束後的行為指紋（`S_{frame+1}`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConfirmedFrame {
    pub frame: u32,
    pub input: FrameInput,
    pub fingerprint: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DesyncInfo {
    /// 已完成的幀數（`S_frame`）。
    pub frame: u32,
    pub local: u64,
    pub remote: u64,
}

/// 規劃器內部佇列的大小（[`RollbackPlanner::buffer_sizes`]）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PlannerBufferSizes {
    pub remote_inputs: usize,
    pub fingerprints: usize,
    pub remote_fingerprints: usize,
    pub frames: usize,
}

/// [`RollbackPlanner::on_remote_input`] 對一筆對方輸入的處理結果（Phase 4d：語意檢查）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteInput {
    /// 第一次收到這一幀，已收下。
    Accepted,
    /// 這一幀已收過、內容相同（冗餘傳送與重複封包的正常情況），忽略。
    Duplicate,
    /// 這一幀已收過、**內容不同**：真實輸入一經收下就不能改變，對方不是有問題的程式就是惡意封包。
    /// 沒有覆蓋、沒有影響模擬；呼叫端應以協定違規中止連線。
    Conflict {
        held: PlayerInput,
        received: PlayerInput,
    },
    /// 比保留的歷史還舊（無法比對），忽略。
    Stale,
    /// 領先「連續收到的幀」太遠（合法的對方不會送），沒有收下。
    TooFar,
}

/// rollback 的統計（`Stats::rollback`）。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct RollbackStats {
    pub current_frame: u32,
    pub confirmed_frame: u32,
    /// 累計 rollback 次數。
    pub rollbacks: u32,
    /// 最近一個統計視窗（約 1 秒）的每秒 rollback 次數（由 session 填）。
    pub rollbacks_per_sec: f32,
    /// 累計重跑的幀數。
    pub resim_frames: u64,
    pub avg_depth: f32,
    pub max_depth: u32,
    /// 每次重跑（含還原）的耗時，由呼叫端量測後回報（[`RollbackPlanner::record_resim`]）。
    pub resim_time_avg: Duration,
    pub resim_time_max: Duration,
    /// 累計的重跑耗時與已量測次數（CSV 記錄器用差值算每秒的平均）。
    pub resim_time_total: Duration,
    pub resims_timed: u32,
    /// 最近一個統計視窗（約 1 秒）內最深的一次重跑（由 session 填）。
    pub window_max_depth: u32,
    /// 本地的幀數優勢（平均；正數＝領先）與對方回報的優勢。
    pub frame_advantage: f32,
    pub remote_advantage: f32,
    /// 用預測輸入模擬過的幀數，以及其中預測正確／錯誤的數量（只計「已經知道真實輸入」的幀）。
    pub predicted_frames: u64,
    pub prediction_correct: u64,
    pub prediction_wrong: u64,
    /// 時間同步放慢（多等一幀）的次數。
    pub holds: u32,
    pub stalls: u32,
    pub stall_time: Duration,
}

impl RollbackStats {
    /// 預測準確率：預測正確 ÷（正確＋錯誤）。還沒有任何被驗證過的預測時為 `None`。
    pub fn prediction_accuracy(&self) -> Option<f32> {
        let total = self.prediction_correct + self.prediction_wrong;
        (total > 0).then(|| self.prediction_correct as f32 / total as f32)
    }
}

#[derive(Debug, Clone, Copy)]
struct Rec {
    /// 模擬這一幀時對方的輸入（真實或預測）。
    used: PlayerInput,
    predicted: bool,
}

#[derive(Debug)]
pub struct RollbackPlanner {
    cfg: RollbackConfig,
    started: bool,
    cur: u32,
    confirmed: u32,

    /// 本地輸入：`local[i]` 是第 `local_base + i` 幀（含開頭預先填好的 `input_delay` 個空輸入）。
    local: VecDeque<PlayerInput>,
    local_base: u32,
    /// 對方的真實輸入（第 `drained` 幀起）。
    remote: BTreeMap<u32, PlayerInput>,
    /// 從第 0 幀起連續收到到哪（第 `< remote_contig` 幀都收到了）。
    remote_contig: u32,
    /// 最後一個連續收到的對方輸入（預測用）。
    last_real: PlayerInput,
    /// `frames[i]` 是第 `confirmed + i` 幀的紀錄，長度 ＝ `cur − confirmed`。
    frames: VecDeque<Rec>,

    /// `fps[n]` ＝ 目前時間軸上 `S_n` 的行為指紋（只有 `n ≤ confirmed` 的才對外）。
    fps: BTreeMap<u32, u64>,
    /// 對方送來、本地還沒確認到那一幀的指紋。
    remote_fps: BTreeMap<u32, u64>,
    /// 已經交給 [`Self::drain_confirmed`] 的幀數。
    drained: u32,

    // ---- 時間同步 ----
    local_adv: VecDeque<f64>,
    remote_adv: VecDeque<f64>,
    last_sender_frame: Option<u32>,
    frames_since_hold: u32,

    // ---- stall 與統計 ----
    stall_since: Option<Duration>,
    stalls: u32,
    stall_time: Duration,
    holds: u32,
    rollbacks: u32,
    depth_sum: u64,
    max_depth: u32,
    /// 自上次 [`RollbackPlanner::take_window_max_depth`] 以來最深的一次重跑。
    window_max_depth: u32,
    resims_timed: u32,
    resim_time_sum: Duration,
    resim_time_max: Duration,
    predicted_total: u64,
    prediction_correct: u64,
    prediction_wrong: u64,
}

impl RollbackPlanner {
    pub fn new(cfg: RollbackConfig) -> Self {
        let cfg = cfg.normalized();
        Self {
            started: false,
            cur: 0,
            confirmed: 0,
            // 前 `input_delay` 幀沒有任何取樣可用，本地輸入是空的（並且會送給對方，對方不必猜）。
            local: std::iter::repeat_n(PlayerInput::NONE, usize::from(cfg.input_delay)).collect(),
            local_base: 0,
            remote: BTreeMap::new(),
            remote_contig: 0,
            last_real: PlayerInput::NONE,
            frames: VecDeque::new(),
            fps: BTreeMap::new(),
            remote_fps: BTreeMap::new(),
            drained: 0,
            local_adv: VecDeque::new(),
            remote_adv: VecDeque::new(),
            last_sender_frame: None,
            frames_since_hold: HOLD_COOLDOWN,
            stall_since: None,
            stalls: 0,
            stall_time: Duration::ZERO,
            holds: 0,
            rollbacks: 0,
            depth_sum: 0,
            max_depth: 0,
            window_max_depth: 0,
            resims_timed: 0,
            resim_time_sum: Duration::ZERO,
            resim_time_max: Duration::ZERO,
            predicted_total: 0,
            prediction_correct: 0,
            prediction_wrong: 0,
            cfg,
        }
    }

    // ---- 查詢 -----------------------------------------------------------------

    pub fn config(&self) -> &RollbackConfig {
        &self.cfg
    }

    /// 目前幀：狀態是 `S_cur`（可能含預測輸入）。
    pub fn current_frame(&self) -> u32 {
        self.cur
    }

    /// 已確認幀：`S_C` 之前的輸入都是真實的、不會再被改變。
    pub fn confirmed_frame(&self) -> u32 {
        self.confirmed
    }

    /// 從第 0 幀起連續收到對方輸入到哪（session 用它回 `Ack`）。
    pub fn remote_contiguous(&self) -> u32 {
        self.remote_contig
    }

    /// 本地已取樣到的幀（下一個要取樣的幀號）。
    pub fn local_next(&self) -> u32 {
        self.local_base + self.local.len() as u32
    }

    // ---- 輸入 -----------------------------------------------------------------

    /// 收到對方在第 `frame` 幀的真實輸入。輸入一經收下不會被覆蓋：重複的（內容相同）回傳
    /// [`RemoteInput::Duplicate`]；**內容不同**回傳 [`RemoteInput::Conflict`]（不覆蓋）；領先「連續收到」太多的
    /// 不收（[`RemoteInput::TooFar`]，記憶體有上限）。已用掉的幀仍保留 [`REMOTE_HISTORY`] 幀供比對。
    pub fn on_remote_input(&mut self, frame: u32, input: PlayerInput) -> RemoteInput {
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
        while let Some(&real) = self.remote.get(&self.remote_contig) {
            self.last_real = real;
            self.remote_contig += 1;
        }
        RemoteInput::Accepted
    }

    /// 對方最新的已確認指紋（每個 `Input` 封包都帶）。
    pub fn on_remote_fingerprint(&mut self, fp: ConfirmedFingerprint) {
        if self.remote_fps.insert(fp.frame, fp.fingerprint).is_none() {
            while self.remote_fps.len() > MAX_PENDING_REMOTE_FPS {
                self.remote_fps.pop_first();
            }
        }
    }

    /// 對方封包裡的時間同步資訊。`rtt` 是目前平滑的往返時間（還沒量到就先不動作）。
    pub fn on_remote_sync(&mut self, sender_frame: u32, peer_advantage: i8, rtt: Option<Duration>) {
        // 舊的（亂序、重送）封包的幀號不會比已收過的新：忽略，避免拿舊時間戳算優勢。
        if self
            .last_sender_frame
            .is_some_and(|last| sender_frame <= last)
        {
            return;
        }
        self.last_sender_frame = Some(sender_frame);
        let Some(rtt) = rtt else { return };
        let one_way_frames = rtt.as_secs_f64() / 2.0 * NTSC_FPS;
        let advantage = f64::from(self.cur) - (f64::from(sender_frame) + one_way_frames);
        push_sample(&mut self.local_adv, advantage);
        push_sample(&mut self.remote_adv, f64::from(peer_advantage));
    }

    /// 本地的幀數優勢（平均，四捨五入夾在 `i8`），放進送出的 `Input` 封包。
    pub fn advantage_to_send(&self) -> i8 {
        average(&self.local_adv).round().clamp(-127.0, 127.0) as i8
    }

    /// 呼叫端量測一次重跑（還原＋重跑整段請求）的耗時後回報。
    pub fn record_resim(&mut self, duration: Duration) {
        self.resims_timed += 1;
        self.resim_time_sum += duration;
        self.resim_time_max = self.resim_time_max.max(duration);
    }

    // ---- 指紋 -----------------------------------------------------------------

    /// 呼叫端執行完 `SaveState { frame }` 之後回報該狀態的行為指紋。
    pub fn state_saved(&mut self, frame: u32, fingerprint: u64) {
        self.fps.insert(frame, fingerprint);
    }

    /// 上限：只有 `≤ 這個幀` 的指紋可以對外／比對。正式路徑是已確認幀；破壞性測試改成目前幀（預測幀）。
    fn fingerprint_limit(&self) -> u32 {
        match self.cfg.sabotage {
            Sabotage::PublishUnconfirmedFingerprint => self.cur,
            _ => self.confirmed,
        }
    }

    /// 要放進 `Input` 封包的指紋：最新一個已確認、且已經算出指紋的幀。
    pub fn confirmed_fingerprint(&self) -> Option<ConfirmedFingerprint> {
        self.fps
            .range(..=self.fingerprint_limit())
            .next_back()
            .map(|(&frame, &fingerprint)| ConfirmedFingerprint { frame, fingerprint })
    }

    /// 比對「對方送來的指紋」與本地同一幀的指紋。本地還沒確認到的先留著（之後再比）；
    /// 比較久以前已被丟棄的直接忽略。第一個不符的回傳 `Some`。
    pub fn poll_desync(&mut self) -> Option<DesyncInfo> {
        let ready: Vec<u32> = self
            .remote_fps
            .range(..=self.fingerprint_limit())
            .map(|(&n, _)| n)
            .collect();
        for n in ready {
            if let Some(remote) = self.remote_fps.remove(&n)
                && let Some(&local) = self.fps.get(&n)
                && local != remote
            {
                return Some(DesyncInfo {
                    frame: n,
                    local,
                    remote,
                });
            }
        }
        None
    }

    /// 取走新確認的幀（最終的雙方輸入與指紋），依幀號排序。**呼叫端必須在每次執行完請求之後呼叫**
    /// （它同時釋放已用完的輸入與指紋）。指紋還沒回報的幀留到下一次。
    pub fn drain_confirmed(&mut self) -> Vec<ConfirmedFrame> {
        let mut out = Vec::new();
        while self.drained < self.confirmed {
            let f = self.drained;
            let (Some(&fingerprint), Some(&remote)) = (self.fps.get(&(f + 1)), self.remote.get(&f))
            else {
                break;
            };
            let input = PlayerInput::merge(self.local_at(f), remote, self.cfg.local_player);
            out.push(ConfirmedFrame {
                frame: f,
                input,
                fingerprint,
            });
            self.drained += 1;
        }
        while self.local_base < self.drained && self.local.pop_front().is_some() {
            self.local_base += 1;
        }
        self.remote = self
            .remote
            .split_off(&self.drained.saturating_sub(REMOTE_HISTORY));
        let keep_from = self.drained.min(self.confirmed.saturating_sub(FP_HISTORY));
        self.fps = self.fps.split_off(&keep_from);
        out
    }

    // ---- 規劃 -----------------------------------------------------------------

    fn local_at(&self, frame: u32) -> PlayerInput {
        frame
            .checked_sub(self.local_base)
            .and_then(|i| self.local.get(i as usize))
            .copied()
            .unwrap_or_else(|| {
                debug_assert!(false, "本地輸入缺第 {frame} 幀");
                PlayerInput::NONE
            })
    }

    /// 預測：最後一個已確認的輸入的按鍵；reset 永遠預測為 `false`。
    fn predict(&self) -> PlayerInput {
        PlayerInput::new(self.last_real.buttons, false)
    }

    /// 第 `f` 幀現在能用的最佳輸入：`(合併後的輸入, 是否預測, 對方的輸入)`。
    fn sim_input(&self, f: u32) -> (FrameInput, bool, PlayerInput) {
        let (remote, predicted) = match self.remote.get(&f) {
            Some(&real) => (real, false),
            None => (self.predict(), true),
        };
        (
            PlayerInput::merge(self.local_at(f), remote, self.cfg.local_player),
            predicted,
            remote,
        )
    }

    fn rec(&mut self, frame: u32) -> &mut Rec {
        let i = (frame - self.confirmed) as usize;
        &mut self.frames[i]
    }

    /// 每個幀節拍呼叫一次。`keyboard` 是這個節拍的鍵盤狀態，**只有真的推進新的一幀時才會被取樣**
    /// （回傳的 [`Plan::sampled`]）。
    pub fn advance(&mut self, now: Duration, keyboard: PlayerInput) -> Plan {
        let mut requests = Vec::new();
        if !self.started {
            self.started = true;
            requests.push(Request::SaveState { frame: 0 });
        }
        self.reconcile(&mut requests);

        // 一個節拍的模擬量上限是 K 幀：重跑用掉 `depth` 幀之後，新的一幀只有在 `depth + 1 ≤ K` 時才推進。
        // （沒有這一條時，視窗已滿、K 幀都預測錯誤 → 重跑 K 幀之後確認幀跟上、視窗重新打開，
        // 同一個節拍還會再推進新的一幀，合計 K+1 幀；`a_plan_never_contains_more_than_k_advance_frames`
        // 在 K=1 時抓到這個情況。留到下一個迴圈（約 1 ms 後）再推進，只影響時間，不影響結果。）
        let advances_planned = requests
            .iter()
            .filter(|r| matches!(r, Request::AdvanceFrame { .. }))
            .count() as u32;
        if self.cur - self.confirmed >= self.cfg.window || advances_planned >= self.cfg.window {
            if self.stall_since.is_none() {
                self.stall_since = Some(now);
                self.stalls += 1;
            }
            return Plan {
                requests,
                outcome: Outcome::Stalled,
                sampled: None,
            };
        }
        if let Some(since) = self.stall_since.take() {
            self.stall_time += now.saturating_sub(since);
        }
        if self.should_hold() {
            return Plan {
                requests,
                outcome: Outcome::Held,
                sampled: None,
            };
        }

        // 取樣本地輸入（套用在 `cur + input_delay` 幀），推進新的一幀（輸出開啟）。
        self.local.push_back(keyboard);
        let f = self.cur;
        let (input, predicted, used) = self.sim_input(f);
        self.frames.push_back(Rec { used, predicted });
        if predicted {
            self.predicted_total += 1;
        }
        requests.push(Request::AdvanceFrame {
            frame: f,
            input,
            output_enabled: true,
        });
        self.cur += 1;
        requests.push(Request::SaveState { frame: self.cur });
        self.frames_since_hold = self.frames_since_hold.saturating_add(1);
        // 這一幀若一開始就用真實輸入（沒有預測），已確認幀可以立刻跟上，不必等下一個節拍。
        self.update_confirmed();
        Plan {
            requests,
            outcome: Outcome::Advanced,
            sampled: Some(keyboard),
        }
    }

    /// 步驟 1–3：比對預測、必要時還原重跑、更新已確認幀。
    fn reconcile(&mut self, requests: &mut Vec<Request>) {
        let confirmed = self.confirmed;
        let mut first_bad: Option<u32> = None;
        for (i, rec) in self.frames.iter_mut().enumerate() {
            let f = confirmed + i as u32;
            if !rec.predicted {
                continue;
            }
            let Some(&real) = self.remote.get(&f) else {
                continue;
            };
            if rec.used == real {
                self.prediction_correct += 1;
                rec.predicted = false;
            } else {
                self.prediction_wrong += 1;
                first_bad.get_or_insert(f);
            }
        }

        if let Some(bad) = first_bad {
            if self.cfg.sabotage == Sabotage::SkipRollback {
                // 破壞性測試：偵測到了，但不還原（把紀錄改成真實輸入，當作沒事）。
                for i in 0..self.frames.len() {
                    if let Some(&real) = self.remote.get(&(confirmed + i as u32)) {
                        self.frames[i] = Rec {
                            used: real,
                            predicted: false,
                        };
                    }
                }
            } else {
                self.rollback(bad, requests);
            }
        }

        self.update_confirmed();
    }

    /// 已確認幀前進到第一個「真實輸入還沒到」的幀（或目前幀）。前提：`[confirmed, cur)` 裡已知真實輸入的幀，
    /// 模擬時用的就是真實輸入（比對與還原重跑之後成立；推進新幀時若真實輸入已到也成立）。
    fn update_confirmed(&mut self) {
        let mut c = self.confirmed;
        while c < self.cur
            && self.remote.contains_key(&c)
            && !self.frames[(c - self.confirmed) as usize].predicted
        {
            c += 1;
        }
        self.frames.drain(..(c - self.confirmed) as usize);
        self.confirmed = c;
    }

    /// 還原到 `bad` 並重跑到目前幀（輸出關閉）。
    fn rollback(&mut self, bad: u32, requests: &mut Vec<Request>) {
        let load = if self.cfg.sabotage == Sabotage::LoadOneFrameLate {
            // 破壞性測試：還原到 F+1 而不是 F；第 F 幀的紀錄當作已經修好。
            if let Some(&real) = self.remote.get(&bad) {
                *self.rec(bad) = Rec {
                    used: real,
                    predicted: false,
                };
            }
            bad + 1
        } else {
            bad
        };
        requests.push(Request::LoadState { frame: load });
        // `S_load` 之後的狀態都會重算並重新回報；`S_load` 本身是被還原回來的那份，指紋不變。
        self.fps.retain(|&n, _| n <= load);
        for f in load..self.cur {
            let (input, predicted, used) = self.sim_input(f);
            *self.rec(f) = Rec { used, predicted };
            requests.push(Request::AdvanceFrame {
                frame: f,
                input,
                output_enabled: false,
            });
            requests.push(Request::SaveState { frame: f + 1 });
        }
        let depth = self.cur - load;
        self.rollbacks += 1;
        self.depth_sum += u64::from(depth);
        self.max_depth = self.max_depth.max(depth);
        self.window_max_depth = self.window_max_depth.max(depth);
    }

    /// 取走並歸零「自上次以來最深的一次重跑」（session 每個統計視窗呼叫一次）。
    pub fn take_window_max_depth(&mut self) -> u32 {
        std::mem::take(&mut self.window_max_depth)
    }

    /// 時間同步：領先太多就放慢一幀。
    fn should_hold(&mut self) -> bool {
        if !self.cfg.time_sync
            || self.frames_since_hold < HOLD_COOLDOWN
            || self.local_adv.len() < SYNC_MIN_SAMPLES
            || self.remote_adv.len() < SYNC_MIN_SAMPLES
        {
            return false;
        }
        let recommended = (average(&self.local_adv) - average(&self.remote_adv)) / 2.0;
        if recommended < HOLD_THRESHOLD {
            return false;
        }
        self.holds += 1;
        self.frames_since_hold = 0;
        // 多等一幀之後：本地領先少 1、對方領先多 1。視窗裡的舊樣本依此修正，否則它們要很久才會被新樣本換掉，
        // 期間會連續過度放慢。
        for s in &mut self.local_adv {
            *s -= 1.0;
        }
        for s in &mut self.remote_adv {
            *s += 1.0;
        }
        true
    }

    /// 內部佇列目前的大小（診斷與封包洪流測試用）。
    pub fn buffer_sizes(&self) -> PlannerBufferSizes {
        PlannerBufferSizes {
            remote_inputs: self.remote.len(),
            fingerprints: self.fps.len(),
            remote_fingerprints: self.remote_fps.len(),
            frames: self.frames.len(),
        }
    }

    pub fn stats(&self) -> RollbackStats {
        RollbackStats {
            current_frame: self.cur,
            confirmed_frame: self.confirmed,
            rollbacks: self.rollbacks,
            rollbacks_per_sec: 0.0,
            resim_frames: self.depth_sum,
            avg_depth: if self.rollbacks > 0 {
                self.depth_sum as f32 / self.rollbacks as f32
            } else {
                0.0
            },
            max_depth: self.max_depth,
            resim_time_avg: if self.resims_timed > 0 {
                self.resim_time_sum / self.resims_timed
            } else {
                Duration::ZERO
            },
            resim_time_max: self.resim_time_max,
            resim_time_total: self.resim_time_sum,
            resims_timed: self.resims_timed,
            window_max_depth: 0,
            frame_advantage: average(&self.local_adv) as f32,
            remote_advantage: average(&self.remote_adv) as f32,
            predicted_frames: self.predicted_total,
            prediction_correct: self.prediction_correct,
            prediction_wrong: self.prediction_wrong,
            holds: self.holds,
            stalls: self.stalls,
            stall_time: self.stall_time,
        }
    }
}

fn push_sample(window: &mut VecDeque<f64>, sample: f64) {
    window.push_back(sample);
    while window.len() > SYNC_WINDOW {
        window.pop_front();
    }
}

fn average(window: &VecDeque<f64>) -> f64 {
    if window.is_empty() {
        0.0
    } else {
        window.iter().sum::<f64>() / window.len() as f64
    }
}

#[cfg(test)]
mod tests {
    //! 規劃器的單元測試：**完全沒有 `Nes`**。「模擬器」是一條狀態雜湊鏈（每一幀 `state = mix(state, 輸入)`），
    //! 所以任何一幀用錯輸入、或還原錯了幀，最後的狀態都會不同。
    use super::*;
    use nes_core::Buttons;

    fn mix(state: u64, input: FrameInput) -> u64 {
        let bits = u64::from(input.p1.bits())
            | u64::from(input.p2.bits()) << 8
            | u64::from(input.reset) << 16;
        (state ^ bits.wrapping_add(0x9E37_79B9_7F4A_7C15))
            .wrapping_mul(0xBF58_476D_1CE4_E5B9)
            .rotate_left(29)
    }

    /// 假的模擬器與快照環形緩衝（用 `HashMap` 存 `S_n`，只給測試用）。
    struct Fake {
        state: u64,
        frame: u32,
        saved: std::collections::HashMap<u32, u64>,
        advances: Vec<(u32, bool)>,
        loads: Vec<u32>,
    }

    impl Fake {
        fn new() -> Self {
            Self {
                state: 1,
                frame: 0,
                saved: Default::default(),
                advances: Vec::new(),
                loads: Vec::new(),
            }
        }

        fn run(&mut self, planner: &mut RollbackPlanner, plan: &Plan) {
            for r in &plan.requests {
                match *r {
                    Request::SaveState { frame } => {
                        assert_eq!(frame, self.frame, "SaveState 的幀號必須是目前狀態的幀號");
                        self.saved.insert(frame, self.state);
                        planner.state_saved(frame, self.state);
                    }
                    Request::LoadState { frame } => {
                        self.state = self.saved[&frame];
                        self.frame = frame;
                        self.loads.push(frame);
                    }
                    Request::AdvanceFrame {
                        frame,
                        input,
                        output_enabled,
                    } => {
                        assert_eq!(frame, self.frame, "AdvanceFrame 的幀號必須是目前狀態的幀號");
                        self.state = mix(self.state, input);
                        self.frame += 1;
                        self.advances.push((frame, output_enabled));
                    }
                }
            }
        }
    }

    fn pi(bits: u8) -> PlayerInput {
        PlayerInput::new(Buttons::from_bits_truncate(bits), false)
    }

    /// 兩位玩家的真實輸入（第 f 幀）。
    fn real_local(f: u32) -> PlayerInput {
        pi((f / 2).wrapping_mul(7) as u8 | 1)
    }
    fn real_remote(f: u32) -> PlayerInput {
        pi((f / 3).wrapping_mul(11) as u8 ^ 0x40)
    }

    /// 離線標準答案：`S_n` 的狀態鏈（本地玩家 0、輸入延遲 `d`：第 f < d 幀本地輸入為空）。
    fn expected_chain(frames: u32, d: u32) -> Vec<u64> {
        let mut states = vec![1u64];
        for f in 0..frames {
            let local = if f < d {
                PlayerInput::NONE
            } else {
                real_local(f - d)
            };
            let input = PlayerInput::merge(local, real_remote(f), 0);
            states.push(mix(*states.last().unwrap(), input));
        }
        states
    }

    fn cfg(window: u32, d: u8) -> RollbackConfig {
        RollbackConfig {
            window,
            input_delay: d,
            local_player: 0,
            time_sync: false,
            sabotage: Sabotage::None,
        }
    }

    /// 每個節拍：對方在 `latency` 幀前送的輸入才到（真實輸入延遲 `latency` 幀抵達）。
    fn drive(
        planner: &mut RollbackPlanner,
        fake: &mut Fake,
        ticks: u32,
        latency: u32,
    ) -> Vec<Outcome> {
        let mut outcomes = Vec::new();
        for t in 0..ticks {
            // 第 t 個節拍抵達的對方輸入：第 t − latency 幀。
            if let Some(f) = t.checked_sub(latency) {
                planner.on_remote_input(f, real_remote(f));
            }
            let plan = planner.advance(
                Duration::from_millis(u64::from(t) * 16),
                real_local(planner.current_frame()),
            );
            fake.run(planner, &plan);
            outcomes.push(plan.outcome);
        }
        outcomes
    }

    #[test]
    fn no_rollback_when_real_inputs_arrive_before_they_are_needed() {
        let mut planner = RollbackPlanner::new(cfg(8, 2));
        let mut fake = Fake::new();
        // 輸入延遲 2、對方的輸入提前抵達（latency 0，且第 t 個節拍就有第 t 幀）。
        for t in 0..100u32 {
            planner.on_remote_input(t, real_remote(t));
            let plan = planner.advance(Duration::ZERO, real_local(t));
            fake.run(&mut planner, &plan);
            assert_eq!(plan.outcome, Outcome::Advanced);
        }
        assert_eq!(planner.stats().rollbacks, 0);
        assert_eq!(fake.state, expected_chain(100, 2)[100]);
        assert!(
            fake.advances.iter().all(|&(_, out)| out),
            "沒有重跑，全部都是輸出開啟的新幀"
        );
        assert_eq!(planner.confirmed_frame(), 100);
        let s = planner.stats();
        assert_eq!(s.predicted_frames, 0);
    }

    #[test]
    fn late_inputs_cause_rollbacks_but_the_final_state_is_exact() {
        for latency in [1u32, 3, 5, 7] {
            for d in [0u8, 1, 4] {
                let mut planner = RollbackPlanner::new(cfg(8, d));
                let mut fake = Fake::new();
                let ticks = 200;
                drive(&mut planner, &mut fake, ticks, latency);
                // 收齊最後的輸入之後再多跑幾個節拍讓 rollback 收斂。
                let cur = planner.current_frame();
                for f in ticks.saturating_sub(latency)..cur + 8 {
                    planner.on_remote_input(f, real_remote(f));
                }
                let plan = planner.advance(Duration::ZERO, real_local(cur));
                fake.run(&mut planner, &plan);
                let confirmed = planner.confirmed_frame();
                assert!(confirmed >= cur, "latency {latency} D {d}");
                // 已確認的幀，`S_n` 的指紋與離線標準答案逐幀相同。
                let chain = expected_chain(confirmed, u32::from(d));
                for c in planner.drain_confirmed() {
                    assert_eq!(
                        c.fingerprint,
                        chain[c.frame as usize + 1],
                        "latency {latency} D {d} 幀 {}",
                        c.frame
                    );
                }
                assert!(
                    planner.stats().rollbacks > 0,
                    "latency {latency} D {d}：應該發生過 rollback"
                );
            }
        }
    }

    /// 預測視窗：對方的輸入完全沒到，只能領先 K 幀，之後暫停（不出錯、不超出快照範圍）；輸入恢復後繼續。
    #[test]
    fn stalls_when_the_prediction_window_is_full_and_resumes_when_inputs_return() {
        let k = 5;
        let mut planner = RollbackPlanner::new(cfg(k, 1));
        let mut fake = Fake::new();
        let mut outcomes = Vec::new();
        for t in 0..40u32 {
            let plan = planner.advance(Duration::from_millis(u64::from(t) * 16), real_local(t));
            fake.run(&mut planner, &plan);
            outcomes.push(plan.outcome);
        }
        assert_eq!(planner.current_frame(), k, "只能預測 K 幀");
        assert_eq!(planner.confirmed_frame(), 0);
        assert_eq!(
            outcomes.iter().filter(|o| **o == Outcome::Advanced).count() as u32,
            k
        );
        assert!(
            outcomes[k as usize..]
                .iter()
                .all(|o| *o == Outcome::Stalled)
        );
        let s = planner.stats();
        assert_eq!(s.stalls, 1, "一次連續的 stall 只算一次");
        assert!(fake.loads.is_empty(), "沒有真實輸入就沒有還原");

        // 輸入恢復：全部補上（其中有預測錯的），然後正常繼續。
        for f in 0..30 {
            planner.on_remote_input(f, real_remote(f));
        }
        // 這個節拍：K 幀都預測錯誤 → 重跑 K 幀（用光一個節拍的 K 幀上限），新的一幀留到下一個節拍。
        let plan = planner.advance(Duration::from_secs(1), real_local(k));
        fake.run(&mut planner, &plan);
        assert_eq!(plan.outcome, Outcome::Stalled, "重跑已經用掉 K 幀的上限");
        assert_eq!(planner.confirmed_frame(), k);
        let plan = planner.advance(Duration::from_secs(1), real_local(k));
        fake.run(&mut planner, &plan);
        assert_eq!(plan.outcome, Outcome::Advanced);
        assert!(planner.stats().stall_time > Duration::ZERO);
        assert_eq!(planner.confirmed_frame(), k + 1);
        let chain = expected_chain(k + 1, 1);
        assert_eq!(fake.state, chain[(k + 1) as usize]);
    }

    /// 重跑的幀輸出全部關閉，只有最後（新）的一幀開啟；rollback 的深度與請求清單的形狀。
    #[test]
    fn a_rollback_replays_with_output_off_and_only_the_new_frame_with_output_on() {
        let mut planner = RollbackPlanner::new(cfg(8, 0));
        let mut fake = Fake::new();
        // 前 4 幀都用預測（對方輸入沒到；上一個真實輸入＝空）。
        for t in 0..4u32 {
            let plan = planner.advance(Duration::ZERO, real_local(t));
            fake.run(&mut planner, &plan);
        }
        // 第 0 幀的真實輸入到了，而且和預測（空）不同。
        assert_ne!(real_remote(0), PlayerInput::NONE);
        planner.on_remote_input(0, real_remote(0));
        let plan = planner.advance(Duration::ZERO, real_local(4));
        assert_eq!(plan.outcome, Outcome::Advanced);
        let shape: Vec<Request> = plan.requests.clone();
        // LoadState(0)、Advance(0..4) 各接 SaveState、最後才是新的第 4 幀。
        assert_eq!(shape[0], Request::LoadState { frame: 0 });
        let advances: Vec<(u32, bool)> = shape
            .iter()
            .filter_map(|r| match r {
                Request::AdvanceFrame {
                    frame,
                    output_enabled,
                    ..
                } => Some((*frame, *output_enabled)),
                _ => None,
            })
            .collect();
        assert_eq!(
            advances,
            [(0, false), (1, false), (2, false), (3, false), (4, true)]
        );
        assert_eq!(*shape.last().unwrap(), Request::SaveState { frame: 5 });
        let s = planner.stats();
        assert_eq!((s.rollbacks, s.max_depth, s.resim_frames), (1, 4, 4));
    }

    #[test]
    fn prediction_is_the_last_confirmed_input_and_never_predicts_reset() {
        let mut planner = RollbackPlanner::new(cfg(8, 0));
        planner.on_remote_input(0, PlayerInput::new(Buttons::A, true));
        planner.on_remote_input(1, PlayerInput::new(Buttons::B | Buttons::UP, false));
        let mut fake = Fake::new();
        for t in 0..2 {
            let plan = planner.advance(Duration::ZERO, real_local(t));
            fake.run(&mut planner, &plan);
        }
        // 第 2 幀沒有真實輸入：預測 ＝ 第 1 幀（最後一個連續收到的）的按鍵，reset 為 false。
        let plan = planner.advance(Duration::ZERO, PlayerInput::NONE);
        let Request::AdvanceFrame { input, .. } = plan.requests[0] else {
            panic!("{plan:?}")
        };
        assert_eq!(input.p2, Buttons::B | Buttons::UP);
        assert!(!input.reset);
        // 第 0 幀的 reset 是真實輸入：被套用。
        assert!(planner.sim_input(0).0.reset);
    }

    #[test]
    fn a_reset_from_either_player_resets_both_in_the_same_frame() {
        let mut planner = RollbackPlanner::new(cfg(8, 0));
        planner.on_remote_input(0, PlayerInput::new(Buttons::empty(), true));
        let plan = planner.advance(Duration::ZERO, PlayerInput::NONE);
        let Request::AdvanceFrame { input, .. } = plan.requests[1] else {
            panic!("{plan:?}")
        };
        assert!(input.reset, "對方按 Reset → 本地也在同一幀 reset");
        // 本地按 Reset：同樣。
        let mut planner = RollbackPlanner::new(cfg(8, 0));
        planner.on_remote_input(0, PlayerInput::NONE);
        let plan = planner.advance(Duration::ZERO, PlayerInput::new(Buttons::empty(), true));
        let Request::AdvanceFrame { input, .. } = plan.requests[1] else {
            panic!("{plan:?}")
        };
        assert!(input.reset);
    }

    /// 預測正確率的計算：只計「已經知道真實輸入」的預測幀。
    #[test]
    fn prediction_accuracy_counts_verified_predictions_only() {
        let mut planner = RollbackPlanner::new(cfg(8, 0));
        let mut fake = Fake::new();
        for t in 0..6u32 {
            let plan = planner.advance(Duration::ZERO, real_local(t));
            fake.run(&mut planner, &plan);
        }
        assert_eq!(planner.stats().predicted_frames, 6);
        assert_eq!(
            planner.stats().prediction_accuracy(),
            None,
            "還沒有真實輸入，沒有可驗證的預測"
        );
        // 真實輸入：前兩幀＝預測（空）→ 正確；第 2 幀不同 → 錯誤。
        planner.on_remote_input(0, PlayerInput::NONE);
        planner.on_remote_input(1, PlayerInput::NONE);
        planner.on_remote_input(2, pi(0x0F));
        let plan = planner.advance(Duration::ZERO, real_local(6));
        fake.run(&mut planner, &plan);
        let s = planner.stats();
        assert_eq!((s.prediction_correct, s.prediction_wrong), (2, 1));
        assert!((s.prediction_accuracy().unwrap() - 2.0 / 3.0).abs() < 1e-6);
    }

    /// 指紋只在已確認的幀才對外，預測錯誤時作廢的指紋重跑後重新回報。
    #[test]
    fn fingerprints_are_only_published_for_confirmed_states() {
        let mut planner = RollbackPlanner::new(cfg(8, 0));
        let mut fake = Fake::new();
        for t in 0..5u32 {
            let plan = planner.advance(Duration::ZERO, real_local(t));
            fake.run(&mut planner, &plan);
        }
        // 全靠預測：只有 S_0（開機）是確認的。
        let published = planner.confirmed_fingerprint().unwrap();
        assert_eq!(published.frame, 0);
        // 前 3 幀的真實輸入到了、與預測不同 → 還原重跑；已確認的是 S_3，對外的指紋是 S_3 的。
        for f in 0..3 {
            planner.on_remote_input(f, real_remote(f));
        }
        let plan = planner.advance(Duration::ZERO, real_local(5));
        fake.run(&mut planner, &plan);
        assert_eq!(planner.confirmed_frame(), 3);
        let published = planner.confirmed_fingerprint().unwrap();
        assert_eq!(published.frame, 3);
        assert_eq!(
            published.fingerprint,
            expected_chain(3, 0)[3],
            "對外的指紋必須是「以真實輸入模擬」的結果"
        );
    }

    #[test]
    fn a_mismatching_remote_fingerprint_is_reported_a_matching_one_is_not() {
        let mut planner = RollbackPlanner::new(cfg(8, 0));
        let mut fake = Fake::new();
        for f in 0..10 {
            planner.on_remote_input(f, real_remote(f));
        }
        for t in 0..10u32 {
            let plan = planner.advance(Duration::ZERO, real_local(t));
            fake.run(&mut planner, &plan);
        }
        let chain = expected_chain(10, 0);
        planner.on_remote_fingerprint(ConfirmedFingerprint {
            frame: 6,
            fingerprint: chain[6],
        });
        assert_eq!(planner.poll_desync(), None);
        // 對方的指紋還在未來（本地還沒確認到）：先留著。
        planner.on_remote_fingerprint(ConfirmedFingerprint {
            frame: 999,
            fingerprint: 1,
        });
        assert_eq!(planner.poll_desync(), None);
        planner.on_remote_fingerprint(ConfirmedFingerprint {
            frame: 7,
            fingerprint: chain[7] ^ 1,
        });
        assert_eq!(
            planner.poll_desync(),
            Some(DesyncInfo {
                frame: 7,
                local: chain[7],
                remote: chain[7] ^ 1
            })
        );
    }

    #[test]
    fn drain_confirmed_yields_each_frame_once_with_the_final_inputs() {
        let mut planner = RollbackPlanner::new(cfg(8, 1));
        let mut fake = Fake::new();
        let mut all = Vec::new();
        for t in 0..60u32 {
            if let Some(f) = t.checked_sub(3) {
                planner.on_remote_input(f, real_remote(f));
            }
            let plan = planner.advance(Duration::ZERO, real_local(planner.current_frame()));
            fake.run(&mut planner, &plan);
            all.extend(planner.drain_confirmed());
        }
        let frames: Vec<u32> = all.iter().map(|c| c.frame).collect();
        assert_eq!(frames, (0..frames.len() as u32).collect::<Vec<_>>());
        assert!(frames.len() > 40);
        let chain = expected_chain(frames.len() as u32, 1);
        for c in &all {
            let local = if c.frame < 1 {
                PlayerInput::NONE
            } else {
                real_local(c.frame - 1)
            };
            assert_eq!(c.input, PlayerInput::merge(local, real_remote(c.frame), 0));
            assert_eq!(c.fingerprint, chain[c.frame as usize + 1]);
        }
    }

    #[test]
    fn hostile_remote_input_cannot_grow_memory() {
        let mut planner = RollbackPlanner::new(cfg(8, 1));
        for f in (0..100_000u32).step_by(7) {
            planner.on_remote_input(f, PlayerInput::NONE);
        }
        planner.on_remote_input(u32::MAX, PlayerInput::NONE);
        assert!(planner.remote.len() <= MAX_REMOTE_AHEAD as usize);
        for n in 0..10_000u32 {
            planner.on_remote_fingerprint(ConfirmedFingerprint {
                frame: n,
                fingerprint: 0,
            });
        }
        assert!(planner.remote_fps.len() <= MAX_PENDING_REMOTE_FPS);
    }

    /// 時間同步：領先者放慢、落後者不放慢；放慢有間隔；每放慢一幀，真實的領先量就少 1，最後收斂到 0。
    #[test]
    fn time_sync_slows_only_the_leading_side_and_converges() {
        let sync_cfg = RollbackConfig {
            time_sync: true,
            ..cfg(8, 0)
        };
        let mut leader = RollbackPlanner::new(sync_cfg);
        let mut follower = RollbackPlanner::new(sync_cfg);
        let mut fl = Fake::new();
        let mut ff = Fake::new();
        let rtt = Some(Duration::from_millis(100)); // 單程 3 幀
        let mut lead: i32 = 4; // 真實的領先量（幀）
        let (mut holds_leader, mut holds_follower) = (0, 0);
        let mut last_hold: Option<u32> = None;
        for t in 0..200u32 {
            leader.on_remote_input(t, real_remote(t));
            follower.on_remote_input(t, real_remote(t));
            // 對方封包標示的幀，讓「本地優勢」剛好等於真實領先量（領先者 +lead、落後者 −lead）。
            let one_way = 3;
            // 幀號不能是負的（開頭幾個節拍跳過）。
            if let Ok(sf) = u32::try_from(leader.current_frame() as i32 - one_way - lead) {
                leader.on_remote_sync(sf, -(lead as i8), rtt);
            }
            if let Ok(sf) = u32::try_from(follower.current_frame() as i32 - one_way + lead) {
                follower.on_remote_sync(sf, lead as i8, rtt);
            }
            let pl = leader.advance(Duration::ZERO, real_local(leader.current_frame()));
            fl.run(&mut leader, &pl);
            let pf = follower.advance(Duration::ZERO, real_local(follower.current_frame()));
            ff.run(&mut follower, &pf);
            if pl.outcome == Outcome::Held {
                holds_leader += 1;
                lead -= 1;
                if let Some(prev) = last_hold {
                    assert!(
                        t - prev >= HOLD_COOLDOWN,
                        "兩次放慢至少隔 {HOLD_COOLDOWN} 幀"
                    );
                }
                last_hold = Some(t);
            }
            if pf.outcome == Outcome::Held {
                holds_follower += 1;
            }
        }
        assert_eq!(holds_follower, 0, "落後者不放慢");
        assert!(
            (3..=5).contains(&holds_leader),
            "領先 4 幀 → 約放慢 4 次，實際 {holds_leader}"
        );
        assert_eq!(lead, 4 - holds_leader);
        assert!(lead <= 1, "收斂：最後領先量 {lead}");
        assert_eq!(leader.stats().holds, holds_leader as u32);
    }

    /// 隨機的封包到達模式（延遲、亂序、整段丟失、重複）下，單一節拍的 `AdvanceFrame` 請求數永遠 ≤ K，
    /// 而且視窗不變式 `cur − C ≤ K` 永遠成立（完全不需要 `Nes`）。
    #[test]
    fn a_plan_never_contains_more_than_k_advance_frames() {
        use crate::rng::SplitMix64;
        for window in [1u32, 2, 3, 8, 16, 32] {
            for seed in 0..20u64 {
                let mut rng = SplitMix64::new(seed * 977 + u64::from(window));
                let mut planner = RollbackPlanner::new(cfg(window, (seed % 5) as u8));
                let mut fake = Fake::new();
                let mut max_advances = 0usize;
                let mut pending: Vec<(u32, u32)> = Vec::new(); // (到達的節拍, 幀)
                let mut next_remote = 0u32;
                for t in 0..600u32 {
                    // 對方每個節拍產生一個輸入，各自帶隨機的延遲（有些很久，模擬丟包後重送）。
                    if !rng.next_u64().is_multiple_of(4) {
                        let delay = (rng.next_u64() % 40) as u32;
                        pending.push((t + delay, next_remote));
                        next_remote += 1;
                    }
                    // 整段停電：對方 20～60 個節拍完全沒送。
                    let outage = (t / 100) % 3 == 1;
                    pending.retain(|&(at, f)| {
                        if at <= t && !outage {
                            planner.on_remote_input(f, real_remote(f));
                            false
                        } else {
                            true
                        }
                    });
                    let plan = planner.advance(
                        Duration::from_millis(u64::from(t) * 16),
                        real_local(planner.current_frame()),
                    );
                    let advances = plan
                        .requests
                        .iter()
                        .filter(|r| matches!(r, Request::AdvanceFrame { .. }))
                        .count();
                    max_advances = max_advances.max(advances);
                    assert!(
                        advances <= window as usize,
                        "K={window} seed={seed} 節拍 {t}：{advances} 個 AdvanceFrame"
                    );
                    fake.run(&mut planner, &plan);
                    assert!(planner.current_frame() - planner.confirmed_frame() <= window);
                    planner.drain_confirmed();
                }
                assert!(max_advances >= 1);
            }
        }
    }

    // ---- 破壞性測試（規劃器層級）：每一項都必須讓狀態鏈與標準答案不同 ----

    fn run_with_sabotage(sabotage: Sabotage) -> (u64, u64, bool) {
        let mut planner = RollbackPlanner::new(RollbackConfig {
            sabotage,
            ..cfg(8, 1)
        });
        let mut fake = Fake::new();
        drive(&mut planner, &mut fake, 120, 3);
        for f in 117..140 {
            planner.on_remote_input(f, real_remote(f));
        }
        let plan = planner.advance(Duration::ZERO, real_local(planner.current_frame()));
        fake.run(&mut planner, &plan);
        let confirmed = planner.confirmed_frame();
        let chain = expected_chain(confirmed, 1);
        let mut all_match = true;
        for c in planner.drain_confirmed() {
            all_match &= c.fingerprint == chain[c.frame as usize + 1];
        }
        (fake.saved[&confirmed], chain[confirmed as usize], all_match)
    }

    #[test]
    fn sabotage_none_is_exact() {
        let (got, want, all) = run_with_sabotage(Sabotage::None);
        assert_eq!(got, want);
        assert!(all);
    }

    #[test]
    fn restoring_to_f_plus_1_is_caught() {
        let (got, want, all) = run_with_sabotage(Sabotage::LoadOneFrameLate);
        assert!(got != want || !all, "還原到 F+1 必須讓結果錯誤");
    }

    #[test]
    fn skipping_the_rollback_is_caught() {
        let (got, want, all) = run_with_sabotage(Sabotage::SkipRollback);
        assert!(
            got != want || !all,
            "偵測到預測錯誤卻不還原，必須讓結果錯誤"
        );
    }
}
