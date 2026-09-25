//! APU 的單元測試與整合測試（走 `Bus` / `Cpu`，涵蓋 CPU 這一側的 IRQ 行為）。

use super::*;
use crate::bus::Bus;
use crate::cartridge::Cartridge;
use crate::cpu::{Cpu, StatusFlags};
use crate::test_support::{ApuProbe, Asm, apu_probe_rom, build_nrom, build_nrom_irq};
use crate::{Buttons, Nes, StateError};

fn cartridge_of(rom: &[u8]) -> Cartridge {
    Cartridge::from_ines(rom).unwrap()
}

/// 只會 `JMP` 自己的 ROM 上的匯流排。
fn idle_bus() -> Bus {
    let mut a = Asm::new(0x8000);
    let l = a.pc();
    a.jmp(l);
    Bus::new(cartridge_of(&build_nrom(&a, 0x8000, None, &[], &[], false)))
}

/// 有 DMC 取樣資料（`$C000` 起）的匯流排。
fn dmc_bus() -> Bus {
    Bus::new(cartridge_of(&apu_probe_rom(ApuProbe::DEFAULT)))
}

fn run(bus: &mut Bus, cycles: u32) {
    let mut left = cycles;
    while left > 0 {
        let n = left.min(200);
        bus.tick(n as u8);
        left -= n;
    }
}

const ONE_SECOND: u32 = 1_789_773;

// ---- 長度計數器與 $4015 -------------------------------------------------------

#[test]
fn length_counter_loads_every_table_entry() {
    for index in 0..32u8 {
        let mut bus = idle_bus();
        bus.write(0x4015, 0x01);
        bus.write(0x4000, 0x00);
        bus.write(0x4003, index << 3);
        run(&mut bus, 3); // 載入在寫入之後的下一個 cycle 結束時生效
        assert_eq!(
            bus.apu.pulse[0].length.counter, LENGTH_TABLE[index as usize],
            "索引 {index}"
        );
    }
}

#[test]
fn disabled_channel_ignores_length_loads_and_the_counter_is_cleared() {
    let mut bus = idle_bus();
    bus.write(0x4015, 0x01);
    bus.write(0x4003, 0x08);
    run(&mut bus, 3);
    assert_eq!(bus.peek(0x4015) & 0x01, 1);

    bus.write(0x4015, 0x00);
    assert_eq!(bus.peek(0x4015) & 0x01, 0, "停用立即清掉長度計數器");
    bus.write(0x4003, 0x08);
    run(&mut bus, 3);
    assert_eq!(bus.peek(0x4015) & 0x01, 0, "停用時不能載入");
}

#[test]
fn status_register_reports_each_channel_and_bit5_is_open_bus() {
    let mut bus = idle_bus();
    bus.write(0x4015, 0x0F);
    for reg in [0x4003, 0x4007, 0x400B, 0x400F] {
        bus.write(reg, 0x08);
    }
    run(&mut bus, 3);
    // bit 5 來自 open bus：先讓匯流排上留 $FF 再讀。
    bus.write(0x0000, 0xFF);
    let _ = bus.read(0x0000);
    assert_eq!(bus.read(0x4015) & 0x3F, 0x2F);
    bus.write(0x0000, 0x00);
    let _ = bus.read(0x0000);
    assert_eq!(bus.read(0x4015) & 0x3F, 0x0F);
}

#[test]
fn reading_4015_does_not_change_the_open_bus_latch() {
    let mut bus = idle_bus();
    bus.write(0x0000, 0x77);
    let _ = bus.read(0x0000);
    let _ = bus.read(0x4015);
    assert_eq!(
        bus.read(0x5000),
        0x77,
        "$4015 是內部暫存器，不會驅動外部匯流排"
    );
}

// ---- frame counter ----------------------------------------------------------

#[test]
fn frame_irq_sets_reads_clear_and_inhibit_blocks_it() {
    let mut bus = idle_bus();
    bus.write(0x4017, 0x00);
    run(&mut bus, 29_800);
    assert!(!bus.apu.irq(), "29830 cycle 之前不該有 frame IRQ");
    run(&mut bus, 100);
    assert!(bus.apu.irq());
    assert_eq!(bus.peek(0x4015) & 0x40, 0x40);
    assert_eq!(bus.peek(0x4015) & 0x40, 0x40, "peek 沒有副作用");

    let status = bus.read(0x4015);
    assert_eq!(status & 0x40, 0x40);
    assert!(!bus.apu.irq(), "讀 $4015 清掉旗標");

    run(&mut bus, 30_000);
    assert!(bus.apu.irq(), "下一幀又會設起來");
    bus.write(0x4017, 0x40);
    assert!(!bus.apu.irq(), "寫入 IRQ 抑制立即清旗標");
    run(&mut bus, 90_000);
    assert!(!bus.apu.irq());

    bus.write(0x4017, 0x80); // 5 步模式沒有 frame IRQ
    run(&mut bus, 90_000);
    assert!(!bus.apu.irq());
}

#[test]
fn writing_4017_with_bit7_clocks_the_length_counter_immediately_and_zero_does_not() {
    let mut bus = idle_bus();
    bus.write(0x4015, 0x01);
    bus.write(0x4000, 0x00);
    bus.write(0x4003, 0x08); // 254
    run(&mut bus, 3);
    assert_eq!(bus.apu.pulse[0].length.counter, 254);

    bus.write(0x4017, 0x00);
    run(&mut bus, 8);
    assert_eq!(bus.apu.pulse[0].length.counter, 254, "寫 $00 不會立即時脈");

    bus.write(0x4017, 0x80);
    run(&mut bus, 8);
    assert_eq!(
        bus.apu.pulse[0].length.counter, 253,
        "寫 $80 立即 half-frame 時脈"
    );
}

#[test]
fn length_counter_counts_down_at_half_frames_and_halt_freezes_it() {
    let mut bus = idle_bus();
    bus.write(0x4015, 0x03);
    bus.write(0x4000, 0x00); // 不 halt
    bus.write(0x4003, 0x18); // 索引 3 → 2
    bus.write(0x4004, 0x20); // halt
    bus.write(0x4007, 0x18);
    run(&mut bus, 3);
    assert_eq!(bus.peek(0x4015) & 3, 3);
    run(&mut bus, 14_920); // 第一個 half-frame（14913）：2 → 1
    assert_eq!(bus.peek(0x4015) & 3, 3);
    run(&mut bus, 15_000); // 第二個 half-frame（29829）：1 → 0
    assert_eq!(
        bus.peek(0x4015) & 3,
        2,
        "pulse 1 已經歸零，halt 的 pulse 2 還在"
    );
}

// ---- 各聲道 -----------------------------------------------------------------

#[test]
fn sweep_negate_is_ones_complement_on_pulse1_and_twos_complement_on_pulse2() {
    let mut p1 = Pulse::new(true);
    let mut p2 = Pulse::new(false);
    for p in [&mut p1, &mut p2] {
        p.write_register(2, 0x00);
        p.write_register(3, 0x01); // 週期 $100
        p.write_register(1, 0x80 | 0x08 | 0x01); // 啟用、negate、shift 1
        p.tick_sweep();
    }
    assert_eq!(p1.timer_period, 0x100 - 0x80 - 1);
    assert_eq!(p2.timer_period, 0x100 - 0x80);
}

#[test]
fn sweep_raises_the_period_and_mutes_on_overflow() {
    let mut p = Pulse::new(false);
    p.write_register(2, 0x00);
    p.write_register(3, 0x01);
    p.write_register(1, 0x80 | 0x01); // 啟用、往上、shift 1
    p.tick_sweep();
    assert_eq!(p.timer_period, 0x100 + 0x80);

    // 週期 $7FF 加上任何位移都超過 $7FF：即使 sweep 關閉也會靜音。
    let mut m = Pulse::new(false);
    m.write_register(0, 0xBF);
    m.set_enabled(true);
    m.write_register(2, 0xFF);
    m.write_register(3, 0x0F); // 週期 $7FF、長度索引 1
    m.length.apply_pending();
    assert!(m.length.counter > 0);
    for _ in 0..16 {
        m.step_sequencer();
        assert_eq!(m.output(), 0);
    }
}

#[test]
fn pulse_is_muted_below_period_8_and_sounds_above() {
    let mut p = Pulse::new(false);
    p.write_register(0, 0xBF); // duty 2、halt、固定音量 15
    p.set_enabled(true);
    p.write_register(2, 0x04);
    p.write_register(3, 0x08);
    p.length.apply_pending();
    for _ in 0..16 {
        p.step_sequencer();
        assert_eq!(p.output(), 0, "週期 < 8 靜音");
    }
    p.write_register(2, 0x40);
    let mut outputs = Vec::new();
    for _ in 0..16 {
        p.step_sequencer();
        outputs.push(p.output());
    }
    assert!(outputs.contains(&15) && outputs.contains(&0));
}

#[test]
fn envelope_decays_by_one_every_period_plus_one_ticks_and_can_loop() {
    let mut e = Envelope::default();
    e.write(0x02); // 週期 2、不 loop、非固定音量
    e.restart();
    e.tick();
    assert_eq!(e.output(), 15);
    for expected in (0..15).rev() {
        for _ in 0..3 {
            e.tick();
        }
        assert_eq!(e.output(), expected);
    }
    for _ in 0..30 {
        e.tick();
    }
    assert_eq!(e.output(), 0, "不 loop 時停在 0");

    e.write(0x22); // loop
    for _ in 0..3 {
        e.tick();
    }
    assert_eq!(e.output(), 15, "loop 時回到 15");

    e.write(0x1A); // 固定音量 10
    assert_eq!(e.output(), 10);
}

#[test]
fn triangle_walks_the_32_step_sequence_only_while_both_counters_are_nonzero() {
    let mut t = Triangle::new();
    t.set_enabled(true);
    t.write_register(0, 0xFF); // control、reload 127
    t.write_register(2, 0x10);
    t.write_register(3, 0x08);
    t.length.apply_pending();
    t.tick_linear();
    assert_eq!(t.linear_counter, 0x7F);

    let mut seen = vec![t.output()];
    for _ in 0..31 {
        t.step_sequencer();
        seen.push(t.output());
    }
    let expected: Vec<u8> = (0..=15).rev().chain(0..=15).collect();
    assert_eq!(seen, expected);

    // linear counter 為 0：序列停住。
    let mut z = Triangle::new();
    z.set_enabled(true);
    z.write_register(0, 0x00); // 不 control、reload 0
    z.write_register(3, 0x08);
    z.length.apply_pending();
    z.tick_linear();
    assert_eq!(z.linear_counter, 0);
    let before = z.seq;
    z.step_sequencer();
    assert_eq!(z.seq, before);
}

#[test]
fn noise_lfsr_has_the_documented_periods() {
    let mut long = Noise::new();
    let mut steps = 0;
    loop {
        long.step_shift_register();
        steps += 1;
        if long.shift == 1 {
            break;
        }
    }
    assert_eq!(steps, 32767, "長模式週期");

    let mut short = Noise::new();
    short.write_register(2, 0x80);
    let mut steps = 0;
    loop {
        short.step_shift_register();
        steps += 1;
        if short.shift == 1 {
            break;
        }
    }
    assert_eq!(steps, 93, "短模式週期");
}

#[test]
fn noise_and_dmc_rate_tables_are_ntsc() {
    assert_eq!(NOISE_PERIOD_TABLE[0], 4);
    assert_eq!(NOISE_PERIOD_TABLE[15], 4068);
    assert_eq!(DMC_RATE_TABLE[0], 428);
    assert_eq!(DMC_RATE_TABLE[15], 54);
}

// ---- DMC --------------------------------------------------------------------

#[test]
fn dmc_direct_load_sets_the_output_level() {
    let mut bus = idle_bus();
    bus.write(0x4011, 0xFF);
    assert_eq!(bus.apu.dmc.output_level, 0x7F);
    assert_eq!(bus.apu.debug().dmc.output_level, 0x7F);
}

#[test]
fn dmc_fetch_stalls_the_cpu_and_raises_an_irq_at_the_end_of_the_sample() {
    let mut bus = dmc_bus();
    bus.write(0x4010, 0x8F); // IRQ 致能、最快
    bus.write(0x4012, 0x00); // $C000
    bus.write(0x4013, 0x00); // 1 byte
    bus.write(0x4015, 0x10);
    assert!(!bus.apu.irq());

    let before = bus.total_cycles();
    bus.tick(10);
    assert_eq!(
        bus.total_cycles(),
        before + 10 + u64::from(DMC_STALL_CYCLES),
        "抓一個 byte 暫停 CPU {DMC_STALL_CYCLES} 個 cycle"
    );
    assert!(bus.apu.irq(), "取樣抓完（不 loop）就設 DMC IRQ");
    assert_eq!(bus.peek(0x4015) & 0x80, 0x80);
    assert_eq!(bus.peek(0x4015) & 0x10, 0, "bytes remaining = 0");

    bus.write(0x4015, 0x00);
    assert!(!bus.apu.irq(), "寫 $4015 清 DMC IRQ");
}

#[test]
fn dmc_loop_restarts_the_sample_without_an_irq_and_keeps_stalling() {
    let mut bus = dmc_bus();
    bus.write(0x4010, 0x4F); // loop、rate 15（54 cycle／bit）
    bus.write(0x4012, 0x00);
    bus.write(0x4013, 0x00);
    bus.write(0x4015, 0x10);
    let start = bus.total_cycles();
    run(&mut bus, 3000);
    let stalled = bus.total_cycles() - start - 3000;
    assert_eq!(stalled % u64::from(DMC_STALL_CYCLES), 0);
    let fetches = stalled / u64::from(DMC_STALL_CYCLES);
    assert!(
        (5..=9).contains(&fetches),
        "3000 cycle 約抓 7 個 byte，實際 {fetches}"
    );
    assert!(!bus.apu.irq(), "loop 時沒有 IRQ");
}

#[test]
fn dmc_output_follows_the_sample_bits() {
    let mut bus = dmc_bus();
    bus.write(0x4011, 0x40);
    bus.write(0x4010, 0x4F);
    bus.write(0x4012, 0x00);
    bus.write(0x4013, 0x00);
    bus.write(0x4015, 0x10);
    let mut levels = std::collections::BTreeSet::new();
    for _ in 0..200 {
        run(&mut bus, 54);
        levels.insert(bus.apu.dmc.output_level);
    }
    assert!(levels.len() >= 3, "輸出電平隨取樣資料上下移動：{levels:?}");
    assert!(levels.iter().any(|l| *l < 0x40) && levels.iter().any(|l| *l > 0x40));
    assert!(levels.iter().all(|l| *l < 128));
}

// ---- 計時的一致性 -------------------------------------------------------------

#[test]
fn apu_cycle_count_stays_in_step_with_the_bus_across_register_accesses() {
    let mut bus = dmc_bus();
    let regs = [0x4000u16, 0x4003, 0x4008, 0x400F, 0x4010, 0x4015, 0x4017];
    for (i, reg) in regs.iter().cycle().take(200).enumerate() {
        if i % 3 == 0 {
            let _ = bus.read(0x4015);
        } else {
            bus.write(*reg, (i * 7) as u8);
        }
        bus.tick(1 + (i % 5) as u8);
        assert_eq!(bus.apu.cycles, bus.total_cycles(), "第 {i} 次存取之後");
        assert_eq!(bus.apu.ahead, 0);
    }
}

#[test]
fn default_apu_state_is_structurally_valid_and_debug_reflects_registers() {
    let mut bus = idle_bus();
    assert!(bus.apu.is_structurally_valid());
    bus.write(0x4015, 0x01);
    bus.write(0x4000, 0xBF);
    bus.write(0x4002, 0x34);
    bus.write(0x4003, 0x0A);
    bus.write(0x4017, 0x40);
    run(&mut bus, 10);
    let d = bus.apu.debug();
    assert!(d.pulse[0].enabled);
    assert_eq!(d.pulse[0].duty, 2);
    assert_eq!(d.pulse[0].timer_period, 0x234);
    assert!(d.pulse[0].halt && d.pulse[0].constant);
    assert_eq!(d.pulse[0].volume, 15);
    assert!(d.frame_inhibit_irq && !d.frame_mode5);
    assert_eq!(d.status & 1, 1);
    assert!(bus.apu.is_structurally_valid());
}

// ---- CPU 的 IRQ：level-triggered、I 旗標與 CLI/SEI/PLP 的延遲 ------------------------

fn cpu_with(program: &Asm) -> Cpu {
    let mut handler = Asm::new(0x8100);
    handler.lda_imm(0x77);
    let l = handler.pc();
    handler.jmp(l);
    let rom = build_nrom_irq(
        program,
        0x8000,
        None,
        Some(0x8100),
        &[(0x8100, &handler.bytes)],
        &[],
        false,
    );
    let mut cpu = Cpu::new(Bus::new(cartridge_of(&rom)));
    cpu.pc = 0x8000;
    cpu
}

/// 讓 APU 拉起 frame IRQ 線（直接設旗標，省得等 29830 個 cycle）。
fn assert_irq_line(cpu: &mut Cpu) {
    cpu.bus_mut().apu.frame_irq = true;
    assert!(cpu.bus().irq_line());
}

#[test]
fn cli_delays_a_pending_irq_by_one_instruction() {
    let mut a = Asm::new(0x8000);
    a.cli().inx().inx().inx();
    let mut cpu = cpu_with(&a);
    assert!(cpu.status.contains(StatusFlags::INTERRUPT));
    assert_irq_line(&mut cpu);

    cpu.step(); // CLI
    assert!(!cpu.status.contains(StatusFlags::INTERRUPT));
    assert_eq!(cpu.pc, 0x8001);
    cpu.step(); // INX：IRQ 已經在等，但 CLI 的效果要晚一條指令
    assert_eq!((cpu.x, cpu.pc), (1, 0x8002));
    let sp = cpu.sp;
    assert_eq!(cpu.step(), 7, "接著才服務 IRQ");
    assert_eq!(cpu.pc, 0x8100);
    assert_eq!(cpu.sp, sp.wrapping_sub(3));
    assert!(cpu.status.contains(StatusFlags::INTERRUPT));
}

#[test]
fn sei_does_not_stop_an_irq_that_was_already_pending() {
    let mut a = Asm::new(0x8000);
    a.sei().inx().inx();
    let mut cpu = cpu_with(&a);
    cpu.status.remove(StatusFlags::INTERRUPT);
    assert_irq_line(&mut cpu);

    cpu.step(); // SEI
    assert_eq!(cpu.pc, 0x8001);
    assert_eq!(cpu.step(), 7, "SEI 的效果晚一條指令：這一步仍然服務 IRQ");
    assert_eq!((cpu.pc, cpu.x), (0x8100, 0));
}

#[test]
fn plp_that_sets_i_still_lets_one_pending_irq_through() {
    let mut a = Asm::new(0x8000);
    a.lda_imm(0x24).pha();
    a.bytes.push(0x28); // PLP
    a.inx();
    let mut cpu = cpu_with(&a);
    cpu.status.remove(StatusFlags::INTERRUPT);

    cpu.step(); // LDA
    cpu.step(); // PHA
    assert_irq_line(&mut cpu); // IRQ 線在 PLP 之前才拉高（否則 LDA 之後就被服務了）
    cpu.step(); // PLP：I 變成 1
    assert!(cpu.status.contains(StatusFlags::INTERRUPT));
    assert_eq!(cpu.step(), 7);
    assert_eq!((cpu.pc, cpu.x), (0x8100, 0));
}

#[test]
fn irq_line_is_level_triggered_and_clearing_the_source_cancels_it() {
    let mut a = Asm::new(0x8000);
    a.lda_abs(0x4015).cli().inx().inx().inx();
    let mut cpu = cpu_with(&a);
    assert_irq_line(&mut cpu);
    for _ in 0..5 {
        cpu.step();
    }
    assert_eq!(cpu.x, 3, "讀 $4015 已經清掉旗標，CLI 之後沒有 IRQ");
    assert_ne!(cpu.pc, 0x8100);

    // 只要線一直是高的、I = 0，就會反覆服務（處理常式不確認）。
    let mut b = Asm::new(0x8000);
    b.cli().inx().inx();
    let mut cpu = cpu_with(&b);
    assert_irq_line(&mut cpu);
    let mut serviced = 0;
    for _ in 0..12 {
        if cpu.step() == 7 {
            serviced += 1;
        }
    }
    assert_eq!(
        serviced, 1,
        "服務一次之後 I = 1，處理常式在原地跑，不會重入"
    );
}

#[test]
fn an_irq_is_not_taken_while_the_i_flag_is_set() {
    let mut a = Asm::new(0x8000);
    a.inx().inx().inx().inx();
    let mut cpu = cpu_with(&a);
    assert_irq_line(&mut cpu);
    for _ in 0..4 {
        assert_eq!(cpu.step(), 2);
    }
    assert_eq!(cpu.x, 4);
}

#[test]
fn frame_irq_reaches_the_cpu_through_the_real_apu() {
    // 不手動設旗標：$4017 = 0、CLI，等真正的 frame IRQ。
    let mut a = Asm::new(0x8000);
    a.lda_imm(0x00).sta_abs(0x4017).cli();
    let l = a.pc();
    a.inx().jmp(l);
    let mut cpu = cpu_with(&a);
    let mut cycles = 0u64;
    while cpu.pc != 0x8100 && cycles < 40_000 {
        cycles += u64::from(cpu.step());
    }
    assert_eq!(cpu.pc, 0x8100);
    assert!(
        (29_800..=29_860).contains(&cycles),
        "在 {cycles} cycle 進入 IRQ 處理常式"
    );
}

// ---- 存檔 -------------------------------------------------------------------

fn probe_nes() -> Nes {
    Nes::from_rom(&apu_probe_rom(ApuProbe::DEFAULT)).unwrap()
}

#[test]
fn probe_rom_takes_frame_and_dmc_irqs() {
    let mut nes = probe_nes();
    for _ in 0..30 {
        nes.run_frame([Buttons::empty(); 2]);
    }
    assert!(
        nes.peek(0x0002) >= 30,
        "IRQ 處理常式被呼叫 {} 次",
        nes.peek(0x0002)
    );
    assert!(nes.peek(0x0000) > 0, "主迴圈有在跑");
}

#[test]
fn apu_state_round_trips_through_save_state_and_replays_identically() {
    let mut a = probe_nes();
    for _ in 0..30 {
        a.run_frame([Buttons::empty(); 2]);
    }
    let saved = a.save_state();
    let mut b = probe_nes();
    b.load_state(&saved).unwrap();
    assert_eq!(a.state_hash(), b.state_hash());
    for frame in 0..20 {
        a.run_frame([Buttons::empty(); 2]);
        b.run_frame([Buttons::empty(); 2]);
        assert_eq!(a.state_hash(), b.state_hash(), "重播第 {frame} 幀");
    }
}

#[test]
fn apu_registers_are_part_of_the_state_hash() {
    let mut a = probe_nes();
    let mut b = probe_nes();
    for _ in 0..5 {
        a.run_frame([Buttons::empty(); 2]);
        b.run_frame([Buttons::empty(); 2]);
    }
    assert_eq!(a.state_hash(), b.state_hash());
    b.cpu.bus_mut().apu.pulse[1].envelope.volume ^= 1;
    assert_ne!(a.state_hash(), b.state_hash());
}

#[test]
fn tampered_apu_state_is_rejected_by_load_state() {
    type Tamper = fn(&mut Apu);
    let tampers: [(&str, Tamper); 7] = [
        ("pulse.seq", |a| a.pulse[0].seq = 9),
        ("triangle.seq", |a| a.triangle.seq = 40),
        ("noise.period_index", |a| a.noise.period_index = 16),
        ("dmc.bits_remaining", |a| a.dmc.bits_remaining = 0),
        ("frame.step", |a| a.frame.step = 6),
        ("frame.cycle", |a| a.frame.cycle = 60_000),
        ("pulse.timer_period", |a| a.pulse[1].timer_period = 0x800),
    ];
    for (name, tamper) in tampers {
        let mut source = probe_nes();
        tamper(&mut source.cpu.bus_mut().apu);
        let bytes = source.save_state();
        let mut target = probe_nes();
        assert!(
            matches!(target.load_state(&bytes), Err(StateError::Corrupt)),
            "{name} 被竄改的存檔必須被拒絕"
        );
    }
}

// ---- 輸出管線 ---------------------------------------------------------------

#[test]
fn output_has_the_expected_sample_count_and_can_be_switched_off() {
    let mut bus = idle_bus();
    bus.apu.set_sample_rate(44_100.0);
    run(&mut bus, ONE_SECOND);
    let mut samples = Vec::new();
    bus.apu.take_samples(&mut samples);
    assert!(
        (samples.len() as i64 - 44_100).abs() <= 2,
        "{} 個取樣",
        samples.len()
    );

    bus.apu.set_output_enabled(false);
    run(&mut bus, ONE_SECOND / 4);
    bus.apu.take_samples(&mut samples);
    assert_eq!(bus.apu.buffered_samples(), 0);
    let after_off = samples.len();
    bus.apu.take_samples(&mut samples);
    assert_eq!(samples.len(), after_off, "關閉輸出之後不再產生取樣");
}

/// 音高：pulse 週期暫存器 t → 頻率 = 1789773 / (16 × (t + 1))。t = 253 ≈ 440.4 Hz。
#[test]
fn pulse_pitch_matches_the_period_register() {
    for (period, rate) in [
        (253u16, 44_100.0),
        (253, 48_000.0),
        (126, 48_000.0),
        (500, 44_100.0),
    ] {
        let mut bus = idle_bus();
        bus.apu.set_sample_rate(rate);
        bus.write(0x4015, 0x01);
        bus.write(0x4000, 0xBF); // duty 2（50%）、halt、固定音量 15
        bus.write(0x4002, period as u8);
        bus.write(0x4003, (period >> 8) as u8);
        run(&mut bus, ONE_SECOND / 10); // 讓濾波器穩定
        let mut discard = Vec::new();
        bus.apu.take_samples(&mut discard);
        run(&mut bus, ONE_SECOND);
        let mut samples = Vec::new();
        bus.apu.take_samples(&mut samples);

        let rising = samples
            .windows(2)
            .filter(|w| w[0] < 0.0 && w[1] >= 0.0)
            .count();
        let expected = CPU_CLOCK_HZ / (16.0 * (f64::from(period) + 1.0));
        assert!(
            (rising as f64 - expected).abs() <= 2.0,
            "週期 {period} @ {rate} Hz：預期 {expected:.1} Hz，量到 {rising}"
        );
        let peak = samples.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        assert!((0.05..1.0).contains(&peak), "振幅 {peak}");
    }
}

#[test]
fn changing_the_sample_rate_changes_only_the_output() {
    let mut a = idle_bus();
    let mut b = idle_bus();
    for bus in [&mut a, &mut b] {
        bus.write(0x4015, 0x0F);
        bus.write(0x4000, 0xBF);
        bus.write(0x4002, 0x40);
        bus.write(0x4003, 0x08);
    }
    b.apu.set_sample_rate(48_000.0 * 1.005);
    run(&mut a, 200_000);
    run(&mut b, 200_000);
    let mut sa = Vec::new();
    let mut sb = Vec::new();
    a.apu.take_samples(&mut sa);
    b.apu.take_samples(&mut sb);
    assert!(sb.len() > sa.len(), "取樣率高 0.5% → 取樣數多");
    assert_eq!(
        crate::state::encode(&a.apu),
        crate::state::encode(&b.apu),
        "取樣率不影響模擬狀態"
    );
}

#[test]
fn channel_mask_silences_a_channel_without_touching_state() {
    let mut a = idle_bus();
    let mut b = idle_bus();
    b.apu.set_channel_mask(ALL_CHANNELS & !CHANNEL_PULSE1);
    for bus in [&mut a, &mut b] {
        bus.write(0x4015, 0x01);
        bus.write(0x4000, 0xBF);
        bus.write(0x4002, 0x40);
        bus.write(0x4003, 0x08);
        run(bus, 100_000);
    }
    let (mut sa, mut sb) = (Vec::new(), Vec::new());
    a.apu.take_samples(&mut sa);
    b.apu.take_samples(&mut sb);
    assert!(sa.iter().any(|s| s.abs() > 0.05));
    // 三角波沒在跑但序列停在 15：有一個直流偏移，剛開始有 high-pass 的瞬態，取後半段。
    assert!(sb[sb.len() / 2..].iter().all(|s| s.abs() < 1e-6));
    assert_eq!(crate::state::encode(&a.apu), crate::state::encode(&b.apu));
}

// ---- 事件驅動與批次前進必須與「逐 cycle」完全等價 ------------------------------------

/// 最簡單、顯然正確的模型是「一個 cycle 一個 cycle 走」。`Apu::step` 為了效率把區間切成事件
/// 之間的 chunk、並對聽不到的聲道用算術批次前進；這個測試用固定種子的偽隨機暫存器寫入與隨機
/// 長度的區間，比對兩者的完整狀態（含 DMC 暫停累積量）：輸出開啟與關閉都要相同。
#[test]
fn chunked_stepping_is_equivalent_to_stepping_one_cycle_at_a_time() {
    fn next(state: &mut u64) -> u64 {
        *state ^= *state << 13;
        *state ^= *state >> 7;
        *state ^= *state << 17;
        *state
    }
    let cart = cartridge_of(&apu_probe_rom(ApuProbe::DEFAULT));
    let regs: [u16; 24] = [
        0x4000, 0x4001, 0x4002, 0x4003, 0x4004, 0x4005, 0x4006, 0x4007, 0x4008, 0x400A, 0x400B,
        0x400C, 0x400E, 0x400F, 0x4010, 0x4011, 0x4012, 0x4013, 0x4015, 0x4015, 0x4015, 0x4017,
        0x4003, 0x400B,
    ];
    for output in [true, false] {
        for seed in 1..=12u64 {
            let mut rng = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
            let mut chunked = Apu::new();
            let mut single = Apu::new();
            chunked.set_output_enabled(output);
            single.set_output_enabled(output);
            for round in 0..400 {
                let r = next(&mut rng);
                let reg = regs[(r % regs.len() as u64) as usize];
                let value = (r >> 8) as u8;
                for apu in [&mut chunked, &mut single] {
                    apu.sync(&cart);
                    if reg == 0x4015 && value & 0x40 != 0 {
                        let _ = apu.read_status();
                    }
                    apu.write_register(reg, value);
                }
                // 區間長度：多半很短，偶爾很長（跨過 frame counter 的好幾個步驟）。
                let cycles = match (r >> 20) % 8 {
                    0 => 1 + (r >> 24) % 5_000,
                    _ => 1 + (r >> 24) % 60,
                } as u32;
                chunked.step(cycles, &cart);
                for _ in 0..cycles {
                    single.step(1, &cart);
                }
                assert_eq!(
                    crate::state::encode(&chunked),
                    crate::state::encode(&single),
                    "output={output} seed={seed} 第 {round} 輪（寫 {reg:#06X} = {value:#04X}，{cycles} cycle）"
                );
            }
        }
    }
}

// ---- DMC 啟用延遲的奇偶差異 ------------------------------------------------------------

/// 寫 `$4015` 啟用 DMC 之後，第一次抓取樣本要等 2 個 cycle（寫入落在偶數 CPU cycle）或 3 個
/// （奇數）。在 `parity_odd` 指定的奇偶下寫入，回傳（寫入後 `start_delay`、要 `tick(1)` 幾次
/// 才看到緩衝區被填入）。
fn dmc_start_after_enable(parity_odd: bool) -> (u8, u32) {
    let mut bus = dmc_bus();
    bus.write(0x4010, 0x0F); // 最慢的速率、無 IRQ
    bus.write(0x4012, 0x00);
    bus.write(0x4013, 0x01); // 17 bytes：抓完第一個 byte 之後還有剩
    bus.tick(1); // 先讓前面的寫入「存取前先跑的 cycle」被消化掉（`ahead` 歸零）
    // 寫入時 APU 的 cycle 計數（`write_register` 看到的，已含存取前先跑的那個 cycle）的奇偶。
    while ((bus.apu.cycles + 1) & 1 == 1) != parity_odd {
        bus.tick(1);
    }
    bus.write(0x4015, 0x10);
    let delay = bus.apu.dmc.start_delay;
    assert!(bus.apu.dmc.buffer_empty);
    let mut ticks = 0;
    while bus.apu.dmc.buffer_empty {
        bus.tick(1);
        ticks += 1;
        assert!(ticks < 20, "DMC 沒有抓取樣本");
    }
    (delay, ticks)
}

#[test]
fn dmc_start_delay_is_two_cycles_on_even_writes_and_three_on_odd_writes() {
    let (even_delay, even_ticks) = dmc_start_after_enable(false);
    let (odd_delay, odd_ticks) = dmc_start_after_enable(true);
    assert_eq!((even_delay, odd_delay), (2, 3), "啟用延遲");
    // 存取前先跑的那個 cycle 讓第一次 `tick(1)` 什麼都不做（見 §17.2），所以實際要 delay + 1 次。
    assert_eq!((even_ticks, odd_ticks), (3, 4), "抓取發生的時間點");
    assert_eq!(odd_ticks, even_ticks + 1, "奇數寫入晚 1 個 cycle");
}
