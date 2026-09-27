//! 無視窗的對戰模擬器：兩個 [`Session`]、兩個 `Nes`、虛擬時鐘，經由
//! [`SimulatedTransport`] 連線，用腳本化的雙方輸入跑指定幀數。lockstep 與 rollback 兩種模式都支援。
//!
//! `nes-test netsim` 與 `tests/` 共用這一份實作。它是**測試／量測工具**，不在 `nes-app` 的執行路徑上。
//!
//! # 虛擬時鐘
//!
//! 時間只由 [`run_match`] 的迴圈推進（每一步 [`STEP`] ＝ 1 ms），所有 `now` 都是這個虛擬時間：
//! 不 sleep、不讀系統時間，所以（1）不需要真的等待——60 秒的對戰幾十毫秒的迴圈就跑完（模擬 `Nes`
//! 的時間另計）；（2）結果只由設定與種子決定，完全可重現；（3）不會因為機器負載而偶發失敗。
//! 唯一的例外是**選用的** [`MatchConfig::wall_clock`]：只用來量測「重跑一次的真實耗時」（統計用，
//! 不影響任何行為），由呼叫端（`nes-test`）提供，`nes-net` 自己不讀系統時間。
//!
//! # 每個端點每一步做的事（與 `nes-app` 的 emu 執行緒相同）
//!
//! `poll` → 處理事件（Connected 時開機） → 依 60.0988 Hz 的累加器決定該不該推進 →
//! lockstep：取樣本地輸入 → `next_ready_frame`（沒到齊就不推進，也不阻塞）→ `run_frame` → 記錄 → `frame_done`；
//! rollback：`advance`（請求清單）→ [`execute`] → 取走新確認的幀記錄。
//!
//! **時鐘偏差**：[`MatchConfig::clock_skew`] 讓端點 B 的幀時鐘快（或慢）指定的比例，用來驗證時間同步。

use std::collections::BTreeMap;
use std::time::Duration;

use nes_core::error::RomError;
use nes_core::{Buttons, Nes, RomId};

use crate::matchlog::MatchLog;
use crate::protocol::{Mode, Msg, PlayerInput};
use crate::rng::mix64;
use crate::rollback::{Outcome, Plan, Request, RollbackStats, Sabotage};
use crate::session::{EndReason, Event, Session, SessionConfig, Stats};
use crate::simnet::{NetworkConfig, SimStats, SimulatedTransport};
use crate::snapshot::{ExecError, SnapshotRing, execute};
use crate::transport::{Datagram, InMemoryTransport, Transport};

/// 虛擬時鐘每一步的長度（等同 emu 執行緒的 1 ms 輪詢）。
pub const STEP: Duration = Duration::from_millis(1);
/// NES 的畫面更新率（NTSC）。
pub use crate::rollback::NTSC_FPS;

/// 預設：每 3 次取樣換一次按鍵。
pub const DEFAULT_INPUT_HOLD: u32 = 3;

/// 腳本化的雙方輸入：玩家 `player` 的第 `sample` 次取樣。每 `hold` 次取樣換一次按鍵（亂數決定），
/// 兩位玩家、不同種子互不相同。用輸入探針 ROM 時，每一個按鍵位元都會留在 RAM 裡。
/// `resets` 為真時，每位玩家平均每 400 次取樣按一次 Reset。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Script {
    pub seed: u64,
    /// 幾次取樣換一次按鍵（1＝每次都換，最壞情況的預測失誤）。
    pub hold: u32,
    pub resets: bool,
}

impl Script {
    pub fn new(seed: u64) -> Self {
        Self {
            seed,
            hold: DEFAULT_INPUT_HOLD,
            resets: false,
        }
    }

    pub fn input(&self, player: u8, sample: u32) -> PlayerInput {
        let hold = self.hold.max(1);
        let key = self.seed
            ^ (u64::from(player) << 56)
            ^ u64::from(sample / hold).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        let buttons = Buttons::from_bits_truncate(mix64(key) as u8);
        let reset = self.resets
            && mix64(key ^ u64::from(sample).wrapping_mul(0xD6E8_FEB8_6659_FD93) ^ 0x5EE7)
                .is_multiple_of(400);
        PlayerInput::new(buttons, reset)
    }
}

/// 舊介面：不含 reset、每 3 次取樣換一次按鍵。
pub fn script_input(script_seed: u64, player: u8, sample: u32) -> Buttons {
    Script::new(script_seed).input(player, sample).buttons
}

/// 離線的「標準答案」：把雙方腳本依各自的 input delay 合併（第 `f` 幀，玩家 `p` 的輸入是 `f < D_p` 時空、
/// 否則是它的第 `f − D_p` 次取樣；reset 是雙方的 OR），從開機用同一份 ROM 跑 `frames` 幀。
/// **完全不經過網路與 session**，是驗證的獨立基準。
pub fn expected_log_with(
    rom: &[u8],
    script: &Script,
    delays: [u8; 2],
    frames: u32,
) -> Result<MatchLog, RomError> {
    let mut nes = Nes::from_rom(rom)?;
    nes.set_output_enabled(false);
    let mut log = MatchLog::new(&nes);
    for f in 0..frames {
        let of = |player: u8| match f.checked_sub(u32::from(delays[usize::from(player)])) {
            None => PlayerInput::NONE,
            Some(sample) => script.input(player, sample),
        };
        let input = PlayerInput::merge(of(0), of(1), 0);
        nes.run_frame(input);
        log.record(input, &nes);
    }
    Ok(log)
}

/// [`expected_log_with`] 的舊介面：兩端相同的 input delay、不含 reset。
pub fn expected_log(
    rom: &[u8],
    script_seed: u64,
    input_delay: u8,
    frames: u32,
) -> Result<MatchLog, RomError> {
    expected_log_with(rom, &Script::new(script_seed), [input_delay; 2], frames)
}

/// 破壞性測試用：故意讓網路層出錯。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Tamper {
    #[default]
    None,
    /// 端點 A 把「對方（B）的輸入」套用在錯誤的幀（差 1）：收到第 `f` 幀的輸入時，
    /// 換成 B 的第 `f − 1` 幀輸入（第 0 幀用空輸入）。
    AppliesRemoteInputOneFrameLate,
}

/// 包在 transport 外層的故障注入器（只影響**收到**的封包）。
pub struct TamperTransport<T> {
    inner: T,
    tamper: Tamper,
    /// 對方到目前為止送來的原始輸入（第 f 幀 → 輸入）。
    history: BTreeMap<u32, PlayerInput>,
}

impl<T: Transport> TamperTransport<T> {
    pub fn new(inner: T, tamper: Tamper) -> Self {
        Self {
            inner,
            tamper,
            history: BTreeMap::new(),
        }
    }

    fn rewrite(&mut self, datagram: Datagram) -> Option<Datagram> {
        if self.tamper != Tamper::AppliesRemoteInputOneFrameLate {
            return Some(datagram);
        }
        let Ok(Msg::Input {
            session_id,
            start_frame,
            inputs,
            sender_frame,
            frame_advantage,
            confirmed,
        }) = Msg::decode(&datagram.data)
        else {
            return Some(datagram);
        };
        for (i, &b) in inputs.iter().enumerate() {
            self.history.insert(start_frame + i as u32, b);
        }
        let mut shifted = Vec::with_capacity(inputs.len());
        for i in 0..inputs.len() as u32 {
            let f = start_frame + i;
            match f.checked_sub(1) {
                None => shifted.push(PlayerInput::NONE),
                Some(prev) => shifted.push(*self.history.get(&prev)?),
            }
        }
        let data = Msg::Input {
            session_id,
            start_frame,
            inputs: shifted,
            sender_frame,
            frame_advantage,
            confirmed,
        }
        .encode()
        .ok()?;
        Some(Datagram { data, ..datagram })
    }
}

impl<T: Transport> Transport for TamperTransport<T> {
    fn send(&mut self, now: Duration, data: &[u8]) {
        self.inner.send(now, data);
    }
    fn send_to(&mut self, now: Duration, addr: std::net::SocketAddr, data: &[u8]) {
        self.inner.send_to(now, addr, data);
    }
    fn recv(&mut self, now: Duration) -> Vec<Datagram> {
        let received = self.inner.recv(now);
        received
            .into_iter()
            .filter_map(|d| self.rewrite(d))
            .collect()
    }
    fn set_peer(&mut self, addr: std::net::SocketAddr) {
        self.inner.set_peer(addr);
    }
}

type SimTransport = SimulatedTransport<TamperTransport<InMemoryTransport>>;

/// 每秒一筆的追蹤資料（rollback）：由 `Event::Stats` 產生。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TracePoint {
    pub at: Duration,
    pub current_frame: u32,
    pub confirmed_frame: u32,
    /// 這一端估計的幀數優勢（平均）。
    pub estimated_advantage: f32,
    pub rollbacks: u32,
}

/// 一端：session ＋ transport ＋（連線之後的）`Nes` 與紀錄。
pub struct Endpoint<T: Transport> {
    pub session: Session,
    pub transport: T,
    pub nes: Option<Nes>,
    pub log: Option<MatchLog>,
    /// 除了 `Stats` 之外的所有事件（依序）。
    pub events: Vec<Event>,
    pub connected_at: Option<Duration>,
    /// rollback：每秒一筆的追蹤資料。
    pub trace: Vec<TracePoint>,
    /// 每秒一筆的完整統計（虛擬時間，含 lockstep）：餵給 `StatsRecorder` 就是連線中寫出的 CSV。
    pub stats_log: Vec<(Duration, Stats)>,
    /// rollback：執行請求時的內部錯誤（不應發生；有就是 bug）。
    pub exec_error: Option<ExecError>,
    /// rollback：單一節拍的請求清單裡 `AdvanceFrame`（重跑的幀加新的一幀）的最大數量。
    /// 規劃器的不變式是它永遠不超過預測視窗 K。
    pub max_advances_per_plan: usize,
    ring: Option<SnapshotRing>,
    rom: Vec<u8>,
    script: Script,
    target: u32,
    samples: u32,
    acc: Duration,
    last_tick: Option<Duration>,
    /// 這一端的幀時鐘速度（1.0＝標準；1.01＝快 1%）。
    speed: f64,
    /// rollback 的預測視窗（開機時建立快照環形緩衝用）。
    window: u32,
    /// 不產生畫面與音訊（模擬用，快很多）。
    headless: bool,
    wall_clock: Option<fn() -> Duration>,
}

impl<T: Transport> Endpoint<T> {
    pub fn new(
        session: Session,
        transport: T,
        rom: &[u8],
        script_seed: u64,
        target_frames: u32,
    ) -> Self {
        Self {
            session,
            transport,
            nes: None,
            log: None,
            events: Vec::new(),
            connected_at: None,
            trace: Vec::new(),
            stats_log: Vec::new(),
            exec_error: None,
            max_advances_per_plan: 0,
            ring: None,
            rom: rom.to_vec(),
            script: Script::new(script_seed),
            target: target_frames,
            samples: 0,
            acc: Duration::ZERO,
            last_tick: None,
            speed: 1.0,
            window: crate::rollback::DEFAULT_WINDOW,
            headless: true,
            wall_clock: None,
        }
    }

    pub fn with_script(mut self, script: Script) -> Self {
        self.script = script;
        self
    }

    /// 這一端的幀時鐘速度倍率（1.01＝快 1%）。
    pub fn with_speed(mut self, speed: f64) -> Self {
        self.speed = speed;
        self
    }

    pub fn with_window(mut self, window: u32) -> Self {
        self.window = window;
        self
    }

    /// `false`：連畫面與音訊也產生（模擬 GUI 的路徑；較慢）。
    pub fn with_headless(mut self, headless: bool) -> Self {
        self.headless = headless;
        self
    }

    pub fn with_wall_clock(mut self, clock: Option<fn() -> Duration>) -> Self {
        self.wall_clock = clock;
        self
    }

    /// 已經跑完目標幀數，或 session 已結束。
    pub fn done(&self) -> bool {
        self.session.frames_completed() >= self.target || self.session.is_ended()
    }

    /// 已完成的幀數（rollback：已確認幀）。
    pub fn frames(&self) -> u32 {
        self.session.frames_completed()
    }

    /// 推進一步。`paced`：依 60.0988 Hz 的累加器推進（虛擬時鐘）；否則有多少就跑多少
    /// （真實 UDP 測試用，不必等真實的 16.6 ms）。
    pub fn tick(&mut self, now: Duration, paced: bool) {
        self.session.poll(now, &mut self.transport);
        while let Some(event) = self.session.poll_event() {
            match &event {
                Event::Connected { mode, .. } => {
                    if let Ok(mut nes) = Nes::from_rom(&self.rom) {
                        nes.set_output_enabled(!self.headless);
                        self.log = Some(MatchLog::new(&nes));
                        if *mode == Mode::Rollback {
                            self.ring = Some(SnapshotRing::new(self.window, &nes));
                        }
                        self.nes = Some(nes);
                        self.connected_at = Some(now);
                    }
                }
                Event::Stats(stats) => {
                    self.stats_log.push((now, *stats));
                    if let Some(rb) = &stats.rollback {
                        self.trace.push(TracePoint {
                            at: now,
                            current_frame: rb.current_frame,
                            confirmed_frame: rb.confirmed_frame,
                            estimated_advantage: rb.frame_advantage,
                            rollbacks: rb.rollbacks,
                        });
                    }
                    continue;
                }
                _ => {}
            }
            self.events.push(event);
        }
        let dt = now.saturating_sub(self.last_tick.replace(now).unwrap_or(now));
        if paced {
            self.acc += dt;
        }
        let period = Duration::from_secs_f64(1.0 / (NTSC_FPS * self.speed));
        match self.session.mode() {
            Mode::Lockstep => self.step_lockstep(now, paced, period),
            Mode::Rollback => self.step_rollback(now, paced, period),
        }
    }

    fn step_lockstep(&mut self, now: Duration, paced: bool, period: Duration) {
        loop {
            if self.frames() >= self.target || !self.session.is_running() {
                break;
            }
            let (Some(nes), Some(log)) = (self.nes.as_mut(), self.log.as_mut()) else {
                break;
            };
            if paced && self.acc < period {
                break;
            }
            if self.session.local_input_wanted() {
                let input = self.script.input(self.session.local_player(), self.samples);
                self.samples += 1;
                self.session.add_local_input(input);
            }
            match self.session.next_ready_frame(now) {
                Some(input) => {
                    nes.run_frame(input);
                    log.record(input, nes);
                    self.session.frame_done(|| nes.behavior_fingerprint());
                    if paced {
                        self.acc -= period;
                    }
                }
                None => {
                    // stall：不推進、不阻塞；累積的時間最多留一幀（恢復之後不狂追）。
                    self.acc = self.acc.min(period);
                    break;
                }
            }
        }
    }

    fn step_rollback(&mut self, now: Duration, paced: bool, period: Duration) {
        loop {
            if self.frames() >= self.target || !self.session.is_running() || self.nes.is_none() {
                break;
            }
            if paced && self.acc < period {
                break;
            }
            let keyboard = self.script.input(self.session.local_player(), self.samples);
            let plan = self.session.advance(now, keyboard);
            if plan.sampled.is_some() {
                self.samples += 1;
            }
            self.run_plan(&plan);
            match plan.outcome {
                Outcome::Advanced | Outcome::Held => {
                    if paced {
                        self.acc -= period;
                    }
                }
                Outcome::Stalled => {
                    // 預測視窗滿了：不推進；累積的時間最多留一幀（恢復之後不狂追）。
                    self.acc = self.acc.min(period);
                    break;
                }
                Outcome::Idle => break,
            }
        }
    }

    /// 執行規劃器的請求清單並取走新確認的幀。
    fn run_plan(&mut self, plan: &Plan) {
        // 沒有請求也要取走新確認的幀：已確認幀可能只因為對方的真實輸入到了（預測正確）就前進，
        // 那個節拍沒有任何請求，但確認過的幀（與早就回報的指紋）仍然要進紀錄。
        if plan.requests.is_empty() {
            if let (Some(log), true) = (self.log.as_mut(), self.ring.is_some()) {
                for confirmed in self.session.drain_confirmed() {
                    log.push(confirmed.input, confirmed.fingerprint);
                }
            }
            return;
        }
        let advances = plan
            .requests
            .iter()
            .filter(|r| matches!(r, Request::AdvanceFrame { .. }))
            .count();
        self.max_advances_per_plan = self.max_advances_per_plan.max(advances);
        let (Some(nes), Some(ring), Some(log)) =
            (self.nes.as_mut(), self.ring.as_mut(), self.log.as_mut())
        else {
            return;
        };
        let started = self.wall_clock.map(|clock| clock());
        let session = &mut self.session;
        let result = if self.headless {
            // 模擬用：所有幀都不產生畫面與音訊（輸出開關不影響行為指紋，`nes-core` 的測試涵蓋）。
            let quiet: Vec<Request> = plan
                .requests
                .iter()
                .map(|r| match *r {
                    Request::AdvanceFrame { frame, input, .. } => Request::AdvanceFrame {
                        frame,
                        input,
                        output_enabled: false,
                    },
                    other => other,
                })
                .collect();
            execute(&quiet, nes, ring, |f, fp| session.state_saved(f, fp))
        } else {
            execute(&plan.requests, nes, ring, |f, fp| {
                session.state_saved(f, fp)
            })
        };
        match result {
            Ok(report) => {
                if report.loads > 0
                    && let (Some(t0), Some(clock)) = (started, self.wall_clock)
                {
                    session.record_resim(clock().saturating_sub(t0));
                }
            }
            Err(e) => self.exec_error = Some(e),
        }
        for confirmed in session.drain_confirmed() {
            log.push(confirmed.input, confirmed.fingerprint);
        }
    }
}

#[derive(Debug, Clone)]
pub struct MatchConfig {
    pub frames: u32,
    /// 兩個方向使用同樣的網路條件（各自獨立的亂數）。
    pub network: NetworkConfig,
    /// 網路模擬的種子。
    pub seed: u64,
    /// lockstep：雙方共用的輸入延遲；rollback：兩端各自的本地輸入延遲（除非設了 [`Self::delays`]）。
    pub input_delay: u8,
    /// rollback：兩端各自的本地輸入延遲（A、B）；`None` ＝ 都用 `input_delay`。
    pub delays: Option<[u8; 2]>,
    pub redundancy: bool,
    /// 雙方輸入腳本的種子。
    pub script_seed: u64,
    /// 幾次取樣換一次按鍵（1＝每次都換，最壞情況的預測失誤）。
    pub input_hold: u32,
    /// 腳本裡包含 Reset（每位玩家平均每 400 次取樣一次）。
    pub resets: bool,
    /// 報告裡 replay 的檢查點間隔（測試用 1＝每幀比對）。
    pub checkpoint_interval: u16,
    /// 從這個虛擬時間起丟棄所有封包（斷線測試）。
    pub blackout_at: Option<Duration>,
    pub tamper: Tamper,
    /// 虛擬時間的上限（卡死保護）；`None` ＝ 依幀數估算。
    pub max_virtual_time: Option<Duration>,
    pub peer_timeout: Duration,
    pub mode: Mode,
    /// rollback 的預測視窗 K。
    pub window: u32,
    /// 端點 B 的幀時鐘比 A 快的比例（0.01＝快 1%；負數＝慢）。
    pub clock_skew: f64,
    /// rollback 的時間同步（關閉只用於比較實驗）。
    pub time_sync: bool,
    /// 破壞性測試（套用在端點 A 的 rollback 邏輯）。
    pub sabotage: Sabotage,
    /// 量測「重跑一次」的真實耗時用的時間來源（`nes-net` 自己不讀系統時間，由 `nes-test` 提供）。
    pub wall_clock: Option<fn() -> Duration>,
}

impl Default for MatchConfig {
    fn default() -> Self {
        Self {
            frames: 3600,
            network: NetworkConfig::IDEAL,
            seed: 1,
            input_delay: crate::session::DEFAULT_INPUT_DELAY,
            delays: None,
            redundancy: true,
            script_seed: 0x5EED,
            input_hold: DEFAULT_INPUT_HOLD,
            resets: false,
            checkpoint_interval: 60,
            blackout_at: None,
            tamper: Tamper::None,
            max_virtual_time: None,
            peer_timeout: crate::session::PEER_TIMEOUT,
            mode: Mode::Lockstep,
            window: crate::rollback::DEFAULT_WINDOW,
            clock_skew: 0.0,
            time_sync: true,
            sabotage: Sabotage::None,
            wall_clock: None,
        }
    }
}

impl MatchConfig {
    pub fn script(&self) -> Script {
        Script {
            seed: self.script_seed,
            hold: self.input_hold,
            resets: self.resets,
        }
    }

    /// 兩端各自實際使用的輸入延遲（lockstep 一律共用 `input_delay`）。
    pub fn effective_delays(&self) -> [u8; 2] {
        match self.mode {
            Mode::Lockstep => [self.input_delay; 2],
            Mode::Rollback => self.delays.unwrap_or([self.input_delay; 2]),
        }
    }

    /// 這個設定的離線標準答案。
    pub fn expected(&self, rom: &[u8]) -> Result<MatchLog, RomError> {
        expected_log_with(rom, &self.script(), self.effective_delays(), self.frames)
    }
}

pub struct EndReport {
    /// 已完成的幀數（rollback：已確認幀，可能比目標多幾幀）。
    pub frames: u32,
    pub log: MatchLog,
    pub stats: Stats,
    pub events: Vec<Event>,
    pub end_reason: Option<EndReason>,
    pub connected_at: Option<Duration>,
    pub net: SimStats,
    pub trace: Vec<TracePoint>,
    pub stats_log: Vec<(Duration, Stats)>,
    pub exec_error: Option<ExecError>,
    /// rollback：單一節拍的 `AdvanceFrame` 請求數的最大值（不變式：≤ K）。
    pub max_advances_per_plan: usize,
}

impl EndReport {
    pub fn rollback(&self) -> Option<&RollbackStats> {
        self.stats.rollback.as_ref()
    }
}

/// 兩端「目前幀」的差（A − B），每個虛擬秒取樣一次：時間同步的真實收斂過程（不是估計值）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdvantageSample {
    pub second: u32,
    pub a_minus_b: i32,
}

pub struct MatchReport {
    pub target_frames: u32,
    pub virtual_elapsed: Duration,
    pub timed_out: bool,
    /// 端點 A＝Host（玩家 1），B＝Client（玩家 2）。
    pub ends: [EndReport; 2],
    /// A、B 兩端的逐幀指紋第一個不同的幀（已完成的幀數；`None`＝全程相同且長度相同）。
    pub a_vs_b: Option<u32>,
    /// 各端與離線標準答案第一個不同的幀。
    pub vs_expected: [Option<u32>; 2],
    /// rollback：兩端目前幀的差（A − B），每個虛擬秒一筆。
    pub advantage: Vec<AdvantageSample>,
}

impl MatchReport {
    /// 兩端都跑完目標幀數、沒有中途斷線，兩端逐幀指紋相同，且都等於離線標準答案。
    pub fn equivalent(&self) -> bool {
        !self.timed_out
            && self.ends.iter().all(|e| e.frames >= self.target_frames)
            && self.ends.iter().all(|e| e.exec_error.is_none())
            && self.a_vs_b.is_none()
            && self.vs_expected.iter().all(Option::is_none)
    }

    pub fn desync_events(&self) -> usize {
        self.ends
            .iter()
            .flat_map(|e| &e.events)
            .filter(|e| matches!(e, Event::Desync { .. }))
            .count()
    }

    pub fn total_stalls(&self) -> u32 {
        self.ends.iter().map(|e| e.stats.stalls).sum()
    }
}

/// 第一個不同的幀（已完成的幀數，從 1 起算）；長度不同而前綴相同時，回報較短者的長度 + 1。
fn first_difference(a: &[u64], b: &[u64]) -> Option<u32> {
    if let Some(i) = a.iter().zip(b).position(|(x, y)| x != y) {
        return Some(i as u32);
    }
    (a.len() != b.len()).then_some(a.len().min(b.len()) as u32)
}

/// 只取前 `frames + 1` 個指紋（`[n]`＝已完成 `n` 幀之後）。rollback 的已確認幀可能比目標多幾幀。
fn upto(fingerprints: &[u64], frames: u32) -> &[u64] {
    &fingerprints[..fingerprints.len().min(frames as usize + 1)]
}

/// 跑一場模擬對戰。`expected` 是 [`expected_log`]／[`MatchConfig::expected`] 的結果（同樣的 ROM、腳本、
/// input delay、幀數）；呼叫端可以對同一組設定只算一次。
pub fn run_match(
    rom: &[u8],
    cfg: &MatchConfig,
    expected: &MatchLog,
) -> Result<MatchReport, RomError> {
    let rom_id = RomId::of_file(rom);
    let (mem_a, mem_b) = InMemoryTransport::pair();
    let make = |mem, tamper, seed| {
        SimulatedTransport::new(TamperTransport::new(mem, tamper), cfg.network, seed)
    };
    let ta: SimTransport = make(mem_a, cfg.tamper, cfg.seed.wrapping_mul(2));
    let tb: SimTransport = make(
        mem_b,
        Tamper::None,
        cfg.seed.wrapping_mul(2).wrapping_add(1),
    );

    let delays = cfg.effective_delays();
    let mut host_cfg = SessionConfig::host(rom_id, delays[0], 0xC0DE_0000 ^ cfg.seed as u32)
        .with_mode(cfg.mode)
        .with_window(cfg.window);
    host_cfg.redundancy = cfg.redundancy;
    host_cfg.peer_timeout = cfg.peer_timeout;
    host_cfg.time_sync = cfg.time_sync;
    host_cfg.sabotage = cfg.sabotage;
    let mut client_cfg = SessionConfig::client(rom_id)
        .with_input_delay(delays[1])
        .with_window(cfg.window);
    client_cfg.redundancy = cfg.redundancy;
    client_cfg.peer_timeout = cfg.peer_timeout;
    client_cfg.time_sync = cfg.time_sync;

    // 先驗證 ROM 能載入（讓錯誤在這裡就回傳，而不是在 Connected 之後悄悄沒有 `Nes`）。
    Nes::from_rom(rom)?;
    let mut a = Endpoint::new(Session::new(host_cfg), ta, rom, cfg.script_seed, cfg.frames)
        .with_script(cfg.script())
        .with_window(cfg.window)
        .with_wall_clock(cfg.wall_clock);
    let mut b = Endpoint::new(
        Session::new(client_cfg),
        tb,
        rom,
        cfg.script_seed,
        cfg.frames,
    )
    .with_script(cfg.script())
    .with_speed(1.0 + cfg.clock_skew)
    .with_window(cfg.window)
    .with_wall_clock(cfg.wall_clock);

    let limit = cfg
        .max_virtual_time
        .unwrap_or_else(|| Duration::from_secs_f64(f64::from(cfg.frames) / NTSC_FPS * 40.0 + 60.0));
    let mut now = Duration::ZERO;
    let mut blacked_out = false;
    let mut timed_out = false;
    let mut advantage = Vec::new();
    let mut next_sample = Duration::from_secs(1);
    loop {
        if !blacked_out && cfg.blackout_at.is_some_and(|t| now >= t) {
            blacked_out = true;
            a.transport.set_config(cfg.network.blackout());
            b.transport.set_config(cfg.network.blackout());
        }
        a.tick(now, true);
        b.tick(now, true);
        if now >= next_sample {
            if let (Some(pa), Some(pb)) = (a.session.planner(), b.session.planner()) {
                advantage.push(AdvantageSample {
                    second: advantage.len() as u32 + 1,
                    a_minus_b: pa.current_frame() as i32 - pb.current_frame() as i32,
                });
            }
            next_sample += Duration::from_secs(1);
        }
        if a.done() && b.done() {
            break;
        }
        if now >= limit {
            timed_out = true;
            break;
        }
        now += STEP;
    }

    let finish = |e: Endpoint<SimTransport>| -> EndReport {
        let net = e.transport.stats();
        EndReport {
            frames: e.session.frames_completed(),
            stats: e.session.stats(),
            end_reason: e.session.end_reason(),
            connected_at: e.connected_at,
            log: e
                .log
                .unwrap_or_else(|| MatchLog::new(&Nes::from_rom(rom).expect("ROM 已驗證過"))),
            events: e.events,
            net,
            trace: e.trace,
            stats_log: e.stats_log,
            exec_error: e.exec_error,
            max_advances_per_plan: e.max_advances_per_plan,
        }
    };
    let (ea, eb) = (finish(a), finish(b));
    let n = cfg.frames;
    Ok(MatchReport {
        target_frames: n,
        virtual_elapsed: now,
        timed_out,
        a_vs_b: first_difference(
            upto(ea.log.fingerprints(), n),
            upto(eb.log.fingerprints(), n),
        ),
        vs_expected: [
            first_difference(upto(ea.log.fingerprints(), n), expected.fingerprints()),
            first_difference(upto(eb.log.fingerprints(), n), expected.fingerprints()),
        ],
        ends: [ea, eb],
        advantage,
    })
}

/// 便利函式：連同標準答案一起算。
pub fn run_match_with_expected(rom: &[u8], cfg: &MatchConfig) -> Result<MatchReport, RomError> {
    let expected = cfg.expected(rom)?;
    run_match(rom, cfg, &expected)
}
