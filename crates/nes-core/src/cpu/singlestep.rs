//! 用 [SingleStepTests](https://github.com/SingleStepTests/65x02)
//! （`nes6502` 子集：針對 RP2A03 CPU 核心，沒有十進位模式）驗證每個 opcode
//! 執行完一條指令後的最終暫存器與 RAM 狀態，以及這條指令花的 cycle 數
//! （用 `cycles` 陣列的長度比對，不比對陣列裡逐筆的位址/數值/read-or-write，
//! 那是 cycle-level 精度才需要的，見 `cpu/mod.rs` 模組文件的取捨說明）。
//! 每個 opcode 一個 JSON 檔案，各 10,000 筆測試，總共 256 萬筆。
//!
//! ## 為什麼放在這裡（`#[cfg(test)] mod`），不是 `tests/` 目錄
//!
//! SingleStepTests 假設「整個 64KB 位址空間都是可自由讀寫的 RAM」，這跟
//! 真實 NES 的記憶體地圖（RAM 鏡像、PPU 暫存器、PRG-ROM 唯讀）完全不相容
//! （見 `bus.rs` 的 `test_flat_ram` 欄位與 `new_flat_ram_for_testing`）。
//! 這個旁路是 `pub(crate)` 而非 `pub`，只有 crate 內部看得到；`tests/`
//! 目錄下的檔案是編譯成獨立 crate、只能看到公開 API，所以這個驗證只能放在
//! `src/` 底下、用 `#[cfg(test)]` 掛起來的內部模組（效果上還是「不會進
//! production build，只在 `cargo test` 時編譯」，跟外部整合測試的目的一樣，
//! 只是型別可見性的限制讓它必須用這種形式）。
//!
//! ## 為什麼預設用 `#[ignore]`
//!
//! 256 萬筆測試資料超過 900MB，即使 CPU 本身很快，光是把所有 JSON 讀進來
//! 解析也需要一些時間；為了不拖慢 `cargo test --workspace` 的例行執行，這裡
//! 用 `#[ignore]` 排除在預設測試範圍外。手動執行全套：
//!
//! ```text
//! cargo test --release -p nes-core --lib cpu::singlestep -- --ignored --nocapture
//! ```
//!
//! 測試資料不存在時（沒有跑過下載腳本）會直接印訊息並跳過，不算失敗。

use std::collections::BTreeMap;
use std::path::PathBuf;

use super::{Cpu, OPCODES, StatusFlags};
use crate::bus::Bus;
use crate::cartridge::{Cartridge, Mapper, Mirroring, Nrom, PRG_BANK_SIZE, PRG_RAM_SIZE, RomInfo};

#[derive(serde::Deserialize)]
struct TestCase {
    #[allow(dead_code)]
    name: String,
    initial: CpuState,
    #[serde(rename = "final")]
    expected: CpuState,
    /// 逐 cycle 的匯流排讀寫紀錄。我們不比對「哪個位址、什麼時候被讀寫」
    /// （instruction-level 精度不模擬這個，見 `cpu/mod.rs` 模組文件），只用
    /// 這個陣列的長度跟 `Cpu::step()` 回傳的 cycle 數比對。用
    /// `serde::de::IgnoredAny` 而不是完整的型別，讓 serde 只數元素個數、
    /// 不用真的把每筆 `[addr, value, "read"|"write"]` 都解析/配置記憶體，
    /// 解析速度接近完全略過這個欄位。
    cycles: Vec<serde::de::IgnoredAny>,
}

#[derive(serde::Deserialize)]
struct CpuState {
    pc: u16,
    s: u8,
    a: u8,
    x: u8,
    y: u8,
    p: u8,
    ram: Vec<(u16, u8)>,
}

fn singlestep_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../roms/singlestep/v1")
}

fn dummy_cartridge() -> Cartridge {
    Cartridge {
        info: RomInfo {
            prg_rom_banks: 1,
            chr_rom_banks: 1,
            mapper_id: 0,
            mirroring: Mirroring::Horizontal,
            battery_backed: false,
            has_trainer: false,
        },
        prg_rom: vec![0u8; PRG_BANK_SIZE],
        chr_rom: vec![0u8; 8192],
        chr_ram: Vec::new(),
        prg_ram: vec![0u8; PRG_RAM_SIZE],
        mapper: Mapper::Nrom(Nrom::new(1)),
        rom_hash: 0,
    }
}

/// 執行單一筆測試，回傳第一個不一致的欄位描述；`Ok(())` 代表通過。
fn run_one(tc: &TestCase) -> Result<(), String> {
    let bus = Bus::new_flat_ram_for_testing(dummy_cartridge());
    let mut cpu = Cpu::new(bus);

    cpu.a = tc.initial.a;
    cpu.x = tc.initial.x;
    cpu.y = tc.initial.y;
    cpu.sp = tc.initial.s;
    cpu.pc = tc.initial.pc;
    cpu.status = StatusFlags::from_bits_truncate(tc.initial.p);
    for &(addr, value) in &tc.initial.ram {
        cpu.bus_mut().flat_ram_mut()[addr as usize] = value;
    }

    let cycles = cpu.step();
    if cycles as usize != tc.cycles.len() {
        return Err(format!("cycle 數: 期望 {}，實際 {cycles}", tc.cycles.len()));
    }

    if cpu.a != tc.expected.a {
        return Err(format!("A: 期望 {:02X}，實際 {:02X}", tc.expected.a, cpu.a));
    }
    if cpu.x != tc.expected.x {
        return Err(format!("X: 期望 {:02X}，實際 {:02X}", tc.expected.x, cpu.x));
    }
    if cpu.y != tc.expected.y {
        return Err(format!("Y: 期望 {:02X}，實際 {:02X}", tc.expected.y, cpu.y));
    }
    if cpu.sp != tc.expected.s {
        return Err(format!(
            "SP: 期望 {:02X}，實際 {:02X}",
            tc.expected.s, cpu.sp
        ));
    }
    if cpu.pc != tc.expected.pc {
        return Err(format!(
            "PC: 期望 {:04X}，實際 {:04X}",
            tc.expected.pc, cpu.pc
        ));
    }
    if cpu.status.bits() != tc.expected.p {
        return Err(format!(
            "P: 期望 {:02X}，實際 {:02X}",
            tc.expected.p,
            cpu.status.bits()
        ));
    }
    for &(addr, expected_value) in &tc.expected.ram {
        let actual = cpu.bus_mut().flat_ram_mut()[addr as usize];
        if actual != expected_value {
            return Err(format!(
                "RAM[{addr:04X}]: 期望 {expected_value:02X}，實際 {actual:02X}"
            ));
        }
    }

    Ok(())
}

#[test]
#[ignore = "資料量大（256 個檔案，共 256 萬筆），預設不跑；見模組文件的手動執行指令"]
fn singlestep_tests_all_opcodes() {
    let dir = singlestep_dir();
    if !dir.is_dir() {
        eprintln!(
            "找不到 {}，略過 SingleStepTests（不算失敗）。",
            dir.display()
        );
        eprintln!("下載方式見 README 或 Phase 1 報告。");
        return;
    }

    let mut official_pass = 0u64;
    let mut official_total = 0u64;
    let mut unofficial_pass = 0u64;
    let mut unofficial_total = 0u64;
    let mut per_opcode: BTreeMap<u8, (u64, u64)> = BTreeMap::new();
    let mut first_failures: Vec<String> = Vec::new();

    for opcode in 0..=255u16 {
        let opcode = opcode as u8;
        let path = dir.join(format!("{opcode:02x}.json"));
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(cases) = serde_json::from_str::<Vec<TestCase>>(&content) else {
            eprintln!("解析 {} 失敗，略過這個 opcode", path.display());
            continue;
        };

        let info = &OPCODES[opcode as usize];
        let mut pass = 0u64;
        let total = cases.len() as u64;

        for tc in &cases {
            match run_one(tc) {
                Ok(()) => pass += 1,
                Err(reason) => {
                    if first_failures.len() < 20 {
                        first_failures.push(format!(
                            "{opcode:02X} ({}): {} — {reason}",
                            info.mnemonic, tc.name
                        ));
                    }
                }
            }
        }

        if info.official {
            official_pass += pass;
            official_total += total;
        } else {
            unofficial_pass += pass;
            unofficial_total += total;
        }
        per_opcode.insert(opcode, (pass, total));
    }

    println!("---- SingleStepTests 每個 opcode 的通過率 ----");
    for (opcode, (pass, total)) in &per_opcode {
        let info = &OPCODES[*opcode as usize];
        let tag = if info.official { " " } else { "*" };
        println!(
            "{opcode:02X} {tag}{:<5} {pass:>5}/{total:<5}",
            info.mnemonic
        );
    }

    println!("---- 前 20 個失敗案例 ----");
    for f in &first_failures {
        println!("{f}");
    }

    println!(
        "官方 opcode：{official_pass}/{official_total}（{:.2}%）",
        percentage(official_pass, official_total)
    );
    println!(
        "非官方 opcode：{unofficial_pass}/{unofficial_total}（{:.2}%）",
        percentage(unofficial_pass, unofficial_total)
    );

    assert_eq!(
        official_pass, official_total,
        "官方 opcode 應該 100% 通過 SingleStepTests"
    );
}

fn percentage(pass: u64, total: u64) -> f64 {
    if total == 0 {
        0.0
    } else {
        pass as f64 / total as f64 * 100.0
    }
}
