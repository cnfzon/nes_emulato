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
    /// 逐 cycle 的匯流排讀寫紀錄 `[位址, 值, "read"|"write"]`。**不比對順序**
    /// （instruction-level 精度不追求逐 cycle 的次序，見 `cpu/mod.rs` 模組文件），
    /// 比對的是：陣列長度 vs `Cpu::step()` 回傳的 cycle 數、「讀取過的位址集合」、
    /// 「寫入過的 `(位址, 值)`」（見 [`compare_accesses`]）。
    cycles: Vec<(u16, u8, String)>,
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
        rom_id: crate::cartridge::RomId::default(),
    }
}

/// 匯流排存取的比對結果（不看順序）。
#[derive(Default, Clone)]
struct AccessDiff {
    /// 期望有讀、實際沒讀的位址。
    missing_reads: Vec<u16>,
    /// 實際有讀、期望沒有的位址。
    extra_reads: Vec<u16>,
    /// 期望有寫、實際沒寫的 `(位址, 值)`（依次數計）。
    missing_writes: Vec<(u16, u8)>,
    /// 實際有寫、期望沒有的 `(位址, 值)`。
    extra_writes: Vec<(u16, u8)>,
}

impl AccessDiff {
    fn reads_ok(&self) -> bool {
        self.missing_reads.is_empty() && self.extra_reads.is_empty()
    }
    fn writes_ok(&self) -> bool {
        self.missing_writes.is_empty() && self.extra_writes.is_empty()
    }
}

/// 比對「讀取過的位址集合」與「寫入過的 `(位址, 值)`（多重集合）」。
fn compare_accesses(tc: &TestCase, actual: &[(u16, u8, bool)]) -> AccessDiff {
    use std::collections::BTreeSet;
    let expected_reads: BTreeSet<u16> = tc
        .cycles
        .iter()
        .filter(|(_, _, kind)| kind == "read")
        .map(|&(a, _, _)| a)
        .collect();
    let actual_reads: BTreeSet<u16> = actual
        .iter()
        .filter(|(_, _, w)| !w)
        .map(|&(a, _, _)| a)
        .collect();
    let mut expected_writes: Vec<(u16, u8)> = tc
        .cycles
        .iter()
        .filter(|(_, _, kind)| kind == "write")
        .map(|&(a, v, _)| (a, v))
        .collect();
    let mut actual_writes: Vec<(u16, u8)> = actual
        .iter()
        .filter(|(_, _, w)| *w)
        .map(|&(a, v, _)| (a, v))
        .collect();
    expected_writes.sort_unstable();
    actual_writes.sort_unstable();

    let mut diff = AccessDiff {
        missing_reads: expected_reads.difference(&actual_reads).copied().collect(),
        extra_reads: actual_reads.difference(&expected_reads).copied().collect(),
        ..AccessDiff::default()
    };
    // 多重集合差：兩個排序過的 Vec 逐一消去。
    let mut remaining = actual_writes.clone();
    for w in &expected_writes {
        match remaining.iter().position(|x| x == w) {
            Some(i) => {
                remaining.swap_remove(i);
            }
            None => diff.missing_writes.push(*w),
        }
    }
    diff.extra_writes = remaining;
    diff
}

/// 一筆測試的完整執行結果。
struct Run {
    /// `Cpu::step()` 回傳的 cycle 數。
    cycles: u8,
    /// 暫存器與 RAM 是否相符（第一個不一致的描述）。
    state: Result<(), String>,
    access: AccessDiff,
}

/// 執行單一筆測試，回傳第一個不一致的欄位描述；`Ok(())` 代表通過。
///
/// `check_cycles` 為 `false` 時不比對 cycle 數（JAM 類別用，見 [`Category::Jam`]），
/// 其餘暫存器與 RAM 一律比對。**不含**匯流排存取的比對（見 [`run_full`]）。
fn run_one(tc: &TestCase, check_cycles: bool) -> Result<(), String> {
    let run = run_full(tc);
    if check_cycles && run.cycles as usize != tc.cycles.len() {
        return Err(format!(
            "cycle 數: 期望 {}，實際 {}",
            tc.cycles.len(),
            run.cycles
        ));
    }
    run.state
}

fn run_full(tc: &TestCase) -> Run {
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
    let access = compare_accesses(tc, &cpu.bus_mut().take_access_log());
    let state = check_state(tc, &mut cpu);
    Run {
        cycles,
        state,
        access,
    }
}

fn check_state(tc: &TestCase, cpu: &mut Cpu) -> Result<(), String> {
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

/// 12 個 JAM/KIL opcode。真實硬體會卡死；測試資料把「卡死」展開成 11 個
/// cycle 的匯流排活動，而本核心是 instruction-level 精度，用 `jammed` 旗標
/// 表示卡死、`step()` 回傳 2 cycle。所以暫存器與 RAM 必須相符，cycle 數的
/// 差異屬於預期（見 `docs/architecture.md`「SingleStepTests 分類」）。
const JAM_OPCODES: [u8; 12] = [
    0x02, 0x12, 0x22, 0x32, 0x42, 0x52, 0x62, 0x72, 0x92, 0xB2, 0xD2, 0xF2,
];

/// 6 個不穩定 opcode：真實行為取決於類比因素（bus 上的殘留電容、DMA 時機、
/// 晶片批次），SingleStepTests 的期望值只是其中一種取樣，本專案不追求相符。
/// 預期失敗，只列出、不計入閘門。
///
/// Phase 3 之前是 8 個。`$9C`（SHY）與 `$9E`（SHX）依 blargg instr_test 實作後，
/// SingleStepTests 實測 10000/10000（含 cycle 數）——它們的行為其實是確定的
/// （`reg & (H+1)`、跨頁時位址高位元組被換成該值），不是真正不可預測，所以依實際
/// 結果移到「非官方（穩定）」，納入必須 100% 的閘門。
/// `$AB`（LXA）留在這裡：blargg 要 magic `$FF`（A=X=imm），SingleStepTests 的資料
/// 逐 bit 分析是 magic `$EE`，兩者互相衝突；本專案以 blargg 為準，SST 相符率約 56%。
/// 其餘 5 個（`$8B $93 $9B $9F $BB`）仍是 1-byte NOP 佔位，0%。
const UNSTABLE_OPCODES: [u8; 6] = [0x8B, 0x93, 0x9B, 0x9F, 0xAB, 0xBB];

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Category {
    /// 官方 opcode：含 cycle 數，必須 100%。
    Official,
    /// 非官方且穩定：含 cycle 數，必須 100%。
    UnofficialStable,
    /// JAM：暫存器與 RAM 必須 100%，不比對 cycle 數。
    Jam,
    /// 不穩定：預期失敗，僅列出。
    Unstable,
}

impl Category {
    fn of(opcode: u8) -> Self {
        if UNSTABLE_OPCODES.contains(&opcode) {
            Category::Unstable
        } else if JAM_OPCODES.contains(&opcode) {
            Category::Jam
        } else if OPCODES[opcode as usize].official {
            Category::Official
        } else {
            Category::UnofficialStable
        }
    }

    fn label(self) -> &'static str {
        match self {
            Category::Official => "官方",
            Category::UnofficialStable => "非官方（穩定）",
            Category::Jam => "JAM（不比對 cycle 數）",
            Category::Unstable => "不穩定（預期失敗）",
        }
    }
}

/// 一個類別的累計結果。
#[derive(Default, Clone, Copy)]
struct Tally {
    pass: u64,
    total: u64,
}

/// 只看暫存器與 RAM（不看 cycle 數）能否通過，用來單獨列出 JAM 的 cycle 數差異
/// 是否「只有」cycle 數不同。
fn cycles_only_mismatch(tc: &TestCase) -> bool {
    run_one(tc, true).is_err() && run_one(tc, false).is_ok()
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

    let mut tallies: BTreeMap<Category, Tally> = BTreeMap::new();
    let mut per_opcode: BTreeMap<u8, (Category, u64, u64)> = BTreeMap::new();
    let mut jam_cycle_only_diffs = 0u64;
    let mut jam_cycle_diff_examples: Vec<String> = Vec::new();
    let mut first_gate_failures: Vec<String> = Vec::new();

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
        let category = Category::of(opcode);
        let check_cycles = category != Category::Jam;
        let mut pass = 0u64;
        let total = cases.len() as u64;

        for tc in &cases {
            match run_one(tc, check_cycles) {
                Ok(()) => {
                    pass += 1;
                    if category == Category::Jam && cycles_only_mismatch(tc) {
                        jam_cycle_only_diffs += 1;
                        if jam_cycle_diff_examples.is_empty()
                            || !jam_cycle_diff_examples
                                .iter()
                                .any(|e| e.starts_with(&format!("{opcode:02X} ")))
                        {
                            let actual = {
                                let mut cpu =
                                    Cpu::new(Bus::new_flat_ram_for_testing(dummy_cartridge()));
                                cpu.pc = tc.initial.pc;
                                cpu.bus_mut().flat_ram_mut()[tc.initial.pc as usize] = opcode;
                                cpu.step()
                            };
                            jam_cycle_diff_examples.push(format!(
                                "{opcode:02X} (JAM): 期望 {} cycles，實際 {actual}",
                                tc.cycles.len()
                            ));
                        }
                    }
                }
                Err(reason) => {
                    if category != Category::Unstable && first_gate_failures.len() < 20 {
                        first_gate_failures.push(format!(
                            "[{}] {opcode:02X} ({}): {} — {reason}",
                            category.label(),
                            info.mnemonic,
                            tc.name
                        ));
                    }
                }
            }
        }

        let tally = tallies.entry(category).or_default();
        tally.pass += pass;
        tally.total += total;
        per_opcode.insert(opcode, (category, pass, total));
    }

    println!("---- SingleStepTests 每個 opcode 的通過率（僅列出未 100% 者）----");
    for (opcode, (category, pass, total)) in &per_opcode {
        if pass != total {
            let info = &OPCODES[*opcode as usize];
            println!(
                "{opcode:02X} {:<5} {pass:>5}/{total:<5} [{}]",
                info.mnemonic,
                category.label()
            );
        }
    }

    println!("---- 各類別彙總 ----");
    for (category, t) in &tallies {
        println!(
            "{:<24} {}/{}（{:.2}%）",
            category.label(),
            t.pass,
            t.total,
            percentage(t.pass, t.total)
        );
    }

    println!(
        "---- JAM：暫存器/RAM 相符但 cycle 數不同的案例數：{jam_cycle_only_diffs}（預期，不算失敗）----"
    );
    for e in &jam_cycle_diff_examples {
        println!("{e}");
    }

    let unstable_ops: Vec<String> = per_opcode
        .iter()
        .filter(|(_, (c, _, _))| *c == Category::Unstable)
        .map(|(op, (_, pass, total))| format!("{op:02X}({pass}/{total})"))
        .collect();
    println!(
        "---- 不穩定 opcode（預期失敗，僅列出）：{} ----",
        unstable_ops.join(" ")
    );

    if !first_gate_failures.is_empty() {
        println!("---- 閘門失敗案例（最多 20 筆）----");
        for f in &first_gate_failures {
            println!("{f}");
        }
    }

    for category in [
        Category::Official,
        Category::UnofficialStable,
        Category::Jam,
    ] {
        let t = tallies.get(&category).copied().unwrap_or_default();
        assert!(t.total > 0, "類別 {} 沒有任何測試資料", category.label());
        assert_eq!(
            t.pass,
            t.total,
            "類別 {} 應 100% 通過，實際 {}/{}",
            category.label(),
            t.pass,
            t.total
        );
    }
}

fn percentage(pass: u64, total: u64) -> f64 {
    if total == 0 {
        0.0
    } else {
        pass as f64 / total as f64 * 100.0
    }
}

/// 「刻意不模擬的 dummy read」：這個 opcode 的哪些「期望有讀、實際沒讀」的位址是允許的。
///
/// instruction-level 只模擬**會碰到 I/O 暫存器**的 dummy read：索引定址（abs,X / abs,Y /
/// (ind),Y）修正位址前的那次讀取——它落在資料位址空間，可以是 `$2007` 這種有副作用的
/// 暫存器。其餘的 dummy read 位址由指令本身決定、落在固定區域，沒有副作用，不模擬：
///
/// | 定址模式／指令 | 那次 dummy read 的位址 | 判斷 |
/// |---|---|---|
/// | Implied / Accumulator | `PC + 1`（讀下一個 byte 後丟棄） | 指令流，除非程式在 I/O 位址執行，否則無副作用 |
/// | 堆疊指令（PHA/PHP/PLA/PLP/JSR/RTS/RTI/BRK） | 堆疊頁 `$0100-$01FF`、`PC + 1`；RTS 另有「彈出的返回位址」 | RAM 與指令流 |
/// | ZeroPage,X / ZeroPage,Y / (zp,X) | 未加索引的 zero page 位址 `$00-$FF` | zero page 是 RAM，無副作用 |
/// | Relative（分支） | 分支成立時的 `PC + 2`、跨頁時的未修正目標 | 指令流 |
///
/// 其餘模式（Immediate、ZeroPage、Absolute、Indirect、abs,X、abs,Y、(ind),Y）必須讀取集合
/// **完全相符**，也不得有多讀。
fn unmodeled_read_allowed(opcode: u8, pc: u16, addr: u16) -> bool {
    use super::AddrMode::*;
    let stack_page = (0x0100..=0x01FF).contains(&addr);
    let next = addr == pc.wrapping_add(1);
    match OPCODES[opcode as usize].mode {
        Implied | Accumulator => match opcode {
            // RTS 最後一次讀的是彈出的返回位址（指令流），無法從初始狀態單看，整個放行。
            0x60 => true,
            // 堆疊指令：堆疊頁或 PC+1。
            0x00 | 0x08 | 0x28 | 0x40 | 0x48 | 0x68 => stack_page || next,
            _ => next,
        },
        Absolute => opcode == 0x20 && stack_page, // JSR
        ZeroPageX | ZeroPageY | IndirectX => addr <= 0x00FF,
        Relative => true,
        _ => false,
    }
}

/// 匯流排存取比對（Phase 3.1）：除了暫存器／RAM／cycle 數，也比對每筆測試「讀取過的位址集合」
/// 與「寫入過的 `(位址, 值)`」，**不比對順序**。JAM 不參與（測試資料把卡死展開成 11 個
/// cycle 的活動，本核心沒有對應行為）。
///
/// 只印結果與每個 opcode 的失敗摘要（含範例）；閘門的判定見 [`ACCESS_GATE_MODES`]。
#[test]
#[ignore = "資料量大，預設不跑；見模組文件的手動執行指令"]
fn singlestep_bus_accesses() {
    let dir = singlestep_dir();
    if !dir.is_dir() {
        eprintln!("找不到 {}，略過（不算失敗）。", dir.display());
        return;
    }

    #[derive(Default, Clone)]
    struct OpStat {
        total: u64,
        reads_ok: u64,
        writes_ok: u64,
        both_ok: u64,
        /// 通過閘門的筆數：寫入相符、沒有多讀，且缺的讀取都在 [`unmodeled_read_allowed`] 之內。
        gate_ok: u64,
        example: Option<(String, AccessDiff)>,
    }
    let mut per_opcode: BTreeMap<u8, OpStat> = BTreeMap::new();
    for opcode in 0..=255u16 {
        let opcode = opcode as u8;
        if JAM_OPCODES.contains(&opcode) {
            continue;
        }
        let path = dir.join(format!("{opcode:02x}.json"));
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(cases) = serde_json::from_str::<Vec<TestCase>>(&content) else {
            eprintln!("解析 {} 失敗，略過", path.display());
            continue;
        };
        let stat = per_opcode.entry(opcode).or_default();
        for tc in &cases {
            let run = run_full(tc);
            stat.total += 1;
            let (r, w) = (run.access.reads_ok(), run.access.writes_ok());
            stat.reads_ok += r as u64;
            stat.writes_ok += w as u64;
            stat.both_ok += (r && w) as u64;
            let a = &run.access;
            stat.gate_ok += (w
                && a.extra_reads.is_empty()
                && a.missing_reads
                    .iter()
                    .all(|&m| unmodeled_read_allowed(opcode, tc.initial.pc, m)))
                as u64;
            if !(r && w) && stat.example.is_none() {
                stat.example = Some((tc.name.clone(), run.access));
            }
        }
    }

    let mut by_category: BTreeMap<Category, (u64, u64, u64)> = BTreeMap::new(); // total, reads_ok, writes_ok
    let mut gate: BTreeMap<Category, (u64, u64)> = BTreeMap::new(); // total, gate_ok
    let mut by_mode: BTreeMap<String, (u64, u64)> = BTreeMap::new(); // total, both_ok
    println!("---- 匯流排存取比對：每個 opcode 未 100% 者（讀取位址集合 / 寫入 (位址,值)）----");
    for (opcode, st) in &per_opcode {
        let info = &OPCODES[*opcode as usize];
        let cat = Category::of(*opcode);
        let e = by_category.entry(cat).or_default();
        e.0 += st.total;
        e.1 += st.reads_ok;
        e.2 += st.writes_ok;
        let g = gate.entry(cat).or_default();
        g.0 += st.total;
        g.1 += st.gate_ok;
        let m = by_mode.entry(format!("{:?}", info.mode)).or_default();
        m.0 += st.total;
        m.1 += st.both_ok;
        if st.both_ok != st.total {
            let ex = st.example.as_ref().map(|(name, d)| {
                format!(
                    "  例 {name}: 缺讀 {:04X?} 多讀 {:04X?} 缺寫 {:02X?} 多寫 {:02X?}",
                    d.missing_reads, d.extra_reads, d.missing_writes, d.extra_writes
                )
            });
            println!(
                "{opcode:02X} {:<4} {:<9} 讀 {:>5}/{:<5} 寫 {:>5}/{:<5} [{}]{}",
                info.mnemonic,
                format!("{:?}", info.mode),
                st.reads_ok,
                st.total,
                st.writes_ok,
                st.total,
                cat.label(),
                ex.unwrap_or_default()
            );
        }
    }
    println!("---- 各類別彙總（讀取集合 / 寫入）----");
    for (cat, (total, r, w)) in &by_category {
        println!(
            "{:<24} 讀 {r}/{total}（{:.2}%）  寫 {w}/{total}（{:.2}%）",
            cat.label(),
            percentage(*r, *total),
            percentage(*w, *total)
        );
    }
    println!("---- 閘門（寫入相符、無多讀、缺讀只限不模擬的 dummy read）----");
    for (cat, (total, ok)) in &gate {
        println!(
            "{:<24} {ok}/{total}（{:.2}%）",
            cat.label(),
            percentage(*ok, *total)
        );
    }
    println!("---- 依定址模式彙總（讀寫皆相符 / 總數）----");
    for (mode, (total, ok)) in &by_mode {
        println!("{mode:<12} {ok}/{total}（{:.2}%）", percentage(*ok, *total));
    }

    for cat in [Category::Official, Category::UnofficialStable] {
        let (total, ok) = gate.get(&cat).copied().unwrap_or_default();
        assert!(total > 0, "類別 {} 沒有資料", cat.label());
        assert_eq!(ok, total, "類別 {} 的匯流排存取閘門應 100%", cat.label());
    }
}
