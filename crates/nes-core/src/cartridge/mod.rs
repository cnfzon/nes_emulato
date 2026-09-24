//! 卡帶：iNES 檔案解析 + mapper 定址邏輯。

pub mod ines;
pub mod mapper;

pub use mapper::{Mapper, Nrom};

use crate::error::RomError;

pub const PRG_BANK_SIZE: usize = 16 * 1024;
pub const CHR_BANK_SIZE: usize = 8 * 1024;
/// 沒有 CHR-ROM（header 的 CHR bank 數為 0）時，卡帶改用的 CHR-RAM 大小。
pub const CHR_RAM_SIZE: usize = 8 * 1024;
/// 卡帶內建 PRG-RAM（`$6000-$7FFF`）大小。Phase 0 先固定配置 8KB（NES 卡帶
/// 最常見的大小），不管 `battery_backed` 與否；Bus 尚未接上這個位址範圍。
/// Phase 1 若要精準依 header 決定大小，可以讀 flags8 或 NES 2.0 的欄位。
pub const PRG_RAM_SIZE: usize = 8 * 1024;

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

    /// 靜態 PRG-ROM 資料。**不**進 save state（`#[serde(skip)]`）：同一份
    /// ROM 的內容永遠不變，每次 rollback 存讀檔都重複序列化它既浪費空間又
    /// 拖慢速度。讀檔（`Nes::load_state`）之後由目前已載入的 ROM 接回來，
    /// 用 `rom_hash` 確保接回來的是同一份 ROM。
    #[serde(skip)]
    pub prg_rom: Vec<u8>,
    /// 靜態 CHR-ROM 資料，理由同 `prg_rom`。CHR 為 RAM
    /// （`info.chr_rom_banks == 0`）的卡帶這裡永遠是空的。
    #[serde(skip)]
    pub chr_rom: Vec<u8>,

    /// CHR-RAM：執行期可寫，只有 `info.chr_rom_banks == 0` 的卡帶會配置
    /// （固定大小 `CHR_RAM_SIZE`），否則長度為 0。屬於「狀態」，必須進
    /// save state。
    pub chr_ram: Vec<u8>,
    /// PRG-RAM（`$6000-$7FFF`）。執行期可寫，固定大小 `PRG_RAM_SIZE`，必須
    /// 進 save state。
    pub prg_ram: Vec<u8>,

    pub mapper: Mapper,

    /// `xxh3_64(prg_rom ++ chr_rom)`。因為 `prg_rom`/`chr_rom` 不進存檔，
    /// `load_state` 靠這個雜湊確認存檔真的屬於「目前已載入的這份 ROM」，
    /// 不符合就拒絕讀檔（見 [`crate::error::StateError::RomMismatch`]），
    /// 避免把別的遊戲的存檔套進來後讀到對不上的垃圾資料。
    pub rom_hash: u64,
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

    /// 寫入 PPU 位址空間 `$0000..=$1FFF`。只有 CHR-RAM 卡帶可寫；CHR-ROM 的
    /// 寫入被忽略（硬體上沒有效果）。
    pub fn write_chr(&mut self, addr: u16, value: u8) {
        if self.chr_rom.is_empty() && !self.chr_ram.is_empty() {
            let len = self.chr_ram.len();
            self.chr_ram[addr as usize % len] = value;
        }
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
