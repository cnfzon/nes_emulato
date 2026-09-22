//! 卡帶：iNES 檔案解析 + mapper 定址邏輯。

pub mod ines;
pub mod mapper;

pub use mapper::{Mapper, Nrom};

use crate::error::RomError;

pub const PRG_BANK_SIZE: usize = 16 * 1024;
pub const CHR_BANK_SIZE: usize = 8 * 1024;
/// 沒有 CHR-ROM（header 的 CHR bank 數為 0）時，卡帶改用的 CHR-RAM 大小。
pub const CHR_RAM_SIZE: usize = 8 * 1024;

/// 螢幕捲動的鏡像方式，由 iNES header 的 flags6 決定。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Mirroring {
    Horizontal,
    Vertical,
    FourScreen,
}

/// 從 iNES header 解析出來、跟遊戲邏輯無關的靜態中繼資料。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RomInfo {
    pub prg_rom_banks: u8,
    pub chr_rom_banks: u8,
    pub mapper_id: u8,
    pub mirroring: Mirroring,
    pub battery_backed: bool,
    pub has_trainer: bool,
}

/// 一整張卡帶：PRG/CHR 資料 + mapper 狀態。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Cartridge {
    pub info: RomInfo,
    pub prg_rom: Vec<u8>,
    pub chr_rom: Vec<u8>,
    pub chr_ram: Vec<u8>,
    pub mapper: Mapper,
}

impl Cartridge {
    /// 解析一份 iNES ROM 檔案的原始位元組。
    pub fn from_ines(bytes: &[u8]) -> Result<Self, RomError> {
        ines::parse(bytes)
    }

    /// 讀取 CPU 位址空間 `$8000..=$FFFF` 範圍內的一個 byte。
    pub fn read_prg(&self, addr: u16) -> u8 {
        self.mapper.read_prg(&self.prg_rom, addr)
    }

    /// 讀取 PPU 位址空間 `$0000..=$1FFF`（pattern table）範圍內的一個 byte。
    pub fn read_chr(&self, addr: u16) -> u8 {
        if self.chr_rom.is_empty() {
            self.mapper.read_chr(&self.chr_ram, addr)
        } else {
            self.mapper.read_chr(&self.chr_rom, addr)
        }
    }
}
