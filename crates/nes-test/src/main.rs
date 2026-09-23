//! `nes-test`：命令列除錯 / 測試工具。
//!
//! - `info <rom>`：解析並印出 iNES header。
//! - `nestest <rom> <log>`：對照官方 nestest log 逐指令驗證 CPU。
//! - `blargg <rom>`：跑 blargg 測試 ROM 並回報結果（尚未實作）。

mod nestest_log;

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "nes-test",
    version,
    about = "NES 模擬核心的命令列測試 / 除錯工具"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// 解析並印出一份 iNES ROM 的 header 資訊。
    Info { rom: PathBuf },
    /// 執行 nestest.nes，逐指令比對官方 nestest.log。
    Nestest {
        rom: PathBuf,
        log: PathBuf,
        /// 反組譯文字（含 "= xx" 記憶體值）也列入比對，而不是只顯示差異警告。
        #[arg(long)]
        strict: bool,
    },
    /// 執行 blargg 測試 ROM 並回報 pass/fail（尚未實作）。
    Blargg { rom: PathBuf },
}

fn main() -> ExitCode {
    env_logger::init();
    let cli = Cli::parse();

    match cli.command {
        Command::Info { rom } => cmd_info(&rom),
        Command::Nestest { rom, log, strict } => cmd_nestest(&rom, &log, strict),
        Command::Blargg { rom: _ } => {
            eprintln!("blargg: not implemented yet");
            ExitCode::FAILURE
        }
    }
}

fn cmd_info(path: &Path) -> ExitCode {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("讀取 ROM 檔案失敗: {e}");
            return ExitCode::FAILURE;
        }
    };

    match nes_core::Cartridge::from_ines(&bytes) {
        Ok(cart) => {
            let info = &cart.info;
            println!("檔案: {}", path.display());
            println!(
                "PRG-ROM: {} x 16KB = {} bytes",
                info.prg_rom_banks,
                cart.prg_rom.len()
            );
            println!(
                "CHR-ROM: {} x 8KB = {} bytes",
                info.chr_rom_banks,
                cart.chr_rom.len()
            );
            println!("Mapper: {}", info.mapper_id);
            println!("Mirroring: {:?}", info.mirroring);
            println!("Battery-backed: {}", info.battery_backed);
            println!("Trainer: {}", info.has_trainer);
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("解析 iNES header 失敗: {e}");
            ExitCode::FAILURE
        }
    }
}

/// nestest 在「automation mode」下的固定進入點：跳過需要人工按鍵互動的
/// 視覺測試選單，直接從 CPU-only 的測試序列開始。
const NESTEST_AUTOMATION_ENTRY: u16 = 0xC000;
/// 不一致時往回列出的上下文行數。
const CONTEXT_LINES: usize = 5;

/// `compare_nestest_log` 找到的第一個問題。跟 I/O、印訊息的邏輯分開，讓
/// 比對邏輯本身可以脫離檔案系統直接單元測試。
#[derive(Debug)]
enum NestestFailure {
    /// log 某一行不符合 nestest.log 的格式，`parse_line` 解析失敗。這算
    /// 失敗，不是略過——一份格式錯誤/損毀的 log 不該被當成「跳過這行、
    /// 當作通過」。
    ParseError { line: usize, raw: String },
    /// 某一行的核心欄位（PC/bytes/A/X/Y/P/SP/CYC/PPU，或 `--strict` 時的
    /// 反組譯文字）跟我們自己 `trace()` 出來的不一致。
    Mismatch {
        line: usize,
        expected: String,
        actual: String,
        /// 反組譯文字也不同，但沒開 `--strict` 所以不算進 `core_mismatch`
        /// 時的提示訊息。
        disasm_note: Option<String>,
        /// 不一致那一行之前的最近幾行（都是已經比對通過的），方便除錯。
        context: Vec<String>,
    },
}

/// 逐行比對：每比對一行之前呼叫 `nes.trace()` 取得「即將執行的這條指令」
/// 的 trace，跟 `log_content` 對應行的欄位比對；通過就呼叫
/// `nes.step_instruction()` 真的執行那條指令，再比對下一行。
///
/// 成功回傳比對過的總行數；失敗回傳第一個問題（詳見 [`NestestFailure`]）。
fn compare_nestest_log(
    nes: &mut nes_core::Nes,
    log_content: &str,
    strict: bool,
) -> Result<usize, NestestFailure> {
    let mut history: VecDeque<String> = VecDeque::with_capacity(CONTEXT_LINES);
    let mut line_count = 0usize;

    for (i, expected_line) in log_content.lines().enumerate() {
        line_count += 1;
        let line_no = i + 1;
        let actual_line = nes.trace();

        let Some(expected) = nestest_log::parse_line(expected_line) else {
            return Err(NestestFailure::ParseError {
                line: line_no,
                raw: expected_line.to_string(),
            });
        };
        let actual = nestest_log::parse_line(&actual_line)
            .expect("我們自己產生的 trace 一定能被自己的 parser 解析");

        let core_mismatch = expected.pc != actual.pc
            || expected.bytes != actual.bytes
            || expected.a != actual.a
            || expected.x != actual.x
            || expected.y != actual.y
            || expected.p != actual.p
            || expected.sp != actual.sp
            || expected.cyc != actual.cyc
            || expected.ppu_scanline != actual.ppu_scanline
            || expected.ppu_cycle != actual.ppu_cycle;
        let disasm_mismatch = expected.disasm != actual.disasm;

        if core_mismatch || (strict && disasm_mismatch) {
            let disasm_note = (disasm_mismatch && !strict).then(|| {
                "反組譯文字也不同，但預設不列入比對結果；加 --strict 會讓它也算不一致".to_string()
            });
            return Err(NestestFailure::Mismatch {
                line: line_no,
                expected: expected_line.to_string(),
                actual: actual_line,
                disasm_note,
                context: history.into_iter().collect(),
            });
        }

        history.push_back(actual_line);
        if history.len() > CONTEXT_LINES {
            history.pop_front();
        }

        nes.step_instruction();
    }

    Ok(line_count)
}

fn cmd_nestest(rom_path: &Path, log_path: &Path, strict: bool) -> ExitCode {
    let rom_bytes = match std::fs::read(rom_path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("讀取 ROM 檔案失敗: {e}");
            return ExitCode::FAILURE;
        }
    };
    let mut nes = match nes_core::Nes::from_rom(&rom_bytes) {
        Ok(n) => n,
        Err(e) => {
            eprintln!("解析 ROM 失敗: {e}");
            return ExitCode::FAILURE;
        }
    };
    // from_rom 內部已經跑過一次正常的 reset（SP=$FD, P=$24, 7 cycles）；
    // 這裡只覆寫 PC，模擬 nestest 的 automation mode。
    nes.override_pc(NESTEST_AUTOMATION_ENTRY);

    let log_content = match std::fs::read_to_string(log_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("讀取 log 檔案失敗: {e}");
            return ExitCode::FAILURE;
        }
    };

    match compare_nestest_log(&mut nes, &log_content, strict) {
        Ok(line_count) => {
            let err2 = nes.peek(0x02);
            let err3 = nes.peek(0x03);
            if err2 == 0 && err3 == 0 {
                println!("nestest: {line_count} 行全數比對通過，錯誤碼 $02=$03=00");
                ExitCode::SUCCESS
            } else {
                eprintln!(
                    "nestest: {line_count} 行 log 比對通過，但錯誤碼 $02={err2:02X} $03={err3:02X}（非 0，代表某個官方指令測試失敗）"
                );
                ExitCode::FAILURE
            }
        }
        Err(NestestFailure::ParseError { line, raw }) => {
            eprintln!("無法解析 log 第 {line} 行: {raw}");
            ExitCode::FAILURE
        }
        Err(NestestFailure::Mismatch {
            line,
            expected,
            actual,
            disasm_note,
            context,
        }) => {
            eprintln!("在第 {line} 行發現不一致：");
            eprintln!("期望: {expected}");
            eprintln!("實際: {actual}");
            if let Some(note) = disasm_note {
                eprintln!("（{note}）");
            }
            eprintln!("--- 前 {} 行上下文 ---", context.len());
            for h in &context {
                eprintln!("{h}");
            }
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_minimal_rom(path: &Path) {
        let mut bytes = vec![0u8; 16];
        bytes[0..4].copy_from_slice(b"NES\x1A");
        bytes[4] = 1;
        bytes[5] = 1;
        bytes.extend(vec![0u8; 16 * 1024]);
        bytes.extend(vec![0u8; 8 * 1024]);
        let mut f = std::fs::File::create(path).unwrap();
        f.write_all(&bytes).unwrap();
    }

    #[test]
    fn info_command_succeeds_on_valid_rom() {
        let path = std::env::temp_dir().join("nes_test_scratch_rom.nes");
        write_minimal_rom(&path);
        let code = cmd_info(&path);
        std::fs::remove_file(&path).ok();
        assert_eq!(code, ExitCode::SUCCESS);
    }

    #[test]
    fn info_command_fails_on_missing_file() {
        let path = std::env::temp_dir().join("nes_test_scratch_rom_does_not_exist.nes");
        std::fs::remove_file(&path).ok();
        let code = cmd_info(&path);
        assert_eq!(code, ExitCode::FAILURE);
    }

    /// 一份 32KB PRG-ROM 的最小 iNES ROM，`$8000` 開始放 3 個 `NOP`
    /// （`$EA`），reset vector 指到 `$8000`。三個 `NOP` 讓我們可以用自己的
    /// `trace()` 產生三行「保證正確」的參考 log，再刻意弄壞其中一行來測
    /// 比對邏輯，不需要手刻脆弱的假 log 字串。
    fn build_nop_rom() -> Vec<u8> {
        let mut prg = vec![0u8; 0x8000];
        prg[0] = 0xEA;
        prg[1] = 0xEA;
        prg[2] = 0xEA;
        prg[0x7FFC] = 0x00; // reset vector -> $8000
        prg[0x7FFD] = 0x80;

        let mut rom = vec![0u8; 16];
        rom[0..4].copy_from_slice(b"NES\x1A");
        rom[4] = 2; // 32KB PRG（不鏡像）
        rom[5] = 1;
        rom.extend(prg);
        rom.extend(vec![0u8; 8192]);
        rom
    }

    #[test]
    fn compare_nestest_log_accepts_a_correct_log() {
        let rom = build_nop_rom();

        let mut reference = nes_core::Nes::from_rom(&rom).unwrap();
        let line1 = reference.trace();
        reference.step_instruction();
        let line2 = reference.trace();
        reference.step_instruction();
        let line3 = reference.trace();

        let good_log = format!("{line1}\n{line2}\n{line3}\n");
        let mut nes = nes_core::Nes::from_rom(&rom).unwrap();
        let result = compare_nestest_log(&mut nes, &good_log, false);
        assert!(
            matches!(result, Ok(3)),
            "預期比對 3 行都通過，實際: {result:?}"
        );
    }

    #[test]
    fn compare_nestest_log_reports_correct_line_number_on_mismatch() {
        let rom = build_nop_rom();

        let mut reference = nes_core::Nes::from_rom(&rom).unwrap();
        let line1 = reference.trace();
        reference.step_instruction();
        let line2 = reference.trace();
        reference.step_instruction();
        let line3 = reference.trace();

        // 刻意弄壞第 2 行：把 A 暫存器的值改掉。
        let corrupted_line2 = line2.replace("A:00", "A:FF");
        assert_ne!(
            line2, corrupted_line2,
            "測試前提：line2 裡應該真的有 A:00 可以替換"
        );

        let bad_log = format!("{line1}\n{corrupted_line2}\n{line3}\n");
        let mut nes = nes_core::Nes::from_rom(&rom).unwrap();
        match compare_nestest_log(&mut nes, &bad_log, false) {
            Err(NestestFailure::Mismatch { line, .. }) => assert_eq!(line, 2),
            other => panic!("預期在第 2 行回報不一致，實際: {other:?}"),
        }
    }

    #[test]
    fn compare_nestest_log_treats_unparseable_line_as_failure_not_skip() {
        let rom = build_nop_rom();
        let mut nes = nes_core::Nes::from_rom(&rom).unwrap();

        match compare_nestest_log(&mut nes, "this is not a valid nestest log line\n", false) {
            Err(NestestFailure::ParseError { line, .. }) => assert_eq!(line, 1),
            other => panic!("預期回報解析失敗（不是略過），實際: {other:?}"),
        }
    }
}
