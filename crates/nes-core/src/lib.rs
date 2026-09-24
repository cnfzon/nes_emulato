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
        if !bus.ppu.is_structurally_valid() {
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
            apu_frame_counter: bus.apu.frame_counter,
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

        let rom = build_nrom(&a, 0x8000, None, &[], &test_chr(), false);
        let mut nes = Nes::from_rom(&rom).unwrap();
        for _ in 0..4 {
            nes.run_frame([Buttons::empty(); 2]);
        }

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
}
