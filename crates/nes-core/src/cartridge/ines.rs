//! iNES 1.0 header 解析。
//!
//! Header 格式（16 bytes）：
//!
//! | offset | 內容                                                  |
//! |--------|-------------------------------------------------------|
//! | 0..4   | magic number `"NES\x1A"`                               |
//! | 4      | PRG-ROM 大小，單位 16KB                                 |
//! | 5      | CHR-ROM 大小，單位 8KB（0 代表使用 CHR-RAM）             |
//! | 6      | flags6：mirroring / battery / trainer / mapper 低 4 位元 |
//! | 7      | flags7：mapper 高 4 位元 / NES 2.0 標記                  |
//! | 8..16  | flags8..15（PRG-RAM 大小、TV 系統等，本階段忽略）         |
//!
//! 若偵測到 NES 2.0（flags7 的 bit2/bit3 為 `10`），回傳
//! [`RomError::Nes20Unsupported`] 而不是嘗試用 iNES 1.0 規則誤解析。

use super::{
    CHR_BANK_SIZE, CHR_RAM_SIZE, Cartridge, Cnrom, Mapper, Mirroring, Mmc1, Nrom, PRG_BANK_SIZE,
    PRG_RAM_SIZE, RomId, RomInfo, Uxrom,
};
use crate::error::RomError;

pub const HEADER_SIZE: usize = 16;
pub const TRAINER_SIZE: usize = 512;

const MAGIC: [u8; 4] = *b"NES\x1A";

pub fn parse(bytes: &[u8]) -> Result<Cartridge, RomError> {
    if bytes.len() < HEADER_SIZE {
        return Err(RomError::TooSmall);
    }
    if bytes[0..4] != MAGIC {
        return Err(RomError::BadMagic);
    }

    let prg_rom_banks = bytes[4];
    let chr_rom_banks = bytes[5];
    let flags6 = bytes[6];
    let flags7 = bytes[7];

    // NES 2.0 identifier: bits 2-3 of flags7 == 0b10.
    if flags7 & 0x0C == 0x08 {
        return Err(RomError::Nes20Unsupported);
    }

    // 沒有 PRG-ROM 的卡帶連 reset vector 都讀不到；拒絕它，避免之後讀 PRG 時越界。
    if prg_rom_banks == 0 {
        return Err(RomError::NoPrgRom);
    }

    let mapper_id = (flags7 & 0xF0) | (flags6 >> 4);
    let mirroring = if flags6 & 0x08 != 0 {
        Mirroring::FourScreen
    } else if flags6 & 0x01 != 0 {
        Mirroring::Vertical
    } else {
        Mirroring::Horizontal
    };
    let battery_backed = flags6 & 0x02 != 0;
    let has_trainer = flags6 & 0x04 != 0;

    let mut offset = HEADER_SIZE;
    if has_trainer {
        offset += TRAINER_SIZE;
    }

    let prg_size = prg_rom_banks as usize * PRG_BANK_SIZE;
    let chr_size = chr_rom_banks as usize * CHR_BANK_SIZE;
    let expected = offset + prg_size + chr_size;
    if bytes.len() < expected {
        return Err(RomError::Truncated {
            expected,
            actual: bytes.len(),
        });
    }

    let prg_rom = bytes[offset..offset + prg_size].to_vec();
    offset += prg_size;
    let chr_rom = bytes[offset..offset + chr_size].to_vec();

    let chr_ram = if chr_rom_banks == 0 {
        vec![0u8; CHR_RAM_SIZE]
    } else {
        Vec::new()
    };
    let prg_ram = vec![0u8; PRG_RAM_SIZE];

    // 整個檔案（含 header 與 trainer）的識別碼，不只 PRG + CHR。
    let rom_id = RomId::of_file(bytes);

    let mapper = match mapper_id {
        0 => Mapper::Nrom(Nrom::new(prg_rom_banks)),
        1 => Mapper::Mmc1(Mmc1::new()),
        2 => Mapper::Uxrom(Uxrom::new()),
        3 => Mapper::Cnrom(Cnrom::new()),
        other => return Err(RomError::UnsupportedMapper(other)),
    };

    let info = RomInfo {
        prg_rom_banks,
        chr_rom_banks,
        mapper_id,
        mirroring,
        battery_backed,
        has_trainer,
    };

    Ok(Cartridge {
        info,
        prg_rom,
        chr_rom,
        chr_ram,
        prg_ram,
        mapper,
        rom_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 組出一份手工的 iNES header + 填滿 0xAA 的 PRG/CHR 資料。
    fn build_rom(
        prg_banks: u8,
        chr_banks: u8,
        flags6: u8,
        flags7: u8,
        with_trainer: bool,
    ) -> Vec<u8> {
        let mut bytes = vec![0u8; HEADER_SIZE];
        bytes[0..4].copy_from_slice(&MAGIC);
        bytes[4] = prg_banks;
        bytes[5] = chr_banks;
        bytes[6] = flags6;
        bytes[7] = flags7;

        if with_trainer {
            bytes.extend(vec![0u8; TRAINER_SIZE]);
        }
        bytes.extend(vec![0xAAu8; prg_banks as usize * PRG_BANK_SIZE]);
        bytes.extend(vec![0xBBu8; chr_banks as usize * CHR_BANK_SIZE]);
        bytes
    }

    #[test]
    fn parses_valid_nrom_header() {
        let rom = build_rom(2, 1, 0b0000_0001, 0b0000_0000, false);
        let cart = parse(&rom).expect("valid header should parse");
        assert_eq!(cart.info.prg_rom_banks, 2);
        assert_eq!(cart.info.chr_rom_banks, 1);
        assert_eq!(cart.info.mapper_id, 0);
        assert_eq!(cart.info.mirroring, Mirroring::Vertical);
        assert!(!cart.info.battery_backed);
        assert!(!cart.info.has_trainer);
        assert_eq!(cart.prg_rom.len(), 2 * PRG_BANK_SIZE);
        assert_eq!(cart.chr_rom.len(), CHR_BANK_SIZE);
        assert!(matches!(cart.mapper, Mapper::Nrom(_)));
    }

    #[test]
    fn chr_ram_allocated_when_chr_banks_is_zero() {
        let rom = build_rom(1, 0, 0, 0, false);
        let cart = parse(&rom).unwrap();
        assert!(cart.chr_rom.is_empty());
        assert_eq!(cart.chr_ram.len(), CHR_RAM_SIZE);
    }

    #[test]
    fn detects_battery_and_trainer_flags() {
        // flags6: mirroring=horizontal(0), battery(bit1), trainer(bit2)
        let rom = build_rom(1, 1, 0b0000_0110, 0, true);
        let cart = parse(&rom).unwrap();
        assert!(cart.info.battery_backed);
        assert!(cart.info.has_trainer);
        assert_eq!(cart.info.mirroring, Mirroring::Horizontal);
    }

    #[test]
    fn four_screen_flag_overrides_mirroring_bit() {
        let rom = build_rom(1, 1, 0b0000_1001, 0, false);
        let cart = parse(&rom).unwrap();
        assert_eq!(cart.info.mirroring, Mirroring::FourScreen);
    }

    #[test]
    fn mapper_id_combines_both_nibbles() {
        // flags6 low nibble = 0001 -> mapper bits 0-3 = 0001
        // flags7 high nibble = 0001_0000 -> mapper bits 4-7 = 0001
        // Mapper 0x11 isn't implemented, so parsing fails with UnsupportedMapper —
        // that failure itself proves the two nibbles were combined correctly.
        let rom = build_rom(1, 1, 0b0001_0000, 0b0001_0000, false);
        assert_eq!(parse(&rom).unwrap_err(), RomError::UnsupportedMapper(0x11));
    }

    #[test]
    fn rejects_rom_without_prg() {
        let rom = build_rom(0, 1, 0, 0, false);
        assert_eq!(parse(&rom).unwrap_err(), RomError::NoPrgRom);
    }

    #[test]
    fn rejects_too_small_file() {
        let bytes = vec![0u8; 8];
        assert_eq!(parse(&bytes).unwrap_err(), RomError::TooSmall);
    }

    #[test]
    fn rejects_bad_magic() {
        let mut rom = build_rom(1, 1, 0, 0, false);
        rom[0] = b'X';
        assert_eq!(parse(&rom).unwrap_err(), RomError::BadMagic);
    }

    #[test]
    fn detects_nes20_and_reports_unsupported() {
        // flags7 bits 2-3 = 0b10 marks NES 2.0.
        let rom = build_rom(1, 1, 0, 0b0000_1000, false);
        assert_eq!(parse(&rom).unwrap_err(), RomError::Nes20Unsupported);
    }

    #[test]
    fn rejects_unsupported_mapper() {
        // mapper id 4 = MMC3, not implemented (Phase 3 只做 0/1/2/3)。
        let rom = build_rom(1, 1, 0b0100_0000, 0, false);
        assert_eq!(parse(&rom).unwrap_err(), RomError::UnsupportedMapper(4));
    }

    #[test]
    fn parses_mmc1_uxrom_and_cnrom_headers() {
        for (id, is_expected) in [
            (
                1u8,
                (|m: &Mapper| matches!(m, Mapper::Mmc1(_))) as fn(&Mapper) -> bool,
            ),
            (2, |m| matches!(m, Mapper::Uxrom(_))),
            (3, |m| matches!(m, Mapper::Cnrom(_))),
        ] {
            let rom = build_rom(2, 1, id << 4, 0, false);
            let cart = parse(&rom).unwrap_or_else(|e| panic!("mapper {id} 應該可以解析: {e}"));
            assert_eq!(cart.info.mapper_id, id);
            assert!(is_expected(&cart.mapper), "mapper {id} 對到錯的變體");
            assert_eq!(cart.mapper.id(), id);
        }
    }

    #[test]
    fn rejects_truncated_data() {
        let mut rom = build_rom(2, 1, 0, 0, false);
        rom.truncate(rom.len() - 100);
        let expected = HEADER_SIZE + 2 * PRG_BANK_SIZE + CHR_BANK_SIZE;
        let actual = rom.len();
        assert_eq!(
            parse(&rom).unwrap_err(),
            RomError::Truncated { expected, actual }
        );
    }
}
