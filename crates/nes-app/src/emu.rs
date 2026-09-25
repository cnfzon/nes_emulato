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
//! TODO：之後可以換成 `spin_sleep`（忙等 + 讓出時間片混合，減少 sleep 的
//! 系統排程抖動）。

use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use nes_core::{Buttons, DebugSnapshot, FrameBuffer, Nes, PpuViews};

use crate::audio::{AudioProducer, RateController};
use crate::commands::{EmuCommand, EmuEvent};

const TARGET_FPS: f64 = 60.0988;

/// 執行中每隔幾幀更新一次 PPU 影像（pattern table / nametable）。它們要畫 6 張圖，
/// 而且人眼不需要 60Hz 更新；暫停時單步／讀檔之後則會立即更新。
const VIEWS_EVERY_N_FRAMES: u32 = 3;

/// 單次 `TraceToFile` 允許的最大指令數，避免手誤輸入超大數字讓 emu 執行緒
/// 卡在寫檔上（同步執行，期間不處理其他指令）。
const MAX_TRACE_INSTRUCTIONS: u32 = 1_000_000;

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

pub fn run(
    cmd_rx: Receiver<EmuCommand>,
    event_tx: Sender<EmuEvent>,
    mut frame_input: triple_buffer::Input<FrameBuffer>,
    debug_input: triple_buffer::Input<Option<DebugSnapshot>>,
    views_input: triple_buffer::Input<Option<PpuViews>>,
    audio: AudioProducer,
) {
    let mut audio = AudioSide::new(audio);
    let frame_duration = Duration::from_secs_f64(1.0 / TARGET_FPS);

    let mut nes: Option<Nes> = None;
    let mut current_input = [Buttons::empty(); 2];
    let mut paused = false;
    let mut saved_state: Option<Vec<u8>> = None;
    let mut sinks = DebugSinks {
        enabled: false,
        snapshot: debug_input,
        views_palette: None,
        views: views_input,
        frames_since_views: 0,
    };

    let mut accumulator = Duration::ZERO;
    let mut last_tick = Instant::now();

    let mut fps_window_start = Instant::now();
    let mut fps_window_frames: u64 = 0;

    // 測試用屏障（`EmuCommand::Barrier`）：本輪結果發布之後才回覆。
    #[cfg(test)]
    let mut barriers: Vec<Sender<()>> = Vec::new();

    'outer: loop {
        // 本輪指令處理是否改變了模擬狀態（載入 ROM/讀檔/單步）。改變了就要
        // 立刻重發快照與幀數——暫停中 `run_frame` 不會再被呼叫，不補發的話
        // Debugger 面板會停在舊狀態。
        let mut state_changed = false;

        for cmd in cmd_rx.try_iter() {
            match cmd {
                EmuCommand::LoadRom(bytes) => match Nes::from_rom(&bytes) {
                    Ok(mut new_nes) => {
                        let info = new_nes.rom_info().clone();
                        audio.configure(&mut new_nes);
                        nes = Some(new_nes);
                        state_changed = true;
                        let _ = event_tx.send(EmuEvent::RomLoaded(info));
                    }
                    Err(e) => {
                        let _ = event_tx.send(EmuEvent::Error(e.to_string()));
                    }
                },
                EmuCommand::SetInput(player, buttons) => {
                    if let Some(slot) = current_input.get_mut(player as usize) {
                        *slot = buttons;
                    }
                }
                EmuCommand::Pause => {
                    paused = true;
                    audio.output.set_paused(true);
                }
                EmuCommand::Resume => {
                    paused = false;
                    audio.rate.reset(audio.output.target_fill());
                    audio.output.set_paused(false);
                }
                EmuCommand::SetAudioChannelMask(mask) => {
                    audio.channel_mask = mask;
                    if let Some(n) = &mut nes {
                        n.set_audio_channel_mask(mask);
                    }
                }
                EmuCommand::SetDebugEnabled(enabled) => {
                    sinks.enabled = enabled;
                    // 剛打開面板時立刻送一份目前狀態，不必等到下一幀
                    // 才有資料（尤其是暫停中時，run_frame 不會再被呼叫）。
                    if enabled && let Some(n) = &nes {
                        sinks.snapshot.write(Some(n.debug_snapshot()));
                    }
                }
                EmuCommand::SetDebugViews(palette) => {
                    sinks.views_palette = palette;
                    // 同上：立刻產生一次，不必等 N 幀（暫停中也才看得到）。
                    if let Some(n) = &nes {
                        sinks.publish_views(n);
                    }
                }
                EmuCommand::SaveState => {
                    if let Some(n) = &nes {
                        saved_state = Some(n.save_state());
                    }
                }
                EmuCommand::LoadState => {
                    if let (Some(n), Some(state)) = (&mut nes, &saved_state) {
                        match n.load_state(state) {
                            Ok(()) => state_changed = true,
                            Err(e) => {
                                let _ = event_tx.send(EmuEvent::Error(e.to_string()));
                            }
                        }
                    }
                }
                EmuCommand::StepInstruction => {
                    if !paused {
                        let _ = event_tx.send(EmuEvent::Error(NOT_PAUSED_MSG.to_string()));
                    } else if let Some(n) = &mut nes {
                        n.step_instruction();
                        // 顯示「畫到一半」的畫面，可以看到掃描線逐步畫出來。
                        frame_input.write(n.frame_buffer().clone());
                        state_changed = true;
                    }
                }
                EmuCommand::StepFrame => {
                    if !paused {
                        let _ = event_tx.send(EmuEvent::Error(NOT_PAUSED_MSG.to_string()));
                    } else if let Some(n) = &mut nes {
                        let fb = n.run_frame(current_input).clone();
                        frame_input.write(fb);
                        state_changed = true;
                    }
                }
                EmuCommand::TraceToFile { count, path } => {
                    if !paused {
                        let _ = event_tx.send(EmuEvent::Error(NOT_PAUSED_MSG.to_string()));
                    } else if let Some(n) = &mut nes {
                        let count = count.min(MAX_TRACE_INSTRUCTIONS);
                        let result = write_trace(n, count, &path);
                        // 不論成功與否，模擬狀態可能已經前進了。
                        state_changed = true;
                        let _ = event_tx.send(match result {
                            Ok(()) => EmuEvent::TraceWritten { path, lines: count },
                            Err(e) => EmuEvent::Error(format!("寫入 trace 失敗: {e}")),
                        });
                    }
                }
                EmuCommand::Quit => break 'outer,
                #[cfg(test)]
                EmuCommand::Barrier(ack) => barriers.push(ack),
            }
        }

        if state_changed && let Some(n) = &nes {
            publish_state(n, &mut sinks, &event_tx, true);
        }
        // 暫停中單步、讀檔產生的音訊不播放（暫停時要完全靜音）。
        if paused && let Some(n) = &mut nes {
            audio.discard(n);
        }
        #[cfg(test)]
        for ack in barriers.drain(..) {
            let _ = ack.send(());
        }

        let now = Instant::now();
        accumulator += now.duration_since(last_tick);
        last_tick = now;

        while accumulator >= frame_duration {
            accumulator -= frame_duration;
            if !paused && let Some(n) = &mut nes {
                let fb = n.run_frame(current_input).clone();
                frame_input.write(fb);
                audio.after_frame(n);
                fps_window_frames += 1;
                publish_state(n, &mut sinks, &event_tx, false);
            }
        }

        if fps_window_start.elapsed() >= Duration::from_secs(1) {
            let fps = fps_window_frames as f64 / fps_window_start.elapsed().as_secs_f64();
            let _ = event_tx.send(EmuEvent::FpsReport { fps });
            fps_window_frames = 0;
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
                    Ok(EmuEvent::RomLoaded(_)) => return,
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
}
