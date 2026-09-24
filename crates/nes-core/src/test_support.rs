//! 測試用的迷你組譯器與合成 ROM 產生器（只在 `cargo test` 或啟用 `testing`
//! feature 時編譯）。
//!
//! PPU 的行為要用「真的會跑的程式」才測得到（NMI、掃描、OAM DMA、搖桿）。
//! 這裡提供剛好夠用的 6502 機器碼產生器，讓測試能在 repo 內自帶 ROM，而不
//! 依賴被 gitignore 的外部測試 ROM。

use crate::cartridge::{CHR_BANK_SIZE, PRG_BANK_SIZE};

/// 6502 機器碼產生器。所有 `emit` 出來的指令從 `org` 開始連續擺放。
pub struct Asm {
    org: u16,
    pub bytes: Vec<u8>,
}

impl Asm {
    pub fn new(org: u16) -> Self {
        Self {
            org,
            bytes: Vec::new(),
        }
    }

    /// 下一條指令的位址。
    pub fn pc(&self) -> u16 {
        self.org + self.bytes.len() as u16
    }

    fn emit(&mut self, bytes: &[u8]) -> &mut Self {
        self.bytes.extend_from_slice(bytes);
        self
    }

    fn abs(&mut self, opcode: u8, addr: u16) -> &mut Self {
        self.emit(&[opcode, addr as u8, (addr >> 8) as u8])
    }

    pub fn sei(&mut self) -> &mut Self {
        self.emit(&[0x78])
    }
    pub fn cld(&mut self) -> &mut Self {
        self.emit(&[0xD8])
    }
    pub fn txs(&mut self) -> &mut Self {
        self.emit(&[0x9A])
    }
    pub fn txa(&mut self) -> &mut Self {
        self.emit(&[0x8A])
    }
    pub fn inx(&mut self) -> &mut Self {
        self.emit(&[0xE8])
    }
    pub fn rti(&mut self) -> &mut Self {
        self.emit(&[0x40])
    }
    pub fn lda_imm(&mut self, v: u8) -> &mut Self {
        self.emit(&[0xA9, v])
    }
    pub fn ldx_imm(&mut self, v: u8) -> &mut Self {
        self.emit(&[0xA2, v])
    }
    pub fn and_imm(&mut self, v: u8) -> &mut Self {
        self.emit(&[0x29, v])
    }
    pub fn cpx_imm(&mut self, v: u8) -> &mut Self {
        self.emit(&[0xE0, v])
    }
    pub fn lda_abs(&mut self, addr: u16) -> &mut Self {
        self.abs(0xAD, addr)
    }
    pub fn lda_abs_x(&mut self, addr: u16) -> &mut Self {
        self.abs(0xBD, addr)
    }
    pub fn sta_abs(&mut self, addr: u16) -> &mut Self {
        self.abs(0x8D, addr)
    }
    pub fn sta_abs_x(&mut self, addr: u16) -> &mut Self {
        self.abs(0x9D, addr)
    }
    pub fn inc_abs(&mut self, addr: u16) -> &mut Self {
        self.abs(0xEE, addr)
    }
    pub fn jmp(&mut self, addr: u16) -> &mut Self {
        self.abs(0x4C, addr)
    }
    pub fn bit_abs(&mut self, addr: u16) -> &mut Self {
        self.abs(0x2C, addr)
    }
    pub fn bvs(&mut self, target: u16) -> &mut Self {
        let next = self.pc() + 2;
        let rel = target.wrapping_sub(next) as i16;
        assert!((-128..=127).contains(&rel), "分支距離超出範圍");
        self.emit(&[0x70, rel as i8 as u8])
    }
    pub fn bvc(&mut self, target: u16) -> &mut Self {
        let next = self.pc() + 2;
        let rel = target.wrapping_sub(next) as i16;
        assert!((-128..=127).contains(&rel), "分支距離超出範圍");
        self.emit(&[0x50, rel as i8 as u8])
    }
    pub fn bne(&mut self, target: u16) -> &mut Self {
        let next = self.pc() + 2;
        let rel = target.wrapping_sub(next) as i16;
        assert!((-128..=127).contains(&rel), "分支距離超出範圍");
        self.emit(&[0xD0, rel as i8 as u8])
    }
    pub fn bpl(&mut self, target: u16) -> &mut Self {
        let next = self.pc() + 2;
        let rel = target.wrapping_sub(next) as i16;
        assert!((-128..=127).contains(&rel), "分支距離超出範圍");
        self.emit(&[0x10, rel as i8 as u8])
    }

    /// `LDA #hi; STA $2006; LDA #lo; STA $2006`：設定 PPU 位址。
    pub fn set_ppu_addr(&mut self, addr: u16) -> &mut Self {
        self.lda_imm((addr >> 8) as u8)
            .sta_abs(0x2006)
            .lda_imm(addr as u8)
            .sta_abs(0x2006)
    }
}

/// 一塊要放進 PRG 的資料：`(CPU 位址, 內容)`。
pub type DataBlock<'a> = (u16, &'a [u8]);

/// 組出一份 32KB PRG + 8KB CHR-ROM 的 NROM ROM。
///
/// - `code` 放在它自己的 `org`（必須落在 `$8000..$FFFA`）。
/// - `reset` 是 RESET 向量；`nmi` 是 NMI 向量（`None` 時指向 `reset`）。
/// - `data` 是額外放進 PRG 的資料區塊（調色盤、精靈表……）。
/// - `chr` 是 CHR-ROM 內容（補零到 8KB）。
/// - `vertical`：iNES header 的 mirroring 位元。
pub fn build_nrom(
    code: &Asm,
    reset: u16,
    nmi: Option<u16>,
    data: &[DataBlock],
    chr: &[u8],
    vertical: bool,
) -> Vec<u8> {
    let mut prg = vec![0u8; 2 * PRG_BANK_SIZE];
    let put = |prg: &mut Vec<u8>, addr: u16, bytes: &[u8]| {
        let start = addr as usize - 0x8000;
        prg[start..start + bytes.len()].copy_from_slice(bytes);
    };
    put(&mut prg, code.org, &code.bytes);
    for (addr, bytes) in data {
        put(&mut prg, *addr, bytes);
    }
    let nmi = nmi.unwrap_or(reset);
    put(&mut prg, 0xFFFA, &[nmi as u8, (nmi >> 8) as u8]);
    put(&mut prg, 0xFFFC, &[reset as u8, (reset >> 8) as u8]);
    put(&mut prg, 0xFFFE, &[reset as u8, (reset >> 8) as u8]);

    let mut chr_rom = chr.to_vec();
    chr_rom.resize(CHR_BANK_SIZE, 0);

    let mut rom = vec![0u8; 16];
    rom[0..4].copy_from_slice(b"NES\x1A");
    rom[4] = 2;
    rom[5] = 1;
    rom[6] = u8::from(vertical);
    rom.extend(prg);
    rom.extend(chr_rom);
    rom
}

/// 一個 8×8 tile 的 CHR 資料：`plane0`/`plane1` 各 8 byte。
pub fn tile(plane0: [u8; 8], plane1: [u8; 8]) -> Vec<u8> {
    let mut t = plane0.to_vec();
    t.extend(plane1);
    t
}

/// 測試用 CHR：tile 0 空白、1/2/3 是純色 1/2/3、4 是棋盤格（色 1 與 0）、
/// 5 是有「缺角」的形狀（色 3，右上角透明）、6 = 純色 3 但只有上半（給 8×16 測）。
pub fn test_chr() -> Vec<u8> {
    let full = [0xFF; 8];
    let none = [0x00; 8];
    let mut chr = Vec::new();
    chr.extend(tile(none, none)); // 0
    chr.extend(tile(full, none)); // 1: 色 1
    chr.extend(tile(none, full)); // 2: 色 2
    chr.extend(tile(full, full)); // 3: 色 3
    chr.extend(tile([0xAA; 8], none)); // 4: 棋盤 / 直條
    chr.extend(tile(
        [0xF0, 0xF0, 0xF0, 0xF0, 0xFF, 0xFF, 0xFF, 0xFF],
        [0xF0, 0xF0, 0xF0, 0xF0, 0xFF, 0xFF, 0xFF, 0xFF],
    )); // 5: 上半只有左 4 格
    chr.extend(tile(
        [0xFF, 0xFF, 0xFF, 0xFF, 0x00, 0x00, 0x00, 0x00],
        [0xFF, 0xFF, 0xFF, 0xFF, 0x00, 0x00, 0x00, 0x00],
    )); // 6: 只有上半
    chr
}

/// 會實際渲染的合成 ROM：上傳調色盤/nametable/屬性/精靈，開啟 NMI 與渲染，
/// NMI 處理常式每幀把 `$00` 加 1 並把它寫進水平捲動（畫面每幀左移 1 px）。
///
/// 給決定性 / rollback / golden frame / 效能測試用。
pub fn rendering_rom() -> Vec<u8> {
    const PALETTE: [u8; 32] = [
        0x0F, 0x16, 0x2A, 0x30, 0x0F, 0x11, 0x21, 0x31, 0x0F, 0x15, 0x25, 0x35, 0x0F, 0x19, 0x29,
        0x39, 0x0F, 0x06, 0x1A, 0x30, 0x0F, 0x12, 0x22, 0x32, 0x0F, 0x14, 0x24, 0x34, 0x0F, 0x18,
        0x28, 0x38,
    ];
    // 4 個精靈：(Y, tile, attr, X)
    const SPRITES: [u8; 16] = [
        39, 5, 0x00, 100, // 0：前景
        60, 3, 0x20, 100, // 1：在背景之後
        80, 5, 0xC1, 60, // 2：翻轉 + 調色盤 1
        120, 5, 0x03, 250, // 3：靠近右緣
    ];
    const PALETTE_ADDR: u16 = 0x8200;
    const SPRITE_ADDR: u16 = 0x8240;
    const NMI_ADDR: u16 = 0x8100;

    let mut a = Asm::new(0x8000);
    a.sei().cld().ldx_imm(0xFF).txs();

    // 調色盤
    a.set_ppu_addr(0x3F00).ldx_imm(0);
    let l = a.pc();
    a.lda_abs_x(PALETTE_ADDR)
        .sta_abs(0x2007)
        .inx()
        .cpx_imm(32)
        .bne(l);

    // nametable 0 前 512 個位置：tile = X & 7
    a.set_ppu_addr(0x2000).ldx_imm(0);
    let l = a.pc();
    a.txa().and_imm(0x07).sta_abs(0x2007).inx().bne(l);
    let l = a.pc();
    a.txa().and_imm(0x07).sta_abs(0x2007).inx().bne(l);

    // 屬性表 $23C0：64 個位元組，值 = X
    a.set_ppu_addr(0x23C0).ldx_imm(0);
    let l = a.pc();
    a.txa().sta_abs(0x2007).inx().cpx_imm(64).bne(l);

    // 精靈 → OAM
    a.lda_imm(0).sta_abs(0x2003).ldx_imm(0);
    let l = a.pc();
    a.lda_abs_x(SPRITE_ADDR)
        .sta_abs(0x2004)
        .inx()
        .cpx_imm(16)
        .bne(l);

    // 捲動 0、開 NMI、開渲染
    a.lda_imm(0).sta_abs(0x2005).sta_abs(0x2005);
    a.lda_imm(0x80).sta_abs(0x2000);
    a.lda_imm(0x1E).sta_abs(0x2001);
    let forever = a.pc();
    a.jmp(forever);

    let mut nmi = Asm::new(NMI_ADDR);
    nmi.inc_abs(0x0000)
        .lda_abs(0x0000)
        .sta_abs(0x2005)
        .lda_imm(0)
        .sta_abs(0x2005)
        .rti();

    build_nrom(
        &a,
        0x8000,
        Some(NMI_ADDR),
        &[
            (NMI_ADDR, &nmi.bytes),
            (PALETTE_ADDR, &PALETTE),
            (SPRITE_ADDR, &SPRITES),
        ],
        &test_chr(),
        false,
    )
}
