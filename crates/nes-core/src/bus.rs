//! 系統匯流排：CPU 位址空間的中央調度者。
//!
//! `Bus` 擁有 `Ppu`、`Apu`、`Cartridge` 與兩個 `Joypad`。CPU 透過
//! `Bus::read`/`Bus::write`/`Bus::peek` 存取所有記憶體對映裝置。
//!
//! # 位址解碼
//!
//! | 範圍              | 內容                                                    |
//! |-------------------|---------------------------------------------------------|
//! | `$0000-$1FFF`     | 2KB 內部 RAM，每 `$0800` 鏡像一次                         |
//! | `$2000-$3FFF`     | PPU 暫存器，每 8 bytes 鏡像一次（本階段是 stub，Phase 2 才有真正的側效應） |
//! | `$4000-$4017`     | APU/IO 暫存器（本階段是 stub）                             |
//! | `$4018-$401F`     | APU/IO 測試模式，一般停用 → open bus                        |
//! | `$4020-$5FFF`     | 未對映 → open bus                                        |
//! | `$6000-$7FFF`     | 卡帶 PRG-RAM                                             |
//! | `$8000-$FFFF`     | 卡帶 PRG-ROM（交給 `Mapper`）                              |
//!
//! **open bus**：真實硬體沒有裝置回應的位址，讀到的是資料匯流排上「上一次
//! 殘留」的值，不是固定的 0。`Bus` 用 `open_bus` 欄位追蹤「最後一次被讀出或
//! 寫入的值」，未對映區域的讀取就回傳它，比回傳寫死的 `0` 更接近硬體行為。

use crate::apu::Apu;
use crate::cartridge::Cartridge;
use crate::joypad::Joypad;
use crate::ppu::Ppu;

/// NTSC 下 PPU 一條掃描線的 dot 數。
const DOTS_PER_SCANLINE: u64 = 341;
/// NTSC 下一幀的掃描線數。
const SCANLINES_PER_FRAME: u64 = 262;

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

    /// CPU 從開機以來累積的總 cycle 數，由 `tick` 累加。trace 的 `CYC:`
    /// 欄位、PPU dot 計算都從這裡換算，是唯一的計時真相來源（`Cpu` 本身不
    /// 再重複存一份）。
    total_cycles: u64,
    /// 資料匯流排上最後一次讀出或寫入的值，未對映位址的讀取回傳這個。
    open_bus: u8,

    /// 只給 CPU 一致性測試（SingleStepTests）用：`Some` 時整個 64KB 位址
    /// 空間變成一塊 flat RAM，`read`/`write`/`peek` 直接讀寫這裡、完全繞過
    /// 正常的 NES 記憶體地圖（RAM 鏡像／PPU 暫存器／PRG-ROM 唯讀）。
    /// SingleStepTests 的測試資料假設整個位址空間都是可自由讀寫的 RAM，跟
    /// 真實 NES 的記憶體地圖不相容，需要這個旁路。正常的遊戲路徑
    /// （`Nes::run_frame`）永遠不會設定它；`#[cfg(test)]` 確保正式建置
    /// （`cargo build`/release）裡這個欄位根本不存在，也不會有相關分支。
    #[cfg(test)]
    #[serde(skip)]
    test_flat_ram: Option<Vec<u8>>,
}

impl Bus {
    pub fn new(cartridge: Cartridge) -> Self {
        Self {
            ram: vec![0u8; 0x0800],
            ppu: Ppu::default(),
            apu: Apu::default(),
            cartridge,
            joypads: [Joypad::default(); 2],
            total_cycles: 0,
            open_bus: 0,
            #[cfg(test)]
            test_flat_ram: None,
        }
    }

    /// 只給測試用：建立一個整個 64KB 位址空間都是可讀寫 RAM 的 `Bus`，繞過
    /// 真正的 NES 記憶體地圖。`cartridge` 只是用來滿足型別需求，flat RAM
    /// 模式下完全不會被存取。
    #[cfg(test)]
    pub(crate) fn new_flat_ram_for_testing(cartridge: Cartridge) -> Self {
        let mut bus = Self::new(cartridge);
        bus.test_flat_ram = Some(vec![0u8; 0x1_0000]);
        bus
    }

    #[cfg(test)]
    pub(crate) fn flat_ram_mut(&mut self) -> &mut [u8] {
        self.test_flat_ram
            .as_deref_mut()
            .expect("flat ram 測試模式未啟用")
    }

    /// `test_flat_ram` 旁路的讀取端。正式建置（`#[cfg(not(test))]`）裡這個
    /// 欄位根本不存在，這個版本永遠回傳 `None`，讓 `read`/`peek` 的呼叫端
    /// 程式碼可以保持 cfg-free、兩種建置共用同一份邏輯。
    #[cfg(test)]
    fn test_flat_ram_read(&self, addr: u16) -> Option<u8> {
        self.test_flat_ram.as_ref().map(|ram| ram[addr as usize])
    }

    #[cfg(not(test))]
    fn test_flat_ram_read(&self, _addr: u16) -> Option<u8> {
        None
    }

    /// `test_flat_ram` 旁路的寫入端；回傳 `true` 代表已經處理掉這次寫入。
    #[cfg(test)]
    fn test_flat_ram_write(&mut self, addr: u16, value: u8) -> bool {
        if let Some(ram) = &mut self.test_flat_ram {
            ram[addr as usize] = value;
            true
        } else {
            false
        }
    }

    #[cfg(not(test))]
    fn test_flat_ram_write(&mut self, _addr: u16, _value: u8) -> bool {
        false
    }

    /// 讀取 CPU 位址空間中的一個 byte（**有**副作用：更新 `open_bus`；
    /// Phase 2 接上真正的 PPU 之後，讀 `$2002`/`$2007` 也會在這裡清旗標/
    /// 前進位址）。
    pub fn read(&mut self, addr: u16) -> u8 {
        if let Some(v) = self.test_flat_ram_read(addr) {
            return v;
        }
        let value = self.read_mapped(addr);
        self.open_bus = value;
        value
    }

    /// 跟 `read` 解碼邏輯完全相同，但**沒有**副作用，給 trace log 與
    /// debugger 用。目前 Phase 1 的 PPU/APU 還是 stub，`read`/`peek` 對它們
    /// 而言行為一樣；但两者刻意分開實作成兩個方法，這樣 Phase 2 幫 PPU 加上
    /// 讀取側效應時，只需要改 `read_mapped` 內 `read`/`peek` 分岔的那幾行，
    /// 不會不小心讓 trace 也觸發側效應。
    pub fn peek(&self, addr: u16) -> u8 {
        if let Some(v) = self.test_flat_ram_read(addr) {
            return v;
        }
        match addr {
            0x0000..=0x1FFF => self.ram[(addr & 0x07FF) as usize],
            0x2000..=0x3FFF => self.peek_ppu_register(mirror_ppu_register(addr)),
            0x4000..=0x4017 => self.peek_apu_io_register(addr),
            0x4018..=0x401F => self.open_bus,
            0x4020..=0x5FFF => self.open_bus,
            0x6000..=0x7FFF => self.cartridge.prg_ram[(addr - 0x6000) as usize],
            0x8000..=0xFFFF => self.cartridge.read_prg(addr),
        }
    }

    fn read_mapped(&mut self, addr: u16) -> u8 {
        match addr {
            0x0000..=0x1FFF => self.ram[(addr & 0x07FF) as usize],
            0x2000..=0x3FFF => self.read_ppu_register(mirror_ppu_register(addr)),
            0x4000..=0x4017 => self.read_apu_io_register(addr),
            0x4018..=0x401F => self.open_bus,
            0x4020..=0x5FFF => self.open_bus,
            0x6000..=0x7FFF => self.cartridge.prg_ram[(addr - 0x6000) as usize],
            0x8000..=0xFFFF => self.cartridge.read_prg(addr),
        }
    }

    /// 寫入 CPU 位址空間中的一個 byte。
    pub fn write(&mut self, addr: u16, value: u8) {
        if self.test_flat_ram_write(addr, value) {
            return;
        }
        self.open_bus = value;
        match addr {
            0x0000..=0x1FFF => self.ram[(addr & 0x07FF) as usize] = value,
            0x2000..=0x3FFF => self.write_ppu_register(mirror_ppu_register(addr), value),
            0x4000..=0x4017 => self.write_apu_io_register(addr, value),
            0x4018..=0x401F => {}
            0x4020..=0x5FFF => {}
            0x6000..=0x7FFF => self.cartridge.prg_ram[(addr - 0x6000) as usize] = value,
            // NROM（目前唯一支援的 mapper）沒有可寫暫存器；之後的 mapper
            // （MMC1 等）靠寫這個範圍切換 bank，接口留在這裡。
            0x8000..=0xFFFF => {}
        }
    }

    // ---- PPU/APU 暫存器：Phase 1 stub -----------------------------------
    //
    // 只做「每 8 bytes 鏡像」「位址落在正確範圍」的解碼，暫存器本身的讀寫
    // 側效應（vblank 清旗標、OAMDATA/PPUDATA 自動遞增、joypad 移位暫存器
    // 等）留到 Phase 2/3。這裡先把原始 byte 存進/取出 `Ppu`/`Apu` 對應欄位，
    // 讓之後接上真正邏輯時至少資料本身已經在正確的地方。

    fn read_ppu_register(&mut self, addr: u16) -> u8 {
        self.peek_ppu_register(addr)
    }

    fn peek_ppu_register(&self, addr: u16) -> u8 {
        match addr {
            0x2000 => self.ppu.ctrl,
            0x2001 => self.ppu.mask,
            0x2002 => self.ppu.status,
            0x2003 => self.ppu.oam_addr,
            0x2004 => self.ppu.oam[self.ppu.oam_addr as usize],
            0x2007 => self.ppu.data_buffer,
            _ => self.open_bus,
        }
    }

    fn write_ppu_register(&mut self, addr: u16, value: u8) {
        match addr {
            0x2000 => self.ppu.ctrl = value,
            0x2001 => self.ppu.mask = value,
            0x2003 => self.ppu.oam_addr = value,
            0x2004 => {
                self.ppu.oam[self.ppu.oam_addr as usize] = value;
                self.ppu.oam_addr = self.ppu.oam_addr.wrapping_add(1);
            }
            0x2005 | 0x2006 => {} // scroll / addr latch：Phase 2
            0x2007 => self.ppu.data_buffer = value,
            _ => {}
        }
    }

    fn read_apu_io_register(&mut self, addr: u16) -> u8 {
        self.peek_apu_io_register(addr)
    }

    fn peek_apu_io_register(&self, addr: u16) -> u8 {
        match addr {
            0x4015 => self.apu.status,
            // $4016/$4017 是搖桿的移位暫存器讀取，真正的按鍵回傳邏輯屬於
            // Phase 2/3（joypad shift register），這裡先回傳 open bus。
            _ => self.open_bus,
        }
    }

    fn write_apu_io_register(&mut self, addr: u16, value: u8) {
        match addr {
            0x4000..=0x4003 => self.apu.pulse1[(addr - 0x4000) as usize] = value,
            0x4004..=0x4007 => self.apu.pulse2[(addr - 0x4004) as usize] = value,
            0x4008..=0x400B => self.apu.triangle[(addr - 0x4008) as usize] = value,
            0x400C..=0x400F => self.apu.noise[(addr - 0x400C) as usize] = value,
            0x4010..=0x4013 => self.apu.dmc[(addr - 0x4010) as usize] = value,
            0x4014 => {} // OAM DMA：Phase 2
            0x4015 => self.apu.status = value,
            0x4016 => {
                // $4016 bit0 是兩個搖桿共用的 strobe。
                let strobe = value & 0x01 != 0;
                self.joypads[0].strobe = strobe;
                self.joypads[1].strobe = strobe;
            }
            0x4017 => self.apu.frame_counter = value,
            _ => {}
        }
    }

    // ---- 計時 ----------------------------------------------------------

    /// 累加 CPU cycle 數。PPU 的 dot 數是 CPU cycle 數的 3 倍（NTSC）。
    pub fn tick(&mut self, cycles: u8) {
        self.total_cycles += cycles as u64;
    }

    pub fn total_cycles(&self) -> u64 {
        self.total_cycles
    }

    /// 目前的 PPU `(scanline, cycle)`，給 trace 的 `PPU:` 欄位用。
    pub fn ppu_dot(&self) -> (u16, u16) {
        let dots_per_frame = DOTS_PER_SCANLINE * SCANLINES_PER_FRAME;
        let dots = (self.total_cycles * 3) % dots_per_frame;
        (
            (dots / DOTS_PER_SCANLINE) as u16,
            (dots % DOTS_PER_SCANLINE) as u16,
        )
    }
}

/// `$2000-$3FFF` 每 8 bytes 鏡像一次，映射回 `$2000-$2007`。
fn mirror_ppu_register(addr: u16) -> u16 {
    0x2000 + (addr & 0x0007)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cartridge::{Cartridge, Mapper, Nrom, RomInfo};
    use crate::cartridge::{Mirroring, PRG_BANK_SIZE};

    fn test_cartridge() -> Cartridge {
        let prg_rom = vec![0xEAu8; PRG_BANK_SIZE]; // 全部填 NOP
        Cartridge {
            info: RomInfo {
                prg_rom_banks: 1,
                chr_rom_banks: 1,
                mapper_id: 0,
                mirroring: Mirroring::Horizontal,
                battery_backed: false,
                has_trainer: false,
            },
            prg_rom,
            chr_rom: vec![0u8; 8192],
            chr_ram: Vec::new(),
            prg_ram: vec![0u8; crate::cartridge::PRG_RAM_SIZE],
            mapper: Mapper::Nrom(Nrom::new(1)),
            rom_hash: 0,
        }
    }

    #[test]
    fn ram_mirrors_every_0x800() {
        let mut bus = Bus::new(test_cartridge());
        bus.write(0x0000, 0x42);
        assert_eq!(bus.read(0x0800), 0x42);
        assert_eq!(bus.read(0x1000), 0x42);
        assert_eq!(bus.read(0x1800), 0x42);
    }

    #[test]
    fn ppu_register_mirrors_every_8_bytes() {
        let mut bus = Bus::new(test_cartridge());
        bus.write(0x2000, 0x11);
        assert_eq!(bus.peek(0x2008), 0x11);
        assert_eq!(bus.peek(0x3FF8), 0x11);
    }

    #[test]
    fn prg_ram_is_readable_and_writable() {
        let mut bus = Bus::new(test_cartridge());
        bus.write(0x6000, 0x99);
        bus.write(0x7FFF, 0x55);
        assert_eq!(bus.read(0x6000), 0x99);
        assert_eq!(bus.read(0x7FFF), 0x55);
        assert_eq!(bus.read(0x6001), 0); // 沒寫過的位置維持初始值
    }

    #[test]
    fn unmapped_region_returns_open_bus() {
        let mut bus = Bus::new(test_cartridge());
        bus.write(0x0000, 0x77); // 讓 open_bus 變成已知值
        let _ = bus.read(0x0000);
        assert_eq!(bus.read(0x4020), 0x77);
        assert_eq!(bus.read(0x5FFF), 0x77);
    }

    #[test]
    fn tick_advances_total_cycles_and_ppu_dot() {
        let mut bus = Bus::new(test_cartridge());
        bus.tick(7);
        assert_eq!(bus.total_cycles(), 7);
        assert_eq!(bus.ppu_dot(), (0, 21));
    }

    #[test]
    fn peek_has_no_side_effects_on_oam_addr() {
        let bus = Bus::new(test_cartridge());
        let before = bus.peek(0x2004);
        let after = bus.peek(0x2004);
        assert_eq!(before, after);
    }
}
