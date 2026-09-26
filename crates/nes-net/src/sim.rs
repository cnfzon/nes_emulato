//! 無視窗的對戰模擬器：兩個 [`LockstepSession`]、兩個 `Nes`、虛擬時鐘，經由
//! [`SimulatedTransport`] 連線，用腳本化的雙方輸入跑指定幀數。
//!
//! `nes-test netsim` 與 `tests/` 共用這一份實作。它是**測試／量測工具**，不在 `nes-app` 的執行路徑上。
//!
//! # 虛擬時鐘
//!
//! 時間只由 [`run_match`] 的迴圈推進（每一步 [`STEP`] ＝ 1 ms），所有 `now` 都是這個虛擬時間：
//! 不 sleep、不讀系統時間，所以（1）不需要真的等待——60 秒的對戰幾十毫秒的迴圈就跑完（模擬 `Nes`
//! 的時間另計）；（2）結果只由設定與種子決定，完全可重現；（3）不會因為機器負載而偶發失敗。
//!
//! # 每個端點每一步做的事（與 `nes-app` 的 emu 執行緒相同）
//!
//! `poll` → 處理事件（Connected 時開機） → 依 60.0988 Hz 的累加器決定該不該推進 → 取樣本地輸入
//! → `next_ready_frame`（沒到齊就不推進，也不阻塞）→ `run_frame` → 記錄 → `frame_done`。

use std::collections::BTreeMap;
use std::time::Duration;

use nes_core::error::RomError;
use nes_core::{Buttons, FrameInput, Nes, RomId};

use crate::matchlog::MatchLog;
use crate::protocol::Msg;
use crate::rng::mix64;
use crate::session::{EndReason, Event, LockstepSession, SessionConfig, Stats};
use crate::simnet::{NetworkConfig, SimStats, SimulatedTransport};
use crate::transport::{Datagram, InMemoryTransport, Transport};

/// 虛擬時鐘每一步的長度（等同 emu 執行緒的 1 ms 輪詢）。
pub const STEP: Duration = Duration::from_millis(1);
/// NES 的畫面更新率（NTSC）。
pub const NTSC_FPS: f64 = 60.0988;

/// 腳本化的輸入：玩家 `player` 的第 `sample` 次取樣。每 3 次取樣換一次按鍵（亂數決定），
/// 兩位玩家、不同種子互不相同。用輸入探針 ROM 時，每一個按鍵位元都會留在 RAM 裡。
pub fn script_input(script_seed: u64, player: u8, sample: u32) -> Buttons {
    let key = script_seed
        ^ (u64::from(player) << 56)
        ^ u64::from(sample / 3).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    Buttons::from_bits_truncate(mix64(key) as u8)
}

/// 離線的「標準答案」：把雙方腳本依 input delay 合併（第 `f` 幀的輸入是 `f < D` 時空、
/// 否則是各自的第 `f − D` 次取樣），從開機用同一份 ROM 跑 `frames` 幀。
/// **完全不經過網路與 session**，是驗證的獨立基準。
pub fn expected_log(
    rom: &[u8],
    script_seed: u64,
    input_delay: u8,
    frames: u32,
) -> Result<MatchLog, RomError> {
    let mut nes = Nes::from_rom(rom)?;
    nes.set_output_enabled(false);
    let mut log = MatchLog::new(&nes);
    for f in 0..frames {
        let input = match f.checked_sub(u32::from(input_delay)) {
            None => FrameInput::NONE,
            Some(sample) => FrameInput::new(
                script_input(script_seed, 0, sample),
                script_input(script_seed, 1, sample),
            ),
        };
        nes.run_frame(input);
        log.record(input, &nes);
    }
    Ok(log)
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
    /// 對方到目前為止送來的原始輸入（第 f 幀 → 按鍵）。
    history: BTreeMap<u32, Buttons>,
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
                None => shifted.push(Buttons::empty()),
                Some(prev) => shifted.push(*self.history.get(&prev)?),
            }
        }
        let data = Msg::Input {
            session_id,
            start_frame,
            inputs: shifted,
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

/// 一端：session ＋ transport ＋（連線之後的）`Nes` 與紀錄。
pub struct Endpoint<T: Transport> {
    pub session: LockstepSession,
    pub transport: T,
    pub nes: Option<Nes>,
    pub log: Option<MatchLog>,
    /// 除了 `Stats` 之外的所有事件（依序）。
    pub events: Vec<Event>,
    pub connected_at: Option<Duration>,
    rom: Vec<u8>,
    script_seed: u64,
    target: u32,
    samples: u32,
    acc: Duration,
    last_tick: Option<Duration>,
}

impl<T: Transport> Endpoint<T> {
    pub fn new(
        session: LockstepSession,
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
            rom: rom.to_vec(),
            script_seed,
            target: target_frames,
            samples: 0,
            acc: Duration::ZERO,
            last_tick: None,
        }
    }

    /// 已經跑完目標幀數，或 session 已結束。
    pub fn done(&self) -> bool {
        self.session.frames_completed() >= self.target || self.session.is_ended()
    }

    pub fn frames(&self) -> u32 {
        self.session.frames_completed()
    }

    /// 推進一步。`paced`：依 60.0988 Hz 的累加器推進（虛擬時鐘）；否則有多少就跑多少
    /// （真實 UDP 測試用，不必等真實的 16.6 ms）。
    pub fn tick(&mut self, now: Duration, paced: bool) {
        self.session.poll(now, &mut self.transport);
        while let Some(event) = self.session.poll_event() {
            match &event {
                Event::Connected { .. } => {
                    if let Ok(mut nes) = Nes::from_rom(&self.rom) {
                        nes.set_output_enabled(false);
                        self.log = Some(MatchLog::new(&nes));
                        self.nes = Some(nes);
                        self.connected_at = Some(now);
                    }
                }
                Event::Stats(_) => continue,
                _ => {}
            }
            self.events.push(event);
        }
        let dt = now.saturating_sub(self.last_tick.replace(now).unwrap_or(now));
        let period = Duration::from_secs_f64(1.0 / NTSC_FPS);
        if paced {
            self.acc += dt;
        }
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
                let b = script_input(self.script_seed, self.session.local_player(), self.samples);
                self.samples += 1;
                self.session.add_local_input(b);
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
}

#[derive(Debug, Clone)]
pub struct MatchConfig {
    pub frames: u32,
    /// 兩個方向使用同樣的網路條件（各自獨立的亂數）。
    pub network: NetworkConfig,
    /// 網路模擬的種子。
    pub seed: u64,
    pub input_delay: u8,
    pub redundancy: bool,
    /// 雙方輸入腳本的種子。
    pub script_seed: u64,
    /// 報告裡 replay 的檢查點間隔（測試用 1＝每幀比對）。
    pub checkpoint_interval: u16,
    /// 從這個虛擬時間起丟棄所有封包（斷線測試）。
    pub blackout_at: Option<Duration>,
    pub tamper: Tamper,
    /// 虛擬時間的上限（卡死保護）；`None` ＝ 依幀數估算。
    pub max_virtual_time: Option<Duration>,
    pub peer_timeout: Duration,
}

impl Default for MatchConfig {
    fn default() -> Self {
        Self {
            frames: 3600,
            network: NetworkConfig::IDEAL,
            seed: 1,
            input_delay: crate::session::DEFAULT_INPUT_DELAY,
            redundancy: true,
            script_seed: 0x5EED,
            checkpoint_interval: 60,
            blackout_at: None,
            tamper: Tamper::None,
            max_virtual_time: None,
            peer_timeout: crate::session::PEER_TIMEOUT,
        }
    }
}

pub struct EndReport {
    pub frames: u32,
    pub log: MatchLog,
    pub stats: Stats,
    pub events: Vec<Event>,
    pub end_reason: Option<EndReason>,
    pub connected_at: Option<Duration>,
    pub net: SimStats,
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
}

impl MatchReport {
    /// 兩端都跑完目標幀數、沒有中途斷線，兩端逐幀指紋相同，且都等於離線標準答案。
    pub fn equivalent(&self) -> bool {
        !self.timed_out
            && self.ends.iter().all(|e| e.frames == self.target_frames)
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

/// 跑一場模擬對戰。`expected` 是 [`expected_log`] 的結果（同樣的 ROM、腳本種子、input delay、幀數）；
/// 呼叫端可以對同一組設定只算一次。
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

    let mut host_cfg = SessionConfig::host(rom_id, cfg.input_delay, 0xC0DE_0000 ^ cfg.seed as u32);
    host_cfg.redundancy = cfg.redundancy;
    host_cfg.peer_timeout = cfg.peer_timeout;
    let mut client_cfg = SessionConfig::client(rom_id);
    client_cfg.redundancy = cfg.redundancy;
    client_cfg.peer_timeout = cfg.peer_timeout;

    // 先驗證 ROM 能載入（讓錯誤在這裡就回傳，而不是在 Connected 之後悄悄沒有 `Nes`）。
    Nes::from_rom(rom)?;
    let mut a = Endpoint::new(
        LockstepSession::new(host_cfg),
        ta,
        rom,
        cfg.script_seed,
        cfg.frames,
    );
    let mut b = Endpoint::new(
        LockstepSession::new(client_cfg),
        tb,
        rom,
        cfg.script_seed,
        cfg.frames,
    );

    let limit = cfg
        .max_virtual_time
        .unwrap_or_else(|| Duration::from_secs_f64(f64::from(cfg.frames) / NTSC_FPS * 40.0 + 60.0));
    let mut now = Duration::ZERO;
    let mut blacked_out = false;
    let mut timed_out = false;
    loop {
        if !blacked_out && cfg.blackout_at.is_some_and(|t| now >= t) {
            blacked_out = true;
            a.transport.set_config(cfg.network.blackout());
            b.transport.set_config(cfg.network.blackout());
        }
        a.tick(now, true);
        b.tick(now, true);
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
        }
    };
    let (ea, eb) = (finish(a), finish(b));
    Ok(MatchReport {
        target_frames: cfg.frames,
        virtual_elapsed: now,
        timed_out,
        a_vs_b: first_difference(ea.log.fingerprints(), eb.log.fingerprints()),
        vs_expected: [
            first_difference(ea.log.fingerprints(), expected.fingerprints()),
            first_difference(eb.log.fingerprints(), expected.fingerprints()),
        ],
        ends: [ea, eb],
    })
}

/// 便利函式：連同標準答案一起算。
pub fn run_match_with_expected(rom: &[u8], cfg: &MatchConfig) -> Result<MatchReport, RomError> {
    let expected = expected_log(rom, cfg.script_seed, cfg.input_delay, cfg.frames)?;
    run_match(rom, cfg, &expected)
}
