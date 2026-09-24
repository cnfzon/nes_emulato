//! PPU（picture processing unit，2C02）。
//!
//! 參考來源：NESdev Wiki 的 PPU registers / PPU scrolling（loopy 暫存器）/
//! PPU rendering / PPU sprite evaluation / PPU palettes / NMI 各頁。實作順序
//! （暫存器與 mirroring → NMI → 渲染 → 捲動）對照 bugzmanov《Writing NES
//! Emulator in Rust》第 6–8 章的章節順序
//! （<https://bugzmanov.github.io/nes_ebook/chapter_6.html>）；沒有複製其
//! 程式碼。本檔的捲動採用真實硬體的 v/t/x/w（loopy）暫存器與逐 dot 的 v
//! 遞增時序，跟教學的簡化版捲動不同。詳見 `ATTRIBUTION.md`。
//!
//! # 時序模型（catch-up）
//!
//! CPU 是 instruction-level：每條指令執行完，`Bus::tick(cycles)` 讓 PPU
//! 追上 `cycles × 3` 個 dot。因此 PPU 暫存器的讀寫發生在「該條指令開始時」
//! 的 PPU 時間點，而真實硬體是發生在指令的最後一個（或倒數幾個）cycle：
//! PPU 落後 CPU 存取時間點最多 `(指令 cycle 數 - 1)` 個 CPU cycle
//! （＝ 3 倍的 dot）。詳細影響見 `docs/architecture.md` §13。
//!
//! # 渲染
//!
//! 以 scanline 為單位：每條可見掃描線在 dot 0 一次畫完整條（見 `render.rs`），
//! 但 v 暫存器的遞增（coarse X、Y、dot 257 的水平複製、pre-render 行
//! dot 280–304 的垂直複製）依真實時序逐 dot 模擬，所以遊戲在 hblank 中改
//! 捲動（例如 SMB 的狀態列分割）會在正確的那條掃描線生效。Sprite 0 hit 在
//! 渲染該條掃描線時算出命中的 x，等 PPU 實際走到那個 dot 才設旗標。
//!
//! # 一幀的邊界
//!
//! `frame_done` 在 scanline 241、dot 1（進入 vblank）時設起：此時 240 條可見
//! 掃描線都已畫完，之後到下一幀開始之前不會再寫入 framebuffer，所以呼叫端
//! 拿到的畫面不會有撕裂。

mod palette;
mod render;
mod views;

pub use palette::{SYSTEM_PALETTE, to_rgba};

use crate::cartridge::{Cartridge, Mirroring};
use crate::frame::FrameBuffer;

/// PPUCTRL bit7：vblank 時觸發 NMI。
pub const CTRL_NMI_ENABLE: u8 = 0x80;
/// PPUSTATUS bit7：vblank 旗標。
pub const STATUS_VBLANK: u8 = 0x80;
/// PPUSTATUS bit6：sprite 0 hit。
pub const STATUS_SPRITE0_HIT: u8 = 0x40;
/// PPUSTATUS bit5：sprite overflow。
pub const STATUS_SPRITE_OVERFLOW: u8 = 0x20;

/// PPUMASK 的「渲染開啟」位元（顯示背景 | 顯示精靈）。
const MASK_RENDERING: u8 = 0x18;

/// 一條掃描線的 dot 數（0–340）。
const DOTS_PER_SCANLINE: u16 = 341;
/// pre-render 掃描線的編號（硬體上的 -1）。
const PRE_RENDER_LINE: u16 = 261;
/// 第一條 vblank 掃描線。
const VBLANK_LINE: u16 = 241;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Ppu {
    pub ctrl: u8,     // $2000 PPUCTRL
    pub mask: u8,     // $2001 PPUMASK
    pub status: u8,   // $2002 PPUSTATUS
    pub oam_addr: u8, // $2003 OAMADDR

    /// 目前 VRAM 位址（loopy v，15 bit）。
    pub v: u16,
    /// 暫存 VRAM 位址（loopy t，15 bit）。
    pub t: u16,
    /// 精細 X 捲動（3 bit）。
    pub fine_x: u8,
    /// $2005/$2006 的寫入 latch（loopy w）：`false` 代表下一次是第一次寫入。
    pub w: bool,
    /// $2007 讀取時的一個 byte 延遲緩衝。
    pub data_buffer: u8,
    /// PPU 對外的 I/O latch（open bus）：最後一次寫入任一 PPU 暫存器（或讀出）
    /// 的值，讀取唯寫暫存器時回傳它。
    io_latch: u8,

    /// 256 bytes，OAM sprite RAM。用 `Vec` 而非定長陣列是因為 serde 的
    /// `derive(Serialize, Deserialize)` 只原生支援長度 <= 32 的陣列；長度
    /// 由建構時的 `vec![0; 256]` 保證，之後不會改變。
    pub oam: Vec<u8>,
    /// 2KB nametable RAM，同上理由使用 `Vec`（長度固定為 2048）。
    pub vram: Vec<u8>,
    /// 32 byte 調色盤 RAM。`$3F10/$14/$18/$1C` 是 `$3F00/$04/$08/$0C` 的
    /// 鏡像（讀寫皆然），所以 index `0x10/0x14/0x18/0x1C` 這四格永遠不會被
    /// 使用。
    pub palette: [u8; 32],

    /// 目前掃描線：0–239 可見、240 post-render、241–260 vblank、261 pre-render。
    pub scanline: u16,
    /// 目前 dot（0–340）。
    pub cycle: u16,
    /// 已進入 vblank 的次數（＝已完成的 PPU 幀數）。
    pub frame: u64,
    /// 奇偶幀旗標：奇數幀在渲染開啟時，pre-render 行少一個 dot。
    pub odd_frame: bool,

    /// NMI 輸出線目前的電位（`vblank 旗標 && PPUCTRL bit7`），用來做邊緣偵測。
    nmi_line: bool,
    /// 已偵測到、尚未被 CPU 服務的 NMI 邊緣。
    nmi_pending: bool,
    /// 這個 NMI 邊緣是 CPU 寫 PPUCTRL 造成的：寫入發生在該指令的最後一個
    /// cycle，來不及趕上這條指令結尾的 NMI 偵測，所以要「下一條指令之後」
    /// 才會被服務（而 vblank 開始造成的邊緣則在下一次偵測就被服務）。
    nmi_delay: bool,
    /// 剛完成一幀（進入 vblank）。由 `Nes::run_frame` 消耗。
    frame_done: bool,

    /// 目前這條掃描線的 sprite 0 hit 要在哪個 dot 設旗標；`0` 代表這條線
    /// 沒有命中。
    sprite0_hit_dot: u16,
    /// 這條掃描線是否偵測到 sprite overflow（在 dot 256 才設旗標）。
    overflow_pending: bool,
    /// 上一次 dot 328/336 的預取（coarse X 遞增）次數，0–2。渲染下一條掃描線
    /// 時要把 v 倒退這麼多個 tile 才是該線第一個 tile。
    prefetch_incs: u8,

    /// 渲染輸出。**不進 save state**：一幀的每個像素都會在下一幀重畫，而
    /// 存檔只發生在幀邊界，所以它是「輸出」而不是「狀態」。讀檔後由
    /// 下一次 `run_frame` 重新畫滿。
    #[serde(skip, default = "FrameBuffer::blank")]
    pub(crate) frame_buffer: FrameBuffer,
}

impl Default for Ppu {
    fn default() -> Self {
        Self {
            ctrl: 0,
            mask: 0,
            status: 0,
            oam_addr: 0,
            v: 0,
            t: 0,
            fine_x: 0,
            w: false,
            data_buffer: 0,
            io_latch: 0,
            oam: vec![0; 256],
            vram: vec![0; 2048],
            palette: [0; 32],
            scanline: 0,
            cycle: 0,
            frame: 0,
            odd_frame: false,
            nmi_line: false,
            nmi_pending: false,
            nmi_delay: false,
            frame_done: false,
            sprite0_hit_dot: 0,
            overflow_pending: false,
            prefetch_incs: 0,
            frame_buffer: FrameBuffer::blank(),
        }
    }
}

impl Ppu {
    /// 檢查（從存檔還原的）內部欄位是否都在硬體可能出現的範圍內。存檔損毀或被
    /// 竄改時這些欄位可能是任意值，之後的時序/渲染邏輯不該因此 panic。
    pub(crate) fn is_structurally_valid(&self) -> bool {
        self.oam.len() == 256
            && self.vram.len() == 2048
            && self.scanline <= PRE_RENDER_LINE
            && self.cycle < DOTS_PER_SCANLINE
            && self.v <= 0x7FFF
            && self.t <= 0x7FFF
            && self.fine_x <= 7
            && self.prefetch_incs <= 2
            && self.sprite0_hit_dot < DOTS_PER_SCANLINE
    }

    /// 冷開機以外的 reset 訊號：清除 PPUCTRL/PPUMASK、寫入 latch 與讀取緩衝，
    /// 其餘（OAM、VRAM、調色盤、掃描線位置）維持不變，跟真實硬體一致。
    pub fn reset(&mut self) {
        self.ctrl = 0;
        self.mask = 0;
        self.w = false;
        self.data_buffer = 0;
        self.nmi_line = false;
        self.nmi_pending = false;
        self.nmi_delay = false;
    }

    // ---- NMI / 幀邊界 --------------------------------------------------

    /// 重新計算 NMI 輸出線；只有「低 → 高」的邊緣才會登記一次 NMI。
    ///
    /// 這同時涵蓋兩種情況：vblank 開始時（旗標由 0 變 1，且 PPUCTRL bit7 已
    /// 開），以及 vblank 期間把 PPUCTRL bit7 從 0 寫成 1（旗標已在、致能才
    /// 變 1）。讀 `$2002` 清掉 vblank 旗標會讓線拉低，之後再度致能就又是一次
    /// 新的邊緣。
    ///
    /// `from_cpu_write`：邊緣是 CPU 寫 PPUCTRL 造成的（見 `nmi_delay`）。
    fn update_nmi_line(&mut self, from_cpu_write: bool) {
        let line = self.status & STATUS_VBLANK != 0 && self.ctrl & CTRL_NMI_ENABLE != 0;
        if line && !self.nmi_line {
            self.nmi_pending = true;
            self.nmi_delay = from_cpu_write;
        }
        self.nmi_line = line;
    }

    /// CPU 在指令之間呼叫：取走待處理的 NMI（有就回傳 `true` 並清除）。
    /// 由 CPU 寫 PPUCTRL 造成的 NMI 會多等一次偵測（見 `nmi_delay`）。
    pub(crate) fn take_nmi(&mut self) -> bool {
        if !self.nmi_pending {
            return false;
        }
        if self.nmi_delay {
            self.nmi_delay = false;
            return false;
        }
        self.nmi_pending = false;
        true
    }

    /// 取走「剛完成一幀」旗標。
    pub(crate) fn take_frame_done(&mut self) -> bool {
        std::mem::take(&mut self.frame_done)
    }

    /// 清掉「剛完成一幀」旗標（`run_frame` 開始時呼叫，避免上一次單步遺留）。
    pub(crate) fn clear_frame_done(&mut self) {
        self.frame_done = false;
    }

    /// 最近一次完成的畫面。
    pub(crate) fn frame_buffer(&self) -> &FrameBuffer {
        &self.frame_buffer
    }

    // ---- 暫存器 --------------------------------------------------------

    /// 讀取 PPU 暫存器（`reg` 為 `$2000-$2007` 的低 3 位元）。**有**副作用。
    pub(crate) fn read_register(&mut self, reg: u16, cart: &Cartridge) -> u8 {
        match reg & 7 {
            2 => {
                let value = (self.status & 0xE0) | (self.io_latch & 0x1F);
                self.status &= !STATUS_VBLANK;
                self.w = false;
                self.update_nmi_line(false);
                // 讀 $2002 只重新驅動 latch 的高 3 位元。
                self.io_latch = (self.io_latch & 0x1F) | (value & 0xE0);
                value
            }
            4 => {
                let value = self.peek_oam_data();
                self.io_latch = value;
                value
            }
            7 => {
                let value = self.read_data(cart);
                self.io_latch = value;
                value
            }
            _ => self.io_latch,
        }
    }

    /// 跟 [`Ppu::read_register`] 解碼相同，但**沒有**副作用（trace / debugger 用）。
    pub(crate) fn peek_register(&self, reg: u16) -> u8 {
        match reg & 7 {
            2 => (self.status & 0xE0) | (self.io_latch & 0x1F),
            4 => self.peek_oam_data(),
            7 => {
                let addr = self.v & 0x3FFF;
                if addr >= 0x3F00 {
                    self.read_palette(addr) | (self.io_latch & 0xC0)
                } else {
                    self.data_buffer
                }
            }
            _ => self.io_latch,
        }
    }

    /// 寫入 PPU 暫存器。
    pub(crate) fn write_register(&mut self, reg: u16, value: u8, cart: &mut Cartridge) {
        self.io_latch = value;
        match reg & 7 {
            0 => {
                self.ctrl = value;
                // t 的 nametable 選擇位元（bit 10–11）取自 PPUCTRL 低 2 位元。
                self.t = (self.t & !0x0C00) | ((value as u16 & 0x03) << 10);
                self.update_nmi_line(true);
            }
            1 => self.mask = value,
            3 => self.oam_addr = value,
            4 => {
                self.oam[self.oam_addr as usize] = value;
                self.oam_addr = self.oam_addr.wrapping_add(1);
            }
            5 => {
                if !self.w {
                    self.t = (self.t & !0x001F) | (value as u16 >> 3);
                    self.fine_x = value & 0x07;
                } else {
                    self.t = (self.t & !0x73E0)
                        | ((value as u16 & 0x07) << 12)
                        | ((value as u16 >> 3) << 5);
                }
                self.w = !self.w;
            }
            6 => {
                if !self.w {
                    self.t = (self.t & 0x00FF) | ((value as u16 & 0x3F) << 8);
                } else {
                    self.t = (self.t & 0x7F00) | value as u16;
                    self.v = self.t;
                }
                self.w = !self.w;
            }
            7 => {
                let addr = self.v & 0x3FFF;
                self.write_memory(addr, value, cart);
                self.increment_v();
            }
            _ => {} // $2002 唯讀
        }
    }

    fn peek_oam_data(&self) -> u8 {
        let value = self.oam[self.oam_addr as usize];
        // sprite 的第 3 個 byte（attribute）的 bit2–4 在硬體上不存在，讀回 0。
        if self.oam_addr & 0x03 == 0x02 {
            value & 0xE3
        } else {
            value
        }
    }

    fn read_data(&mut self, cart: &Cartridge) -> u8 {
        let addr = self.v & 0x3FFF;
        let result = if addr >= 0x3F00 {
            // 調色盤不經緩衝，但緩衝仍會被「底下」的 nametable 位元組（$2Fxx）填入。
            self.data_buffer = self.read_memory(addr & 0x2FFF, cart);
            self.read_palette(addr) | (self.io_latch & 0xC0)
        } else {
            let buffered = self.data_buffer;
            self.data_buffer = self.read_memory(addr, cart);
            buffered
        };
        self.increment_v();
        result
    }

    /// `$2007` 存取後 v 依 PPUCTRL bit2 遞增 1 或 32。
    fn increment_v(&mut self) {
        let step = if self.ctrl & 0x04 != 0 { 32 } else { 1 };
        self.v = self.v.wrapping_add(step) & 0x7FFF;
    }

    // ---- PPU 位址空間 --------------------------------------------------

    /// nametable 位址（`$2000-$3EFF`）換成 2KB VRAM 內的偏移，依卡帶的 mirroring。
    fn nametable_offset(addr: u16, mirroring: Mirroring) -> usize {
        let index = (addr.wrapping_sub(0x2000) & 0x0FFF) as usize;
        let table = index / 0x400;
        let within = index % 0x400;
        let physical = match mirroring {
            Mirroring::Horizontal => table >> 1,
            // FourScreen 由 `Nes::from_rom` 拒絕；這裡不 panic，退回 vertical。
            Mirroring::Vertical | Mirroring::FourScreen => table & 1,
        };
        physical * 0x400 + within
    }

    /// 調色盤位址 → `palette` 陣列索引（含 `$10/$14/$18/$1C` 鏡像）。
    fn palette_index(addr: u16) -> usize {
        let index = (addr & 0x1F) as usize;
        if index & 0x03 == 0 {
            index & 0x0F
        } else {
            index
        }
    }

    fn read_palette(&self, addr: u16) -> u8 {
        let value = self.palette[Self::palette_index(addr)] & 0x3F;
        if self.mask & 0x01 != 0 {
            value & 0x30
        } else {
            value
        }
    }

    /// 讀 PPU 位址空間的一個 byte（沒有任何副作用）。
    pub(crate) fn read_memory(&self, addr: u16, cart: &Cartridge) -> u8 {
        let addr = addr & 0x3FFF;
        match addr {
            0x0000..=0x1FFF => cart.read_chr(addr),
            0x2000..=0x3EFF => self.vram[Self::nametable_offset(addr, cart.info.mirroring)],
            _ => self.read_palette(addr),
        }
    }

    fn write_memory(&mut self, addr: u16, value: u8, cart: &mut Cartridge) {
        let addr = addr & 0x3FFF;
        match addr {
            0x0000..=0x1FFF => cart.write_chr(addr, value),
            0x2000..=0x3EFF => {
                let offset = Self::nametable_offset(addr, cart.info.mirroring);
                self.vram[offset] = value;
            }
            _ => self.palette[Self::palette_index(addr)] = value & 0x3F,
        }
    }

    // ---- v 暫存器的硬體遞增 --------------------------------------------

    /// coarse X 遞增；越過 31 時翻轉水平 nametable 位元。
    fn increment_coarse_x(&mut self) {
        if self.v & 0x001F == 31 {
            self.v = (self.v & !0x001F) ^ 0x0400;
        } else {
            self.v = self.v.wrapping_add(1) & 0x7FFF;
        }
    }

    /// Y 遞增（fine Y，滿 8 進位到 coarse Y；coarse Y 29 翻轉垂直 nametable）。
    fn increment_y(&mut self) {
        if self.v & 0x7000 != 0x7000 {
            self.v = self.v.wrapping_add(0x1000) & 0x7FFF;
            return;
        }
        self.v &= !0x7000;
        let mut coarse_y = (self.v & 0x03E0) >> 5;
        if coarse_y == 29 {
            coarse_y = 0;
            self.v ^= 0x0800;
        } else if coarse_y == 31 {
            coarse_y = 0;
        } else {
            coarse_y += 1;
        }
        self.v = (self.v & !0x03E0) | (coarse_y << 5);
    }

    /// dot 257：hori(v) = hori(t)。
    fn copy_horizontal(&mut self) {
        self.v = (self.v & !0x041F) | (self.t & 0x041F);
    }

    /// pre-render 行 dot 280–304：vert(v) = vert(t)。
    fn copy_vertical(&mut self) {
        self.v = (self.v & !0x7BE0) | (self.t & 0x7BE0);
    }

    // ---- 時序 ----------------------------------------------------------

    /// 推進 `dots` 個 PPU dot（CPU 的 1 個 cycle ＝ 3 個 dot）。
    pub(crate) fn step_dots(&mut self, dots: u32, cart: &Cartridge) {
        for _ in 0..dots {
            self.tick_dot(cart);
        }
    }

    fn tick_dot(&mut self, cart: &Cartridge) {
        let line = self.scanline;
        let dot = self.cycle;
        let rendering = self.mask & MASK_RENDERING != 0;

        if line < 240 {
            if dot == 0 {
                self.render_scanline(line, cart);
            }
            if self.sprite0_hit_dot != 0 && dot == self.sprite0_hit_dot {
                self.status |= STATUS_SPRITE0_HIT;
                self.sprite0_hit_dot = 0;
            }
            if dot == 256 && self.overflow_pending {
                self.status |= STATUS_SPRITE_OVERFLOW;
                self.overflow_pending = false;
            }
        } else if line == VBLANK_LINE && dot == 1 {
            self.status |= STATUS_VBLANK;
            self.frame += 1;
            self.frame_done = true;
            self.update_nmi_line(false);
        } else if line == PRE_RENDER_LINE && dot == 1 {
            self.status &= !(STATUS_VBLANK | STATUS_SPRITE0_HIT | STATUS_SPRITE_OVERFLOW);
            self.sprite0_hit_dot = 0;
            self.overflow_pending = false;
            self.update_nmi_line(false);
        }

        if line < 240 || line == PRE_RENDER_LINE {
            if dot == 321 {
                self.prefetch_incs = 0;
            }
            if rendering {
                match dot {
                    1..=256 if dot & 7 == 0 => {
                        self.increment_coarse_x();
                        if dot == 256 {
                            self.increment_y();
                        }
                    }
                    257 => self.copy_horizontal(),
                    280..=304 if line == PRE_RENDER_LINE => self.copy_vertical(),
                    328 | 336 => {
                        self.increment_coarse_x();
                        self.prefetch_incs += 1;
                    }
                    _ => {}
                }
            }
        }

        // 前進到下一個 dot。奇數幀且渲染開啟時，pre-render 行少最後一個 dot。
        let skip_last_dot = line == PRE_RENDER_LINE && dot == 339 && self.odd_frame && rendering;
        self.cycle += 1;
        if self.cycle >= DOTS_PER_SCANLINE || skip_last_dot {
            self.cycle = 0;
            self.scanline += 1;
            if self.scanline > PRE_RENDER_LINE {
                self.scanline = 0;
                self.odd_frame = !self.odd_frame;
            }
        }
    }
}

#[cfg(test)]
mod tests;
