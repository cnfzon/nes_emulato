//! 系統匯流排：CPU 位址空間的中央調度者。
//!
//! `Bus` 擁有 `Ppu`、`Apu`、`Cartridge` 與兩個 `Joypad`。CPU 透過
//! `Bus::read` / `Bus::write` 存取所有記憶體對映裝置；Phase 0 這兩個方法
//! 還沒有實作完整的位址解碼，先回傳 / 忽略即可，因為目前沒有任何呼叫路徑
//! （`Nes::run_frame`）真的需要讀寫記憶體。

use crate::cartridge::Cartridge;
use crate::joypad::Joypad;
use crate::ppu::Ppu;

use crate::apu::Apu;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Bus {
    /// 2KB 內部 RAM。用 `Vec` 而非定長陣列，理由見 [`crate::ppu::Ppu`] 的
    /// `oam`/`vram` 欄位註解（serde derive 只原生支援長度 <= 32 的陣列）；
    /// 長度固定為 `0x0800`，由 `Bus::new` 保證。
    pub ram: Vec<u8>,
    pub ppu: Ppu,
    pub apu: Apu,
    pub cartridge: Cartridge,
    pub joypads: [Joypad; 2],
}

impl Bus {
    pub fn new(cartridge: Cartridge) -> Self {
        Self {
            ram: vec![0u8; 0x0800],
            ppu: Ppu::default(),
            apu: Apu::default(),
            cartridge,
            joypads: [Joypad::default(); 2],
        }
    }

    /// 讀取 CPU 位址空間中的一個 byte。
    ///
    /// TODO Phase 1: 實作完整位址解碼——
    /// `$0000-$1FFF` 2KB RAM 鏡像四次、`$2000-$3FFF` PPU 暫存器鏡像、
    /// `$4000-$4017` APU/IO 暫存器、`$4020-$FFFF` 卡帶 PRG-ROM/PRG-RAM。
    pub fn read(&mut self, _addr: u16) -> u8 {
        0
    }

    /// 寫入 CPU 位址空間中的一個 byte。
    ///
    /// TODO Phase 1: 對應 `read` 的位址解碼表，並處理 `$4014` OAM DMA。
    pub fn write(&mut self, _addr: u16, _value: u8) {}
}
