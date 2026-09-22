//! 256 個 entry 的 opcode 表：每個 opcode byte 對應定址模式、指令長度、
//! 基礎 cycle 數、是否有跨頁 penalty、以及是否為官方（documented）指令。
//!
//! 參考資料：這張表的內容（cycle 數、定址模式、官方/非官方分類）是對照
//! 公開的 6502/2A03 opcode 參考表（例如 NESdev wiki 的 "CPU unofficial
//! opcodes" 頁面、oxyron.de 的 6502 opcode matrix）手動查表整理，程式碼本身
//! （表格結構、`OpcodeInfo`/`AddrMode` 型別設計）為本專案自行撰寫，沒有複製
//! 任何模擬器或教學的原始碼。

/// 6502 的 13 種定址模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddrMode {
    Implied,
    Accumulator,
    Immediate,
    ZeroPage,
    ZeroPageX,
    ZeroPageY,
    Absolute,
    AbsoluteX,
    AbsoluteY,
    Indirect,
    IndirectX,
    IndirectY,
    Relative,
}

/// 指令的語意分類。`execute()` 用這個 enum 做 match，取代原本的字串比對——
/// 好處除了少一次字串比較，編譯器還能檢查 `execute()` 的 match 是否窮舉了
/// 每一種變體（少寫一種會編譯錯誤，不會等到執行期才用 `unreachable!()`
/// panic）。同一個 mnemonic 邏輯上對應好幾個 opcode byte（例如 `Lda` 對應
/// 8 種定址模式）時共用同一個 variant；`Sbc` 同時涵蓋官方 `$E9` 與非官方
/// `*SBC`（`$EB`），因為兩者行為完全相同。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mnemonic {
    // 讀取 / 寫入
    Lda,
    Ldx,
    Ldy,
    Sta,
    Stx,
    Sty,
    // 傳輸
    Tax,
    Tay,
    Txa,
    Tya,
    Tsx,
    Txs,
    // 堆疊
    Pha,
    Php,
    Pla,
    Plp,
    // 算術
    Adc,
    Sbc,
    // 遞增 / 遞減
    Inc,
    Dec,
    Inx,
    Iny,
    Dex,
    Dey,
    // 位移
    Asl,
    Lsr,
    Rol,
    Ror,
    // 邏輯
    And,
    Ora,
    Eor,
    // 比較
    Cmp,
    Cpx,
    Cpy,
    // 分支
    Bcc,
    Bcs,
    Beq,
    Bne,
    Bmi,
    Bpl,
    Bvc,
    Bvs,
    // 跳躍 / 子程式
    Jmp,
    Jsr,
    Rts,
    Rti,
    // 旗標
    Clc,
    Sec,
    Cli,
    Sei,
    Clv,
    Cld,
    Sed,
    // 其他
    Bit,
    Nop,
    Brk,
    // 非官方
    Lax,
    Sax,
    Dcp,
    Isb,
    Slo,
    Rla,
    Sre,
    Rra,
    Anc,
    Alr,
    Arr,
    Axs,
    Jam,
}

impl Mnemonic {
    /// 給反組譯 / trace 顯示用的助記符文字。
    pub const fn as_str(self) -> &'static str {
        match self {
            Mnemonic::Lda => "LDA",
            Mnemonic::Ldx => "LDX",
            Mnemonic::Ldy => "LDY",
            Mnemonic::Sta => "STA",
            Mnemonic::Stx => "STX",
            Mnemonic::Sty => "STY",
            Mnemonic::Tax => "TAX",
            Mnemonic::Tay => "TAY",
            Mnemonic::Txa => "TXA",
            Mnemonic::Tya => "TYA",
            Mnemonic::Tsx => "TSX",
            Mnemonic::Txs => "TXS",
            Mnemonic::Pha => "PHA",
            Mnemonic::Php => "PHP",
            Mnemonic::Pla => "PLA",
            Mnemonic::Plp => "PLP",
            Mnemonic::Adc => "ADC",
            Mnemonic::Sbc => "SBC",
            Mnemonic::Inc => "INC",
            Mnemonic::Dec => "DEC",
            Mnemonic::Inx => "INX",
            Mnemonic::Iny => "INY",
            Mnemonic::Dex => "DEX",
            Mnemonic::Dey => "DEY",
            Mnemonic::Asl => "ASL",
            Mnemonic::Lsr => "LSR",
            Mnemonic::Rol => "ROL",
            Mnemonic::Ror => "ROR",
            Mnemonic::And => "AND",
            Mnemonic::Ora => "ORA",
            Mnemonic::Eor => "EOR",
            Mnemonic::Cmp => "CMP",
            Mnemonic::Cpx => "CPX",
            Mnemonic::Cpy => "CPY",
            Mnemonic::Bcc => "BCC",
            Mnemonic::Bcs => "BCS",
            Mnemonic::Beq => "BEQ",
            Mnemonic::Bne => "BNE",
            Mnemonic::Bmi => "BMI",
            Mnemonic::Bpl => "BPL",
            Mnemonic::Bvc => "BVC",
            Mnemonic::Bvs => "BVS",
            Mnemonic::Jmp => "JMP",
            Mnemonic::Jsr => "JSR",
            Mnemonic::Rts => "RTS",
            Mnemonic::Rti => "RTI",
            Mnemonic::Clc => "CLC",
            Mnemonic::Sec => "SEC",
            Mnemonic::Cli => "CLI",
            Mnemonic::Sei => "SEI",
            Mnemonic::Clv => "CLV",
            Mnemonic::Cld => "CLD",
            Mnemonic::Sed => "SED",
            Mnemonic::Bit => "BIT",
            Mnemonic::Nop => "NOP",
            Mnemonic::Brk => "BRK",
            Mnemonic::Lax => "LAX",
            Mnemonic::Sax => "SAX",
            Mnemonic::Dcp => "DCP",
            Mnemonic::Isb => "ISB",
            Mnemonic::Slo => "SLO",
            Mnemonic::Rla => "RLA",
            Mnemonic::Sre => "SRE",
            Mnemonic::Rra => "RRA",
            Mnemonic::Anc => "ANC",
            Mnemonic::Alr => "ALR",
            Mnemonic::Arr => "ARR",
            Mnemonic::Axs => "AXS",
            Mnemonic::Jam => "JAM",
        }
    }
}

impl core::fmt::Display for Mnemonic {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 一個 opcode byte 對應的靜態中繼資料。
#[derive(Debug, Clone, Copy)]
pub struct OpcodeInfo {
    pub mnemonic: Mnemonic,
    pub mode: AddrMode,
    pub bytes: u8,
    pub cycles: u8,
    /// 跨頁時是否要 +1 cycle。只對「讀取類」定址模式（AbsoluteX/AbsoluteY/
    /// IndirectY）的指令為 true；store 與 RMW 指令固定使用 `cycles`
    /// （已經是最壞情況的 cycle 數），一律是 false。
    pub page_cross_penalty: bool,
    /// 是否為官方（documented）指令。
    pub official: bool,
}

const fn op(
    mnemonic: Mnemonic,
    mode: AddrMode,
    bytes: u8,
    cycles: u8,
    page_cross_penalty: bool,
    official: bool,
) -> OpcodeInfo {
    OpcodeInfo {
        mnemonic,
        mode,
        bytes,
        cycles,
        page_cross_penalty,
        official,
    }
}

use AddrMode::*;

/// 給非法/未定義 opcode（極少數真的完全沒有一致行為的 slot）用的佔位項目：
/// 當成 2-cycle NOP(Implied) 處理。目前 256 個 opcode 全部都有指派到某個
/// 已知行為（含 JAM），這個常數只是保底，理論上不會被用到。
const UNDEFINED: OpcodeInfo = op(Mnemonic::Nop, Implied, 1, 2, false, false);

pub const OPCODES: [OpcodeInfo; 256] = {
    let mut table = [UNDEFINED; 256];

    // ---- 官方指令 --------------------------------------------------
    // LDA
    table[0xA9] = op(Mnemonic::Lda, Immediate, 2, 2, false, true);
    table[0xA5] = op(Mnemonic::Lda, ZeroPage, 2, 3, false, true);
    table[0xB5] = op(Mnemonic::Lda, ZeroPageX, 2, 4, false, true);
    table[0xAD] = op(Mnemonic::Lda, Absolute, 3, 4, false, true);
    table[0xBD] = op(Mnemonic::Lda, AbsoluteX, 3, 4, true, true);
    table[0xB9] = op(Mnemonic::Lda, AbsoluteY, 3, 4, true, true);
    table[0xA1] = op(Mnemonic::Lda, IndirectX, 2, 6, false, true);
    table[0xB1] = op(Mnemonic::Lda, IndirectY, 2, 5, true, true);
    // LDX
    table[0xA2] = op(Mnemonic::Ldx, Immediate, 2, 2, false, true);
    table[0xA6] = op(Mnemonic::Ldx, ZeroPage, 2, 3, false, true);
    table[0xB6] = op(Mnemonic::Ldx, ZeroPageY, 2, 4, false, true);
    table[0xAE] = op(Mnemonic::Ldx, Absolute, 3, 4, false, true);
    table[0xBE] = op(Mnemonic::Ldx, AbsoluteY, 3, 4, true, true);
    // LDY
    table[0xA0] = op(Mnemonic::Ldy, Immediate, 2, 2, false, true);
    table[0xA4] = op(Mnemonic::Ldy, ZeroPage, 2, 3, false, true);
    table[0xB4] = op(Mnemonic::Ldy, ZeroPageX, 2, 4, false, true);
    table[0xAC] = op(Mnemonic::Ldy, Absolute, 3, 4, false, true);
    table[0xBC] = op(Mnemonic::Ldy, AbsoluteX, 3, 4, true, true);
    // STA
    table[0x85] = op(Mnemonic::Sta, ZeroPage, 2, 3, false, true);
    table[0x95] = op(Mnemonic::Sta, ZeroPageX, 2, 4, false, true);
    table[0x8D] = op(Mnemonic::Sta, Absolute, 3, 4, false, true);
    table[0x9D] = op(Mnemonic::Sta, AbsoluteX, 3, 5, false, true);
    table[0x99] = op(Mnemonic::Sta, AbsoluteY, 3, 5, false, true);
    table[0x81] = op(Mnemonic::Sta, IndirectX, 2, 6, false, true);
    table[0x91] = op(Mnemonic::Sta, IndirectY, 2, 6, false, true);
    // STX / STY
    table[0x86] = op(Mnemonic::Stx, ZeroPage, 2, 3, false, true);
    table[0x96] = op(Mnemonic::Stx, ZeroPageY, 2, 4, false, true);
    table[0x8E] = op(Mnemonic::Stx, Absolute, 3, 4, false, true);
    table[0x84] = op(Mnemonic::Sty, ZeroPage, 2, 3, false, true);
    table[0x94] = op(Mnemonic::Sty, ZeroPageX, 2, 4, false, true);
    table[0x8C] = op(Mnemonic::Sty, Absolute, 3, 4, false, true);

    // 傳輸
    table[0xAA] = op(Mnemonic::Tax, Implied, 1, 2, false, true);
    table[0xA8] = op(Mnemonic::Tay, Implied, 1, 2, false, true);
    table[0x8A] = op(Mnemonic::Txa, Implied, 1, 2, false, true);
    table[0x98] = op(Mnemonic::Tya, Implied, 1, 2, false, true);
    table[0xBA] = op(Mnemonic::Tsx, Implied, 1, 2, false, true);
    table[0x9A] = op(Mnemonic::Txs, Implied, 1, 2, false, true);

    // 堆疊
    table[0x48] = op(Mnemonic::Pha, Implied, 1, 3, false, true);
    table[0x08] = op(Mnemonic::Php, Implied, 1, 3, false, true);
    table[0x68] = op(Mnemonic::Pla, Implied, 1, 4, false, true);
    table[0x28] = op(Mnemonic::Plp, Implied, 1, 4, false, true);

    // ADC / SBC
    table[0x69] = op(Mnemonic::Adc, Immediate, 2, 2, false, true);
    table[0x65] = op(Mnemonic::Adc, ZeroPage, 2, 3, false, true);
    table[0x75] = op(Mnemonic::Adc, ZeroPageX, 2, 4, false, true);
    table[0x6D] = op(Mnemonic::Adc, Absolute, 3, 4, false, true);
    table[0x7D] = op(Mnemonic::Adc, AbsoluteX, 3, 4, true, true);
    table[0x79] = op(Mnemonic::Adc, AbsoluteY, 3, 4, true, true);
    table[0x61] = op(Mnemonic::Adc, IndirectX, 2, 6, false, true);
    table[0x71] = op(Mnemonic::Adc, IndirectY, 2, 5, true, true);
    table[0xE9] = op(Mnemonic::Sbc, Immediate, 2, 2, false, true);
    table[0xE5] = op(Mnemonic::Sbc, ZeroPage, 2, 3, false, true);
    table[0xF5] = op(Mnemonic::Sbc, ZeroPageX, 2, 4, false, true);
    table[0xED] = op(Mnemonic::Sbc, Absolute, 3, 4, false, true);
    table[0xFD] = op(Mnemonic::Sbc, AbsoluteX, 3, 4, true, true);
    table[0xF9] = op(Mnemonic::Sbc, AbsoluteY, 3, 4, true, true);
    table[0xE1] = op(Mnemonic::Sbc, IndirectX, 2, 6, false, true);
    table[0xF1] = op(Mnemonic::Sbc, IndirectY, 2, 5, true, true);

    // INC / DEC（記憶體，RMW，固定 cycle）
    table[0xE6] = op(Mnemonic::Inc, ZeroPage, 2, 5, false, true);
    table[0xF6] = op(Mnemonic::Inc, ZeroPageX, 2, 6, false, true);
    table[0xEE] = op(Mnemonic::Inc, Absolute, 3, 6, false, true);
    table[0xFE] = op(Mnemonic::Inc, AbsoluteX, 3, 7, false, true);
    table[0xC6] = op(Mnemonic::Dec, ZeroPage, 2, 5, false, true);
    table[0xD6] = op(Mnemonic::Dec, ZeroPageX, 2, 6, false, true);
    table[0xCE] = op(Mnemonic::Dec, Absolute, 3, 6, false, true);
    table[0xDE] = op(Mnemonic::Dec, AbsoluteX, 3, 7, false, true);
    table[0xE8] = op(Mnemonic::Inx, Implied, 1, 2, false, true);
    table[0xC8] = op(Mnemonic::Iny, Implied, 1, 2, false, true);
    table[0xCA] = op(Mnemonic::Dex, Implied, 1, 2, false, true);
    table[0x88] = op(Mnemonic::Dey, Implied, 1, 2, false, true);

    // 位移（Accumulator 或記憶體，RMW，固定 cycle）
    table[0x0A] = op(Mnemonic::Asl, Accumulator, 1, 2, false, true);
    table[0x06] = op(Mnemonic::Asl, ZeroPage, 2, 5, false, true);
    table[0x16] = op(Mnemonic::Asl, ZeroPageX, 2, 6, false, true);
    table[0x0E] = op(Mnemonic::Asl, Absolute, 3, 6, false, true);
    table[0x1E] = op(Mnemonic::Asl, AbsoluteX, 3, 7, false, true);
    table[0x4A] = op(Mnemonic::Lsr, Accumulator, 1, 2, false, true);
    table[0x46] = op(Mnemonic::Lsr, ZeroPage, 2, 5, false, true);
    table[0x56] = op(Mnemonic::Lsr, ZeroPageX, 2, 6, false, true);
    table[0x4E] = op(Mnemonic::Lsr, Absolute, 3, 6, false, true);
    table[0x5E] = op(Mnemonic::Lsr, AbsoluteX, 3, 7, false, true);
    table[0x2A] = op(Mnemonic::Rol, Accumulator, 1, 2, false, true);
    table[0x26] = op(Mnemonic::Rol, ZeroPage, 2, 5, false, true);
    table[0x36] = op(Mnemonic::Rol, ZeroPageX, 2, 6, false, true);
    table[0x2E] = op(Mnemonic::Rol, Absolute, 3, 6, false, true);
    table[0x3E] = op(Mnemonic::Rol, AbsoluteX, 3, 7, false, true);
    table[0x6A] = op(Mnemonic::Ror, Accumulator, 1, 2, false, true);
    table[0x66] = op(Mnemonic::Ror, ZeroPage, 2, 5, false, true);
    table[0x76] = op(Mnemonic::Ror, ZeroPageX, 2, 6, false, true);
    table[0x6E] = op(Mnemonic::Ror, Absolute, 3, 6, false, true);
    table[0x7E] = op(Mnemonic::Ror, AbsoluteX, 3, 7, false, true);

    // 邏輯
    table[0x29] = op(Mnemonic::And, Immediate, 2, 2, false, true);
    table[0x25] = op(Mnemonic::And, ZeroPage, 2, 3, false, true);
    table[0x35] = op(Mnemonic::And, ZeroPageX, 2, 4, false, true);
    table[0x2D] = op(Mnemonic::And, Absolute, 3, 4, false, true);
    table[0x3D] = op(Mnemonic::And, AbsoluteX, 3, 4, true, true);
    table[0x39] = op(Mnemonic::And, AbsoluteY, 3, 4, true, true);
    table[0x21] = op(Mnemonic::And, IndirectX, 2, 6, false, true);
    table[0x31] = op(Mnemonic::And, IndirectY, 2, 5, true, true);
    table[0x09] = op(Mnemonic::Ora, Immediate, 2, 2, false, true);
    table[0x05] = op(Mnemonic::Ora, ZeroPage, 2, 3, false, true);
    table[0x15] = op(Mnemonic::Ora, ZeroPageX, 2, 4, false, true);
    table[0x0D] = op(Mnemonic::Ora, Absolute, 3, 4, false, true);
    table[0x1D] = op(Mnemonic::Ora, AbsoluteX, 3, 4, true, true);
    table[0x19] = op(Mnemonic::Ora, AbsoluteY, 3, 4, true, true);
    table[0x01] = op(Mnemonic::Ora, IndirectX, 2, 6, false, true);
    table[0x11] = op(Mnemonic::Ora, IndirectY, 2, 5, true, true);
    table[0x49] = op(Mnemonic::Eor, Immediate, 2, 2, false, true);
    table[0x45] = op(Mnemonic::Eor, ZeroPage, 2, 3, false, true);
    table[0x55] = op(Mnemonic::Eor, ZeroPageX, 2, 4, false, true);
    table[0x4D] = op(Mnemonic::Eor, Absolute, 3, 4, false, true);
    table[0x5D] = op(Mnemonic::Eor, AbsoluteX, 3, 4, true, true);
    table[0x59] = op(Mnemonic::Eor, AbsoluteY, 3, 4, true, true);
    table[0x41] = op(Mnemonic::Eor, IndirectX, 2, 6, false, true);
    table[0x51] = op(Mnemonic::Eor, IndirectY, 2, 5, true, true);

    // 比較
    table[0xC9] = op(Mnemonic::Cmp, Immediate, 2, 2, false, true);
    table[0xC5] = op(Mnemonic::Cmp, ZeroPage, 2, 3, false, true);
    table[0xD5] = op(Mnemonic::Cmp, ZeroPageX, 2, 4, false, true);
    table[0xCD] = op(Mnemonic::Cmp, Absolute, 3, 4, false, true);
    table[0xDD] = op(Mnemonic::Cmp, AbsoluteX, 3, 4, true, true);
    table[0xD9] = op(Mnemonic::Cmp, AbsoluteY, 3, 4, true, true);
    table[0xC1] = op(Mnemonic::Cmp, IndirectX, 2, 6, false, true);
    table[0xD1] = op(Mnemonic::Cmp, IndirectY, 2, 5, true, true);
    table[0xE0] = op(Mnemonic::Cpx, Immediate, 2, 2, false, true);
    table[0xE4] = op(Mnemonic::Cpx, ZeroPage, 2, 3, false, true);
    table[0xEC] = op(Mnemonic::Cpx, Absolute, 3, 4, false, true);
    table[0xC0] = op(Mnemonic::Cpy, Immediate, 2, 2, false, true);
    table[0xC4] = op(Mnemonic::Cpy, ZeroPage, 2, 3, false, true);
    table[0xCC] = op(Mnemonic::Cpy, Absolute, 3, 4, false, true);

    // 分支（cycles 是「沒 taken」的基礎值；taken/跨頁的加成在 execute 裡處理）
    table[0x90] = op(Mnemonic::Bcc, Relative, 2, 2, false, true);
    table[0xB0] = op(Mnemonic::Bcs, Relative, 2, 2, false, true);
    table[0xF0] = op(Mnemonic::Beq, Relative, 2, 2, false, true);
    table[0xD0] = op(Mnemonic::Bne, Relative, 2, 2, false, true);
    table[0x30] = op(Mnemonic::Bmi, Relative, 2, 2, false, true);
    table[0x10] = op(Mnemonic::Bpl, Relative, 2, 2, false, true);
    table[0x50] = op(Mnemonic::Bvc, Relative, 2, 2, false, true);
    table[0x70] = op(Mnemonic::Bvs, Relative, 2, 2, false, true);

    // 跳躍 / 子程式
    table[0x4C] = op(Mnemonic::Jmp, Absolute, 3, 3, false, true);
    table[0x6C] = op(Mnemonic::Jmp, Indirect, 3, 5, false, true);
    table[0x20] = op(Mnemonic::Jsr, Absolute, 3, 6, false, true);
    table[0x60] = op(Mnemonic::Rts, Implied, 1, 6, false, true);
    table[0x40] = op(Mnemonic::Rti, Implied, 1, 6, false, true);

    // 旗標
    table[0x18] = op(Mnemonic::Clc, Implied, 1, 2, false, true);
    table[0x38] = op(Mnemonic::Sec, Implied, 1, 2, false, true);
    table[0x58] = op(Mnemonic::Cli, Implied, 1, 2, false, true);
    table[0x78] = op(Mnemonic::Sei, Implied, 1, 2, false, true);
    table[0xB8] = op(Mnemonic::Clv, Implied, 1, 2, false, true);
    table[0xD8] = op(Mnemonic::Cld, Implied, 1, 2, false, true);
    table[0xF8] = op(Mnemonic::Sed, Implied, 1, 2, false, true);

    // 其他
    table[0x24] = op(Mnemonic::Bit, ZeroPage, 2, 3, false, true);
    table[0x2C] = op(Mnemonic::Bit, Absolute, 3, 4, false, true);
    table[0xEA] = op(Mnemonic::Nop, Implied, 1, 2, false, true);
    table[0x00] = op(Mnemonic::Brk, Implied, 1, 7, false, true);

    // ---- 非官方指令 --------------------------------------------------
    // LAX：LDA+LDX 合體
    table[0xA7] = op(Mnemonic::Lax, ZeroPage, 2, 3, false, false);
    table[0xB7] = op(Mnemonic::Lax, ZeroPageY, 2, 4, false, false);
    table[0xAF] = op(Mnemonic::Lax, Absolute, 3, 4, false, false);
    table[0xBF] = op(Mnemonic::Lax, AbsoluteY, 3, 4, true, false);
    table[0xA3] = op(Mnemonic::Lax, IndirectX, 2, 6, false, false);
    table[0xB3] = op(Mnemonic::Lax, IndirectY, 2, 5, true, false);
    // SAX：STA (A&X) 合體
    table[0x87] = op(Mnemonic::Sax, ZeroPage, 2, 3, false, false);
    table[0x97] = op(Mnemonic::Sax, ZeroPageY, 2, 4, false, false);
    table[0x8F] = op(Mnemonic::Sax, Absolute, 3, 4, false, false);
    table[0x83] = op(Mnemonic::Sax, IndirectX, 2, 6, false, false);
    // DCP：DEC + CMP
    table[0xC7] = op(Mnemonic::Dcp, ZeroPage, 2, 5, false, false);
    table[0xD7] = op(Mnemonic::Dcp, ZeroPageX, 2, 6, false, false);
    table[0xCF] = op(Mnemonic::Dcp, Absolute, 3, 6, false, false);
    table[0xDF] = op(Mnemonic::Dcp, AbsoluteX, 3, 7, false, false);
    table[0xDB] = op(Mnemonic::Dcp, AbsoluteY, 3, 7, false, false);
    table[0xC3] = op(Mnemonic::Dcp, IndirectX, 2, 8, false, false);
    table[0xD3] = op(Mnemonic::Dcp, IndirectY, 2, 8, false, false);
    // ISB/ISC：INC + SBC
    table[0xE7] = op(Mnemonic::Isb, ZeroPage, 2, 5, false, false);
    table[0xF7] = op(Mnemonic::Isb, ZeroPageX, 2, 6, false, false);
    table[0xEF] = op(Mnemonic::Isb, Absolute, 3, 6, false, false);
    table[0xFF] = op(Mnemonic::Isb, AbsoluteX, 3, 7, false, false);
    table[0xFB] = op(Mnemonic::Isb, AbsoluteY, 3, 7, false, false);
    table[0xE3] = op(Mnemonic::Isb, IndirectX, 2, 8, false, false);
    table[0xF3] = op(Mnemonic::Isb, IndirectY, 2, 8, false, false);
    // SLO：ASL + ORA
    table[0x07] = op(Mnemonic::Slo, ZeroPage, 2, 5, false, false);
    table[0x17] = op(Mnemonic::Slo, ZeroPageX, 2, 6, false, false);
    table[0x0F] = op(Mnemonic::Slo, Absolute, 3, 6, false, false);
    table[0x1F] = op(Mnemonic::Slo, AbsoluteX, 3, 7, false, false);
    table[0x1B] = op(Mnemonic::Slo, AbsoluteY, 3, 7, false, false);
    table[0x03] = op(Mnemonic::Slo, IndirectX, 2, 8, false, false);
    table[0x13] = op(Mnemonic::Slo, IndirectY, 2, 8, false, false);
    // RLA：ROL + AND
    table[0x27] = op(Mnemonic::Rla, ZeroPage, 2, 5, false, false);
    table[0x37] = op(Mnemonic::Rla, ZeroPageX, 2, 6, false, false);
    table[0x2F] = op(Mnemonic::Rla, Absolute, 3, 6, false, false);
    table[0x3F] = op(Mnemonic::Rla, AbsoluteX, 3, 7, false, false);
    table[0x3B] = op(Mnemonic::Rla, AbsoluteY, 3, 7, false, false);
    table[0x23] = op(Mnemonic::Rla, IndirectX, 2, 8, false, false);
    table[0x33] = op(Mnemonic::Rla, IndirectY, 2, 8, false, false);
    // SRE：LSR + EOR
    table[0x47] = op(Mnemonic::Sre, ZeroPage, 2, 5, false, false);
    table[0x57] = op(Mnemonic::Sre, ZeroPageX, 2, 6, false, false);
    table[0x4F] = op(Mnemonic::Sre, Absolute, 3, 6, false, false);
    table[0x5F] = op(Mnemonic::Sre, AbsoluteX, 3, 7, false, false);
    table[0x5B] = op(Mnemonic::Sre, AbsoluteY, 3, 7, false, false);
    table[0x43] = op(Mnemonic::Sre, IndirectX, 2, 8, false, false);
    table[0x53] = op(Mnemonic::Sre, IndirectY, 2, 8, false, false);
    // RRA：ROR + ADC
    table[0x67] = op(Mnemonic::Rra, ZeroPage, 2, 5, false, false);
    table[0x77] = op(Mnemonic::Rra, ZeroPageX, 2, 6, false, false);
    table[0x6F] = op(Mnemonic::Rra, Absolute, 3, 6, false, false);
    table[0x7F] = op(Mnemonic::Rra, AbsoluteX, 3, 7, false, false);
    table[0x7B] = op(Mnemonic::Rra, AbsoluteY, 3, 7, false, false);
    table[0x63] = op(Mnemonic::Rra, IndirectX, 2, 8, false, false);
    table[0x73] = op(Mnemonic::Rra, IndirectY, 2, 8, false, false);
    // 立即值運算的非官方指令
    table[0x0B] = op(Mnemonic::Anc, Immediate, 2, 2, false, false);
    table[0x2B] = op(Mnemonic::Anc, Immediate, 2, 2, false, false);
    table[0x4B] = op(Mnemonic::Alr, Immediate, 2, 2, false, false);
    table[0x6B] = op(Mnemonic::Arr, Immediate, 2, 2, false, false);
    table[0xCB] = op(Mnemonic::Axs, Immediate, 2, 2, false, false);
    table[0xEB] = op(Mnemonic::Sbc, Immediate, 2, 2, false, false); // *SBC，跟官方 SBC 行為相同

    // 非官方 NOP（各種定址模式，含跨頁 penalty 的 read）
    table[0x1A] = op(Mnemonic::Nop, Implied, 1, 2, false, false);
    table[0x3A] = op(Mnemonic::Nop, Implied, 1, 2, false, false);
    table[0x5A] = op(Mnemonic::Nop, Implied, 1, 2, false, false);
    table[0x7A] = op(Mnemonic::Nop, Implied, 1, 2, false, false);
    table[0xDA] = op(Mnemonic::Nop, Implied, 1, 2, false, false);
    table[0xFA] = op(Mnemonic::Nop, Implied, 1, 2, false, false);
    table[0x80] = op(Mnemonic::Nop, Immediate, 2, 2, false, false);
    table[0x82] = op(Mnemonic::Nop, Immediate, 2, 2, false, false);
    table[0x89] = op(Mnemonic::Nop, Immediate, 2, 2, false, false);
    table[0xC2] = op(Mnemonic::Nop, Immediate, 2, 2, false, false);
    table[0xE2] = op(Mnemonic::Nop, Immediate, 2, 2, false, false);
    table[0x04] = op(Mnemonic::Nop, ZeroPage, 2, 3, false, false);
    table[0x44] = op(Mnemonic::Nop, ZeroPage, 2, 3, false, false);
    table[0x64] = op(Mnemonic::Nop, ZeroPage, 2, 3, false, false);
    table[0x14] = op(Mnemonic::Nop, ZeroPageX, 2, 4, false, false);
    table[0x34] = op(Mnemonic::Nop, ZeroPageX, 2, 4, false, false);
    table[0x54] = op(Mnemonic::Nop, ZeroPageX, 2, 4, false, false);
    table[0x74] = op(Mnemonic::Nop, ZeroPageX, 2, 4, false, false);
    table[0xD4] = op(Mnemonic::Nop, ZeroPageX, 2, 4, false, false);
    table[0xF4] = op(Mnemonic::Nop, ZeroPageX, 2, 4, false, false);
    table[0x0C] = op(Mnemonic::Nop, Absolute, 3, 4, false, false);
    table[0x1C] = op(Mnemonic::Nop, AbsoluteX, 3, 4, true, false);
    table[0x3C] = op(Mnemonic::Nop, AbsoluteX, 3, 4, true, false);
    table[0x5C] = op(Mnemonic::Nop, AbsoluteX, 3, 4, true, false);
    table[0x7C] = op(Mnemonic::Nop, AbsoluteX, 3, 4, true, false);
    table[0xDC] = op(Mnemonic::Nop, AbsoluteX, 3, 4, true, false);
    table[0xFC] = op(Mnemonic::Nop, AbsoluteX, 3, 4, true, false);

    // JAM / KIL：讓 CPU 卡死。cycle 數表上寫 2，實際上 step() 會偵測
    // jammed 狀態後直接短路，不會再照這個表推進。
    table[0x02] = op(Mnemonic::Jam, Implied, 1, 2, false, false);
    table[0x12] = op(Mnemonic::Jam, Implied, 1, 2, false, false);
    table[0x22] = op(Mnemonic::Jam, Implied, 1, 2, false, false);
    table[0x32] = op(Mnemonic::Jam, Implied, 1, 2, false, false);
    table[0x42] = op(Mnemonic::Jam, Implied, 1, 2, false, false);
    table[0x52] = op(Mnemonic::Jam, Implied, 1, 2, false, false);
    table[0x62] = op(Mnemonic::Jam, Implied, 1, 2, false, false);
    table[0x72] = op(Mnemonic::Jam, Implied, 1, 2, false, false);
    table[0x92] = op(Mnemonic::Jam, Implied, 1, 2, false, false);
    table[0xB2] = op(Mnemonic::Jam, Implied, 1, 2, false, false);
    table[0xD2] = op(Mnemonic::Jam, Implied, 1, 2, false, false);
    table[0xF2] = op(Mnemonic::Jam, Implied, 1, 2, false, false);

    table
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_opcode_has_at_least_one_byte() {
        for entry in OPCODES.iter() {
            assert!(entry.bytes >= 1);
        }
    }

    #[test]
    fn known_official_opcodes_are_marked_official() {
        assert!(OPCODES[0xA9].official); // LDA #imm
        assert!(OPCODES[0x4C].official); // JMP abs
        assert!(!OPCODES[0xA7].official); // LAX zp
        assert!(!OPCODES[0x02].official); // JAM
    }
}
