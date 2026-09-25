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
        rom_id: crate::cartridge::RomId::default(),
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

// ---- Phase 3：不穩定 opcode（SHY / SHX / LXA）與分段 catch-up ---------------

#[test]
fn shy_stores_y_and_h_plus_one_without_page_cross() {
    let mut cpu = new_test_cpu();
    cpu.pc = 0x8000;
    cpu.x = 1;
    cpu.y = 0xFF;
    load_program(&mut cpu, 0x8000, &[0x9C, 0x00, 0x01]); // SHY $0100,X -> $0101
    assert_eq!(cpu.step(), 5);
    // value = Y($FF) & (H($01) + 1) = 2
    assert_eq!(cpu.bus().peek(0x0101), 2);
}

#[test]
fn shy_page_cross_replaces_the_target_high_byte_with_the_value() {
    let mut cpu = new_test_cpu();
    cpu.pc = 0x8000;
    cpu.x = 1;
    cpu.y = 0x05;
    load_program(&mut cpu, 0x8000, &[0x9C, 0xFF, 0x02]); // SHY $02FF,X：一般會寫 $0300
    cpu.step();
    // value = Y & (H+1) = 5 & 3 = 1；跨頁 → 目標 = (1 << 8) | $00 = $0100。
    assert_eq!(cpu.bus().peek(0x0100), 1);
    assert_eq!(cpu.bus().peek(0x0300), 0, "沒有寫到原本的目標");
}

#[test]
fn shx_stores_x_and_h_plus_one_and_handles_page_cross() {
    let mut cpu = new_test_cpu();
    cpu.pc = 0x8000;
    cpu.y = 1;
    cpu.x = 0x05;
    load_program(&mut cpu, 0x8000, &[0x9E, 0xFF, 0x02]); // SHX $02FF,Y
    assert_eq!(cpu.step(), 5);
    assert_eq!(cpu.bus().peek(0x0100), 5 & 3);

    cpu.pc = 0x8010;
    cpu.y = 1;
    cpu.x = 0xFF;
    load_program(&mut cpu, 0x8010, &[0x9E, 0x10, 0x01]); // SHX $0110,Y -> $0111，不跨頁
    cpu.step();
    // value = X($FF) & (H($01) + 1) = 2
    assert_eq!(cpu.bus().peek(0x0111), 2);
}

#[test]
fn lxa_loads_the_immediate_into_a_and_x_with_magic_ff() {
    let mut cpu = new_test_cpu();
    cpu.pc = 0x8000;
    cpu.a = 0x12;
    load_program(&mut cpu, 0x8000, &[0xAB, 0x84]); // LXA #$84
    assert_eq!(cpu.step(), 2);
    assert_eq!((cpu.a, cpu.x), (0x84, 0x84));
    assert!(cpu.status.contains(StatusFlags::NEGATIVE));
    assert!(!cpu.status.contains(StatusFlags::ZERO));
}

/// 分段 catch-up：4 cycle 的 `LDA $2002` 在指令內的最後一個 cycle 讀取，所以讀取前
/// PPU 已經前進 3 個 CPU cycle（9 dot）。vblank 旗標在 scanline 241、dot 1 被處理的
/// 那個 tick 設起：從 (240, 334) 出發第 9 個 tick 剛好處理它，讀得到；從 (240, 333)
/// 出發要第 10 個 tick，讀不到。
#[test]
fn split_catch_up_makes_ppu_register_reads_see_the_dots_before_the_last_cycle() {
    for (start_dot, expect_vblank) in [(334u16, true), (333, false)] {
        let mut cpu = new_test_cpu();
        cpu.pc = 0x8000;
        cpu.bus_mut().ppu.scanline = 240;
        cpu.bus_mut().ppu.cycle = start_dot;
        load_program(&mut cpu, 0x8000, &[0xAD, 0x02, 0x20]); // LDA $2002
        assert_eq!(cpu.step(), 4);
        assert_eq!(
            cpu.a & 0x80 != 0,
            expect_vblank,
            "起始 dot {start_dot}：讀到的 vblank 旗標"
        );
    }
}

/// `step()` 回傳的 cycle 數與 `Bus` 累計的 cycle 數不因分段而改變（含跨頁與分支）。
#[test]
fn split_catch_up_keeps_total_cycles_identical() {
    let mut cpu = new_test_cpu();
    cpu.pc = 0x8000;
    cpu.x = 0xFF;
    // LDA $8001,X → $8100（跨頁 5，讀到非 0 讓 Z=0）；BNE +0 成立不跨頁（3）；NOP（2）。
    load_program(&mut cpu, 0x8000, &[0xBD, 0x01, 0x80, 0xD0, 0x00, 0xEA]);
    poke_prg(&mut cpu, 0x8100, 0x01);
    let before = cpu.bus().total_cycles();
    let spent: u64 = (0..3).map(|_| cpu.step() as u64).sum();
    assert_eq!(spent, 5 + 3 + 2);
    assert_eq!(cpu.bus().total_cycles() - before, spent);
}

/// RMW 指令對 `$8000+` 的寫入被拆成「舊值、緊接的新值」兩次；不需要 `consecutive` 的
/// mapper（NROM）看不出差別，最終沒有任何副作用。
#[test]
fn rmw_on_rom_space_is_a_harmless_no_op_for_nrom() {
    let mut cpu = new_test_cpu();
    cpu.pc = 0x8000;
    load_program(&mut cpu, 0x8000, &[0xEE, 0x00, 0x90]); // INC $9000
    poke_prg(&mut cpu, 0x9000, 0x41);
    assert_eq!(cpu.step(), 6);
    assert_eq!(cpu.bus().peek(0x9000), 0x41, "ROM 不可寫");
}

// ---- Phase 3.1：索引定址的 dummy read -----------------------------------------

/// 用 flat RAM 匯流排跑一條指令，回傳匯流排存取的順序 `(位址, 值, 是否為寫入)`。
fn access_log_of(program: &[u8], x: u8, y: u8, ram: &[(u16, u8)]) -> Vec<(u16, u8, bool)> {
    let mut cpu = Cpu::new(Bus::new_flat_ram_for_testing(new_test_cpu_cartridge()));
    cpu.pc = 0x0400;
    cpu.x = x;
    cpu.y = y;
    for (i, b) in program.iter().enumerate() {
        cpu.bus_mut().flat_ram_mut()[0x0400 + i] = *b;
    }
    for &(a, v) in ram {
        cpu.bus_mut().flat_ram_mut()[a as usize] = v;
    }
    cpu.step();
    cpu.bus_mut().take_access_log()
}

fn new_test_cpu_cartridge() -> Cartridge {
    new_test_cpu().bus().cartridge.clone()
}

#[test]
fn indexed_read_dummy_reads_the_uncorrected_address_only_when_the_page_is_crossed() {
    // LDA $10F0,X：X=$10 → $1100 跨頁；未修正位址 = $10 高位元組 + $00 = $1000。
    let crossed = access_log_of(&[0xBD, 0xF0, 0x10], 0x10, 0, &[(0x1100, 0x77)]);
    assert_eq!(
        crossed,
        [
            (0x0400, 0xBD, false),
            (0x0401, 0xF0, false),
            (0x0402, 0x10, false),
            (0x1000, 0, false), // dummy read（未修正位址）
            (0x1100, 0x77, false),
        ]
    );
    // X=$05 → $10F5 沒跨頁：只有真正的讀取。
    let same_page = access_log_of(&[0xBD, 0xF0, 0x10], 0x05, 0, &[]);
    assert_eq!(same_page.len(), 4);
    assert!(same_page.iter().all(|&(a, _, _)| a != 0x1000));
}

#[test]
fn indexed_store_always_dummy_reads_even_without_a_page_cross() {
    // STA $2000,X 的位址 `$2007`（X=7）沒跨頁，store 仍先讀一次 `$2007`，再寫。
    let log = access_log_of(&[0x9D, 0x00, 0x20], 0x07, 0, &[]);
    assert_eq!(&log[3..], [(0x2007, 0, false), (0x2007, 0, true)]);
    // (ind),Y 的 store：STA ($20),Y，指標 $20/$21 = $10F0，Y=$10 → 跨頁，未修正 $1000。
    let log = access_log_of(&[0x91, 0x20], 0, 0x10, &[(0x20, 0xF0), (0x21, 0x10)]);
    assert!(log.contains(&(0x1000, 0, false)), "{log:04X?}");
    assert!(log.last().unwrap().2);
}

/// RMW（`INC abs,X`）的存取順序：dummy read（未修正位址）→ 真正的讀取 → 寫回舊值
/// （dummy write）→ 寫入新值。
#[test]
fn rmw_indexed_access_order_is_dummy_read_then_read_then_old_then_new_write() {
    let log = access_log_of(&[0xFE, 0xF0, 0x10], 0x10, 0, &[(0x1100, 0x41)]);
    assert_eq!(
        &log[3..],
        [
            (0x1000, 0, false), // dummy read（未修正）
            (0x1100, 0x41, false),
            (0x1100, 0x41, true), // dummy write：舊值
            (0x1100, 0x42, true), // 新值
        ]
    );
}

/// 其他定址模式（zero page 索引、implied……）不模擬 dummy read：存取次數與位址不變。
#[test]
fn zero_page_indexed_and_implied_have_no_dummy_read() {
    let log = access_log_of(&[0xB5, 0x80], 0x05, 0, &[(0x85, 0x12)]); // LDA $80,X
    assert_eq!(log.len(), 3); // opcode、operand、資料
    assert_eq!(log[2], (0x0085, 0x12, false));
    let log = access_log_of(&[0xEA], 0, 0, &[]); // NOP
    assert_eq!(log.len(), 1);
}

/// 真的碰到 I/O 暫存器：`LDA $20F8,X`（X=$0F → `$2107`，跨頁）先 dummy read `$2007`，
/// 使 PPU 位址多前進一次；不跨頁的 `LDA $2000,X`（X=7）只讀一次。
#[test]
fn dummy_read_advances_the_ppu_address_through_2007() {
    let mut cpu = new_test_cpu();
    cpu.pc = 0x8000;
    cpu.x = 0x0F;
    load_program(&mut cpu, 0x8000, &[0xBD, 0xF8, 0x20]);
    cpu.bus_mut().ppu.v = 0x0000;
    cpu.step();
    // dummy read $2007 + 真正讀 $2107（也是 $2007 的鏡像）＝ 前進 2。
    assert_eq!(cpu.bus().ppu.v, 2);

    let mut cpu = new_test_cpu();
    cpu.pc = 0x8000;
    cpu.x = 0x07;
    load_program(&mut cpu, 0x8000, &[0xBD, 0x00, 0x20]); // LDA $2000,X = $2007，不跨頁
    cpu.bus_mut().ppu.v = 0x0000;
    cpu.step();
    assert_eq!(cpu.bus().ppu.v, 1);

    // STA $2000,X（X=7）：dummy read 使 v 前進 1，接著寫入 $2007 再前進 1。
    let mut cpu = new_test_cpu();
    cpu.pc = 0x8000;
    cpu.x = 0x07;
    load_program(&mut cpu, 0x8000, &[0x9D, 0x00, 0x20]);
    cpu.bus_mut().ppu.v = 0x0000;
    cpu.step();
    assert_eq!(cpu.bus().ppu.v, 2);
}

/// 對 PPU 暫存器的 RMW 現在也寫兩次（舊值、新值）：`INC $2006`（$2006 唯寫，讀到 latch）。
#[test]
fn rmw_on_a_ppu_register_writes_twice() {
    let mut cpu = new_test_cpu();
    cpu.pc = 0x8000;
    load_program(&mut cpu, 0x8000, &[0xEE, 0x06, 0x20]); // INC $2006
    cpu.bus_mut().ppu.w = false;
    cpu.step();
    // 兩次寫入 $2006 → 第二次寫完成一組位址，w 回到 false。
    assert!(!cpu.bus().ppu.w);
}
