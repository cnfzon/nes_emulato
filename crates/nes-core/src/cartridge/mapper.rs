//! Mapper 定址邏輯與 bank 暫存器。
//!
//! 用 `enum Mapper` 而不是 trait object（`Box<dyn Mapper>`），原因是 mapper
//! 狀態必須完整進 save state：`enum` 可以直接 `derive(Serialize, Deserialize)`，
//! `Box<dyn Trait>` 沒辦法（trait object 不知道具體型別，無法反序列化回正確的
//! variant）。詳見 `docs/architecture.md` 的「為何 Mapper 用 enum」章節。
//!
//! **變體順序就是存檔的 variant tag**：只能在最後追加，不能重排或插入
//! （否則要遞增 `STATE_FORMAT_VERSION`）。
//!
//! # 安全性
//!
//! bank 暫存器是存檔可還原的欄位，可能是任意值；所有位址換算都用「實際 ROM
//! 長度」取模，不依賴任何額外的 bank 數欄位，所以不論暫存器內容為何都不會越界
//! （`run_frame` 路徑不得 panic）。
//!
//! # 不模擬 bus conflict
//!
//! UxROM / CNROM 的部分卡帶（沒有把 ROM 輸出與 CPU 資料匯流排隔離）在寫入時會發生
//! 匯流排衝突：實際寫入的值是「CPU 送出的值 AND 該位址 ROM 的內容」。本專案
//! **不模擬**：寫入值直接生效。理由：多數遊戲的寫入值本來就與 ROM 內容一致
//! （軟體已避開衝突），模擬與否結果相同；模擬它需要在寫入時讀 ROM，且沒有任何
//! 驗收 ROM 依賴它。已知風險：極少數靠 bus conflict 取值的遊戲會表現不同。

use super::{CHR_BANK_SIZE, Mirroring, PRG_BANK_SIZE};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum Mapper {
    Nrom(Nrom),
    Mmc1(Mmc1),
    Uxrom(Uxrom),
    Cnrom(Cnrom),
}

/// 一個 mapper 除錯資訊的列：`(名稱, 內容)`。
pub type DebugRow = (String, String);

impl Mapper {
    /// iNES mapper 編號。
    pub fn id(&self) -> u8 {
        match self {
            Mapper::Nrom(_) => 0,
            Mapper::Mmc1(_) => 1,
            Mapper::Uxrom(_) => 2,
            Mapper::Cnrom(_) => 3,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Mapper::Nrom(_) => "NROM",
            Mapper::Mmc1(_) => "MMC1",
            Mapper::Uxrom(_) => "UxROM",
            Mapper::Cnrom(_) => "CNROM",
        }
    }

    /// 讀 CPU 位址 `$8000..=$FFFF` 的一個 byte。
    pub fn read_prg(&self, prg_rom: &[u8], addr: u16) -> u8 {
        match self {
            Mapper::Nrom(m) => m.read_prg(prg_rom, addr),
            Mapper::Mmc1(m) => m.read_prg(prg_rom, addr),
            Mapper::Uxrom(m) => m.read_prg(prg_rom, addr),
            Mapper::Cnrom(_) => mirrored_prg(prg_rom, addr),
        }
    }

    /// CPU 寫入 `$8000..=$FFFF`（mapper 暫存器）。
    ///
    /// `consecutive`：這次寫入緊接在上一個 CPU cycle 的寫入之後。instruction-level
    /// 的 CPU 只有讀-改-寫指令會產生這種情況（先寫舊值、下一個 cycle 再寫新值），
    /// 由 [`crate::Bus::write_rmw`] 標記；MMC1 會忽略它。
    pub fn write_prg(&mut self, addr: u16, value: u8, consecutive: bool) {
        match self {
            Mapper::Nrom(_) => {}
            Mapper::Mmc1(m) => m.write(addr, value, consecutive),
            Mapper::Uxrom(m) => m.bank = value,
            Mapper::Cnrom(m) => m.chr_bank = value,
        }
    }

    /// PPU 位址 `$0000..=$1FFF` 對應到 CHR 記憶體（長度 `chr_len`）內的偏移。
    /// 結果一定 `< chr_len`（`chr_len` 為 0 時回傳 0）。讀與寫共用。
    pub fn chr_offset(&self, addr: u16, chr_len: usize) -> usize {
        let addr = (addr & 0x1FFF) as usize;
        let offset = match self {
            Mapper::Nrom(_) | Mapper::Uxrom(_) => addr,
            Mapper::Mmc1(m) => m.chr_offset(addr),
            Mapper::Cnrom(m) => m.chr_bank as usize * CHR_BANK_SIZE + addr,
        };
        offset % chr_len.max(1)
    }

    pub fn read_chr(&self, chr: &[u8], addr: u16) -> u8 {
        chr.get(self.chr_offset(addr, chr.len()))
            .copied()
            .unwrap_or(0)
    }

    /// mapper 自己控制的 nametable mirroring；`None` 代表由 iNES header 固定。
    pub fn mirroring(&self) -> Option<Mirroring> {
        match self {
            Mapper::Mmc1(m) => Some(m.mirroring()),
            _ => None,
        }
    }

    /// `$6000-$7FFF` 的 PRG-RAM 目前是否啟用。停用時讀取回傳 open bus、寫入被忽略。
    pub fn prg_ram_enabled(&self) -> bool {
        match self {
            Mapper::Mmc1(m) => m.prg_ram_enabled(),
            _ => true,
        }
    }

    /// 存檔還原後的欄位是否在硬體可能出現的範圍內。
    pub(crate) fn is_structurally_valid(&self) -> bool {
        match self {
            Mapper::Mmc1(m) => m.is_structurally_valid(),
            _ => true,
        }
    }

    /// Debugger 顯示用：bank 暫存器的原始內容，接著是它們目前造成的實際對應。
    /// `prg_rom_len` / `chr_len` 是目前 ROM 與 CHR 記憶體的位元組數。
    pub fn debug_rows(&self, prg_rom_len: usize, chr_len: usize) -> Vec<DebugRow> {
        let n = prg_banks(prg_rom_len);
        let row = |k: &str, v: String| (k.to_string(), v);
        match self {
            Mapper::Nrom(_) => vec![row("PRG", format!("{} 個 16KB bank，無切換", n))],
            Mapper::Uxrom(m) => vec![
                row("bank 暫存器", format!("${:02X}", m.bank)),
                row("PRG $8000-$BFFF", format!("bank {}", m.bank as usize % n)),
                row("PRG $C000-$FFFF", format!("bank {}（固定最後一個）", n - 1)),
            ],
            Mapper::Cnrom(m) => {
                let chr_banks = (chr_len / CHR_BANK_SIZE).max(1);
                vec![
                    row("CHR bank 暫存器", format!("${:02X}", m.chr_bank)),
                    row(
                        "CHR $0000-$1FFF",
                        format!("8KB bank {}", m.chr_bank as usize % chr_banks),
                    ),
                ]
            }
            Mapper::Mmc1(m) => {
                let (lo, hi) = m.prg_banks(n);
                let (c0, c1) = m.chr_banks_4k();
                let chr_4k = (chr_len / 0x1000).max(1);
                vec![
                    row("Control ($8000)", format!("${:02X}", m.control)),
                    row("CHR bank 0 ($A000)", format!("${:02X}", m.chr0)),
                    row("CHR bank 1 ($C000)", format!("${:02X}", m.chr1)),
                    row("PRG bank ($E000)", format!("${:02X}", m.prg)),
                    row(
                        "移位暫存器",
                        format!("${:02X}（已寫入 {} / 5 bit）", m.shift, m.shift_count),
                    ),
                    row("PRG 模式", m.prg_mode_text().to_string()),
                    row("PRG $8000-$BFFF", format!("bank {lo}")),
                    row("PRG $C000-$FFFF", format!("bank {hi}")),
                    row(
                        "CHR 模式",
                        if m.control & 0x10 == 0 {
                            "8KB".into()
                        } else {
                            "兩個 4KB".to_string()
                        },
                    ),
                    row("CHR $0000-$0FFF", format!("4KB bank {}", c0 % chr_4k)),
                    row("CHR $1000-$1FFF", format!("4KB bank {}", c1 % chr_4k)),
                    row("Mirroring", format!("{:?}", m.mirroring())),
                    row(
                        "PRG-RAM",
                        if m.prg_ram_enabled() {
                            "啟用".into()
                        } else {
                            "停用".to_string()
                        },
                    ),
                ]
            }
        }
    }
}

/// PRG-ROM 有幾個 16KB bank（至少 1，避免取模時除以 0）。
fn prg_banks(prg_rom_len: usize) -> usize {
    (prg_rom_len / PRG_BANK_SIZE).max(1)
}

/// 讀 16KB 一個 bank 內的一個 byte（`bank` 會先對實際 bank 數取模）。
fn read_bank16(prg_rom: &[u8], bank: usize, addr: u16) -> u8 {
    let index = (bank % prg_banks(prg_rom.len())) * PRG_BANK_SIZE + (addr as usize & 0x3FFF);
    prg_rom.get(index).copied().unwrap_or(0)
}

/// 16KB ROM 鏡像成 32KB、32KB 直接對應（NROM、CNROM 的 PRG）。
fn mirrored_prg(prg_rom: &[u8], addr: u16) -> u8 {
    let offset = addr.wrapping_sub(0x8000) as usize;
    prg_rom
        .get(offset % prg_rom.len().max(1))
        .copied()
        .unwrap_or(0)
}

// ---- Mapper 0：NROM ----------------------------------------------------

/// Mapper 0（NROM）：沒有 bank switching，PRG-ROM 16KB 時鏡像成 32KB。
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub struct Nrom {
    prg_banks: u8,
}

impl Nrom {
    pub fn new(prg_banks: u8) -> Self {
        Self { prg_banks }
    }

    /// `addr` 必須落在 CPU 位址空間的 `$8000..=$FFFF`。
    pub fn read_prg(&self, prg_rom: &[u8], addr: u16) -> u8 {
        let mut offset = (addr.wrapping_sub(0x8000)) as usize;
        if self.prg_banks <= 1 {
            offset %= PRG_BANK_SIZE;
        }
        prg_rom
            .get(offset % prg_rom.len().max(1))
            .copied()
            .unwrap_or(0)
    }

    /// `addr` 必須落在 PPU pattern table 位址空間的 `$0000..=$1FFF`。
    pub fn read_chr(&self, chr: &[u8], addr: u16) -> u8 {
        chr.get(addr as usize % chr.len().max(1))
            .copied()
            .unwrap_or(0)
    }
}

// ---- Mapper 1：MMC1 ----------------------------------------------------

/// Mapper 1（MMC1）：5-bit 序列寫入的移位暫存器 + 四個內部暫存器。
///
/// - `$8000-$9FFF` → control：bit 0–1 mirroring（0 單畫面 A、1 單畫面 B、2 vertical、
///   3 horizontal）、bit 2–3 PRG 模式、bit 4 CHR 模式。
/// - `$A000-$BFFF` → CHR bank 0；`$C000-$DFFF` → CHR bank 1。
/// - `$E000-$FFFF` → PRG bank（bit 0–3）與 PRG-RAM 停用旗標（bit 4）。
///
/// 只支援 PRG ≤ 256KB（沒有 SUROM 的 512KB 外掛 bit）與固定 8KB PRG-RAM
/// （沒有 SXROM 的 PRG-RAM 換 bank）。超出的 bank 編號對實際 ROM 大小取模。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Mmc1 {
    /// 移位暫存器（5 bit）：新的 bit 從 bit 4 進入、往低位移。
    shift: u8,
    /// 移位暫存器已收到幾個 bit（0–4；收滿 5 個就寫進暫存器並清為 0）。
    shift_count: u8,
    control: u8,
    chr0: u8,
    chr1: u8,
    prg: u8,
}

impl Default for Mmc1 {
    fn default() -> Self {
        Self::new()
    }
}

impl Mmc1 {
    /// 開機狀態：control = `$0C`（PRG 模式 3：`$C000` 固定最後一個 bank，
    /// `$8000` 可切換）。真實晶片的開機值不確定，這是常見且能讓所有遊戲的
    /// reset 程式啟動的選擇；遊戲一定會先寫 `$80` 自行重置。
    pub fn new() -> Self {
        Self {
            shift: 0,
            shift_count: 0,
            control: 0x0C,
            chr0: 0,
            chr1: 0,
            prg: 0,
        }
    }

    fn write(&mut self, addr: u16, value: u8, consecutive: bool) {
        // 連續 CPU cycle 的第二次寫入被硬體忽略（見 `Mapper::write_prg`）。
        if consecutive {
            return;
        }
        if value & 0x80 != 0 {
            // 重置移位暫存器，並把 PRG 模式設為 3（control |= $0C）。
            self.shift = 0;
            self.shift_count = 0;
            self.control |= 0x0C;
            return;
        }
        self.shift = (self.shift >> 1) | ((value & 1) << 4);
        self.shift_count += 1;
        if self.shift_count == 5 {
            let data = self.shift & 0x1F;
            // 目標暫存器由第 5 次寫入的位址 bit 13–14 決定。
            match (addr >> 13) & 3 {
                0 => self.control = data,
                1 => self.chr0 = data,
                2 => self.chr1 = data,
                _ => self.prg = data,
            }
            self.shift = 0;
            self.shift_count = 0;
        }
    }

    /// `$8000-$BFFF` 與 `$C000-$FFFF` 各自對應的 16KB bank（已對 `n` 取模）。
    fn prg_banks(&self, n: usize) -> (usize, usize) {
        let bank = (self.prg & 0x0F) as usize;
        let (lo, hi) = match (self.control >> 2) & 3 {
            // 32KB 模式：忽略 bank 的最低位，一次換兩個 16KB。
            0 | 1 => (bank & !1, (bank & !1) + 1),
            // `$8000` 固定第一個 bank，`$C000` 可切換。
            2 => (0, bank),
            // `$8000` 可切換，`$C000` 固定最後一個 bank。
            _ => (bank, n - 1),
        };
        (lo % n, hi % n)
    }

    fn prg_mode_text(&self) -> &'static str {
        match (self.control >> 2) & 3 {
            0 | 1 => "0/1：32KB 切換",
            2 => "2：$8000 固定第一個，$C000 切換",
            _ => "3：$8000 切換，$C000 固定最後一個",
        }
    }

    fn read_prg(&self, prg_rom: &[u8], addr: u16) -> u8 {
        let n = prg_banks(prg_rom.len());
        let (lo, hi) = self.prg_banks(n);
        read_bank16(prg_rom, if addr < 0xC000 { lo } else { hi }, addr)
    }

    /// `$0000-$0FFF` 與 `$1000-$1FFF` 各自對應的 4KB CHR bank（未取模）。
    fn chr_banks_4k(&self) -> (usize, usize) {
        if self.control & 0x10 == 0 {
            // 8KB 模式：忽略 CHR bank 0 的最低位，一次換兩個 4KB。
            let base = (self.chr0 & 0x1E) as usize;
            (base, base + 1)
        } else {
            (self.chr0 as usize, self.chr1 as usize)
        }
    }

    /// `addr`：`$0000..=$1FFF`。
    fn chr_offset(&self, addr: usize) -> usize {
        let (b0, b1) = self.chr_banks_4k();
        let bank = if addr < 0x1000 { b0 } else { b1 };
        bank * 0x1000 + (addr & 0x0FFF)
    }

    fn mirroring(&self) -> Mirroring {
        match self.control & 3 {
            0 => Mirroring::SingleScreenLower,
            1 => Mirroring::SingleScreenUpper,
            2 => Mirroring::Vertical,
            _ => Mirroring::Horizontal,
        }
    }

    fn prg_ram_enabled(&self) -> bool {
        self.prg & 0x10 == 0
    }

    fn is_structurally_valid(&self) -> bool {
        self.shift < 0x20
            && self.shift_count < 5
            && self.control < 0x20
            && self.chr0 < 0x20
            && self.chr1 < 0x20
            && self.prg < 0x20
    }
}

// ---- Mapper 2：UxROM ---------------------------------------------------

/// Mapper 2（UxROM）：`$8000-$BFFF` 可切換 16KB bank，`$C000-$FFFF` 固定為
/// 最後一個 bank；CHR 通常是 8KB CHR-RAM。任何 `$8000-$FFFF` 的寫入都是 bank 值。
#[derive(Debug, Clone, Copy, Default, serde::Serialize, serde::Deserialize)]
pub struct Uxrom {
    bank: u8,
}

impl Uxrom {
    pub fn new() -> Self {
        Self::default()
    }

    fn read_prg(&self, prg_rom: &[u8], addr: u16) -> u8 {
        let n = prg_banks(prg_rom.len());
        let bank = if addr < 0xC000 {
            self.bank as usize
        } else {
            n - 1
        };
        read_bank16(prg_rom, bank, addr)
    }
}

// ---- Mapper 3：CNROM ---------------------------------------------------

/// Mapper 3（CNROM）：PRG 固定（同 NROM），任何 `$8000-$FFFF` 的寫入切換 8KB CHR bank。
#[derive(Debug, Clone, Copy, Default, serde::Serialize, serde::Deserialize)]
pub struct Cnrom {
    chr_bank: u8,
}

impl Cnrom {
    pub fn new() -> Self {
        Self::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 每個 16KB bank 的第一個 byte 是 bank 編號的 PRG-ROM。
    fn numbered_prg(banks: usize) -> Vec<u8> {
        let mut prg = vec![0u8; banks * PRG_BANK_SIZE];
        for b in 0..banks {
            prg[b * PRG_BANK_SIZE] = b as u8;
        }
        prg
    }

    /// 每個 4KB 的第一個 byte 是 4KB bank 編號的 CHR-ROM（共 `banks_4k` 個）。
    fn numbered_chr(banks_4k: usize) -> Vec<u8> {
        let mut chr = vec![0u8; banks_4k * 0x1000];
        for b in 0..banks_4k {
            chr[b * 0x1000] = b as u8;
        }
        chr
    }

    /// 依 MMC1 的規則送 5 次寫入（bit 0 先進），寫到 `addr` 指定的暫存器。
    fn mmc1_write5(m: &mut Mapper, addr: u16, value: u8) {
        for i in 0..5 {
            m.write_prg(addr, (value >> i) & 1, false);
        }
    }

    #[test]
    fn nrom_32kb_reads_directly() {
        let mut prg = vec![0u8; 2 * PRG_BANK_SIZE];
        prg[0] = 0x11;
        prg[PRG_BANK_SIZE] = 0x22;
        let nrom = Nrom::new(2);
        assert_eq!(nrom.read_prg(&prg, 0x8000), 0x11);
        assert_eq!(nrom.read_prg(&prg, 0xC000), 0x22);
    }

    #[test]
    fn nrom_16kb_mirrors_across_both_halves() {
        let mut prg = vec![0u8; PRG_BANK_SIZE];
        prg[0] = 0x42;
        let nrom = Nrom::new(1);
        assert_eq!(nrom.read_prg(&prg, 0x8000), 0x42);
        assert_eq!(nrom.read_prg(&prg, 0xC000), 0x42);
    }

    #[test]
    fn uxrom_switches_8000_and_fixes_c000_to_the_last_bank() {
        let prg = numbered_prg(8);
        let mut m = Mapper::Uxrom(Uxrom::new());
        assert_eq!(m.read_prg(&prg, 0x8000), 0);
        assert_eq!(m.read_prg(&prg, 0xC000), 7);
        m.write_prg(0x8000, 3, false);
        assert_eq!(m.read_prg(&prg, 0x8000), 3);
        assert_eq!(m.read_prg(&prg, 0xC000), 7, "$C000 永遠是最後一個 bank");
        m.write_prg(0xFFFF, 5, false); // 整個 $8000-$FFFF 都是 bank 暫存器
        assert_eq!(m.read_prg(&prg, 0xBFFF), 0, "bank 5 的最後一個 byte 是 0");
        assert_eq!(m.read_prg(&prg, 0x8000), 5);
        m.write_prg(0x8000, 0xFF, false); // 超出範圍取模，不 panic
        assert_eq!(m.read_prg(&prg, 0x8000), 0xFF % 8);
    }

    #[test]
    fn cnrom_switches_8kb_chr_and_keeps_prg_fixed() {
        let prg = numbered_prg(2);
        let chr: Vec<u8> = (0..4u8).flat_map(|b| vec![b; CHR_BANK_SIZE]).collect();
        let mut m = Mapper::Cnrom(Cnrom::new());
        assert_eq!(m.read_chr(&chr, 0x0000), 0);
        m.write_prg(0x8000, 2, false);
        assert_eq!(m.read_chr(&chr, 0x0000), 2);
        assert_eq!(m.read_chr(&chr, 0x1FFF), 2);
        m.write_prg(0x8000, 7, false); // 7 % 4 = 3
        assert_eq!(m.read_chr(&chr, 0x0123), 3);
        assert_eq!(m.read_prg(&prg, 0x8000), 0);
        assert_eq!(m.read_prg(&prg, 0xC000), 1);
    }

    #[test]
    fn mmc1_five_writes_load_a_register_lsb_first() {
        let mut m = Mapper::Mmc1(Mmc1::new());
        // 只寫 4 次：還沒生效。
        for _ in 0..4 {
            m.write_prg(0x8000, 1, false);
        }
        let Mapper::Mmc1(r) = &m else { unreachable!() };
        assert_eq!(r.control, 0x0C);
        assert_eq!(r.shift_count, 4);
        // 第 5 次寫入 0：收到 1,1,1,1,0（先進的在低位）→ 0b01111 = $0F。
        m.write_prg(0x8000, 0, false);
        let Mapper::Mmc1(r) = &m else { unreachable!() };
        assert_eq!(r.control, 0x0F);
        assert_eq!((r.shift, r.shift_count), (0, 0), "寫入後移位暫存器清空");
    }

    #[test]
    fn mmc1_register_is_selected_by_the_address_of_the_fifth_write() {
        let mut m = Mapper::Mmc1(Mmc1::new());
        mmc1_write5(&mut m, 0xA000, 0x05);
        mmc1_write5(&mut m, 0xC000, 0x06);
        mmc1_write5(&mut m, 0xE000, 0x07);
        mmc1_write5(&mut m, 0x9FFF, 0x12);
        let Mapper::Mmc1(r) = &m else { unreachable!() };
        assert_eq!((r.chr0, r.chr1, r.prg, r.control), (5, 6, 7, 0x12));
    }

    #[test]
    fn mmc1_bit7_resets_the_shift_register_and_forces_prg_mode_3() {
        let mut m = Mapper::Mmc1(Mmc1::new());
        mmc1_write5(&mut m, 0x8000, 0x00); // control = 0：PRG 模式 0、CHR 8KB、單畫面 A
        m.write_prg(0x8000, 1, false);
        m.write_prg(0x8000, 1, false);
        m.write_prg(0x8000, 0x80, false); // 重置
        let Mapper::Mmc1(r) = &m else { unreachable!() };
        assert_eq!((r.shift, r.shift_count), (0, 0));
        assert_eq!(r.control & 0x0C, 0x0C, "PRG 模式被設為 3");
        // 重置後又從第 1 個 bit 開始數。
        mmc1_write5(&mut m, 0xE000, 0x03);
        let Mapper::Mmc1(r) = &m else { unreachable!() };
        assert_eq!(r.prg, 3);
    }

    #[test]
    fn mmc1_prg_modes() {
        let prg = numbered_prg(8);
        let mut m = Mapper::Mmc1(Mmc1::new());
        let first = |m: &Mapper, a| m.read_prg(&prg, a);

        // 模式 3（開機預設）：$8000 切換、$C000 固定最後。
        mmc1_write5(&mut m, 0xE000, 2);
        assert_eq!((first(&m, 0x8000), first(&m, 0xC000)), (2, 7));

        // 模式 2：$8000 固定第一個、$C000 切換。
        mmc1_write5(&mut m, 0x8000, 0b01000 | 0b00010);
        mmc1_write5(&mut m, 0xE000, 5);
        assert_eq!((first(&m, 0x8000), first(&m, 0xC000)), (0, 5));

        // 模式 0/1：32KB，忽略最低位。bank 5 → 4、5。
        for mode_bits in [0b00000u8, 0b00100] {
            mmc1_write5(&mut m, 0x8000, mode_bits | 0b00010);
            mmc1_write5(&mut m, 0xE000, 5);
            assert_eq!((first(&m, 0x8000), first(&m, 0xC000)), (4, 5));
            assert_eq!(first(&m, 0xBFFF), 0);
        }
    }

    #[test]
    fn mmc1_chr_modes() {
        let chr = numbered_chr(8); // 8 個 4KB bank
        let mut m = Mapper::Mmc1(Mmc1::new());

        // 4KB 模式：兩個獨立的 4KB。
        mmc1_write5(&mut m, 0x8000, 0b10000 | 0b01110 | 0b00010);
        mmc1_write5(&mut m, 0xA000, 3);
        mmc1_write5(&mut m, 0xC000, 6);
        assert_eq!(m.read_chr(&chr, 0x0000), 3);
        assert_eq!(m.read_chr(&chr, 0x1000), 6);

        // 8KB 模式：只看 CHR bank 0、忽略最低位；bank 3 → 4KB bank 2、3。
        mmc1_write5(&mut m, 0x8000, 0b01110);
        assert_eq!(m.read_chr(&chr, 0x0000), 2);
        assert_eq!(m.read_chr(&chr, 0x1000), 3);
    }

    #[test]
    fn mmc1_control_selects_mirroring() {
        let mut m = Mapper::Mmc1(Mmc1::new());
        for (bits, expected) in [
            (0, Mirroring::SingleScreenLower),
            (1, Mirroring::SingleScreenUpper),
            (2, Mirroring::Vertical),
            (3, Mirroring::Horizontal),
        ] {
            mmc1_write5(&mut m, 0x8000, 0b01100 | bits);
            assert_eq!(m.mirroring(), Some(expected));
        }
        assert_eq!(Mapper::Nrom(Nrom::new(1)).mirroring(), None);
    }

    #[test]
    fn mmc1_prg_ram_can_be_disabled_by_prg_bit4() {
        let mut m = Mapper::Mmc1(Mmc1::new());
        assert!(m.prg_ram_enabled());
        mmc1_write5(&mut m, 0xE000, 0x10);
        assert!(!m.prg_ram_enabled());
        mmc1_write5(&mut m, 0xE000, 0x00);
        assert!(m.prg_ram_enabled());
        assert!(Mapper::Uxrom(Uxrom::new()).prg_ram_enabled());
    }

    #[test]
    fn mmc1_ignores_a_write_on_the_cycle_right_after_another_write() {
        // 序列寫入的第 5 次是「連續」寫入 → 被忽略，暫存器不會載入。
        let mut m = Mapper::Mmc1(Mmc1::new());
        for _ in 0..4 {
            m.write_prg(0xE000, 1, false);
        }
        m.write_prg(0xE000, 1, true);
        let Mapper::Mmc1(r) = &m else { unreachable!() };
        assert_eq!((r.prg, r.shift_count), (0, 4));

        // 連續寫入的 bit 7 也不會重置。
        m.write_prg(0x8000, 0x80, true);
        let Mapper::Mmc1(r) = &m else { unreachable!() };
        assert_eq!(r.shift_count, 4);
    }

    #[test]
    fn out_of_range_bank_registers_never_index_out_of_bounds() {
        let prg = numbered_prg(2);
        let chr = numbered_chr(2);
        let mut mmc1 = Mmc1::new();
        mmc1.prg = 0x1F;
        mmc1.chr0 = 0x1F;
        mmc1.chr1 = 0x1F;
        let m = Mapper::Mmc1(mmc1);
        for addr in [0x8000u16, 0xBFFF, 0xC000, 0xFFFF] {
            let _ = m.read_prg(&prg, addr);
        }
        for addr in [0x0000u16, 0x0FFF, 0x1000, 0x1FFF] {
            let _ = m.read_chr(&chr, addr);
        }
        let _ = m.read_chr(&[], 0x0000);
        let _ = m.read_prg(&[], 0x8000);
    }

    #[test]
    fn debug_rows_report_the_effective_banks() {
        let mut m = Mapper::Mmc1(Mmc1::new());
        mmc1_write5(&mut m, 0xE000, 2);
        let rows = m.debug_rows(8 * PRG_BANK_SIZE, 8 * 0x1000);
        let get = |k: &str| rows.iter().find(|(name, _)| name == k).unwrap().1.clone();
        assert_eq!(get("PRG $8000-$BFFF"), "bank 2");
        assert_eq!(get("PRG $C000-$FFFF"), "bank 7");
        assert_eq!(get("PRG bank ($E000)"), "$02");
    }

    #[test]
    fn tampered_mmc1_registers_are_reported_as_structurally_invalid() {
        assert!(Mmc1::new().is_structurally_valid());
        for tamper in [
            (|m: &mut Mmc1| m.shift_count = 5) as fn(&mut Mmc1),
            |m| m.shift = 0x20,
            |m| m.control = 0xFF,
            |m| m.chr0 = 0x20,
            |m| m.chr1 = 0x80,
            |m| m.prg = 0x20,
        ] {
            let mut m = Mmc1::new();
            tamper(&mut m);
            assert!(!Mapper::Mmc1(m).is_structurally_valid());
        }
    }
}
