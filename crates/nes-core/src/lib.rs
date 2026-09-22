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
pub use cpu::Cpu;
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
        let cpu = Cpu::new(bus);
        // TODO Phase 1: Bus::read 實作完成後，從 $FFFC/$FFFD 讀取 reset
        // vector 來設定 cpu.pc，取代現在寫死的 0。
        Ok(Self {
            cpu,
            frame_buffer: FrameBuffer::blank(),
            frame_count: 0,
        })
    }

    /// 推進一幀模擬，回傳這一幀畫好的畫面。
    ///
    /// 由外部（GUI / netplay 迴圈）每幀呼叫一次並傳入雙人輸入，而不是靠
    /// `Nes` 自己起執行緒或呼叫 callback —— 這樣 rollback 才能在任意一幀
    /// 暫停、讀檔、用不同輸入重跑，行為完全可預期。
    pub fn run_frame(&mut self, input: [Buttons; 2]) -> &FrameBuffer {
        self.cpu.bus_mut().joypads[0].state = input[0];
        self.cpu.bus_mut().joypads[1].state = input[1];

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
    pub fn load_state(&mut self, bytes: &[u8]) -> Result<(), StateError> {
        *self = state::decode(bytes)?;
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
            cpu_status: self.cpu.status,
            cpu_cycles: self.cpu.cycles,
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
}
