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

pub use bus::Bus;
pub use cartridge::{Cartridge, Mapper, Mirroring, RomInfo};
pub use cpu::{Cpu, StatusFlags};
pub use debug::DebugSnapshot;
pub use error::{RomError, StateError};
pub use frame::FrameBuffer;
pub use joypad::{Buttons, Joypad};

/// 一台完整的 NES 主機。
///
/// 所有權鏈：`Nes` 擁有 `Cpu`，`Cpu` 擁有 `Bus`，`Bus` 擁有
/// `Ppu` / `Apu` / `Cartridge` / `[Joypad; 2]`。整個型別樹都是 plain owned
/// data（沒有 `Rc`/`RefCell`/`Arc`/`Mutex`），所以 `Nes` 可以整包 `Clone`，
/// 也可以整包 `Serialize`/`Deserialize` —— 這正是 rollback 需要的能力。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Nes {
    cpu: Cpu,
    frame_buffer: FrameBuffer,
    frame_count: u64,
}

impl Nes {
    /// 從一份 iNES ROM 檔案的原始位元組建立一台新的 NES。
    pub fn from_rom(rom: &[u8]) -> Result<Self, RomError> {
        let cartridge = Cartridge::from_ines(rom)?;
        let bus = Bus::new(cartridge);
        let mut cpu = Cpu::new(bus);
        cpu.reset();
        Ok(Self {
            cpu,
            frame_buffer: FrameBuffer::blank(),
            frame_count: 0,
        })
    }

    /// NTSC 下一幀（1/60.0988 秒）大約對應的 CPU cycle 數
    /// （`21_477_272.7 Hz 主時脈 / 12 / 60.0988 Hz ≈ 29780.5`，取整數 29781）。
    const CPU_CYCLES_PER_FRAME: u64 = 29781;

    /// 推進一幀模擬，回傳這一幀畫好的畫面。
    ///
    /// 由外部（GUI / netplay 迴圈）每幀呼叫一次並傳入雙人輸入，而不是靠
    /// `Nes` 自己起執行緒或呼叫 callback —— 這樣 rollback 才能在任意一幀
    /// 暫停、讀檔、用不同輸入重跑，行為完全可預期。
    ///
    /// 因為 CPU 是 instruction-level 精度（見 `cpu` 模組文件），無法精準停在
    /// 剛好 `CPU_CYCLES_PER_FRAME` 那個 cycle：這裡的作法是「跑到累積 cycle
    /// 數達到或超過預算為止」，最多多跑一條指令的 cycle 數（最壞情況 8
    /// cycles，相對一整幀 29781 cycles 是可忽略的誤差）。PPU 還沒實作
    /// （Phase 2），畫面仍然使用 `render_test_pattern` 佔位。
    pub fn run_frame(&mut self, input: [Buttons; 2]) -> &FrameBuffer {
        self.cpu.bus_mut().joypads[0].state = input[0];
        self.cpu.bus_mut().joypads[1].state = input[1];

        let target = self.cpu.bus().total_cycles() + Self::CPU_CYCLES_PER_FRAME;
        while self.cpu.bus().total_cycles() < target {
            self.cpu.step();
        }

        self.frame_count += 1;
        self.frame_buffer
            .render_test_pattern(self.frame_count, input);

        &self.frame_buffer
    }

    /// 把自從上次呼叫以來累積的音訊取樣附加到 `out`。
    pub fn drain_audio(&mut self, out: &mut Vec<f32>) {
        self.cpu.bus_mut().apu.take_samples(out);
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
        if bus.ppu.vram.len() != 2048 {
            return Err(StateError::Corrupt);
        }
        if bus.ppu.oam.len() != 256 {
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
            ppu_scanline: bus.ppu.scanline,
            ppu_cycle: bus.ppu.cycle,
            ppu_frame: bus.ppu.frame,
            apu_frame_counter: bus.apu.frame_counter,
        }
    }

    /// 目前載入的 ROM 中繼資料。
    pub fn rom_info(&self) -> &RomInfo {
        &self.cpu.bus().cartridge.info
    }

    /// 執行「一條」CPU 指令（不是一整幀），回傳這條指令花的 cycle 數。
    ///
    /// 給 `nes-test` 這類需要「一條一條指令跑、每條都要比對」的工具用
    /// （例如 nestest log 逐行比對）；一般遊戲邏輯應該用 [`Nes::run_frame`]。
    ///
    /// 只有啟用 `testing` cargo feature 才會編譯進去——這是測試/除錯工具
    /// 專用的旁路 API，不是給一般遊戲邏輯（`nes-app`）用的，用 feature 把它
    /// 從正式建置的公開介面上移除，避免使用者不小心繞過 `run_frame` 直接
    /// 操作 CPU。`nes-test` 在自己的 `Cargo.toml` 啟用這個 feature。
    #[cfg(feature = "testing")]
    pub fn step_cpu_instruction(&mut self) -> u8 {
        self.cpu.step()
    }

    /// 目前這條（尚未執行的）指令的 nestest.log 格式 trace 行。只在
    /// `testing` feature 下可用，理由同 [`Nes::step_cpu_instruction`]。
    #[cfg(feature = "testing")]
    pub fn trace(&self) -> String {
        self.cpu.trace()
    }

    /// 覆寫 PC。給 nestest 的「automation mode」用：先正常 `from_rom`
    /// （內部已經跑過一次真正的 reset），再手動把 PC 蓋成 `$C000`，跳過
    /// nestest.nes 裡需要人工按鍵互動的視覺測試選單。只在 `testing`
    /// feature 下可用，理由同 [`Nes::step_cpu_instruction`]。
    #[cfg(feature = "testing")]
    pub fn override_pc(&mut self, pc: u16) {
        self.cpu.pc = pc;
    }

    /// side-effect-free 的記憶體讀取，給測試工具檢查特定位址用（例如
    /// nestest 執行完後檢查錯誤碼 `$02`/`$03` 是否為 0）。只在 `testing`
    /// feature 下可用，理由同 [`Nes::step_cpu_instruction`]。
    #[cfg(feature = "testing")]
    pub fn peek(&self, addr: u16) -> u8 {
        self.cpu.bus().peek(addr)
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
        assert_eq!(a.frame_buffer.as_bytes(), b.frame_buffer.as_bytes());
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
        assert_ne!(snap, DebugSnapshot::default());
    }
}
