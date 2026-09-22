//! Mapper 定址邏輯。
//!
//! 用 `enum Mapper` 而不是 trait object（`Box<dyn Mapper>`），原因是 mapper
//! 狀態必須完整進 save state：`enum` 可以直接 `derive(Serialize, Deserialize)`，
//! `Box<dyn Trait>` 沒辦法（trait object 不知道具體型別，無法反序列化回正確的
//! variant）。詳見 `docs/architecture.md` 的「為何 Mapper 用 enum」章節。

use super::PRG_BANK_SIZE;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum Mapper {
    Nrom(Nrom),
    // TODO Phase 2+: Mmc1(Mmc1)  — 5-bit shift register 切換 PRG/CHR bank 與鏡像模式
    // TODO Phase 2+: Uxrom(Uxrom) — 可切換的 16KB PRG bank + 固定最後一個 bank
    // TODO Phase 2+: Cnrom(Cnrom) — 可切換的 8KB CHR bank，PRG 固定
}

impl Mapper {
    pub fn read_prg(&self, prg_rom: &[u8], addr: u16) -> u8 {
        match self {
            Mapper::Nrom(m) => m.read_prg(prg_rom, addr),
        }
    }

    pub fn read_chr(&self, chr: &[u8], addr: u16) -> u8 {
        match self {
            Mapper::Nrom(m) => m.read_chr(chr, addr),
        }
    }
}

/// Mapper 0（NROM）：沒有 bank switching，PRG-ROM 16KB 時鏡像成 32KB。
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub struct Nrom {
    prg_banks: u8,
}

impl Nrom {
    pub fn new(prg_banks: u8) -> Self {
        Self { prg_banks }
    }

    /// `addr` 必須落在 CPU 位址空間的 `$8000..=$FFFF`。
    pub fn read_prg(&self, prg_rom: &[u8], addr: u16) -> u8 {
        let mut offset = (addr.wrapping_sub(0x8000)) as usize;
        if self.prg_banks <= 1 {
            offset %= PRG_BANK_SIZE;
        }
        prg_rom[offset % prg_rom.len().max(1)]
    }

    /// `addr` 必須落在 PPU pattern table 位址空間的 `$0000..=$1FFF`。
    pub fn read_chr(&self, chr: &[u8], addr: u16) -> u8 {
        chr[addr as usize % chr.len().max(1)]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nrom_32kb_reads_directly() {
        let mut prg = vec![0u8; 2 * PRG_BANK_SIZE];
        prg[0] = 0x11;
        prg[PRG_BANK_SIZE] = 0x22;
        let nrom = Nrom::new(2);
        assert_eq!(nrom.read_prg(&prg, 0x8000), 0x11);
        assert_eq!(nrom.read_prg(&prg, 0xC000), 0x22);
    }

    #[test]
    fn nrom_16kb_mirrors_across_both_halves() {
        let mut prg = vec![0u8; PRG_BANK_SIZE];
        prg[0] = 0x42;
        let nrom = Nrom::new(1);
        assert_eq!(nrom.read_prg(&prg, 0x8000), 0x42);
        assert_eq!(nrom.read_prg(&prg, 0xC000), 0x42);
    }
}
