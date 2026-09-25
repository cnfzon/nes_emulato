//! `nes-core`：決定性的 NES 模擬核心。
//!
//! 設計限制（詳見 `docs/architecture.md` 的「決定性規則清單」）：
//! - `#![forbid(unsafe_code)]`
//! - 不依賴任何 I/O、時間、亂數、執行緒或 GUI crate
//! - 不得在會影響狀態的邏輯中迭代 `HashMap`（本 crate 目前沒有任何 `HashMap`）
//! - 對外 API 是「外部驅動、每次跑一幀」的形式：呼叫端傳入輸入、拿回畫面，
//!   不使用 callback 或帶 lifetime 的 closure（理由見 `docs/architecture.md`）。

#![forbid(unsafe_code)]

pub mod apu;
pub mod bus;
pub mod cartridge;
pub mod cpu;
pub mod debug;
pub mod error;
pub mod frame;
pub mod joypad;
pub mod ppu;
pub mod state;
/// 測試用的迷你組譯器與合成 ROM（`cargo test` 或 `testing` feature 才編譯）。
#[cfg(any(test, feature = "testing"))]
pub mod test_support;

pub use bus::Bus;
pub use cartridge::{Cartridge, Mapper, Mirroring, RomInfo};
pub use cpu::{Cpu, StatusFlags};
pub use debug::{DebugSnapshot, PpuImage, PpuViews};
pub use error::{RomError, StateError};
pub use frame::FrameBuffer;
pub use joypad::{Buttons, Joypad};
pub use state::{CORE_BEHAVIOR_VERSION, STATE_FORMAT_VERSION, STATE_MAGIC, StateHeader};

/// 一台完整的 NES 主機。
///
/// 所有權鏈：`Nes` 擁有 `Cpu`，`Cpu` 擁有 `Bus`，`Bus` 擁有
/// `Ppu` / `Apu` / `Cartridge` / `[Joypad; 2]`。整個型別樹都是 plain owned
/// data（沒有 `Rc`/`RefCell`/`Arc`/`Mutex`），所以 `Nes` 可以整包 `Clone`，
/// 也可以整包 `Serialize`/`Deserialize` —— 這正是 rollback 需要的能力。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Nes {
    cpu: Cpu,
    /// 已完成的 `run_frame` 次數（跟 `Ppu::frame` 不同：單步除錯時 PPU 可能
    /// 自己跨過 vblank，但那不算一次 `run_frame`）。
    frame_count: u64,
}

impl Nes {
    /// 從一份 iNES ROM 檔案的原始位元組建立一台新的 NES。
    ///
    /// four-screen（4 螢幕 nametable）卡帶需要額外的 2KB VRAM，目前不支援，
    /// 回傳 [`RomError::FourScreenUnsupported`]。
    pub fn from_rom(rom: &[u8]) -> Result<Self, RomError> {
        let cartridge = Cartridge::from_ines(rom)?;
        if cartridge.info.mirroring == Mirroring::FourScreen {
            return Err(RomError::FourScreenUnsupported);
        }
        let bus = Bus::new(cartridge);
        let mut cpu = Cpu::new(bus);
        cpu.reset();
        Ok(Self {
            cpu,
            frame_count: 0,
        })
    }

    /// 按下 reset 鍵：CPU 重置（PC 取自 reset vector）、PPU 清掉 PPUCTRL /
    /// PPUMASK 等暫存器。RAM、VRAM、OAM、卡帶內容維持不變。
    pub fn reset(&mut self) {
        self.cpu.bus_mut().ppu.reset();
        self.cpu.bus_mut().apu.reset(true);
        self.cpu.reset();
    }

    /// 推進一幀模擬，回傳這一幀畫好的畫面。
    ///
    /// 一幀的邊界是「PPU 完成一幀」（進入 vblank：scanline 241、dot 1，此時
    /// 240 條可見掃描線都已畫完），不是固定的 CPU cycle 預算。因為 CPU 是
    /// instruction-level，PPU 可能越過邊界最多一條指令（最壞 8 cycles ＝ 24
    /// dot；OAM DMA 則是 513/514 cycles）才被發現——越過的部分只是 vblank，
    /// 不會影響畫面。
    ///
    /// **輸入在這一幀開始時鎖定**：整幀期間搖桿讀到的按鍵狀態不變。
    ///
    /// 由外部（GUI / netplay 迴圈）每幀呼叫一次並傳入雙人輸入，而不是靠
    /// `Nes` 自己起執行緒或呼叫 callback —— 這樣 rollback 才能在任意一幀
    /// 暫停、讀檔、用不同輸入重跑，行為完全可預期。
    ///
    /// 這個方法在任何 ROM 內容下都不會 panic：所有記憶體存取都在硬體位址空間
    /// 內取模，CPU 卡死（JAM）時仍會每步推進 2 cycles，所以 PPU 一定會走完一幀。
    pub fn run_frame(&mut self, input: [Buttons; 2]) -> &FrameBuffer {
        let bus = self.cpu.bus_mut();
        bus.joypads[0].state = input[0];
        bus.joypads[1].state = input[1];
        // 單步除錯可能已經讓 PPU 越過 vblank 起點；這裡重新開始計一幀。
        bus.ppu.clear_frame_done();

        while !self.cpu.bus_mut().ppu.take_frame_done() {
            self.cpu.step();
        }

        self.frame_count += 1;
        self.cpu.bus().ppu.frame_buffer()
    }

    /// 把自從上次呼叫以來累積的音訊取樣（單聲道、`f32`、取樣率見
    /// [`Nes::audio_sample_rate`]）附加到 `out`。
    ///
    /// 音訊是**輸出**，與畫面同地位：不進存檔、不影響 `state_hash`（見 `apu/output.rs`）。
    pub fn drain_audio(&mut self, out: &mut Vec<f32>) {
        self.cpu.bus_mut().apu.take_samples(out);
    }

    /// 設定音訊輸出的取樣率（Hz，夾在 8k–192k）。app 端做動態速率控制時會頻繁微調，
    /// 所以只重算濾波係數，不清除已產生的取樣。
    pub fn set_audio_sample_rate(&mut self, hz: f64) {
        self.cpu.bus_mut().apu.set_sample_rate(hz);
    }

    pub fn audio_sample_rate(&self) -> f64 {
        self.cpu.bus().apu.sample_rate()
    }

    /// 開關輸出。關閉時**不產生畫面（不寫 framebuffer）也不產生音訊**，只推進模擬狀態；
    /// `state_hash` 與開啟時逐位元相同（測試 `output_switch_does_not_change_the_state_hash`）。
    /// Phase 4 的 rollback 重跑幀時會用到。預設開啟。
    ///
    /// 關閉期間 `run_frame` 回傳的 `FrameBuffer` 是舊內容。輸出設定不進存檔，`load_state`
    /// 會保留它。
    pub fn set_output_enabled(&mut self, enabled: bool) {
        let bus = self.cpu.bus_mut();
        bus.ppu.set_output_enabled(enabled);
        bus.apu.set_output_enabled(enabled);
    }

    pub fn output_enabled(&self) -> bool {
        self.cpu.bus().apu.output_enabled()
    }

    /// 設定聽得到的聲道（[`apu::CHANNEL_PULSE1`] 等位元，1 = 聽得到）。只影響混音，
    /// 不影響模擬狀態。
    pub fn set_audio_channel_mask(&mut self, mask: u8) {
        self.cpu.bus_mut().apu.set_channel_mask(mask);
    }

    pub fn audio_channel_mask(&self) -> u8 {
        self.cpu.bus().apu.channel_mask()
    }

    /// 把目前狀態序列化成一份存檔（postcard 編碼）。
    pub fn save_state(&self) -> Vec<u8> {
        state::encode(self)
    }

    /// 從一份存檔還原狀態，取代 `self` 目前的內容。
    ///
    /// 步驟：
    /// 1. postcard 解碼——資料截斷/格式錯誤會在這裡回傳
    ///    [`StateError::Decode`]。
    /// 2. 檢查解碼出來的內部欄位長度是否符合硬體規格（RAM/VRAM/OAM/
    ///    CHR-RAM/PRG-RAM）；不符合代表存檔損毀或被竄改，回傳
    ///    [`StateError::Corrupt`]。
    /// 3. 比對 `rom_hash` 是否跟目前已載入的 ROM 相符；不符合代表這份存檔
    ///    屬於另一個遊戲，回傳 [`StateError::RomMismatch`]。
    /// 4. 因為 `prg_rom`/`chr_rom` 不進存檔（`#[serde(skip)]`），從 `self`
    ///    目前持有的 ROM 資料接回解碼出來的 `Nes`，再整個取代 `self`。
    pub fn load_state(&mut self, bytes: &[u8]) -> Result<(), StateError> {
        let mut decoded: Nes = state::decode(bytes)?;
        decoded.validate_structure()?;

        let expected_hash = self.cpu.bus().cartridge.rom_hash;
        let found_hash = decoded.cpu.bus().cartridge.rom_hash;
        if expected_hash != found_hash {
            return Err(StateError::RomMismatch {
                expected: expected_hash,
                found: found_hash,
            });
        }

        let prg_rom = self.cpu.bus().cartridge.prg_rom.clone();
        let chr_rom = self.cpu.bus().cartridge.chr_rom.clone();
        let decoded_bus = decoded.cpu.bus_mut();
        decoded_bus.cartridge.prg_rom = prg_rom;
        decoded_bus.cartridge.chr_rom = chr_rom;
        // 輸出設定（開關、取樣率、聲道遮罩）不進存檔：從目前的實例帶過來；
        // 音訊濾波器與尚未取走的取樣則重設。
        let current = self.cpu.bus();
        decoded_bus
            .ppu
            .set_output_enabled(current.apu.output_enabled());
        decoded_bus.apu.adopt_output_settings(&current.apu);

        *self = decoded;
        Ok(())
    }

    /// 檢查所有「應該有固定長度」的 `Vec` 欄位是否真的符合硬體規格。
    ///
    /// postcard 對 `Vec<u8>` 是「長度前綴 + 內容」的編碼，理論上可以被竄改成
    /// 任意長度；這裡逐一驗證，避免之後的記憶體存取邏輯（Phase 1 的
    /// `Bus::read`/`write`）因為長度不對而 panic 或算出垃圾結果。
    fn validate_structure(&self) -> Result<(), StateError> {
        let bus = self.cpu.bus();

        if bus.ram.len() != 0x0800 {
            return Err(StateError::Corrupt);
        }
        if !bus.ppu.is_structurally_valid() || !bus.apu.is_structurally_valid() {
            return Err(StateError::Corrupt);
        }

        let expected_chr_ram_len = if bus.cartridge.info.chr_rom_banks == 0 {
            cartridge::CHR_RAM_SIZE
        } else {
            0
        };
        if bus.cartridge.chr_ram.len() != expected_chr_ram_len {
            return Err(StateError::Corrupt);
        }
        if bus.cartridge.prg_ram.len() != cartridge::PRG_RAM_SIZE {
            return Err(StateError::Corrupt);
        }
        // mapper 的種類必須與 header 一致、暫存器必須在硬體範圍內。
        if bus.cartridge.mapper.id() != bus.cartridge.info.mapper_id
            || !bus.cartridge.mapper.is_structurally_valid()
        {
            return Err(StateError::Corrupt);
        }

        Ok(())
    }

    /// 目前狀態的 xxh3-64 雜湊值，用來做 rollback / netplay 的 desync 偵測。
    pub fn state_hash(&self) -> u64 {
        xxhash_rust::xxh3::xxh3_64(&self.save_state())
    }

    /// 給 GUI Debugger 面板看的唯讀摘要。
    pub fn debug_snapshot(&self) -> DebugSnapshot {
        let bus = self.cpu.bus();
        DebugSnapshot {
            cpu_pc: self.cpu.pc,
            cpu_a: self.cpu.a,
            cpu_x: self.cpu.x,
            cpu_y: self.cpu.y,
            cpu_sp: self.cpu.sp,
            cpu_status: self.cpu.status.bits(),
            cpu_cycles: bus.total_cycles(),
            cpu_disassembly: self.cpu.current_disassembly(),
            cpu_jammed: self.cpu.jammed,
            frame_count: self.frame_count,
            ppu_scanline: bus.ppu.scanline,
            ppu_cycle: bus.ppu.cycle,
            ppu_frame: bus.ppu.frame,
            ppu_ctrl: bus.ppu.ctrl,
            ppu_mask: bus.ppu.mask,
            ppu_status: bus.ppu.status,
            ppu_oam_addr: bus.ppu.oam_addr,
            ppu_v: bus.ppu.v,
            ppu_t: bus.ppu.t,
            ppu_fine_x: bus.ppu.fine_x,
            ppu_w: bus.ppu.w,
            palette_ram: bus.ppu.palette,
            oam: bus.ppu.oam.clone(),
            apu: bus.apu.debug(),
            mapper_id: bus.cartridge.mapper.id(),
            mapper_name: bus.cartridge.mapper.name().to_string(),
            mapper_regs: bus.cartridge.mapper_debug_rows(),
        }
    }

    /// 目前的畫面緩衝（**可能只畫到一半**：單步除錯停在一幀中間時，已經走過
    /// 的掃描線是新的，其餘還是上一幀的內容）。給 Debugger 顯示用；一般遊戲邏輯
    /// 應該用 [`Nes::run_frame`] 的回傳值（一定是完整的一幀）。
    pub fn frame_buffer(&self) -> &FrameBuffer {
        self.cpu.bus().ppu.frame_buffer()
    }

    /// Debugger 用的 PPU 影像：2 張 pattern table（128×128）與 4 張 nametable
    /// （256×240）。`palette_index`（0–7）選擇 pattern table 用的調色盤：
    /// 0–3 背景、4–7 精靈。
    ///
    /// 唯讀、沒有副作用，但要畫 6 張圖，**只在 Debugger 面板開著且該分頁可見時
    /// 才呼叫**，不得每幀計算。
    pub fn debug_ppu_views(&self, palette_index: u8) -> PpuViews {
        let bus = self.cpu.bus();
        bus.ppu.build_views(&bus.cartridge, palette_index)
    }

    /// 目前載入的 ROM 中繼資料。
    pub fn rom_info(&self) -> &RomInfo {
        &self.cpu.bus().cartridge.info
    }

    /// 執行「一條」CPU 指令（不是一整幀），回傳這條指令花的 cycle 數。
    ///
    /// Debugger 的正式功能（單步除錯、trace 輸出），也給 `nes-test` 逐行比對
    /// nestest log 用。一般遊戲邏輯應該用 [`Nes::run_frame`]。
    ///
    /// # 只能在暫停狀態下使用
    ///
    /// 這個方法會打破「以幀為單位」的決定性：`run_frame` 保證每幀恰好推進到
    /// 固定的 cycle 預算，而這裡讓 CPU 停在幀的中間。之後再呼叫 `run_frame`
    /// 會從那個位置繼續、並且多/少跑一段，所以兩台機器只要有一台單步過，
    /// 兩邊的狀態就不再對得上。**netplay 進行中不得呼叫。**
    /// 呼叫端（例如 `nes-app` 的 emu 執行緒）必須先確保模擬已暫停。
    pub fn step_instruction(&mut self) -> u8 {
        self.cpu.step()
    }

    /// 目前這條（尚未執行的）指令的 nestest.log 格式 trace 行。
    ///
    /// 唯讀、沒有副作用（內部只用 `peek` 讀記憶體），任何時候都可以呼叫。
    pub fn trace(&self) -> String {
        self.cpu.trace()
    }

    /// side-effect-free 的記憶體讀取：不會觸發 PPU/APU 暫存器的讀取副作用
    /// （例如清除 vblank 旗標），任何時候都可以呼叫。
    pub fn peek(&self, addr: u16) -> u8 {
        self.cpu.bus().peek(addr)
    }

    /// 讀 PPU 位址空間（`$0000-$3FFF`：pattern table、nametable、調色盤）的一個
    /// byte，沒有副作用。給 Debugger 與測試工具用（例如讀出 test ROM 畫在
    /// nametable 上的文字）。
    pub fn peek_ppu(&self, addr: u16) -> u8 {
        let bus = self.cpu.bus();
        bus.ppu.read_memory(addr, &bus.cartridge)
    }

    /// 覆寫 PC。給 nestest 的「automation mode」用：先正常 `from_rom`
    /// （內部已經跑過一次真正的 reset），再手動把 PC 蓋成 `$C000`，跳過
    /// nestest.nes 裡需要人工按鍵互動的視覺測試選單。這是純測試用途的
    /// 旁路，只在 `testing` feature 下可用。
    #[cfg(feature = "testing")]
    pub fn override_pc(&mut self, pc: u16) {
        self.cpu.pc = pc;
    }

    pub fn frame_count(&self) -> u64 {
        self.frame_count
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

    /// 跟 `test_rom` header 相同、但內容不同的第二份 ROM，用來測試
    /// `load_state` 的 `rom_hash` 檢查。
    fn other_test_rom() -> Vec<u8> {
        let mut bytes = vec![0u8; HEADER_SIZE];
        bytes[0..4].copy_from_slice(b"NES\x1A");
        bytes[4] = 1; // PRG banks
        bytes[5] = 1; // CHR banks
        bytes.extend(vec![0xCCu8; PRG_BANK_SIZE]);
        bytes.extend(vec![0xDDu8; CHR_BANK_SIZE]);
        bytes
    }

    fn inputs_for(n: u64) -> Vec<[Buttons; 2]> {
        (0..n)
            .map(|i| {
                let p1 = if i % 3 == 0 {
                    Buttons::LEFT
                } else {
                    Buttons::empty()
                };
                let p2 = if i % 5 == 0 {
                    Buttons::A
                } else {
                    Buttons::empty()
                };
                [p1, p2]
            })
            .collect()
    }

    #[test]
    fn save_then_load_preserves_state_hash() {
        let mut nes = Nes::from_rom(&test_rom()).unwrap();
        for input in inputs_for(10) {
            nes.run_frame(input);
        }
        let before = nes.state_hash();
        let saved = nes.save_state();

        nes.load_state(&saved).unwrap();
        let after = nes.state_hash();

        assert_eq!(before, after);
    }

    #[test]
    fn identical_input_sequences_produce_identical_hashes_across_instances() {
        let rom = test_rom();
        let mut a = Nes::from_rom(&rom).unwrap();
        let mut b = Nes::from_rom(&rom).unwrap();

        for input in inputs_for(60) {
            a.run_frame(input);
            b.run_frame(input);
        }

        assert_eq!(a.state_hash(), b.state_hash());
        assert_eq!(
            a.cpu.bus().ppu.frame_buffer().as_bytes(),
            b.cpu.bus().ppu.frame_buffer().as_bytes()
        );
    }

    /// rollback 的核心性質：在第 k 幀存檔、跑到 k+m、讀回存檔、用「相同」的
    /// 輸入重新跑到 k+m，結果必須跟完全沒有讀檔時一模一樣。
    #[test]
    fn rollback_replay_matches_uninterrupted_run() {
        let rom = test_rom();
        let inputs = inputs_for(30);

        let mut baseline = Nes::from_rom(&rom).unwrap();
        for input in &inputs {
            baseline.run_frame(*input);
        }
        let baseline_hash = baseline.state_hash();

        let k = 12;
        let m = 10;
        let mut replay = Nes::from_rom(&rom).unwrap();
        for input in &inputs[..k] {
            replay.run_frame(*input);
        }
        let checkpoint = replay.save_state();

        for input in &inputs[k..k + m] {
            replay.run_frame(*input);
        }

        replay.load_state(&checkpoint).unwrap();
        for input in &inputs[k..] {
            replay.run_frame(*input);
        }

        assert_eq!(replay.state_hash(), baseline_hash);
    }

    #[test]
    fn load_state_rejects_truncated_bytes() {
        let mut nes = Nes::from_rom(&test_rom()).unwrap();
        let bytes = nes.save_state();
        let truncated = &bytes[..bytes.len() / 2];

        let result = nes.load_state(truncated);

        assert!(matches!(result, Err(StateError::Decode(_))));
    }

    #[test]
    fn load_state_rejects_tampered_length() {
        let rom = test_rom();

        // 手動破壞一份「有效」存檔的內部不變量（RAM 長度不再是 0x0800），
        // 藉此驗證 load_state 會在讀回這種資料時偵測到並拒絕，而不是照樣
        // 接受後讓後續的記憶體存取邏輯壞掉。
        let mut source = Nes::from_rom(&rom).unwrap();
        source.cpu.bus_mut().ram.push(0);
        let corrupted = source.save_state();

        let mut target = Nes::from_rom(&rom).unwrap();
        let result = target.load_state(&corrupted);

        assert!(matches!(result, Err(StateError::Corrupt)));
    }

    #[test]
    fn load_state_rejects_mismatched_rom() {
        let source = Nes::from_rom(&other_test_rom()).unwrap();
        let state_from_other_rom = source.save_state();

        let mut target = Nes::from_rom(&test_rom()).unwrap();
        let result = target.load_state(&state_from_other_rom);

        assert!(matches!(result, Err(StateError::RomMismatch { .. })));
    }

    /// 診斷 Debugger 面板顯示全 0 的 bug：確認 `from_rom`（內部呼叫
    /// `Cpu::reset`）之後 PC/SP/P 是硬體規定的重置值，而不是
    /// `Cpu::new` 給的「開機前」預設值（PC=0、SP=0xFD 剛好和重置值重疊、
    /// P=0x00）。
    #[test]
    fn from_rom_reset_state_matches_hardware_reset_vector() {
        let nes = Nes::from_rom(&test_rom()).unwrap();
        let bus = nes.cpu.bus();

        let lo = bus.peek(cpu::RESET_VECTOR);
        let hi = bus.peek(cpu::RESET_VECTOR.wrapping_add(1));
        let expected_pc = u16::from_le_bytes([lo, hi]);

        assert_eq!(nes.cpu.pc, expected_pc);
        assert_eq!(nes.cpu.sp, 0xFD);
        assert_eq!(nes.cpu.status.bits(), 0x24);
    }

    /// 確認 `run_frame` 真的有驅動 CPU 執行指令，而不是只推進 PPU／畫面。
    #[test]
    fn run_frame_advances_cpu_cycles_and_pc() {
        let mut nes = Nes::from_rom(&test_rom()).unwrap();
        let pc_before = nes.cpu.pc;
        let cycles_before = nes.cpu.bus().total_cycles();

        nes.run_frame([Buttons::empty(); 2]);

        assert!(nes.cpu.bus().total_cycles() > cycles_before);
        assert_ne!(nes.cpu.pc, pc_before);
    }

    /// 確認 `debug_snapshot()` 回傳的欄位跟 `Nes` 內部實際狀態一致，
    /// 且在跑過幾幀之後不等於 `DebugSnapshot::default()`——這正是
    /// Debugger 面板顯示全 0 那個 bug 想要防止再發生的性質。
    #[test]
    fn debug_snapshot_reflects_real_state_after_running_frames() {
        let mut nes = Nes::from_rom(&test_rom()).unwrap();
        for input in inputs_for(5) {
            nes.run_frame(input);
        }

        let snap = nes.debug_snapshot();

        assert_eq!(snap.cpu_pc, nes.cpu.pc);
        assert_eq!(snap.cpu_a, nes.cpu.a);
        assert_eq!(snap.cpu_x, nes.cpu.x);
        assert_eq!(snap.cpu_y, nes.cpu.y);
        assert_eq!(snap.cpu_sp, nes.cpu.sp);
        assert_eq!(snap.cpu_status, nes.cpu.status.bits());
        assert_eq!(snap.cpu_cycles, nes.cpu.bus().total_cycles());
        assert_eq!(snap.ppu_frame, nes.cpu.bus().ppu.frame);
        assert_eq!(snap.frame_count, 5);
        assert_ne!(snap, DebugSnapshot::default());
    }

    /// `step_instruction` 推進 PC 與 cycle 數；`trace` 是唯讀的（呼叫前後
    /// 狀態雜湊不變）且反映「下一條要執行」的指令；`peek` 沒有副作用。
    #[test]
    fn debugger_api_step_trace_peek() {
        let mut nes = Nes::from_rom(&test_rom()).unwrap();
        let hash_before = nes.state_hash();
        let line = nes.trace();
        assert_eq!(nes.state_hash(), hash_before, "trace 不得改變狀態");
        assert!(line.starts_with(&format!("{:04X}", nes.cpu.pc)));

        let cycles_before = nes.cpu.bus().total_cycles();
        let pc_before = nes.cpu.pc;
        let spent = nes.step_instruction();
        assert!(spent > 0);
        assert_eq!(nes.cpu.bus().total_cycles(), cycles_before + spent as u64);
        assert_ne!(nes.cpu.pc, pc_before);

        assert_eq!(nes.peek(0x8000), 0xAA);
        assert_eq!(nes.state_hash(), nes.state_hash());
    }

    // ---- Phase 2：PPU / NMI / DMA / 搖桿 的整合測試 ------------------------

    use crate::test_support::{Asm, build_nrom, rendering_rom, test_chr};

    fn framebuffer_hash(nes: &Nes) -> u64 {
        nes.cpu.bus().ppu.frame_buffer().hash64()
    }

    fn distinct_pixels(nes: &Nes) -> usize {
        let mut colors = std::collections::BTreeSet::new();
        for px in nes
            .cpu
            .bus()
            .ppu
            .frame_buffer()
            .as_bytes()
            .as_chunks::<4>()
            .0
        {
            colors.insert([px[0], px[1], px[2]]);
        }
        colors.len()
    }

    /// 竄改存檔裡 PPU 的掃描線/dot/v 之類的欄位（超出硬體範圍）必須被拒絕，而不是
    /// 讀進來之後在 run_frame 裡 panic。
    #[test]
    fn load_state_rejects_out_of_range_ppu_fields() {
        let rom = rendering_rom();
        let mut source = Nes::from_rom(&rom).unwrap();
        source.run_frame([Buttons::empty(); 2]);

        for tamper in [
            (|n: &mut Nes| n.cpu.bus_mut().ppu.scanline = 500) as fn(&mut Nes),
            |n| n.cpu.bus_mut().ppu.cycle = 9999,
            |n| n.cpu.bus_mut().ppu.v = 0xFFFF,
            |n| n.cpu.bus_mut().ppu.fine_x = 200,
        ] {
            let mut bad = source.clone();
            tamper(&mut bad);
            let bytes = bad.save_state();

            let mut target = Nes::from_rom(&rom).unwrap();
            assert!(matches!(
                target.load_state(&bytes),
                Err(StateError::Corrupt)
            ));
        }
    }

    #[test]
    fn four_screen_rom_is_rejected() {
        let mut rom = test_rom();
        rom[6] |= 0x08;
        assert!(matches!(
            Nes::from_rom(&rom),
            Err(RomError::FourScreenUnsupported)
        ));
    }

    #[test]
    fn rendering_rom_draws_a_real_picture_and_takes_nmis() {
        let mut nes = Nes::from_rom(&rendering_rom()).unwrap();
        for _ in 0..10 {
            nes.run_frame([Buttons::empty(); 2]);
        }
        // 背景 tile（多種顏色）+ 精靈：一張真的畫面，不是單色。
        assert!(distinct_pixels(&nes) >= 5, "畫面顏色數太少");
        // NMI 處理常式每幀把 $00 加 1；初始化在第一幀內完成，之後每幀一次 NMI。
        let nmis = nes.peek(0x0000);
        assert!(
            (8..=10).contains(&nmis),
            "10 幀應該約 9–10 次 NMI，實際 {nmis}"
        );
    }

    #[test]
    fn run_frame_ends_at_the_start_of_vblank() {
        let mut nes = Nes::from_rom(&rendering_rom()).unwrap();
        for _ in 0..3 {
            nes.run_frame([Buttons::empty(); 2]);
            let ppu = &nes.cpu.bus().ppu;
            assert_eq!(ppu.scanline, 241, "幀邊界在 vblank 起點");
            assert!(ppu.cycle < 40, "最多越過一條指令的 dot 數");
            assert_ne!(ppu.status & crate::ppu::STATUS_VBLANK, 0);
        }
    }

    #[test]
    fn rendering_is_deterministic_across_instances() {
        let rom = rendering_rom();
        let mut a = Nes::from_rom(&rom).unwrap();
        let mut b = Nes::from_rom(&rom).unwrap();
        for input in inputs_for(60) {
            a.run_frame(input);
            b.run_frame(input);
            assert_eq!(framebuffer_hash(&a), framebuffer_hash(&b));
        }
        assert_eq!(a.state_hash(), b.state_hash());
    }

    /// rollback 性質：在真的有畫面的情況下（PPU 狀態、捲動暫存器、NMI 都在動），
    /// 於第 k 幀存檔、往前跑、讀檔、用相同輸入重跑，結果（狀態雜湊與畫面）必須跟
    /// 沒中斷的一路跑完完全相同。
    #[test]
    fn rollback_replay_matches_uninterrupted_run_with_real_rendering() {
        let rom = rendering_rom();
        let inputs = inputs_for(40);

        let mut baseline = Nes::from_rom(&rom).unwrap();
        for input in &inputs {
            baseline.run_frame(*input);
        }

        let mut replay = Nes::from_rom(&rom).unwrap();
        for input in &inputs[..15] {
            replay.run_frame(*input);
        }
        let checkpoint = replay.save_state();
        for input in &inputs[15..30] {
            replay.run_frame(*input);
        }
        replay.load_state(&checkpoint).unwrap();
        for input in &inputs[15..] {
            replay.run_frame(*input);
        }

        assert_eq!(replay.state_hash(), baseline.state_hash());
        assert_eq!(framebuffer_hash(&replay), framebuffer_hash(&baseline));
    }

    /// 存檔必須包含 CPU 與 PPU 之間的相位：在「一幀中間」（單步除錯停下來）
    /// 存檔、讀檔後繼續，結果要跟不中斷的一路跑完相同。
    #[test]
    fn mid_frame_save_state_preserves_cpu_ppu_phase() {
        let rom = rendering_rom();
        let input = [Buttons::empty(); 2];

        let mut baseline = Nes::from_rom(&rom).unwrap();
        for _ in 0..5 {
            baseline.run_frame(input);
        }
        for _ in 0..4000 {
            baseline.step_instruction();
        }
        for _ in 0..5 {
            baseline.run_frame(input);
        }

        let mut replay = Nes::from_rom(&rom).unwrap();
        for _ in 0..5 {
            replay.run_frame(input);
        }
        for _ in 0..4000 {
            replay.step_instruction();
        }
        let mid_frame = replay.save_state();
        let ppu_position = (replay.cpu.bus().ppu.scanline, replay.cpu.bus().ppu.cycle);
        replay.run_frame(input); // 讓狀態走遠
        replay.load_state(&mid_frame).unwrap();
        assert_eq!(
            (replay.cpu.bus().ppu.scanline, replay.cpu.bus().ppu.cycle),
            ppu_position,
            "讀檔後 PPU 掃描線/dot 位置必須還原"
        );
        for _ in 0..5 {
            replay.run_frame(input);
        }

        assert_eq!(replay.state_hash(), baseline.state_hash());
        assert_eq!(framebuffer_hash(&replay), framebuffer_hash(&baseline));
    }

    /// 黃金畫面：`rendering_rom` 在固定輸入下跑 30 幀的畫面雜湊。任何讓畫面
    /// 改變的 PPU 修改（不論對錯）都會讓這個測試失敗，強迫作者確認變動是預期的，
    /// 再更新常數。只存雜湊，不存圖片。
    #[test]
    fn golden_frame_hash_of_rendering_rom() {
        let mut nes = Nes::from_rom(&rendering_rom()).unwrap();
        for _ in 0..30 {
            nes.run_frame([Buttons::empty(); 2]);
        }
        assert_eq!(
            framebuffer_hash(&nes),
            GOLDEN_RENDERING_ROM_30_FRAMES,
            "畫面雜湊改變了；如果是預期的 PPU 變更，請更新 GOLDEN_RENDERING_ROM_30_FRAMES"
        );
    }

    const GOLDEN_RENDERING_ROM_30_FRAMES: u64 = 0x7647a9d5508a0acf;

    /// 搖桿：一幀開始時鎖定輸入，程式 strobe 之後讀 8 次得到 A、B、Select、Start、
    /// 上、下、左、右。
    #[test]
    fn joypad_reads_reflect_the_input_locked_at_frame_start() {
        let mut code = Asm::new(0x8000);
        code.lda_imm(1)
            .sta_abs(0x4016)
            .lda_imm(0)
            .sta_abs(0x4016)
            .ldx_imm(0);
        let l = code.pc();
        code.lda_abs(0x4016)
            .and_imm(1)
            .sta_abs_x(0x0200)
            .inx()
            .cpx_imm(8)
            .bne(l);
        let forever = code.pc();
        code.jmp(forever);
        let rom = build_nrom(&code, 0x8000, None, &[], &test_chr(), false);

        let mut nes = Nes::from_rom(&rom).unwrap();
        nes.run_frame([
            Buttons::A | Buttons::START | Buttons::RIGHT,
            Buttons::empty(),
        ]);

        let bits: Vec<u8> = (0..8).map(|i| nes.peek(0x0200 + i)).collect();
        assert_eq!(bits, [1, 0, 0, 1, 0, 0, 0, 1]);
    }

    /// `STA $4014` 觸發 OAM DMA：256 byte 進 OAM，CPU 暫停 513 或 514 個 cycle。
    #[test]
    fn sta_4014_runs_oam_dma_and_stalls_the_cpu() {
        let mut code = Asm::new(0x8000);
        code.lda_imm(0x5A)
            .sta_abs(0x0203)
            .lda_imm(0x02)
            .sta_abs(0x4014);
        let forever = code.pc();
        code.jmp(forever);
        let rom = build_nrom(&code, 0x8000, None, &[], &test_chr(), false);

        let mut nes = Nes::from_rom(&rom).unwrap();
        for _ in 0..3 {
            nes.step_instruction(); // LDA, STA, LDA
        }
        let before = nes.cpu.bus().total_cycles();
        let store_cycles = nes.step_instruction() as u64; // STA $4014（含 DMA）
        let elapsed = nes.cpu.bus().total_cycles() - before;

        assert_eq!(nes.cpu.bus().ppu.oam[3], 0x5A);
        let stall = elapsed - store_cycles;
        assert!(stall == 513 || stall == 514, "DMA 暫停 {stall} cycles");
    }

    #[test]
    fn reset_clears_ppu_registers_but_keeps_ram() {
        let mut nes = Nes::from_rom(&rendering_rom()).unwrap();
        for _ in 0..3 {
            nes.run_frame([Buttons::empty(); 2]);
        }
        assert_ne!(nes.cpu.bus().ppu.mask, 0);
        assert_ne!(nes.peek(0x0000), 0);

        nes.reset();
        assert_eq!(nes.cpu.bus().ppu.mask, 0);
        assert_eq!(nes.cpu.bus().ppu.ctrl, 0);
        assert_ne!(nes.peek(0x0000), 0, "RAM 內容 reset 後保留");
        assert_eq!(nes.cpu.pc, 0x8000);
    }

    #[test]
    fn debug_ppu_views_have_expected_shapes_and_show_the_nametable() {
        let mut nes = Nes::from_rom(&rendering_rom()).unwrap();
        for _ in 0..3 {
            nes.run_frame([Buttons::empty(); 2]);
        }
        let views = nes.debug_ppu_views(0);
        assert_eq!(views.pattern_tables[0].width, 128);
        assert_eq!(views.pattern_tables[0].height, 128);
        assert_eq!(views.nametables[0].width, 256);
        assert_eq!(views.nametables[0].height, 240);
        // rendering_rom 用 horizontal mirroring：$2000 與 $2400 相同。
        assert_eq!(views.nametables[0], views.nametables[1]);
        // 只顯示背景 tile 的 nametable 影像不該是單色。
        let colors: std::collections::BTreeSet<_> = views.nametables[0]
            .rgba
            .as_chunks::<4>()
            .0
            .iter()
            .map(|p| [p[0], p[1], p[2]])
            .collect();
        assert!(colors.len() >= 3);
    }

    /// 「run_frame 路徑不得 panic」：不論 PRG/CHR 內容是什麼垃圾（隨機位元組會
    /// 執行到未定義行為的 opcode、亂寫 PPU 暫存器、亂觸發 DMA 與 NMI），跑幾幀都不
    /// 可以 panic（debug 建置有溢位檢查）。用固定種子的 xorshift，結果可重現。
    #[test]
    fn random_garbage_roms_never_panic_in_run_frame() {
        fn xorshift(state: &mut u64) -> u8 {
            *state ^= *state << 13;
            *state ^= *state >> 7;
            *state ^= *state << 17;
            (*state >> 24) as u8
        }

        for seed in 1..=24u64 {
            let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
            let mut rom = vec![0u8; 16];
            rom[0..4].copy_from_slice(b"NES");
            rom[4] = 1 + (seed % 2) as u8; // 16KB 或 32KB PRG
            rom[5] = (seed % 3) as u8; // 0（CHR-RAM）、1、2 個 CHR bank
            rom[6] = (seed % 2) as u8; // 交替 mirroring
            for _ in 0..(rom[4] as usize * 0x4000 + rom[5] as usize * 0x2000) {
                rom.push(xorshift(&mut state));
            }
            let mut nes = Nes::from_rom(&rom).unwrap();
            for frame in 0..4 {
                let pad = xorshift(&mut state);
                nes.run_frame([Buttons::from_bits_truncate(pad), Buttons::empty()]);
                let _ = nes.debug_snapshot();
                if frame == 2 {
                    let _ = nes.debug_ppu_views(pad);
                    let saved = nes.save_state();
                    nes.load_state(&saved).unwrap();
                }
            }
        }
    }

    /// SMB 式的畫面分割：等 sprite 0 hit 之後在 hblank 前改水平捲動。合成 ROM 每幀：
    /// 等 vblank → 捲動歸零 → 等 `$2002` bit6（sprite 0 hit）先清除再設起 → 寫 `$2005`
    /// 水平捲動 8 px。
    /// sprite 0 在第 40 條掃描線命中，所以第 0–40 條線用捲動 0，第 41 條線起用捲動 8
    /// （水平位置的 hori(v)=hori(t) 發生在該條線的 dot 257）。
    #[test]
    fn sprite0_split_scroll_takes_effect_on_the_next_scanline() {
        let rom = sprite0_split_rom();
        let mut nes = Nes::from_rom(&rom).unwrap();
        for _ in 0..4 {
            nes.run_frame([Buttons::empty(); 2]);
        }
        check_sprite0_split(&nes);
    }

    /// 見 `sprite0_split_scroll_takes_effect_on_the_next_scanline`。
    fn sprite0_split_rom() -> Vec<u8> {
        let mut a = Asm::new(0x8000);
        a.sei().cld().ldx_imm(0xFF).txs();
        a.set_ppu_addr(0x3F00);
        for color in [0x0F, 0x16, 0x2A, 0x30] {
            a.lda_imm(color).sta_abs(0x2007);
        }
        a.set_ppu_addr(0x2000).ldx_imm(0);
        for _ in 0..2 {
            let l = a.pc();
            a.txa().and_imm(0x07).sta_abs(0x2007).inx().bne(l);
        }
        // sprite 0：Y = 39（第一列在掃描線 40）、tile 3（純色 3）、X = 100。
        a.lda_imm(0).sta_abs(0x2003);
        for byte in [39, 3, 0, 100] {
            a.lda_imm(byte).sta_abs(0x2004);
        }
        a.lda_imm(0x00).sta_abs(0x2000);
        a.lda_imm(0x1E).sta_abs(0x2001);

        let wait_vblank = a.pc();
        a.bit_abs(0x2002).bpl(wait_vblank);
        a.lda_imm(0).sta_abs(0x2005).sta_abs(0x2005);
        // 先等上一幀的 sprite 0 hit 旗標在 pre-render 行被清掉，再等這一幀的命中
        // （SMB 也是這樣做，否則會在 vblank 就看到舊的旗標）。
        let wait_clear = a.pc();
        a.bit_abs(0x2002).bvs(wait_clear);
        let wait_hit = a.pc();
        a.bit_abs(0x2002).bvc(wait_hit);
        a.lda_imm(8).sta_abs(0x2005).lda_imm(0).sta_abs(0x2005);
        a.jmp(wait_vblank);

        build_nrom(&a, 0x8000, None, &[], &test_chr(), false)
    }

    fn check_sprite0_split(nes: &Nes) {
        let fb = nes.cpu.bus().ppu.frame_buffer().as_bytes();
        let px = |x: usize, y: usize| {
            let i = (y * crate::frame::WIDTH + x) * 4;
            [fb[i], fb[i + 1], fb[i + 2]]
        };
        let backdrop = crate::ppu::SYSTEM_PALETTE[0x0F];
        let color1 = crate::ppu::SYSTEM_PALETTE[0x16];

        // 第 0 欄是 tile 0（空白）→ 背景色；捲動 8 px 之後第 0 欄變成 tile 1（純色 1）。
        // （nametable 只填了前 16 列 tile，所以只檢查到第 127 條線。）
        for y in [0, 10, 39, 40] {
            assert_eq!(px(4, y), backdrop, "第 {y} 條線應該用捲動 0");
        }
        for y in [41, 42, 100, 120] {
            assert_eq!(px(4, y), color1, "第 {y} 條線應該用捲動 8");
        }
    }

    // ---- Phase 3：存檔版本、mapper、行為指紋 -------------------------------------

    use crate::state::{CORE_BEHAVIOR_VERSION, HEADER_LEN, STATE_FORMAT_VERSION, STATE_MAGIC};
    use crate::test_support::{CHR_MARK, PRG_MARK, build_mapper_rom};

    /// 一份什麼都不做的 mapper 測試 ROM（程式碼只有一個無窮迴圈）。
    fn idle_mapper_rom(mapper: u8, prg_banks: usize, chr_banks_8k: usize) -> Vec<u8> {
        let mut code = Asm::new(0xE000);
        let forever = code.pc();
        code.jmp(forever);
        build_mapper_rom(mapper, prg_banks, chr_banks_8k, &code, &[])
    }

    /// 經 CPU 匯流排送 5 次寫入，把 `value` 的低 5 bit（bit 0 先）寫進 MMC1 的暫存器。
    fn mmc1_write(nes: &mut Nes, addr: u16, value: u8) {
        for i in 0..5 {
            nes.cpu.bus_mut().write(addr, (value >> i) & 1);
        }
    }

    fn mapper_row(nes: &Nes, key: &str) -> String {
        nes.debug_snapshot()
            .mapper_regs
            .into_iter()
            .find(|(k, _)| k == key)
            .unwrap_or_else(|| panic!("找不到 mapper 資訊列 {key}"))
            .1
    }

    #[test]
    fn save_state_starts_with_magic_and_both_version_numbers() {
        let bytes = Nes::from_rom(&test_rom()).unwrap().save_state();
        assert!(bytes.len() > HEADER_LEN);
        assert_eq!(bytes[0..4], STATE_MAGIC);
        assert_eq!(
            u16::from_le_bytes([bytes[4], bytes[5]]),
            STATE_FORMAT_VERSION
        );
        assert_eq!(
            u16::from_le_bytes([bytes[6], bytes[7]]),
            CORE_BEHAVIOR_VERSION
        );
    }

    /// 竄改 header 的某個位置後讀檔，必須被 `VersionMismatch` 拒絕；且 `expected`
    /// 是目前版本、`found` 是被竄改後的內容。
    fn assert_header_tamper_rejected(tamper: impl Fn(&mut Vec<u8>)) -> StateError {
        let mut nes = Nes::from_rom(&test_rom()).unwrap();
        let mut bytes = nes.save_state();
        tamper(&mut bytes);
        let before = nes.state_hash();
        let err = nes
            .load_state(&bytes)
            .expect_err("竄改過的 header 必須被拒絕");
        assert_eq!(nes.state_hash(), before, "被拒絕的讀檔不得改動狀態");
        err
    }

    #[test]
    fn load_state_rejects_tampered_magic() {
        let err = assert_header_tamper_rejected(|b| b[0] ^= 0xFF);
        let StateError::VersionMismatch { expected, found } = err else {
            panic!("應為 VersionMismatch，實際 {err:?}");
        };
        assert_eq!(expected, crate::StateHeader::CURRENT);
        assert_ne!(found.magic, STATE_MAGIC);
        assert_eq!(found.format_version, STATE_FORMAT_VERSION);
    }

    #[test]
    fn load_state_rejects_tampered_format_version() {
        let err = assert_header_tamper_rejected(|b| {
            let v = STATE_FORMAT_VERSION + 1;
            b[4..6].copy_from_slice(&v.to_le_bytes());
        });
        let StateError::VersionMismatch { expected, found } = err else {
            panic!("應為 VersionMismatch，實際 {err:?}");
        };
        assert_eq!(expected.format_version, STATE_FORMAT_VERSION);
        assert_eq!(found.format_version, STATE_FORMAT_VERSION + 1);
        assert_eq!(found.core_version, CORE_BEHAVIOR_VERSION);
    }

    #[test]
    fn load_state_rejects_tampered_core_behavior_version() {
        let err = assert_header_tamper_rejected(|b| {
            let v = CORE_BEHAVIOR_VERSION + 1;
            b[6..8].copy_from_slice(&v.to_le_bytes());
        });
        let StateError::VersionMismatch { expected, found } = err else {
            panic!("應為 VersionMismatch，實際 {err:?}");
        };
        assert_eq!(expected.core_version, CORE_BEHAVIOR_VERSION);
        assert_eq!(found.core_version, CORE_BEHAVIOR_VERSION + 1);
        assert_eq!(found.format_version, STATE_FORMAT_VERSION);
    }

    #[test]
    fn load_state_rejects_input_shorter_than_the_header() {
        let mut nes = Nes::from_rom(&test_rom()).unwrap();
        assert!(matches!(nes.load_state(b"NES"), Err(StateError::Decode(_))));
        assert!(matches!(nes.load_state(&[]), Err(StateError::Decode(_))));
    }

    /// 行為指紋：把 `CORE_BEHAVIOR_VERSION` 與「合成 ROM 在固定輸入下的狀態雜湊」綁在一起。
    /// 這個測試失敗代表模擬結果（狀態或存檔格式）變了：確認變動是預期的之後，
    /// **必須同時**遞增 `CORE_BEHAVIOR_VERSION`（或 `STATE_FORMAT_VERSION`）並更新這裡的
    /// 兩個常數。判定規則見 `docs/architecture.md` §15。
    #[test]
    fn behavior_fingerprint_is_pinned_to_the_version_numbers() {
        const PINNED_CORE_BEHAVIOR_VERSION: u16 = 3;
        const PINNED_STATE_FORMAT_VERSION: u16 = 2;
        const FINGERPRINT_NROM: u64 = 0xcbf95010461fa807;
        const FINGERPRINT_MMC1: u64 = 0xde9bf3d88ee097c0;
        const FINGERPRINT_DUMMY_READ: u64 = 0x7ede5740dc7425f7;
        const FINGERPRINT_APU_PROBE: u64 = 0xe83244a5491484db;

        let mut nrom = Nes::from_rom(&rendering_rom()).unwrap();
        for input in inputs_for(60) {
            nrom.run_frame(input);
        }
        let mut mmc1 = Nes::from_rom(&mmc1_churn_rom()).unwrap();
        for input in inputs_for(60) {
            mmc1.run_frame(input);
        }
        let mut probe = Nes::from_rom(&crate::test_support::dummy_read_probe_rom()).unwrap();
        for input in inputs_for(60) {
            probe.run_frame(input);
        }
        // frame IRQ、`$4015`、DMC（IRQ 與抓取樣本暫停 CPU）、各聲道的計數器。
        let mut apu = Nes::from_rom(&crate::test_support::apu_probe_rom(
            crate::test_support::ApuProbe::DEFAULT,
        ))
        .unwrap();
        for input in inputs_for(60) {
            apu.run_frame(input);
        }
        let actual = (
            CORE_BEHAVIOR_VERSION,
            STATE_FORMAT_VERSION,
            nrom.state_hash(),
            mmc1.state_hash(),
            probe.state_hash(),
            apu.state_hash(),
        );
        assert_eq!(
            actual,
            (
                PINNED_CORE_BEHAVIOR_VERSION,
                PINNED_STATE_FORMAT_VERSION,
                FINGERPRINT_NROM,
                FINGERPRINT_MMC1,
                FINGERPRINT_DUMMY_READ,
                FINGERPRINT_APU_PROBE
            ),
            "模擬行為改變：請確認是預期的，遞增版本號並更新指紋（見測試文件）"
        );
    }

    /// APU 探針對它用到的每個功能都敏感：改變 frame counter 模式、IRQ 抑制、有沒有確認
    /// `$4015`、有沒有 DMC，最後的狀態雜湊都不同（否則行為指紋抓不到那部分的退步）。
    #[test]
    fn apu_probe_hash_depends_on_each_apu_feature_it_exercises() {
        use crate::test_support::{ApuProbe, apu_probe_rom};
        let hash = |probe: ApuProbe| {
            let mut nes = Nes::from_rom(&apu_probe_rom(probe)).unwrap();
            for input in inputs_for(60) {
                nes.run_frame(input);
            }
            nes.state_hash()
        };
        let base = ApuProbe::DEFAULT;
        let hashes = [
            hash(base),
            hash(ApuProbe {
                frame_counter: 0x40, // 抑制 frame IRQ
                ..base
            }),
            hash(ApuProbe {
                frame_counter: 0x80, // 5 步模式（沒有 frame IRQ）
                ..base
            }),
            hash(ApuProbe {
                ack_frame_irq: false, // 不讀 $4015 → frame IRQ 旗標不會清
                ..base
            }),
            hash(ApuProbe { dmc: false, ..base }),
        ];
        for i in 0..hashes.len() {
            for j in i + 1..hashes.len() {
                assert_ne!(hashes[i], hashes[j], "變體 {i} 與 {j} 的狀態雜湊相同");
            }
        }
    }

    /// 輸出（畫面與音訊）開關：同樣的輸入跑 N 幀，開與關的 `state_hash` 必須逐幀相同。
    /// 涵蓋會依賴渲染結果的狀態（sprite 0 hit 的 ROM）與 APU（IRQ、DMC）。
    #[test]
    fn output_switch_does_not_change_the_state_hash() {
        use crate::test_support::{ApuProbe, apu_probe_rom};
        let roms: [(&str, Vec<u8>); 5] = [
            ("rendering", rendering_rom()),
            ("mmc1", mmc1_churn_rom()),
            ("dummy read", crate::test_support::dummy_read_probe_rom()),
            ("apu probe", apu_probe_rom(ApuProbe::DEFAULT)),
            ("sprite 0 split", sprite0_split_rom()),
        ];
        for (name, rom) in roms {
            let mut on = Nes::from_rom(&rom).unwrap();
            let mut off = Nes::from_rom(&rom).unwrap();
            let mut toggled = Nes::from_rom(&rom).unwrap();
            off.set_output_enabled(false);
            assert!(on.output_enabled() && !off.output_enabled());
            for (frame, input) in inputs_for(60).into_iter().enumerate() {
                // toggled：第 20–39 幀關閉，其餘開啟。
                toggled.set_output_enabled(!(20..40).contains(&frame));
                on.run_frame(input);
                off.run_frame(input);
                toggled.run_frame(input);
                assert_eq!(on.state_hash(), off.state_hash(), "{name}：第 {frame} 幀");
                assert_eq!(
                    on.state_hash(),
                    toggled.state_hash(),
                    "{name}：切換，第 {frame} 幀"
                );
            }
            let (mut from_on, mut from_off) = (Vec::new(), Vec::new());
            on.drain_audio(&mut from_on);
            off.drain_audio(&mut from_off);
            assert!(!from_on.is_empty(), "{name}：開啟時有音訊");
            assert!(from_off.is_empty(), "{name}：關閉時沒有音訊");
        }

        // 關閉時不寫 framebuffer（維持關閉當下的內容），開啟時每幀都重畫。
        // （`from_rom` 的 reset 序列已經畫了第 0 條掃描線，所以基準是關閉當下的畫面。）
        let mut on = Nes::from_rom(&rendering_rom()).unwrap();
        let mut off = Nes::from_rom(&rendering_rom()).unwrap();
        off.set_output_enabled(false);
        let at_switch = off.frame_buffer().as_bytes().to_vec();
        for input in inputs_for(10) {
            on.run_frame(input);
            off.run_frame(input);
        }
        assert_ne!(on.frame_buffer().as_bytes(), &at_switch[..]);
        assert_eq!(off.frame_buffer().as_bytes(), &at_switch[..]);
    }

    /// 輸出設定（開關、取樣率、聲道遮罩）不進存檔：`load_state` 之後保留目前的設定，
    /// 但清掉濾波器與尚未取走的取樣。
    #[test]
    fn load_state_keeps_output_settings_and_resets_the_audio_signal() {
        let rom = crate::test_support::apu_probe_rom(crate::test_support::ApuProbe::DEFAULT);
        let mut nes = Nes::from_rom(&rom).unwrap();
        for _ in 0..10 {
            nes.run_frame([Buttons::empty(); 2]);
        }
        let saved = nes.save_state();
        nes.set_audio_sample_rate(44_100.0);
        nes.set_audio_channel_mask(apu::CHANNEL_TRIANGLE | apu::CHANNEL_NOISE);
        nes.run_frame([Buttons::empty(); 2]);
        assert!(nes.cpu.bus().apu.buffered_samples() > 0);

        nes.load_state(&saved).unwrap();
        assert_eq!(nes.audio_sample_rate(), 44_100.0);
        assert_eq!(
            nes.audio_channel_mask(),
            apu::CHANNEL_TRIANGLE | apu::CHANNEL_NOISE
        );
        assert!(nes.output_enabled());
        assert_eq!(nes.cpu.bus().apu.buffered_samples(), 0);

        nes.set_output_enabled(false);
        nes.load_state(&saved).unwrap();
        assert!(!nes.output_enabled(), "關閉狀態也保留");
    }

    /// 一個會不斷改寫 mapper 暫存器的 MMC1 程式：無窮迴圈 `INC $00; LDA $00; STA $E000`，
    /// 讓 5 次寫入一輪地載入 PRG bank 暫存器，bank 值隨計數器變化。
    fn mmc1_churn_rom() -> Vec<u8> {
        let mut code = Asm::new(0xE000);
        let l = code.pc();
        code.inc_abs(0x0000).lda_abs(0x0000).sta_abs(0xE000).jmp(l);
        build_mapper_rom(1, 8, 2, &code, &[])
    }

    #[test]
    fn mmc1_prg_bank_switching_through_the_bus() {
        let mut nes = Nes::from_rom(&idle_mapper_rom(1, 8, 2)).unwrap();
        // 開機：模式 3，$8000 = bank 0、$C000 = 最後一個 bank。
        assert_eq!(nes.peek(0x8100), PRG_MARK);
        assert_eq!(nes.peek(0xC100), PRG_MARK + 7);

        mmc1_write(&mut nes, 0xE000, 3);
        assert_eq!(nes.peek(0x8100), PRG_MARK + 3);
        assert_eq!(nes.peek(0xC100), PRG_MARK + 7, "$C000 固定最後一個 bank");

        // 模式 2：$8000 固定第一個、$C000 切換。
        mmc1_write(&mut nes, 0x8000, 0b01010);
        mmc1_write(&mut nes, 0xE000, 5);
        assert_eq!(nes.peek(0x8100), PRG_MARK);
        assert_eq!(nes.peek(0xC100), PRG_MARK + 5);

        // 模式 0：32KB，bank 5 → 4、5。
        mmc1_write(&mut nes, 0x8000, 0b00010);
        assert_eq!(nes.peek(0x8100), PRG_MARK + 4);
        assert_eq!(nes.peek(0xC100), PRG_MARK + 5);
    }

    #[test]
    fn mmc1_chr_bank_switching_is_visible_to_the_ppu() {
        let mut nes = Nes::from_rom(&idle_mapper_rom(1, 2, 2)).unwrap(); // 4 個 4KB CHR
        // 4KB 模式：兩個獨立的 4KB。
        mmc1_write(&mut nes, 0x8000, 0b11110);
        mmc1_write(&mut nes, 0xA000, 3);
        mmc1_write(&mut nes, 0xC000, 1);
        assert_eq!(nes.peek_ppu(0x0000), CHR_MARK + 3);
        assert_eq!(nes.peek_ppu(0x1000), CHR_MARK + 1);
        // 8KB 模式：CHR bank 0 = 3 → 忽略最低位 → 4KB bank 2、3。
        mmc1_write(&mut nes, 0x8000, 0b01110);
        assert_eq!(nes.peek_ppu(0x0000), CHR_MARK + 2);
        assert_eq!(nes.peek_ppu(0x1000), CHR_MARK + 3);
    }

    /// PPU 每次存取 nametable 都要查 mapper「目前」的 mirroring：同一份 VRAM 內容，
    /// 隨 MMC1 control 改變而讀到不同的邏輯配置。
    #[test]
    fn mmc1_mirroring_is_looked_up_on_every_nametable_access() {
        let mut nes = Nes::from_rom(&idle_mapper_rom(1, 2, 1)).unwrap();
        let ppu_write = |nes: &mut Nes, addr: u16, value: u8| {
            let bus = nes.cpu.bus_mut();
            bus.write(0x2006, (addr >> 8) as u8);
            bus.write(0x2006, addr as u8);
            bus.write(0x2007, value);
        };
        let tables = |nes: &Nes| [0x2000u16, 0x2400, 0x2800, 0x2C00].map(|a| nes.peek_ppu(a));

        // 先在 vertical（control mirroring = 2）下寫兩張不同的實體 nametable。
        mmc1_write(&mut nes, 0x8000, 0b01110);
        ppu_write(&mut nes, 0x2000, 0x11);
        ppu_write(&mut nes, 0x2400, 0x22);
        assert_eq!(tables(&nes), [0x11, 0x22, 0x11, 0x22], "vertical");

        mmc1_write(&mut nes, 0x8000, 0b01111); // horizontal
        assert_eq!(tables(&nes), [0x11, 0x11, 0x22, 0x22], "horizontal");

        mmc1_write(&mut nes, 0x8000, 0b01100); // 單畫面 A
        assert_eq!(tables(&nes), [0x11; 4], "single screen A");

        mmc1_write(&mut nes, 0x8000, 0b01101); // 單畫面 B
        assert_eq!(tables(&nes), [0x22; 4], "single screen B");

        // 單畫面下經 PPU 寫入，落在被選到的那張實體 nametable。
        ppu_write(&mut nes, 0x2C05, 0x77);
        mmc1_write(&mut nes, 0x8000, 0b01110);
        assert_eq!(nes.peek_ppu(0x2405), 0x77);
        assert_eq!(nes.debug_snapshot().mapper_id, 1);
    }

    #[test]
    fn mmc1_prg_ram_disable_hides_it_from_reads_and_writes() {
        let mut nes = Nes::from_rom(&idle_mapper_rom(1, 2, 1)).unwrap();
        nes.cpu.bus_mut().write(0x6000, 0x55);
        assert_eq!(nes.peek(0x6000), 0x55);

        mmc1_write(&mut nes, 0xE000, 0x10); // PRG-RAM 停用
        nes.cpu.bus_mut().write(0x6000, 0x99); // 被忽略
        let open_bus = nes.cpu.bus_mut().read(0x6000);
        assert_ne!(open_bus, 0x55, "停用時讀到 open bus，不是 RAM 內容");

        mmc1_write(&mut nes, 0xE000, 0x00); // 重新啟用：內容還在、停用期間的寫入沒有生效
        assert_eq!(nes.peek(0x6000), 0x55);
    }

    /// 讀-改-寫指令對 MMC1 只有「舊值」那次寫入生效：`INC $9000`（ROM 該處是 `$00`）
    /// 先寫 `$00`（進入移位暫存器）、下一個 cycle 才寫 `$01`（被忽略），所以只收到 1 個 bit。
    #[test]
    fn mmc1_rmw_instruction_counts_as_a_single_serial_write() {
        let mut code = Asm::new(0xE000);
        code.inc_abs(0x9000);
        let forever = code.pc();
        code.jmp(forever);
        let rom = build_mapper_rom(1, 2, 1, &code, &[(0x9000, &[0x00])]);
        let mut nes = Nes::from_rom(&rom).unwrap();
        assert_eq!(nes.peek(0x9000), 0x00);

        nes.step_instruction(); // INC $9000
        assert!(
            mapper_row(&nes, "移位暫存器").contains("已寫入 1 / 5"),
            "RMW 的兩次相鄰寫入只算一次：{}",
            mapper_row(&nes, "移位暫存器")
        );

        // 對照：兩條獨立的 STA（相隔 ≥ 2 個 cycle）各算一次。
        let mut code = Asm::new(0xE000);
        code.lda_imm(0).sta_abs(0x9000).sta_abs(0x9000);
        let forever = code.pc();
        code.jmp(forever);
        let rom = build_mapper_rom(1, 2, 1, &code, &[]);
        let mut nes = Nes::from_rom(&rom).unwrap();
        for _ in 0..3 {
            nes.step_instruction();
        }
        assert!(mapper_row(&nes, "移位暫存器").contains("已寫入 2 / 5"));
    }

    #[test]
    fn uxrom_switches_8000_and_fixes_c000_through_the_bus() {
        let mut nes = Nes::from_rom(&idle_mapper_rom(2, 8, 0)).unwrap(); // CHR-RAM
        assert_eq!(nes.peek(0x8100), PRG_MARK);
        assert_eq!(nes.peek(0xC100), PRG_MARK + 7);
        nes.cpu.bus_mut().write(0x8000, 4);
        assert_eq!(nes.peek(0x8100), PRG_MARK + 4);
        assert_eq!(nes.peek(0xC100), PRG_MARK + 7);
        assert_eq!(mapper_row(&nes, "PRG $8000-$BFFF"), "bank 4");
        // UxROM 的 CHR-RAM 可寫。
        let bus = nes.cpu.bus_mut();
        bus.write(0x2006, 0x00);
        bus.write(0x2006, 0x10);
        bus.write(0x2007, 0x5A);
        assert_eq!(nes.peek_ppu(0x0010), 0x5A);
    }

    #[test]
    fn cnrom_switches_chr_and_keeps_prg_fixed_through_the_bus() {
        let mut nes = Nes::from_rom(&idle_mapper_rom(3, 2, 4)).unwrap(); // 4 個 8KB CHR
        assert_eq!(nes.peek_ppu(0x0000), CHR_MARK);
        nes.cpu.bus_mut().write(0x8000, 2);
        assert_eq!(
            nes.peek_ppu(0x0000),
            CHR_MARK + 4,
            "8KB bank 2 = 4KB 區塊 4"
        );
        assert_eq!(nes.peek_ppu(0x1000), CHR_MARK + 5);
        assert_eq!(nes.peek(0x8100), PRG_MARK);
        assert_eq!(nes.peek(0xC100), PRG_MARK + 1);
    }

    #[test]
    fn mapper_state_survives_save_and_load_including_a_half_finished_serial_write() {
        let rom = idle_mapper_rom(1, 8, 2);
        let mut nes = Nes::from_rom(&rom).unwrap();
        mmc1_write(&mut nes, 0xE000, 5);
        for bit in [1, 0, 1] {
            nes.cpu.bus_mut().write(0x8000, bit); // 序列寫入進行到一半
        }
        let saved = nes.save_state();
        let hash = nes.state_hash();

        mmc1_write(&mut nes, 0xE000, 2);
        assert_ne!(nes.state_hash(), hash);
        nes.load_state(&saved).unwrap();

        assert_eq!(nes.state_hash(), hash);
        assert_eq!(nes.peek(0x8100), PRG_MARK + 5);
        assert!(mapper_row(&nes, "移位暫存器").contains("已寫入 3 / 5"));
    }

    #[test]
    fn load_state_rejects_a_mapper_that_disagrees_with_the_header() {
        let rom = idle_mapper_rom(1, 2, 1);
        let mut tampered = Nes::from_rom(&rom).unwrap();
        tampered.cpu.bus_mut().cartridge.mapper = Mapper::Uxrom(crate::cartridge::Uxrom::new());
        let bytes = tampered.save_state();

        let mut target = Nes::from_rom(&rom).unwrap();
        assert!(matches!(
            target.load_state(&bytes),
            Err(StateError::Corrupt)
        ));
    }

    /// rollback 性質在 MMC1 上同樣成立：mapper 暫存器（含移位暫存器）一直在變的
    /// 程式，存檔、往前跑、讀檔、重跑，結果與不中斷相同。
    #[test]
    fn rollback_replay_matches_uninterrupted_run_with_an_active_mmc1() {
        let rom = mmc1_churn_rom();
        let inputs = inputs_for(40);

        let mut baseline = Nes::from_rom(&rom).unwrap();
        for input in &inputs {
            baseline.run_frame(*input);
        }

        let mut replay = Nes::from_rom(&rom).unwrap();
        for input in &inputs[..15] {
            replay.run_frame(*input);
        }
        let checkpoint = replay.save_state();
        for input in &inputs[15..30] {
            replay.run_frame(*input);
        }
        replay.load_state(&checkpoint).unwrap();
        for input in &inputs[15..] {
            replay.run_frame(*input);
        }
        assert_eq!(replay.state_hash(), baseline.state_hash());
    }

    /// 隨機垃圾 ROM 在 mapper 1/2/3 下也不得 panic（暫存器被亂寫、bank 超出範圍）。
    #[test]
    fn random_garbage_roms_never_panic_on_mappers_1_2_3() {
        fn xorshift(state: &mut u64) -> u8 {
            *state ^= *state << 13;
            *state ^= *state >> 7;
            *state ^= *state << 17;
            (*state >> 24) as u8
        }
        for seed in 1..=18u64 {
            let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
            let mapper = 1 + (seed % 3) as u8;
            let prg_banks = 1 + (seed % 5) as u8; // 含奇數 bank 數
            let chr_banks = (seed % 4) as u8; // 0 = CHR-RAM
            let mut rom = vec![0u8; 16];
            rom[0..4].copy_from_slice(b"NES\x1A");
            rom[4] = prg_banks;
            rom[5] = chr_banks;
            rom[6] = mapper << 4;
            for _ in 0..(prg_banks as usize * 0x4000 + chr_banks as usize * 0x2000) {
                rom.push(xorshift(&mut state));
            }
            let mut nes = Nes::from_rom(&rom).unwrap();
            for frame in 0..4 {
                let pad = xorshift(&mut state);
                nes.run_frame([Buttons::from_bits_truncate(pad), Buttons::empty()]);
                let _ = nes.debug_snapshot();
                if frame == 2 {
                    let _ = nes.debug_ppu_views(pad);
                    let saved = nes.save_state();
                    nes.load_state(&saved).unwrap();
                }
            }
        }
    }

    // ---- 合成 mapper 測試 ROM（blargg `$6000` 協定）--------------------------------

    /// 跑到 `$6000` 不再是「執行中」（`$80`）為止，回傳 `(結果碼, 結果文字)`。
    /// 簽章 `DE B0 61` 必須存在，否則視為 ROM 根本沒跑起來。
    fn run_blargg_rom(rom: &[u8]) -> (u8, String) {
        let mut nes = Nes::from_rom(rom).unwrap();
        for _ in 0..30 {
            nes.run_frame([Buttons::empty(); 2]);
            if nes.peek(0x6001) == 0xDE && nes.peek(0x6000) != 0x80 {
                break;
            }
        }
        assert_eq!(
            [nes.peek(0x6001), nes.peek(0x6002), nes.peek(0x6003)],
            [0xDE, 0xB0, 0x61],
            "缺少 blargg 簽章"
        );
        let text: String = (0x6004u16..)
            .map(|a| nes.peek(a))
            .take_while(|&b| b != 0)
            .map(char::from)
            .collect();
        (nes.peek(0x6000), text)
    }

    #[test]
    fn uxrom_synthetic_rom_passes_the_blargg_protocol() {
        let (code, text) = run_blargg_rom(&crate::test_support::uxrom_test_rom());
        assert_eq!((code, text.as_str()), (0, "UxROM: Passed"));
    }

    #[test]
    fn cnrom_synthetic_rom_passes_the_blargg_protocol() {
        let (code, text) = run_blargg_rom(&crate::test_support::cnrom_test_rom());
        assert_eq!((code, text.as_str()), (0, "CNROM: Passed"));
    }
}
