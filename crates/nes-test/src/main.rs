//! `nes-test`：命令列除錯 / 測試工具。
//!
//! - `info <rom>`：解析並印出 iNES header。
//! - `nestest <rom> <log>`：對照官方 nestest log 逐指令驗證 CPU（Phase 1 才會實作）。
//! - `blargg <rom>`：跑 blargg 測試 ROM 並回報結果（Phase 1 之後才會實作）。

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
    /// 執行 nestest.nes 並逐指令比對官方 log（尚未實作）。
    Nestest { rom: PathBuf, log: PathBuf },
    /// 執行 blargg 測試 ROM 並回報 pass/fail（尚未實作）。
    Blargg { rom: PathBuf },
}

fn main() -> ExitCode {
    env_logger::init();
    let cli = Cli::parse();

    match cli.command {
        Command::Info { rom } => cmd_info(&rom),
        Command::Nestest { rom: _, log: _ } => {
            eprintln!("nestest: not implemented yet");
            ExitCode::FAILURE
        }
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
}
