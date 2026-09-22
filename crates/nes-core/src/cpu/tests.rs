//! `nestest` 常見失敗點的針對性單元測試（見 `cpu/mod.rs` 模組文件與
//! Phase 1 任務規格列出的 8 個項目）。

use super::*;
use crate::bus::Bus;
use crate::cartridge::{Cartridge, Mapper, Mirroring, Nrom, PRG_BANK_SIZE, PRG_RAM_SIZE, RomInfo};

/// 建立一顆測試用 CPU：32KB PRG-ROM（2 個 bank，`$8000-$FFFF` 不會鏡像，
/// 方便直接把測試程式放在任何位址），初始全部填 `0x00`（BRK）。呼叫端要
/// 自己設定 `cpu.pc` 並用 `load_program` 放程式碼，不呼叫 `reset()`
/// （reset 有自己的獨立測試）。
fn new_test_cpu() -> Cpu {
    let cart = Cartridge {
        info: RomInfo {
            prg_rom_banks: 2,
            chr_rom_banks: 1,
            mapper_id: 0,
            mirroring: Mirroring::Horizontal,
            battery_backed: false,
            has_trainer: false,
        },
        prg_rom: vec![0u8; PRG_BANK_SIZE * 2],
        chr_rom: vec![0u8; 8192],
        chr_ram: Vec::new(),
        prg_ram: vec![0u8; PRG_RAM_SIZE],
        mapper: Mapper::Nrom(Nrom::new(2)),
        rom_hash: 0,
    };
    Cpu::new(Bus::new(cart))
}

fn load_program(cpu: &mut Cpu, addr: u16, bytes: &[u8]) {
    let base = addr as usize - 0x8000;
    cpu.bus_mut().cartridge.prg_rom[base..base + bytes.len()].copy_from_slice(bytes);
}

fn poke_prg(cpu: &mut Cpu, addr: u16, value: u8) {
    cpu.bus_mut().cartridge.prg_rom[addr as usize - 0x8000] = value;
}

// ---- 1. 跨頁 +1 cycle 只適用於讀取類指令 --------------------------------

#[test]
fn read_instruction_gets_page_cross_bonus() {
    let mut cpu = new_test_cpu();
    cpu.x = 0;
    cpu.pc = 0x8000;
    load_program(&mut cpu, 0x8000, &[0xBD, 0x00, 0x80]); // LDA $8000,X（不跨頁）
    assert_eq!(cpu.step(), 4);

    cpu.x = 0xFF;
    cpu.pc = 0x8010;
    load_program(&mut cpu, 0x8010, &[0xBD, 0x01, 0x80]); // LDA $8001,X -> $8100（跨頁）
    assert_eq!(cpu.step(), 5);
}

#[test]
fn store_and_rmw_use_fixed_cycles_regardless_of_page_cross() {
    let mut cpu = new_test_cpu();
    cpu.x = 0xFF;
    cpu.pc = 0x8000;
    load_program(&mut cpu, 0x8000, &[0x9D, 0x01, 0x80]); // STA $8001,X -> $8100（跨頁）
    assert_eq!(cpu.step(), 5); // 固定 5，沒有額外 +1

    cpu.x = 0xFF;
    cpu.pc = 0x8010;
    load_program(&mut cpu, 0x8010, &[0xFE, 0x01, 0x80]); // INC $8001,X -> $8100（跨頁，RMW）
    assert_eq!(cpu.step(), 7); // 固定 7
}

// ---- 2. 分支成立 +1；成立且跨頁 +2 --------------------------------------

#[test]
fn branch_not_taken_costs_base_cycles_only() {
    let mut cpu = new_test_cpu();
    cpu.pc = 0x8000;
    cpu.status.remove(StatusFlags::ZERO);
    load_program(&mut cpu, 0x8000, &[0xF0, 0x10]); // BEQ，Z=0 不成立
    assert_eq!(cpu.step(), 2);
    assert_eq!(cpu.pc, 0x8002);
}

#[test]
fn branch_taken_same_page_costs_one_extra_cycle() {
    let mut cpu = new_test_cpu();
    cpu.pc = 0x8000;
    cpu.status.insert(StatusFlags::ZERO);
    load_program(&mut cpu, 0x8000, &[0xF0, 0x10]); // BEQ +16，成立，目標與下一條指令同頁
    assert_eq!(cpu.step(), 3);
    assert_eq!(cpu.pc, 0x8012);
}

#[test]
fn branch_taken_crossing_page_costs_two_extra_cycles() {
    let mut cpu = new_test_cpu();
    cpu.pc = 0x80F0;
    cpu.status.insert(StatusFlags::ZERO);
    load_program(&mut cpu, 0x80F0, &[0xF0, 0x20]); // BEQ +32，成立且跨頁
    assert_eq!(cpu.step(), 4);
    assert_eq!(cpu.pc, 0x8112);
}

// ---- 3. JMP ($xxFF) 的頁邊界 bug ----------------------------------------

#[test]
fn jmp_indirect_page_boundary_bug() {
    let mut cpu = new_test_cpu();
    cpu.pc = 0x8010;
    load_program(&mut cpu, 0x8010, &[0x6C, 0xFF, 0x80]); // JMP ($80FF)
    poke_prg(&mut cpu, 0x80FF, 0x34); // 低位元組
    poke_prg(&mut cpu, 0x8100, 0x99); // 「正確」高位元組位置（應該被忽略）
    poke_prg(&mut cpu, 0x8000, 0x12); // 硬體 bug 實際讀取的高位元組位置（$xx00）
    cpu.step();
    assert_eq!(cpu.pc, 0x1234);
}

// ---- 4. Stack 位於 $0100 頁，SP 需正確 wrap -----------------------------

#[test]
fn stack_wraps_within_page_one() {
    let mut cpu = new_test_cpu();
    cpu.sp = 0x00;
    cpu.pc = 0x8000;
    cpu.a = 0x42;
    load_program(&mut cpu, 0x8000, &[0x48]); // PHA
    cpu.step();
    assert_eq!(cpu.sp, 0xFF);
    assert_eq!(cpu.bus_mut().read(0x0100), 0x42);
}

// ---- 5. PHP/BRK 推入 B=1,U=1；NMI/IRQ 推入 B=0,U=1；PLP/RTI 忽略兩者 ----

#[test]
fn php_sets_break_and_unused_bits_in_pushed_byte() {
    let mut cpu = new_test_cpu();
    cpu.pc = 0x8000;
    cpu.status = StatusFlags::from_bits_truncate(0x00);
    load_program(&mut cpu, 0x8000, &[0x08]); // PHP
    cpu.step();
    let pushed = cpu.bus_mut().read(0x01FD);
    assert_eq!(pushed, 0b0011_0000); // BREAK | UNUSED
}

#[test]
fn brk_sets_break_and_unused_bits_in_pushed_byte() {
    let mut cpu = new_test_cpu();
    cpu.pc = 0x8000;
    cpu.status = StatusFlags::from_bits_truncate(0x00);
    load_program(&mut cpu, 0x8000, &[0x00]); // BRK
    poke_prg(&mut cpu, 0xFFFE, 0x00);
    poke_prg(&mut cpu, 0xFFFF, 0x90);
    cpu.step();
    // push 順序：ret 高位元組 $01FD、ret 低位元組 $01FC、status $01FB
    // （sp 從 0xFD 開始，每 push 一個 byte 就再 -1）。
    let pushed = cpu.bus_mut().read(0x01FB);
    assert_eq!(pushed, 0b0011_0000);
    assert_eq!(cpu.pc, 0x9000);
    // 回傳位址要跳過 BRK 之後的 signature byte：push 的是 pc+2。
    let ret_hi = cpu.bus_mut().read(0x01FD) as u16;
    let ret_lo = cpu.bus_mut().read(0x01FC) as u16;
    assert_eq!((ret_hi << 8) | ret_lo, 0x8002);
}

#[test]
fn nmi_pushes_status_with_break_clear_and_unused_set() {
    let mut cpu = new_test_cpu();
    cpu.status = StatusFlags::from_bits_truncate(0xFF);
    cpu.pc = 0x1234;
    poke_prg(&mut cpu, 0xFFFA, 0x00);
    poke_prg(&mut cpu, 0xFFFB, 0x90);
    cpu.nmi();
    // push 順序：pc 高 $01FD、pc 低 $01FC、status $01FB。
    let pushed = cpu.bus_mut().read(0x01FB);
    assert_eq!(pushed & 0b0011_0000, 0b0010_0000); // B=0, U=1
    assert_eq!(cpu.pc, 0x9000);
}

#[test]
fn irq_pushes_status_with_break_clear_and_unused_set() {
    let mut cpu = new_test_cpu();
    cpu.status = StatusFlags::from_bits_truncate(0xFF);
    cpu.pc = 0x1234;
    poke_prg(&mut cpu, 0xFFFE, 0x00);
    poke_prg(&mut cpu, 0xFFFF, 0x90);
    cpu.irq();
    let pushed = cpu.bus_mut().read(0x01FB);
    assert_eq!(pushed & 0b0011_0000, 0b0010_0000);
    assert_eq!(cpu.pc, 0x9000);
}

#[test]
fn plp_ignores_break_and_unused_bits_from_stack() {
    let mut cpu = new_test_cpu();
    cpu.pc = 0x8000;
    cpu.sp = 0xFC;
    cpu.bus_mut().write(0x01FD, 0x00); // 手動塞一個 B=0, U=0 的值上堆疊
    load_program(&mut cpu, 0x8000, &[0x28]); // PLP
    cpu.step();
    assert!(cpu.status.contains(StatusFlags::UNUSED)); // 強制變回 1
    assert!(!cpu.status.contains(StatusFlags::BREAK)); // 強制變回 0
}

#[test]
fn rti_ignores_break_and_unused_bits_from_stack() {
    let mut cpu = new_test_cpu();
    cpu.pc = 0x8000;
    cpu.sp = 0xFB;
    cpu.bus_mut().write(0x01FC, 0xFF); // 堆疊上的 P：全部 bit 都設（含 B）
    cpu.bus_mut().write(0x01FD, 0x00); // 回傳位址低位元組
    cpu.bus_mut().write(0x01FE, 0x90); // 回傳位址高位元組
    load_program(&mut cpu, 0x8000, &[0x40]); // RTI
    cpu.step();
    assert!(cpu.status.contains(StatusFlags::UNUSED));
    assert!(!cpu.status.contains(StatusFlags::BREAK));
    assert_eq!(cpu.pc, 0x9000);
}

// ---- 6. D 旗標可設定/清除，但不影響 ADC/SBC（2A03 沒有十進位模式）------

#[test]
fn decimal_flag_can_be_set_and_cleared_but_does_not_affect_adc() {
    let mut cpu = new_test_cpu();
    cpu.pc = 0x8000;
    cpu.a = 0x09;
    cpu.status.remove(StatusFlags::CARRY);
    load_program(&mut cpu, 0x8000, &[0xF8, 0x69, 0x01, 0xD8]); // SED; ADC #$01; CLD
    cpu.step();
    assert!(cpu.status.contains(StatusFlags::DECIMAL));
    cpu.step();
    assert_eq!(cpu.a, 0x0A); // 二進位加法：0x09+0x01=0x0A，不是十進位調整後的 0x10
    cpu.step();
    assert!(!cpu.status.contains(StatusFlags::DECIMAL));
}

// ---- 7. ADC / SBC 的 overflow 旗標 --------------------------------------

#[test]
fn adc_sets_overflow_on_signed_overflow() {
    let mut cpu = new_test_cpu();
    cpu.pc = 0x8000;
    cpu.a = 0x50;
    cpu.status.remove(StatusFlags::CARRY);
    load_program(&mut cpu, 0x8000, &[0x69, 0x50]); // ADC #$50：80+80=160，符號溢位
    cpu.step();
    assert_eq!(cpu.a, 0xA0);
    assert!(cpu.status.contains(StatusFlags::OVERFLOW));
    assert!(cpu.status.contains(StatusFlags::NEGATIVE));
}

#[test]
fn sbc_clears_carry_on_borrow() {
    let mut cpu = new_test_cpu();
    cpu.pc = 0x8000;
    cpu.a = 0x00;
    cpu.status.insert(StatusFlags::CARRY); // carry set = 沒有 borrow
    load_program(&mut cpu, 0x8000, &[0xE9, 0x01]); // SBC #$01
    cpu.step();
    assert_eq!(cpu.a, 0xFF); // 0 - 1 = -1
    assert!(!cpu.status.contains(StatusFlags::CARRY)); // 發生 borrow -> carry 清除
}

// ---- 8. Zero-page indexed 在 $FF 內 wrap ---------------------------------

#[test]
fn zero_page_indexed_wraps_within_page_zero() {
    let mut cpu = new_test_cpu();
    cpu.pc = 0x8000;
    cpu.x = 0x01;
    load_program(&mut cpu, 0x8000, &[0xB5, 0xFF]); // LDA $FF,X -> 應該讀 $00，不是 $0100
    cpu.bus_mut().write(0x0000, 0x77);
    cpu.bus_mut().write(0x0100, 0x99); // 陷阱值：wrap 錯了才會讀到這個
    cpu.step();
    assert_eq!(cpu.a, 0x77);
}

// ---- reset ---------------------------------------------------------------

#[test]
fn reset_reads_vector_sets_sp_and_status_and_takes_7_cycles() {
    let mut cpu = new_test_cpu();
    poke_prg(&mut cpu, 0xFFFC, 0x00);
    poke_prg(&mut cpu, 0xFFFD, 0x90);
    cpu.reset();
    assert_eq!(cpu.pc, 0x9000);
    assert_eq!(cpu.sp, 0xFD);
    assert_eq!(cpu.status.bits(), 0x24);
    assert_eq!(cpu.bus().total_cycles(), 7);
}

// ---- 非官方 opcode --------------------------------------------------------

#[test]
fn lax_loads_both_a_and_x() {
    let mut cpu = new_test_cpu();
    cpu.pc = 0x8000;
    load_program(&mut cpu, 0x8000, &[0xA7, 0x10]); // LAX $10
    cpu.bus_mut().write(0x0010, 0x55);
    cpu.step();
    assert_eq!(cpu.a, 0x55);
    assert_eq!(cpu.x, 0x55);
}

#[test]
fn sax_stores_a_and_x_bitwise_and() {
    let mut cpu = new_test_cpu();
    cpu.pc = 0x8000;
    cpu.a = 0b1100_1100;
    cpu.x = 0b1010_1010;
    load_program(&mut cpu, 0x8000, &[0x87, 0x10]); // SAX $10
    cpu.step();
    assert_eq!(cpu.bus_mut().read(0x0010), 0b1000_1000);
}

#[test]
fn dcp_decrements_memory_then_compares_with_a() {
    let mut cpu = new_test_cpu();
    cpu.pc = 0x8000;
    cpu.a = 0x05;
    load_program(&mut cpu, 0x8000, &[0xC7, 0x10]); // DCP $10
    cpu.bus_mut().write(0x0010, 0x06);
    cpu.step();
    assert_eq!(cpu.bus_mut().read(0x0010), 0x05);
    assert!(cpu.status.contains(StatusFlags::ZERO));
    assert!(cpu.status.contains(StatusFlags::CARRY));
}

#[test]
fn isb_increments_memory_then_subtracts_from_a() {
    let mut cpu = new_test_cpu();
    cpu.pc = 0x8000;
    cpu.a = 0x10;
    cpu.status.insert(StatusFlags::CARRY);
    load_program(&mut cpu, 0x8000, &[0xE7, 0x10]); // ISB $10
    cpu.bus_mut().write(0x0010, 0x04);
    cpu.step();
    assert_eq!(cpu.bus_mut().read(0x0010), 0x05);
    assert_eq!(cpu.a, 0x0B); // 0x10 - 0x05
}

#[test]
fn slo_shifts_left_then_ors_into_a() {
    let mut cpu = new_test_cpu();
    cpu.pc = 0x8000;
    cpu.a = 0x01;
    load_program(&mut cpu, 0x8000, &[0x07, 0x10]); // SLO $10
    cpu.bus_mut().write(0x0010, 0b1000_0001);
    cpu.step();
    assert_eq!(cpu.bus_mut().read(0x0010), 0b0000_0010);
    assert!(cpu.status.contains(StatusFlags::CARRY)); // 舊 bit7 是 1
    assert_eq!(cpu.a, 0b0000_0011);
}

#[test]
fn unofficial_sbc_eb_behaves_like_official_sbc() {
    let mut cpu = new_test_cpu();
    cpu.pc = 0x8000;
    cpu.a = 0x10;
    cpu.status.insert(StatusFlags::CARRY);
    load_program(&mut cpu, 0x8000, &[0xEB, 0x05]); // *SBC #$05
    cpu.step();
    assert_eq!(cpu.a, 0x0B);
}

#[test]
fn unofficial_nop_reads_operand_but_has_no_other_effect() {
    let mut cpu = new_test_cpu();
    cpu.pc = 0x8000;
    let a_before = 0x42;
    cpu.a = a_before;
    load_program(&mut cpu, 0x8000, &[0x04, 0x10]); // NOP $10（非官方，DP 定址）
    cpu.bus_mut().write(0x0010, 0x99);
    let cycles = cpu.step();
    assert_eq!(cycles, 3);
    assert_eq!(cpu.a, a_before);
    assert_eq!(cpu.pc, 0x8002);
}

#[test]
fn jam_opcode_halts_cpu_without_panicking() {
    let mut cpu = new_test_cpu();
    cpu.pc = 0x8000;
    load_program(&mut cpu, 0x8000, &[0x02]); // JAM
    cpu.step();
    assert!(cpu.jammed);
    let pc_after_jam = cpu.pc;

    for _ in 0..5 {
        let cycles = cpu.step();
        assert_eq!(cycles, JAM_CYCLES);
        assert_eq!(cpu.pc, pc_after_jam); // 不再推進
    }
}

#[test]
fn trace_does_not_mutate_cpu_state() {
    let mut cpu = new_test_cpu();
    cpu.pc = 0x8000;
    load_program(&mut cpu, 0x8000, &[0xA9, 0x42]); // LDA #$42
    let before = format!("{cpu:?}");
    let line = cpu.trace();
    let after = format!("{cpu:?}");
    assert_eq!(before, after);
    assert!(line.starts_with("8000  A9 42"));
}
