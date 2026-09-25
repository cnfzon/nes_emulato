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

    pub fn cmp_imm(&mut self, v: u8) -> &mut Self {
        self.emit(&[0xC9, v])
    }
    /// `BEQ`，往前跳過接下來的 `skip` 個位元組。
    pub fn beq_skip(&mut self, skip: u8) -> &mut Self {
        self.emit(&[0xF0, skip])
    }

    pub fn sei(&mut self) -> &mut Self {
        self.emit(&[0x78])
    }
    pub fn cli(&mut self) -> &mut Self {
        self.emit(&[0x58])
    }
    pub fn pha(&mut self) -> &mut Self {
        self.emit(&[0x48])
    }
    pub fn pla(&mut self) -> &mut Self {
        self.emit(&[0x68])
    }
    pub fn nop(&mut self) -> &mut Self {
        self.emit(&[0xEA])
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
    build_nrom_irq(code, reset, nmi, None, data, chr, vertical)
}

/// 同 [`build_nrom`]，另外可以指定 IRQ／BRK 向量（`None` 時指向 `reset`）。
pub fn build_nrom_irq(
    code: &Asm,
    reset: u16,
    nmi: Option<u16>,
    irq: Option<u16>,
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
    let irq = irq.unwrap_or(reset);
    put(&mut prg, 0xFFFE, &[irq as u8, (irq >> 8) as u8]);

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

/// `build_mapper_rom` 在每個 16KB PRG bank 偏移 `$0100` 放的標記：`PRG_MARK + bank 編號`
/// （CPU 位址 `$8100` 或 `$C100` 讀得到目前對應到的是哪個 bank）。
pub const PRG_MARK: u8 = 0xA0;
/// `build_mapper_rom` 在每個 4KB CHR 區塊的第一個 byte 放的標記：`CHR_MARK + 4KB 編號`。
pub const CHR_MARK: u8 = 0xC0;

/// 組出 mapper 測試用 ROM：`prg_banks` 個 16KB PRG bank（每個有 [`PRG_MARK`] 標記）、
/// `chr_banks_8k` 個 8KB CHR-ROM bank（每個 4KB 有 [`CHR_MARK`] 標記；0 代表 CHR-RAM）。
///
/// - `code` 必須落在 `$C000..$FFFA`（最後一個 bank；UxROM / MMC1 模式 3 / CNROM 下
///   都固定可見），RESET / NMI / IRQ 向量都指向 `code` 的起點。
/// - `data`：額外資料，位址 `< $C000` 放進 bank 0、否則放進最後一個 bank。
pub fn build_mapper_rom(
    mapper: u8,
    prg_banks: usize,
    chr_banks_8k: usize,
    code: &Asm,
    data: &[DataBlock],
) -> Vec<u8> {
    assert!(code.org >= 0xC000, "code 必須放在最後一個 bank");
    let mut prg = vec![0u8; prg_banks * PRG_BANK_SIZE];
    for bank in 0..prg_banks {
        prg[bank * PRG_BANK_SIZE + 0x100] = PRG_MARK + bank as u8;
    }
    let last = (prg_banks - 1) * PRG_BANK_SIZE;
    let put = |prg: &mut Vec<u8>, addr: u16, bytes: &[u8]| {
        let base = if addr < 0xC000 { 0 } else { last };
        let start = base + (addr as usize & 0x3FFF);
        prg[start..start + bytes.len()].copy_from_slice(bytes);
    };
    put(&mut prg, code.org, &code.bytes);
    for (addr, bytes) in data {
        put(&mut prg, *addr, bytes);
    }
    let entry = code.org;
    for vector in [0xFFFAu16, 0xFFFC, 0xFFFE] {
        put(&mut prg, vector, &[entry as u8, (entry >> 8) as u8]);
    }

    let mut chr = vec![0u8; chr_banks_8k * CHR_BANK_SIZE];
    for block in 0..chr_banks_8k * 2 {
        chr[block * 0x1000] = CHR_MARK + block as u8;
    }

    let mut rom = vec![0u8; 16];
    rom[0..4].copy_from_slice(b"NES\x1A");
    rom[4] = prg_banks as u8;
    rom[5] = chr_banks_8k as u8;
    rom[6] = mapper << 4;
    rom[7] = mapper & 0xF0;
    rom.extend(prg);
    rom.extend(chr);
    rom
}

// ---- mapper 合成測試 ROM（blargg `$6000` 協定）----------------------------------

/// 失敗／通過處理常式的位址（都在最後一個 bank，永遠可見）。
const REPORT_FAIL: u16 = 0xF000;
const REPORT_PASS: u16 = 0xF100;

fn store_text(a: &mut Asm, text: &[u8]) {
    for (i, byte) in text.iter().enumerate() {
        a.lda_imm(*byte).sta_abs(0x6004 + i as u16);
    }
}

/// 測試程式的骨架：先寫「執行中」狀態與簽章 `DE B0 61`，`check` 逐項加入檢查，
/// 全部通過就跳到 PASS。每項檢查失敗時以「第幾項」（1 起算）當結果碼。
struct Checker {
    asm: Asm,
    count: u8,
}

impl Checker {
    fn new() -> Self {
        let mut asm = Asm::new(0xE000);
        asm.sei().cld().ldx_imm(0xFF).txs();
        asm.lda_imm(0x80).sta_abs(0x6000);
        for (i, b) in [0xDE, 0xB0, 0x61].into_iter().enumerate() {
            asm.lda_imm(b).sta_abs(0x6001 + i as u16);
        }
        Self { asm, count: 0 }
    }

    /// 比對累加器；不符就以目前的項目編號失敗。呼叫前累加器必須已載入要檢查的值。
    fn expect(&mut self, expected: u8) {
        self.count += 1;
        self.asm.cmp_imm(expected).beq_skip(8);
        // 失敗樁（8 bytes）：結果碼存進 $00，跳去共用的失敗處理。
        self.asm
            .lda_imm(self.count)
            .sta_abs(0x0000)
            .jmp(REPORT_FAIL);
    }

    fn finish(mut self, mapper: u8, prg_banks: usize, chr_banks_8k: usize, name: &str) -> Vec<u8> {
        self.asm.jmp(REPORT_PASS);

        let mut fail = Asm::new(REPORT_FAIL);
        store_text(&mut fail, format!("{name}: Failed\0").as_bytes());
        fail.lda_abs(0x0000).sta_abs(0x6000);
        let forever = fail.pc();
        fail.jmp(forever);

        let mut pass = Asm::new(REPORT_PASS);
        store_text(&mut pass, format!("{name}: Passed\0").as_bytes());
        pass.lda_imm(0x00).sta_abs(0x6000);
        let forever = pass.pc();
        pass.jmp(forever);

        build_mapper_rom(
            mapper,
            prg_banks,
            chr_banks_8k,
            &self.asm,
            &[(REPORT_FAIL, &fail.bytes), (REPORT_PASS, &pass.bytes)],
        )
    }
}

/// UxROM（mapper 2）測試 ROM：8 個 16KB PRG bank。依序切換每個 bank，讀 `$8100` 的
/// 識別碼（[`PRG_MARK`] + bank 編號）比對；每次切換後也確認 `$C000-$FFFF` 仍是最後
/// 一個 bank（`$C100` 的識別碼，以及正在執行的程式碼本身 `$E000` 的第一個 byte）。
/// 結果用 blargg 的 `$6000` 協定回報。
pub fn uxrom_test_rom() -> Vec<u8> {
    const BANKS: usize = 8;
    let mut c = Checker::new();
    for bank in (0..BANKS).chain((0..BANKS).rev()) {
        c.asm.lda_imm(bank as u8).sta_abs(0x8000);
        c.asm.lda_abs(0x8100);
        c.expect(PRG_MARK + bank as u8);
        c.asm.lda_abs(0xC100);
        c.expect(PRG_MARK + BANKS as u8 - 1);
        c.asm.lda_abs(0xE000);
        c.expect(0x78); // 程式自己的第一個 byte（SEI）
    }
    c.finish(2, BANKS, 0, "UxROM")
}

/// CNROM（mapper 3）測試 ROM：4 個 8KB CHR bank。依序切換每個 bank，經 `$2006/$2007`
/// 讀 pattern table `$0000` 與 `$1000` 的識別碼（[`CHR_MARK`] + 4KB 區塊編號）比對。
/// `$2007` 讀取有一個 byte 的緩衝，所以設好位址後先讀一次丟掉，第二次才是資料。
/// 同時確認 PRG 固定（`$8100`／`$C100` 不隨切換改變）。
pub fn cnrom_test_rom() -> Vec<u8> {
    const CHR_BANKS: usize = 4;
    let mut c = Checker::new();
    for bank in (0..CHR_BANKS).chain((0..CHR_BANKS).rev()) {
        c.asm.lda_imm(bank as u8).sta_abs(0x8000);
        for half in 0..2u16 {
            c.asm.set_ppu_addr(half * 0x1000);
            c.asm.lda_abs(0x2007); // 緩衝的舊值，丟掉
            c.asm.lda_abs(0x2007);
            c.expect(CHR_MARK + (bank as u16 * 2 + half) as u8);
        }
        c.asm.lda_abs(0x8100);
        c.expect(PRG_MARK);
        c.asm.lda_abs(0xC100);
        c.expect(PRG_MARK + 1);
    }
    c.finish(3, 2, CHR_BANKS, "CNROM")
}

/// 會不斷執行「碰到 `$2007` 的索引定址」的合成 ROM（給行為指紋用）：X = `$0F` 時
/// `STA $2000,X`（store：一律 dummy read）與 `LDA $20F8,X`（跨頁的讀取：dummy read
/// `$2007`）都會讓 PPU 位址多前進一次；`INC $2005` 是對 PPU 暫存器的 RMW（寫兩次）。
pub fn dummy_read_probe_rom() -> Vec<u8> {
    let mut a = Asm::new(0x8000);
    a.sei().cld().ldx_imm(0xFF).txs();
    a.set_ppu_addr(0x2000).ldx_imm(0x0F).lda_imm(0x55);
    let l = a.pc();
    a.sta_abs_x(0x2000)
        .lda_abs_x(0x20F8)
        .inc_abs(0x2005)
        .inc_abs(0x0000)
        .jmp(l);
    build_nrom(&a, 0x8000, None, &[], &test_chr(), false)
}

/// APU 探針 ROM 的參數。
#[derive(Clone, Copy, Debug)]
pub struct ApuProbe {
    /// 寫進 `$4017` 的值（frame counter 模式與 IRQ 抑制）。
    pub frame_counter: u8,
    /// IRQ 處理常式有沒有讀 `$4015` 確認（清 frame IRQ 旗標）。
    pub ack_frame_irq: bool,
    /// 是否啟動會產生 IRQ 的 DMC 取樣。
    pub dmc: bool,
}

impl ApuProbe {
    pub const DEFAULT: ApuProbe = ApuProbe {
        frame_counter: 0x00,
        ack_frame_irq: true,
        dmc: true,
    };
}

/// 會用到 frame IRQ、`$4015`、DMC（含 DMC IRQ 與抓取樣本造成的 CPU 暫停）的合成 ROM
/// （給行為指紋用）。
///
/// 主程式啟用各聲道、把 `$4017` 設成 `frame_counter`、`CLI` 之後進入無窮迴圈，迴圈裡不斷
/// `INC $00` 並讀 `$4015` 存到 `$01`。IRQ 處理常式（`$8100`）把 `$02` 加 1、把進入時的
/// `$4015` 存到 `$03`（會清掉 frame IRQ 旗標）、再寫 `$4015 = $1F`（清 DMC IRQ 並重啟
/// DMC 取樣）。所以最後的 RAM 內容與 CPU 的 cycle 數會反映：frame IRQ 的時間點與頻率、
/// `$4015` 的讀值與清旗標、DMC 的節拍、IRQ 遮蔽與 DMC 暫停。
pub fn apu_probe_rom(probe: ApuProbe) -> Vec<u8> {
    let mut a = Asm::new(0x8000);
    a.sei().cld().ldx_imm(0xFF).txs();
    a.lda_imm(probe.frame_counter).sta_abs(0x4017);
    // 先啟用聲道（停用的聲道不接受長度載入），再設各聲道。
    a.lda_imm(0x0F).sta_abs(0x4015);
    // pulse 1：duty 2、halt、固定音量 15；pulse 2：包絡線、長度只有 2；triangle；noise。
    a.lda_imm(0xBF).sta_abs(0x4000);
    a.lda_imm(0x8A).sta_abs(0x4001); // sweep：往下（pulse 1 用 1 的補數）
    a.lda_imm(0x40).sta_abs(0x4002);
    a.lda_imm(0x08).sta_abs(0x4003);
    a.lda_imm(0x15).sta_abs(0x4004);
    a.lda_imm(0x89).sta_abs(0x4005); // sweep：往下（pulse 2 用 2 的補數）
    a.lda_imm(0x80).sta_abs(0x4006);
    a.lda_imm(0x18).sta_abs(0x4007);
    a.lda_imm(0xFF).sta_abs(0x4008);
    a.lda_imm(0x30).sta_abs(0x400A);
    a.lda_imm(0x08).sta_abs(0x400B);
    a.lda_imm(0x0F).sta_abs(0x400C);
    a.lda_imm(0x84).sta_abs(0x400E); // 短模式
    a.lda_imm(0x08).sta_abs(0x400F);
    if probe.dmc {
        a.lda_imm(0x8F).sta_abs(0x4010); // IRQ 致能、最快的速率
        a.lda_imm(0x00).sta_abs(0x4012); // $C000
        a.lda_imm(0x02).sta_abs(0x4013); // 33 bytes
        a.lda_imm(0x1F);
    } else {
        a.lda_imm(0x0F);
    }
    a.sta_abs(0x4015);
    a.cli();
    let main_loop = a.pc();
    a.inc_abs(0x0000)
        .lda_abs(0x4015)
        .sta_abs(0x0001)
        .jmp(main_loop);

    let mut irq = Asm::new(0x8100);
    irq.pha().inc_abs(0x0002);
    if probe.ack_frame_irq {
        irq.lda_abs(0x4015).sta_abs(0x0003);
    }
    if probe.dmc {
        irq.lda_imm(0x1F).sta_abs(0x4015);
    }
    irq.pla().rti();

    // DMC 取樣資料（$C000 起）。
    let sample: Vec<u8> = (0..64u8).map(|i| i.wrapping_mul(37) ^ 0x5A).collect();
    build_nrom_irq(
        &a,
        0x8000,
        None,
        Some(0x8100),
        &[(0x8100, &irq.bytes), (0xC000, &sample)],
        &test_chr(),
        false,
    )
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
