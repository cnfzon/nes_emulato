//! 6502（2A03）CPU 模擬。
//!
//! 參考來源：整體「先做定址模式表、再做 opcode 表、再做 step()」的設計順序
//! 參考了 bugzmanov《Writing NES Emulator in Rust》第 3 章的敘事結構
//! （<https://bugzmanov.github.io/nes_ebook/chapter_3.html>），但暫存器/旗標
//! 型別、opcode 表格式、定址模式解析、每條指令的執行邏輯與 trace 格式，都是
//! 依 nestest.log 與公開的 6502 opcode 參考資料自行重新設計、實作，沒有複製
//! 教材或其他模擬器的程式碼。非官方 opcode 的行為對照 NESdev wiki 的
//! "CPU unofficial opcodes" 頁面整理而成。
//!
//! `Cpu` 擁有 `Bus`（`Nes` 擁有 `Cpu`），所有記憶體存取都透過
//! `self.bus.read`/`self.bus.write`/`self.bus.peek` 進行。
//!
//! # 精度層級：instruction-level
//!
//! 這顆模擬器是「指令級」（instruction-level）精度，不是「cycle 級」
//! （cycle-level）精度：`step()` 會一次把一條指令從頭到尾執行完，再回報
//! 這條指令總共花了幾個 cycle，而不是每個 cycle 都真的去驅動一次匯流排
//! 讀寫（真實硬體每個 cycle 都會做一次匯流排存取，即使該次存取的結果沒被
//! 用到）。
//!
//! 這個決定的影響記錄在 `docs/architecture.md`；簡單說：
//! - **優點**：實作大幅簡化，`OPCODES` 表直接查表就能決定總 cycle 數，不需要
//!   幫每條指令寫一個「這個 cycle 做什麼」的微碼狀態機。
//!   對 nestest 這種「檢查指令執行完之後的暫存器/cycle 總數」的測試完全足夠。
//! - **代價**：無法通過需要「逐 cycle 觀察匯流排活動」的測試（例如
//!   SingleStepTests 資料裡的 `cycles` 欄位、或是會在指令執行「途中」被
//!   PPU/mapper 用特定時機的側效應影響的邊緣案例）。這些之後如果要做
//!   sprite-0 hit 之類的精細時序才會真的需要 cycle-level 精度，Phase 1
//!   （純 CPU 正確性）不需要。

mod flags;
mod opcodes;

pub use flags::StatusFlags;
pub use opcodes::{AddrMode, Mnemonic, OPCODES, OpcodeInfo};

use crate::bus::Bus;

pub const STACK_BASE: u16 = 0x0100;
pub const RESET_VECTOR: u16 = 0xFFFC;
pub const NMI_VECTOR: u16 = 0xFFFA;
pub const IRQ_VECTOR: u16 = 0xFFFE;

/// JAM/KIL 指令之後，每次 `step()` 回報的固定 cycle 數。真實硬體會卡在一個
/// 內部微碼迴圈裡不斷重複讀取 `$FFFE`/`$FFFF`；本模擬器選擇更簡單、一樣不會
/// panic 的行為：直接不再推進 PC、不再讀取任何位址，只回報固定 cycle 數，讓
/// 呼叫端（`run_frame`）知道 CPU 卡住了（`DebugSnapshot::jammed`）。
const JAM_CYCLES: u8 = 2;

/// LXA（`$AB`）的 magic 常數；見 `Mnemonic::Lxa` 的實作註解。
const LXA_MAGIC: u8 = 0xFF;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Cpu {
    pub a: u8,
    pub x: u8,
    pub y: u8,
    pub sp: u8,
    pub pc: u16,
    pub status: StatusFlags,
    /// JAM/KIL 類 opcode 執行後會設成 true；之後的 `step()` 不再做任何事。
    pub jammed: bool,
    /// 上一條指令倒數第二個 cycle 結束時，IRQ 線的電位（硬體在這個時間點偵測 IRQ）。
    irq_sample: bool,
    /// 上一條指令用來「遮蔽」IRQ 的 I 旗標值。多數指令就是指令之後的 I；CLI／SEI／PLP 的新
    /// I 旗標在最後一個 cycle 才生效，趕不上偵測，所以是**指令之前**的 I（見 [`Cpu::step`]）。
    irq_masked: bool,
    bus: Bus,
}

impl Cpu {
    pub fn new(bus: Bus) -> Self {
        Self {
            a: 0,
            x: 0,
            y: 0,
            sp: 0xFD,
            pc: 0,
            status: StatusFlags::default(),
            jammed: false,
            irq_sample: false,
            irq_masked: true,
            bus,
        }
    }

    pub fn bus(&self) -> &Bus {
        &self.bus
    }

    pub fn bus_mut(&mut self) -> &mut Bus {
        &mut self.bus
    }

    /// 冷開機／重置：SP=$FD、P=$24、PC 從 `$FFFC` 讀取，耗時 7 cycles。
    /// 不清 A/X/Y（跟真實硬體一樣，reset 不保證清空累加器/索引暫存器；
    /// 開機時的 0 是 `Cpu::new` 給的初始值，不是 `reset` 的職責）。
    pub fn reset(&mut self) {
        self.sp = 0xFD;
        self.status = StatusFlags::default();
        self.jammed = false;
        self.irq_sample = false;
        self.irq_masked = true;
        self.pc = self.read_u16(RESET_VECTOR);
        self.bus.tick(7);
    }

    /// NMI：push PC、push P（B=0, U=1）、設 I 旗標、PC 從 `$FFFA` 讀取，
    /// 耗時 7 cycles。
    pub fn nmi(&mut self) {
        self.push_u16(self.pc);
        let flags = (self.status & !StatusFlags::BREAK) | StatusFlags::UNUSED;
        self.push_u8(flags.bits());
        self.status.insert(StatusFlags::INTERRUPT);
        self.pc = self.read_u16(NMI_VECTOR);
        self.bus.tick(7);
    }

    /// IRQ：跟 `nmi()` 完全對稱，只是向量換成 `$FFFE`。
    ///
    /// 「該不該服務」（IRQ 線電位與 I 旗標的遮蔽）由 [`Cpu::step`] 判斷；這個方法只負責
    /// 「服務中斷」這個動作本身，耗時固定 7 cycles。
    pub fn irq(&mut self) {
        self.push_u16(self.pc);
        let flags = (self.status & !StatusFlags::BREAK) | StatusFlags::UNUSED;
        self.push_u8(flags.bits());
        self.status.insert(StatusFlags::INTERRUPT);
        self.pc = self.read_u16(IRQ_VECTOR);
        self.bus.tick(7);
    }

    /// 執行一條指令，回傳實際花掉的 cycle 數，並呼叫 `bus.tick`。
    ///
    /// 指令之間會先檢查 PPU 的 NMI：有待處理的 NMI 就改為服務中斷（7 cycles，
    /// 回傳 7），這一步不執行任何指令。指令若寫了 `$4014`，OAM DMA 的暫停
    /// cycle 會在指令結束後追加（讓 PPU 追上），但**不**計入回傳值——回傳值
    /// 只是指令本身的 cycle 數。
    ///
    /// # 分段 catch-up（時序模型，定案；見 `docs/architecture.md` §13）
    ///
    /// 一條 N cycles 的指令，記憶體存取（讀寫 PPU 暫存器等）發生在最後一個 cycle
    /// 附近。所以：解出運算元之後、執行指令**之前**，先讓 PPU 追上 `N − 1` 個 cycle；
    /// 指令執行完再補最後 1 個 cycle（加上分支 taken／跨頁的額外 cycle）。這讓
    /// 指令內的 PPU 存取與 PPU 的時間差縮到約 1 個 CPU cycle（3 dot）。改變這個
    /// 切法會改變所有模擬結果，必須遞增 `CORE_BEHAVIOR_VERSION`。
    pub fn step(&mut self) -> u8 {
        if self.jammed {
            self.bus.tick(JAM_CYCLES);
            return JAM_CYCLES;
        }

        if self.bus.take_nmi() {
            self.nmi();
            self.irq_sample = false;
            self.irq_masked = true;
            return 7;
        }
        // IRQ 是 level-triggered：只要偵測到的線電位為高、且遮蔽用的 I 旗標為 0 就服務。
        // 服務之後 I = 1，處理常式的第一條指令一定會先執行。
        if self.irq_sample && !self.irq_masked {
            self.irq();
            self.irq_sample = false;
            self.irq_masked = true;
            return 7;
        }

        let opcode = self.bus.read(self.pc);
        self.pc = self.pc.wrapping_add(1);

        let info = &OPCODES[opcode as usize];
        let (addr, page_crossed, uncorrected) = self.resolve_operand(info.mode);

        let mut cycles = info.cycles;
        if info.page_cross_penalty && page_crossed {
            cycles += 1;
        }

        // 索引定址的 dummy read：硬體在位址高位元組修正之前，會先讀一次「低位元組已
        // 加上索引、高位元組尚未進位」的位址。讀取類指令只有跨頁時才有（沒跨頁時
        // 那次就是真正的讀取）；store 與 RMW 指令一律有。這次讀取有副作用
        // （例如對 `$2007` 會推進位址與讀取緩衝），所以要真的做。
        let dummy_read = uncorrected.filter(|_| page_crossed || !info.page_cross_penalty);

        // 指令前先追 `cycles − 1`（每條指令至少 2 cycles），指令後再補最後 1 個
        // cycle 與分支的額外 cycle（分支不碰 PPU，所以額外 cycle 放在後段無妨）。
        let lead = cycles.saturating_sub(1);
        if let Some(dummy) = dummy_read {
            // dummy read 比真正的存取早 1 個 cycle。
            self.bus.tick(lead.saturating_sub(1));
            let _ = self.bus.read(dummy);
            self.bus.tick(lead - lead.saturating_sub(1));
        } else {
            self.bus.tick(lead);
        }

        // IRQ 偵測：硬體在指令倒數第二個 cycle 結束時取樣 IRQ 線，也就是「追上 N − 1 個
        // cycle 之後、最後一個 cycle 之前」。指令自己在最後一個 cycle 造成的變化（寫
        // `$4015`／`$4017`、讀 `$4015` 清旗標）趕不上這次取樣。
        let i_before = self.status.contains(StatusFlags::INTERRUPT);
        self.irq_sample = self.bus.irq_line();

        let extra = self.execute(opcode, info, addr);

        // CLI／SEI／PLP 改變 I 旗標的時間點是它們的最後一個 cycle，晚於偵測：新的 I 旗標
        // 要到「下一條指令之後」才對 IRQ 有效，所以本次偵測用的是指令之前的 I。
        // （RTI 在倒數第二個 cycle 之前就還原了 I，所以立即生效；BRK／IRQ／NMI 自己設 I。）
        self.irq_masked = match info.mnemonic {
            Mnemonic::Cli | Mnemonic::Sei | Mnemonic::Plp => i_before,
            _ => self.status.contains(StatusFlags::INTERRUPT),
        };

        self.bus.tick(cycles - lead + extra);
        self.bus.run_pending_oam_dma();
        cycles + extra
    }

    // ---- 定址模式解析 --------------------------------------------------

    /// 依定址模式讀取 0~2 個 operand byte、把 PC 推進對應長度，回傳
    /// `(有效位址, 是否跨頁, 未修正位址)`。未修正位址只有 abs,X / abs,Y / (ind),Y
    /// 才有：低位元組已加上索引、高位元組還是基底的那個位址（dummy read 讀的位置）。
    /// `Implied`/`Accumulator` 沒有位址概念，回傳 `(0, false, None)`，呼叫端
    /// （`execute`）看 `mode` 決定要不要理它。
    fn resolve_operand(&mut self, mode: AddrMode) -> (u16, bool, Option<u16>) {
        match mode {
            AddrMode::Implied | AddrMode::Accumulator => (0, false, None),
            AddrMode::Immediate => {
                let addr = self.pc;
                self.pc = self.pc.wrapping_add(1);
                (addr, false, None)
            }
            AddrMode::ZeroPage => {
                let addr = self.bus.read(self.pc) as u16;
                self.pc = self.pc.wrapping_add(1);
                (addr, false, None)
            }
            AddrMode::ZeroPageX => {
                let base = self.bus.read(self.pc);
                self.pc = self.pc.wrapping_add(1);
                ((base.wrapping_add(self.x)) as u16, false, None)
            }
            AddrMode::ZeroPageY => {
                let base = self.bus.read(self.pc);
                self.pc = self.pc.wrapping_add(1);
                ((base.wrapping_add(self.y)) as u16, false, None)
            }
            AddrMode::Absolute => (self.read_u16_operand(), false, None),
            AddrMode::AbsoluteX => {
                let base = self.read_u16_operand();
                let addr = base.wrapping_add(self.x as u16);
                (
                    addr,
                    page_crossed(base, addr),
                    Some(uncorrected(base, addr)),
                )
            }
            AddrMode::AbsoluteY => {
                let base = self.read_u16_operand();
                let addr = base.wrapping_add(self.y as u16);
                (
                    addr,
                    page_crossed(base, addr),
                    Some(uncorrected(base, addr)),
                )
            }
            AddrMode::Indirect => {
                let ptr = self.read_u16_operand();
                (self.read_u16_bugged(ptr), false, None)
            }
            AddrMode::IndirectX => {
                let base = self.bus.read(self.pc);
                self.pc = self.pc.wrapping_add(1);
                let ptr = base.wrapping_add(self.x);
                let addr = self.read_u16_zp(ptr);
                (addr, false, None)
            }
            AddrMode::IndirectY => {
                let base = self.bus.read(self.pc);
                self.pc = self.pc.wrapping_add(1);
                let ptr = self.read_u16_zp(base);
                let addr = ptr.wrapping_add(self.y as u16);
                (addr, page_crossed(ptr, addr), Some(uncorrected(ptr, addr)))
            }
            AddrMode::Relative => {
                let offset = self.bus.read(self.pc) as i8;
                self.pc = self.pc.wrapping_add(1);
                let addr = self.pc.wrapping_add(offset as i16 as u16);
                (addr, false, None)
            }
        }
    }

    fn read_u16(&mut self, addr: u16) -> u16 {
        let lo = self.bus.read(addr) as u16;
        let hi = self.bus.read(addr.wrapping_add(1)) as u16;
        (hi << 8) | lo
    }

    fn read_u16_operand(&mut self) -> u16 {
        let lo = self.bus.read(self.pc) as u16;
        self.pc = self.pc.wrapping_add(1);
        let hi = self.bus.read(self.pc) as u16;
        self.pc = self.pc.wrapping_add(1);
        (hi << 8) | lo
    }

    /// zero-page 指標讀取：高低位元組都在 zero page 內 wrap（`$FF` 的下一個
    /// byte 是 `$00`，不會跨到 `$0100`）。
    fn read_u16_zp(&mut self, ptr: u8) -> u16 {
        let lo = self.bus.read(ptr as u16) as u16;
        let hi = self.bus.read(ptr.wrapping_add(1) as u16) as u16;
        (hi << 8) | lo
    }

    /// `JMP ($xxFF)` 的著名硬體 bug：高位元組不會跨到下一頁，而是從
    /// `$xx00` 讀取。
    fn read_u16_bugged(&mut self, ptr: u16) -> u16 {
        let lo = self.bus.read(ptr) as u16;
        let hi_addr = if ptr & 0x00FF == 0x00FF {
            ptr & 0xFF00
        } else {
            ptr.wrapping_add(1)
        };
        let hi = self.bus.read(hi_addr) as u16;
        (hi << 8) | lo
    }

    // ---- 堆疊 --------------------------------------------------------

    fn push_u8(&mut self, value: u8) {
        self.bus.write(STACK_BASE + self.sp as u16, value);
        self.sp = self.sp.wrapping_sub(1);
    }

    fn pop_u8(&mut self) -> u8 {
        self.sp = self.sp.wrapping_add(1);
        self.bus.read(STACK_BASE + self.sp as u16)
    }

    fn push_u16(&mut self, value: u16) {
        self.push_u8((value >> 8) as u8);
        self.push_u8((value & 0xFF) as u8);
    }

    fn pop_u16(&mut self) -> u16 {
        let lo = self.pop_u8() as u16;
        let hi = self.pop_u8() as u16;
        (hi << 8) | lo
    }

    // ---- ALU 輔助 ------------------------------------------------------

    /// ADC 的核心邏輯；SBC 呼叫這個函式時傳入 `value ^ 0xFF`
    /// （2A03 沒有十進位模式，SBC 在二進位下等於 `ADC(~value)`，這也是
    /// *SBC（`$EB`）跟官方 SBC 行為完全相同的原因）。
    fn adc(&mut self, value: u8) {
        let carry_in = self.status.contains(StatusFlags::CARRY) as u16;
        let sum = self.a as u16 + value as u16 + carry_in;
        let result = sum as u8;
        self.status.set(StatusFlags::CARRY, sum > 0xFF);
        let overflow = (!(self.a ^ value) & (self.a ^ result) & 0x80) != 0;
        self.status.set(StatusFlags::OVERFLOW, overflow);
        self.a = result;
        self.status.set_zero_negative(self.a);
    }

    fn compare(&mut self, reg: u8, value: u8) {
        let result = reg.wrapping_sub(value);
        self.status.set(StatusFlags::CARRY, reg >= value);
        self.status.set_zero_negative(result);
    }

    fn asl_value(&mut self, value: u8) -> u8 {
        self.status.set(StatusFlags::CARRY, value & 0x80 != 0);
        let out = value << 1;
        self.status.set_zero_negative(out);
        out
    }

    fn lsr_value(&mut self, value: u8) -> u8 {
        self.status.set(StatusFlags::CARRY, value & 0x01 != 0);
        let out = value >> 1;
        self.status.set_zero_negative(out);
        out
    }

    fn rol_value(&mut self, value: u8) -> u8 {
        let carry_in = self.status.contains(StatusFlags::CARRY) as u8;
        self.status.set(StatusFlags::CARRY, value & 0x80 != 0);
        let out = (value << 1) | carry_in;
        self.status.set_zero_negative(out);
        out
    }

    fn ror_value(&mut self, value: u8) -> u8 {
        let carry_in = self.status.contains(StatusFlags::CARRY) as u8;
        self.status.set(StatusFlags::CARRY, value & 0x01 != 0);
        let out = (value >> 1) | (carry_in << 7);
        self.status.set_zero_negative(out);
        out
    }

    /// 非官方 opcode ARR（`$6B`）：`A = (A & M) ROR 1`，但 C/V 旗標不是照
    /// 一般 ROR 的規則設，而是取結果的 bit6/bit5（ALU 內部借用了加法器的
    /// 進位/溢位電路所產生的著名怪異行為，各 6502 non-official opcode 參考
    /// 資料都有記載）。呼叫前 `self.a` 必須已經是 `A & M` 的結果。
    fn arr(&mut self) {
        let carry_in = self.status.contains(StatusFlags::CARRY) as u8;
        let value = (self.a >> 1) | (carry_in << 7);
        self.a = value;
        self.status.set_zero_negative(value);
        let bit6 = (value >> 6) & 1;
        let bit5 = (value >> 5) & 1;
        self.status.set(StatusFlags::CARRY, bit6 == 1);
        self.status.set(StatusFlags::OVERFLOW, (bit6 ^ bit5) == 1);
    }

    fn branch(&mut self, target: u16, condition: bool) -> u8 {
        if !condition {
            return 0;
        }
        let extra = if page_crossed(self.pc, target) { 2 } else { 1 };
        self.pc = target;
        extra
    }

    fn brk(&mut self) {
        // BRK 實際上是 2-byte 指令（第二個 byte 是被跳過的 signature
        // byte）；push 回去的地址要跳過它，RTI 之後才會接到正確的下一條
        // 指令。此時 self.pc 已經因為 opcode fetch 前進了 1（Implied 定址
        // 模式不會再消耗 operand byte），所以只需要再 +1。
        let ret = self.pc.wrapping_add(1);
        self.push_u16(ret);
        let flags = (self.status | StatusFlags::BREAK | StatusFlags::UNUSED).bits();
        self.push_u8(flags);
        self.status.insert(StatusFlags::INTERRUPT);
        self.pc = self.read_u16(IRQ_VECTOR);
    }

    // ---- 指令執行 ------------------------------------------------------

    /// 依 opcode 的 mnemonic 執行指令。回傳「額外」cycle 數（只有分支指令
    /// taken/跨頁會用到；其他指令固定回傳 0，因為它們的 cycle 數完全由
    /// opcode 表決定）。
    fn execute(&mut self, _opcode: u8, info: &OpcodeInfo, addr: u16) -> u8 {
        match info.mnemonic {
            Mnemonic::Lda => {
                let v = self.bus.read(addr);
                self.a = v;
                self.status.set_zero_negative(v);
                0
            }
            Mnemonic::Ldx => {
                let v = self.bus.read(addr);
                self.x = v;
                self.status.set_zero_negative(v);
                0
            }
            Mnemonic::Ldy => {
                let v = self.bus.read(addr);
                self.y = v;
                self.status.set_zero_negative(v);
                0
            }
            Mnemonic::Sta => {
                self.bus.write(addr, self.a);
                0
            }
            Mnemonic::Stx => {
                self.bus.write(addr, self.x);
                0
            }
            Mnemonic::Sty => {
                self.bus.write(addr, self.y);
                0
            }
            Mnemonic::Tax => {
                self.x = self.a;
                self.status.set_zero_negative(self.x);
                0
            }
            Mnemonic::Tay => {
                self.y = self.a;
                self.status.set_zero_negative(self.y);
                0
            }
            Mnemonic::Txa => {
                self.a = self.x;
                self.status.set_zero_negative(self.a);
                0
            }
            Mnemonic::Tya => {
                self.a = self.y;
                self.status.set_zero_negative(self.a);
                0
            }
            Mnemonic::Tsx => {
                self.x = self.sp;
                self.status.set_zero_negative(self.x);
                0
            }
            Mnemonic::Txs => {
                self.sp = self.x;
                0
            }
            Mnemonic::Pha => {
                self.push_u8(self.a);
                0
            }
            Mnemonic::Php => {
                let flags = (self.status | StatusFlags::BREAK | StatusFlags::UNUSED).bits();
                self.push_u8(flags);
                0
            }
            Mnemonic::Pla => {
                let v = self.pop_u8();
                self.a = v;
                self.status.set_zero_negative(v);
                0
            }
            Mnemonic::Plp => {
                let popped = self.pop_u8();
                self.status = (StatusFlags::from_bits_truncate(popped) & !StatusFlags::BREAK)
                    | StatusFlags::UNUSED;
                0
            }
            Mnemonic::Adc => {
                let v = self.bus.read(addr);
                self.adc(v);
                0
            }
            Mnemonic::Sbc => {
                let v = self.bus.read(addr);
                self.adc(v ^ 0xFF);
                0
            }
            Mnemonic::And => {
                let v = self.bus.read(addr);
                self.a &= v;
                self.status.set_zero_negative(self.a);
                0
            }
            Mnemonic::Ora => {
                let v = self.bus.read(addr);
                self.a |= v;
                self.status.set_zero_negative(self.a);
                0
            }
            Mnemonic::Eor => {
                let v = self.bus.read(addr);
                self.a ^= v;
                self.status.set_zero_negative(self.a);
                0
            }
            Mnemonic::Cmp => {
                let v = self.bus.read(addr);
                self.compare(self.a, v);
                0
            }
            Mnemonic::Cpx => {
                let v = self.bus.read(addr);
                self.compare(self.x, v);
                0
            }
            Mnemonic::Cpy => {
                let v = self.bus.read(addr);
                self.compare(self.y, v);
                0
            }
            Mnemonic::Inc => {
                let old = self.bus.read(addr);
                let v = old.wrapping_add(1);
                self.bus.write_rmw(addr, old, v);
                self.status.set_zero_negative(v);
                0
            }
            Mnemonic::Dec => {
                let old = self.bus.read(addr);
                let v = old.wrapping_sub(1);
                self.bus.write_rmw(addr, old, v);
                self.status.set_zero_negative(v);
                0
            }
            Mnemonic::Inx => {
                self.x = self.x.wrapping_add(1);
                self.status.set_zero_negative(self.x);
                0
            }
            Mnemonic::Iny => {
                self.y = self.y.wrapping_add(1);
                self.status.set_zero_negative(self.y);
                0
            }
            Mnemonic::Dex => {
                self.x = self.x.wrapping_sub(1);
                self.status.set_zero_negative(self.x);
                0
            }
            Mnemonic::Dey => {
                self.y = self.y.wrapping_sub(1);
                self.status.set_zero_negative(self.y);
                0
            }
            Mnemonic::Asl => {
                if info.mode == AddrMode::Accumulator {
                    let v = self.a;
                    self.a = self.asl_value(v);
                } else {
                    let old = self.bus.read(addr);
                    let v = self.asl_value(old);
                    self.bus.write_rmw(addr, old, v);
                }
                0
            }
            Mnemonic::Lsr => {
                if info.mode == AddrMode::Accumulator {
                    let v = self.a;
                    self.a = self.lsr_value(v);
                } else {
                    let old = self.bus.read(addr);
                    let v = self.lsr_value(old);
                    self.bus.write_rmw(addr, old, v);
                }
                0
            }
            Mnemonic::Rol => {
                if info.mode == AddrMode::Accumulator {
                    let v = self.a;
                    self.a = self.rol_value(v);
                } else {
                    let old = self.bus.read(addr);
                    let v = self.rol_value(old);
                    self.bus.write_rmw(addr, old, v);
                }
                0
            }
            Mnemonic::Ror => {
                if info.mode == AddrMode::Accumulator {
                    let v = self.a;
                    self.a = self.ror_value(v);
                } else {
                    let old = self.bus.read(addr);
                    let v = self.ror_value(old);
                    self.bus.write_rmw(addr, old, v);
                }
                0
            }
            Mnemonic::Bit => {
                let v = self.bus.read(addr);
                self.status.set(StatusFlags::ZERO, (self.a & v) == 0);
                self.status.set(StatusFlags::OVERFLOW, v & 0x40 != 0);
                self.status.set(StatusFlags::NEGATIVE, v & 0x80 != 0);
                0
            }
            Mnemonic::Jmp => {
                self.pc = addr;
                0
            }
            Mnemonic::Jsr => {
                let ret = self.pc.wrapping_sub(1);
                self.push_u16(ret);
                self.pc = addr;
                0
            }
            Mnemonic::Rts => {
                let ret = self.pop_u16();
                self.pc = ret.wrapping_add(1);
                0
            }
            Mnemonic::Rti => {
                let popped = self.pop_u8();
                self.status = (StatusFlags::from_bits_truncate(popped) & !StatusFlags::BREAK)
                    | StatusFlags::UNUSED;
                self.pc = self.pop_u16();
                0
            }
            Mnemonic::Brk => {
                self.brk();
                0
            }
            Mnemonic::Clc => {
                self.status.remove(StatusFlags::CARRY);
                0
            }
            Mnemonic::Sec => {
                self.status.insert(StatusFlags::CARRY);
                0
            }
            Mnemonic::Cli => {
                self.status.remove(StatusFlags::INTERRUPT);
                0
            }
            Mnemonic::Sei => {
                self.status.insert(StatusFlags::INTERRUPT);
                0
            }
            Mnemonic::Clv => {
                self.status.remove(StatusFlags::OVERFLOW);
                0
            }
            Mnemonic::Cld => {
                self.status.remove(StatusFlags::DECIMAL);
                0
            }
            Mnemonic::Sed => {
                self.status.insert(StatusFlags::DECIMAL);
                0
            }
            Mnemonic::Nop => {
                if info.mode != AddrMode::Implied {
                    let _ = self.bus.read(addr);
                }
                0
            }
            Mnemonic::Bcc => self.branch(addr, !self.status.contains(StatusFlags::CARRY)),
            Mnemonic::Bcs => self.branch(addr, self.status.contains(StatusFlags::CARRY)),
            Mnemonic::Beq => self.branch(addr, self.status.contains(StatusFlags::ZERO)),
            Mnemonic::Bne => self.branch(addr, !self.status.contains(StatusFlags::ZERO)),
            Mnemonic::Bmi => self.branch(addr, self.status.contains(StatusFlags::NEGATIVE)),
            Mnemonic::Bpl => self.branch(addr, !self.status.contains(StatusFlags::NEGATIVE)),
            Mnemonic::Bvc => self.branch(addr, !self.status.contains(StatusFlags::OVERFLOW)),
            Mnemonic::Bvs => self.branch(addr, self.status.contains(StatusFlags::OVERFLOW)),

            // ---- 非官方 opcode ----
            Mnemonic::Lax => {
                let v = self.bus.read(addr);
                self.a = v;
                self.x = v;
                self.status.set_zero_negative(v);
                0
            }
            Mnemonic::Sax => {
                self.bus.write(addr, self.a & self.x);
                0
            }
            Mnemonic::Dcp => {
                let old = self.bus.read(addr);
                let v = old.wrapping_sub(1);
                self.bus.write_rmw(addr, old, v);
                self.compare(self.a, v);
                0
            }
            Mnemonic::Isb => {
                let old = self.bus.read(addr);
                let v = old.wrapping_add(1);
                self.bus.write_rmw(addr, old, v);
                self.adc(v ^ 0xFF);
                0
            }
            Mnemonic::Slo => {
                let old = self.bus.read(addr);
                let v = self.asl_value(old);
                self.bus.write_rmw(addr, old, v);
                self.a |= v;
                self.status.set_zero_negative(self.a);
                0
            }
            Mnemonic::Rla => {
                let old = self.bus.read(addr);
                let v = self.rol_value(old);
                self.bus.write_rmw(addr, old, v);
                self.a &= v;
                self.status.set_zero_negative(self.a);
                0
            }
            Mnemonic::Sre => {
                let old = self.bus.read(addr);
                let v = self.lsr_value(old);
                self.bus.write_rmw(addr, old, v);
                self.a ^= v;
                self.status.set_zero_negative(self.a);
                0
            }
            Mnemonic::Rra => {
                let old = self.bus.read(addr);
                let v = self.ror_value(old);
                self.bus.write_rmw(addr, old, v);
                self.adc(v);
                0
            }
            Mnemonic::Anc => {
                let v = self.bus.read(addr);
                self.a &= v;
                self.status.set_zero_negative(self.a);
                let negative = self.status.contains(StatusFlags::NEGATIVE);
                self.status.set(StatusFlags::CARRY, negative);
                0
            }
            Mnemonic::Alr => {
                let v = self.bus.read(addr);
                self.a &= v;
                let a = self.a;
                self.a = self.lsr_value(a);
                0
            }
            Mnemonic::Arr => {
                let v = self.bus.read(addr);
                self.a &= v;
                self.arr();
                0
            }
            Mnemonic::Axs => {
                let v = self.bus.read(addr);
                let base = self.a & self.x;
                let (result, borrow) = base.overflowing_sub(v);
                self.status.set(StatusFlags::CARRY, !borrow);
                self.x = result;
                self.status.set_zero_negative(self.x);
                0
            }
            Mnemonic::Shy => {
                let base = addr.wrapping_sub(self.x as u16);
                self.store_and_high(base, addr, self.y);
                0
            }
            Mnemonic::Shx => {
                let base = addr.wrapping_sub(self.y as u16);
                self.store_and_high(base, addr, self.x);
                0
            }
            Mnemonic::Lxa => {
                // A = X = (A | magic) & imm。真實晶片的 magic 因批次／溫度而異
                // （常見 $00、$EE、$FF）。本專案以 blargg instr_test 為準，取 $FF
                // （即 A = X = imm）；SingleStepTests 的資料逐 bit 分析只符合 $EE，
                // 與 blargg 互相衝突（用 $EE 時 03-immediate 失敗），所以 SST 對 $AB
                // 只有約 56% 相符，見 docs/architecture.md §10。
                let v = (self.a | LXA_MAGIC) & self.bus.read(addr);
                self.a = v;
                self.x = v;
                self.status.set_zero_negative(v);
                0
            }
            Mnemonic::Jam => {
                self.jammed = true;
                0
            }
        }
    }

    /// SHY / SHX：把 `reg & (H + 1)` 寫到 `addr`，H 是「未加索引前」基底位址的高位元組。
    /// 索引加法跨頁時，寫入位址的高位元組會被換成寫入的值（硬體上位址高位元與資料
    /// 共用內部匯流排的副作用）。
    fn store_and_high(&mut self, base: u16, addr: u16, reg: u8) {
        let value = reg & ((base >> 8) as u8).wrapping_add(1);
        let target = if page_crossed(base, addr) {
            ((value as u16) << 8) | (addr & 0x00FF)
        } else {
            addr
        };
        self.bus.write(target, value);
    }

    /// 輸出跟 nestest.log 相同格式的一行 trace，例如：
    /// `C000  4C F5 C5  JMP $C5F5                       A:00 X:00 Y:00 P:24 SP:FD PPU:  0, 21 CYC:7`
    ///
    /// 全程只用 `peek`，不呼叫 `read`：PPU 的 `$2002`/`$2007` 之後會有
    /// side effect（清 vblank 旗標、advance PPUDATA 位址……），trace 只是
    /// 觀察目前狀態，絕對不能因為印一行 log 就悄悄改變模擬狀態。
    pub fn trace(&self) -> String {
        let opcode = self.bus.peek(self.pc);
        let info = &OPCODES[opcode as usize];

        let mut bytes = vec![opcode];
        for i in 1..info.bytes {
            bytes.push(self.bus.peek(self.pc.wrapping_add(i as u16)));
        }
        let bytes_str = bytes
            .iter()
            .map(|b| format!("{b:02X}"))
            .collect::<Vec<_>>()
            .join(" ");

        let disasm = self.current_disassembly();
        let (scanline, cycle) = self.bus.ppu_dot();

        format!(
            "{:04X}  {:<9} {:<32}A:{:02X} X:{:02X} Y:{:02X} P:{:02X} SP:{:02X} PPU:{:>3},{:>3} CYC:{}",
            self.pc,
            bytes_str,
            disasm,
            self.a,
            self.x,
            self.y,
            self.status.bits(),
            self.sp,
            scanline,
            cycle,
            self.bus.total_cycles(),
        )
    }

    /// 目前 PC 這條指令的反組譯文字（`MNEMONIC OPERAND`，不含暫存器/CYC），
    /// 給 `DebugSnapshot` 用。只呼叫 `peek`，沒有副作用。
    pub fn current_disassembly(&self) -> String {
        let opcode = self.bus.peek(self.pc);
        let info = &OPCODES[opcode as usize];
        let mut bytes = vec![opcode];
        for i in 1..info.bytes {
            bytes.push(self.bus.peek(self.pc.wrapping_add(i as u16)));
        }
        self.disassemble(info, &bytes)
    }

    /// trace／反組譯的 `= xx`（運算元位址上的目前內容）。
    ///
    /// `$4000-$4015`（APU 暫存器）顯示 `FF`：其中 `$4000-$4014` 是唯寫暫存器，沒有東西可讀；
    /// `$4015` 雖然可讀，但讀取有副作用（清 frame IRQ 旗標），trace 不能做。nestest.log（由
    /// Nintendulator 產生）在這些位址一律顯示 `FF`，沿用這個約定，`nestest --strict` 才能與參考
    /// log 逐字相符。這只影響顯示，不影響 `Bus::peek` 的回傳值。
    fn trace_value(&self, addr: u16) -> u8 {
        if (0x4000..=0x4015).contains(&addr) {
            0xFF
        } else {
            self.bus.peek(addr)
        }
    }

    /// 純觀察用的反組譯，只呼叫 `peek`。`bytes` 是這條指令的原始位元組
    /// （已經包含 opcode 本身）。
    fn disassemble(&self, info: &OpcodeInfo, bytes: &[u8]) -> String {
        let unofficial_prefix = if info.official { "" } else { "*" };
        let mnemonic = format!("{unofficial_prefix}{}", info.mnemonic);

        let operand = match info.mode {
            AddrMode::Implied => String::new(),
            AddrMode::Accumulator => "A".to_string(),
            AddrMode::Immediate => format!("#${:02X}", bytes[1]),
            AddrMode::ZeroPage => {
                let addr = bytes[1] as u16;
                format!("${:02X} = {:02X}", addr, self.bus.peek(addr))
            }
            AddrMode::ZeroPageX => {
                let base = bytes[1];
                let addr = base.wrapping_add(self.x) as u16;
                format!("${base:02X},X @ {addr:02X} = {:02X}", self.bus.peek(addr))
            }
            AddrMode::ZeroPageY => {
                let base = bytes[1];
                let addr = base.wrapping_add(self.y) as u16;
                format!("${base:02X},Y @ {addr:02X} = {:02X}", self.bus.peek(addr))
            }
            AddrMode::Absolute => {
                let addr = u16::from_le_bytes([bytes[1], bytes[2]]);
                if info.mnemonic == Mnemonic::Jmp || info.mnemonic == Mnemonic::Jsr {
                    format!("${addr:04X}")
                } else {
                    format!("${addr:04X} = {:02X}", self.trace_value(addr))
                }
            }
            AddrMode::AbsoluteX => {
                let base = u16::from_le_bytes([bytes[1], bytes[2]]);
                let addr = base.wrapping_add(self.x as u16);
                format!(
                    "${base:04X},X @ {addr:04X} = {:02X}",
                    self.trace_value(addr)
                )
            }
            AddrMode::AbsoluteY => {
                let base = u16::from_le_bytes([bytes[1], bytes[2]]);
                let addr = base.wrapping_add(self.y as u16);
                format!(
                    "${base:04X},Y @ {addr:04X} = {:02X}",
                    self.trace_value(addr)
                )
            }
            AddrMode::Indirect => {
                let ptr = u16::from_le_bytes([bytes[1], bytes[2]]);
                let lo = self.bus.peek(ptr) as u16;
                let hi_addr = if ptr & 0x00FF == 0x00FF {
                    ptr & 0xFF00
                } else {
                    ptr + 1
                };
                let hi = self.bus.peek(hi_addr) as u16;
                format!("(${ptr:04X}) = {:04X}", (hi << 8) | lo)
            }
            AddrMode::IndirectX => {
                let base = bytes[1];
                let ptr = base.wrapping_add(self.x);
                let lo = self.bus.peek(ptr as u16) as u16;
                let hi = self.bus.peek(ptr.wrapping_add(1) as u16) as u16;
                let addr = (hi << 8) | lo;
                format!(
                    "(${base:02X},X) @ {ptr:02X} = {addr:04X} = {:02X}",
                    self.trace_value(addr)
                )
            }
            AddrMode::IndirectY => {
                let base = bytes[1];
                let lo = self.bus.peek(base as u16) as u16;
                let hi = self.bus.peek(base.wrapping_add(1) as u16) as u16;
                let ptr = (hi << 8) | lo;
                let addr = ptr.wrapping_add(self.y as u16);
                format!(
                    "(${base:02X}),Y = {ptr:04X} @ {addr:04X} = {:02X}",
                    self.trace_value(addr)
                )
            }
            AddrMode::Relative => {
                let offset = bytes[1] as i8;
                let target = self
                    .pc
                    .wrapping_add(info.bytes as u16)
                    .wrapping_add(offset as i16 as u16);
                format!("${target:04X}")
            }
        };

        if operand.is_empty() {
            mnemonic
        } else {
            format!("{mnemonic} {operand}")
        }
    }
}

/// 索引加法的「未修正位址」：高位元組取基底的、低位元組取加上索引之後的。
fn uncorrected(base: u16, effective: u16) -> u16 {
    (base & 0xFF00) | (effective & 0x00FF)
}

fn page_crossed(a: u16, b: u16) -> bool {
    (a & 0xFF00) != (b & 0xFF00)
}

#[cfg(test)]
mod singlestep;
#[cfg(test)]
mod tests;
