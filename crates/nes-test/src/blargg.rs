//! blargg 測試 ROM 的 `$6000` 結果協定。
//!
//! 協定（instr_test-v5、ppu_vbl_nmi、oam_read……這類 2010 年代之後的 blargg
//! 測試 ROM 共用的 shell 都遵守）：
//!
//! | 位址            | 內容                                                        |
//! |-----------------|-------------------------------------------------------------|
//! | `$6001-$6003`   | 簽章 `DE B0 61`，代表 `$6000` 的內容有效                       |
//! | `$6000`         | `$80` 執行中；`$81` 需要按 reset（至少等 100ms 再 reset）；其他是結果碼（0 = 通過） |
//! | `$6004-`        | 以 `\0` 結尾的結果文字                                        |
//!
//! 2005 年的舊版測試（`blargg_ppu_tests_2005.09.15b`、`sprite_hit_tests_2005.10.05`）
//! 沒有這個協定，只把結果印在畫面上；這種情況回傳 [`Outcome::NoSignature`] 與
//! 畫面上 nametable 的文字，由呼叫端判讀。

use nes_core::{Buttons, Nes};

pub const SIGNATURE: [u8; 3] = [0xDE, 0xB0, 0x61];
pub const STATUS_RUNNING: u8 = 0x80;
pub const STATUS_NEEDS_RESET: u8 = 0x81;

const STATUS_ADDR: u16 = 0x6000;
const SIGNATURE_ADDR: u16 = 0x6001;
const TEXT_ADDR: u16 = 0x6004;
/// 文字最長讀多少 byte（PRG-RAM 只有 8KB）。
const MAX_TEXT: usize = 0x1FFC;
/// 收到「需要 reset」之後要等幾幀再 reset（≥ 100ms；6 幀 ≈ 100ms，多等一點）。
const RESET_DELAY_FRAMES: u32 = 10;

/// 一次 blargg 測試的結果。
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// 協定完成：`code == 0` 代表通過。
    Finished {
        code: u8,
        text: String,
        frames: u64,
        resets: u32,
    },
    /// 跑滿 `max_frames` 幀，簽章存在但狀態仍是 `$80`（測試沒跑完，或模擬器卡住）。
    Timeout { text: String, frames: u64 },
    /// 跑滿 `max_frames` 幀都沒看到簽章（舊版測試 ROM，或 ROM 跑飛了）。
    /// `screen` 是畫面（nametable 0）上的文字，`verdict` 是從畫面文字判讀出的結果。
    NoSignature {
        screen: String,
        frames: u64,
        verdict: ScreenVerdict,
    },
}

/// 從舊版 blargg 測試 ROM 的畫面文字判讀出的結果。
#[derive(Debug, PartialEq, Eq)]
pub enum ScreenVerdict {
    /// 畫面上有 `$NN` 結果碼（舊版 `blargg_ppu_tests` 的格式；`$01` 代表通過）。
    Code(u8),
    /// 畫面上有 `PASSED`（舊版 `sprite_hit_tests` 的格式）。
    Passed,
    /// 畫面上有 `FAILED`。
    Failed,
    /// 判讀不出來。
    Unknown,
}

impl ScreenVerdict {
    pub fn is_pass(&self) -> bool {
        matches!(self, ScreenVerdict::Passed | ScreenVerdict::Code(1))
    }
}

/// 判讀舊版測試 ROM 的畫面文字。
pub fn parse_screen_verdict(screen: &str) -> ScreenVerdict {
    for line in screen.lines() {
        let line = line.trim();
        if let Some(hex) = line.strip_prefix('$')
            && let Ok(code) = u8::from_str_radix(hex, 16)
        {
            return ScreenVerdict::Code(code);
        }
    }
    let upper = screen.to_uppercase();
    if upper.contains("PASSED") {
        ScreenVerdict::Passed
    } else if upper.contains("FAILED") {
        ScreenVerdict::Failed
    } else {
        ScreenVerdict::Unknown
    }
}

fn has_signature(nes: &Nes) -> bool {
    (0..3).all(|i| nes.peek(SIGNATURE_ADDR + i) == SIGNATURE[i as usize])
}

/// 讀 `$6004` 起、以 `\0` 結尾的結果文字。
pub fn read_text(nes: &Nes) -> String {
    let mut bytes = Vec::new();
    for i in 0..MAX_TEXT {
        let b = nes.peek(TEXT_ADDR + i as u16);
        if b == 0 {
            break;
        }
        bytes.push(b);
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// nametable 0 上的可見文字：tile 編號 `$20-$7E` 當成 ASCII（blargg 的字型
/// 就是這樣排的），其餘當空白；每列去掉尾端空白，略過全空的列。
pub fn screen_text(nes: &Nes) -> String {
    let mut lines = Vec::new();
    for row in 0..30u16 {
        let line: String = (0..32u16)
            .map(|col| {
                let tile = nes.peek_ppu(0x2000 + row * 32 + col);
                if (0x20..0x7F).contains(&tile) {
                    tile as char
                } else {
                    ' '
                }
            })
            .collect();
        let line = line.trim_end();
        if !line.is_empty() {
            lines.push(line.to_owned());
        }
    }
    lines.join("\n")
}

/// 跑 `nes` 直到協定完成或跑滿 `max_frames` 幀。
pub fn run(nes: &mut Nes, max_frames: u64) -> Outcome {
    let input = [Buttons::empty(); 2];
    let mut resets = 0;
    // reset 之後 `$6000` 仍留著舊的 `$81`，要等 ROM 重新寫成別的值才算有效。
    let mut waiting_for_status_change = false;

    for frame in 1..=max_frames {
        nes.run_frame(input);
        if !has_signature(nes) {
            continue;
        }
        let status = nes.peek(STATUS_ADDR);
        if waiting_for_status_change {
            if status != STATUS_NEEDS_RESET {
                waiting_for_status_change = false;
            } else {
                continue;
            }
        }
        match status {
            STATUS_RUNNING => {}
            STATUS_NEEDS_RESET => {
                for _ in 0..RESET_DELAY_FRAMES {
                    nes.run_frame(input);
                }
                nes.reset();
                resets += 1;
                waiting_for_status_change = true;
            }
            code => {
                return Outcome::Finished {
                    code,
                    text: read_text(nes),
                    frames: frame,
                    resets,
                };
            }
        }
    }

    if has_signature(nes) {
        Outcome::Timeout {
            text: read_text(nes),
            frames: max_frames,
        }
    } else {
        let screen = screen_text(nes);
        let verdict = parse_screen_verdict(&screen);
        Outcome::NoSignature {
            screen,
            frames: max_frames,
            verdict,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 組一份 32KB NROM：`$8000` 放 `program`，reset vector 指到 `$8000`。
    fn rom_with_program(program: &[u8]) -> Vec<u8> {
        let mut prg = vec![0xEAu8; 0x8000];
        prg[..program.len()].copy_from_slice(program);
        // 程式最後接一個 `JMP` 到自己的死迴圈。
        let end = 0x8000 + program.len() as u16;
        let idx = program.len();
        prg[idx] = 0x4C;
        prg[idx + 1] = end as u8;
        prg[idx + 2] = (end >> 8) as u8;
        prg[0x7FFC] = 0x00;
        prg[0x7FFD] = 0x80;

        let mut rom = vec![0u8; 16];
        rom[0..4].copy_from_slice(b"NES\x1A");
        rom[4] = 2;
        rom[5] = 1;
        rom.extend(prg);
        rom.extend(vec![0u8; 8192]);
        rom
    }

    /// `LDA #imm; STA abs` 的機器碼。
    fn store(addr: u16, value: u8) -> [u8; 5] {
        [0xA9, value, 0x8D, addr as u8, (addr >> 8) as u8]
    }

    fn signature_program(status: u8, text: &[u8]) -> Vec<u8> {
        let mut p = Vec::new();
        for (i, b) in SIGNATURE.iter().enumerate() {
            p.extend(store(SIGNATURE_ADDR + i as u16, *b));
        }
        for (i, b) in text.iter().chain(&[0u8]).enumerate() {
            p.extend(store(TEXT_ADDR + i as u16, *b));
        }
        p.extend(store(STATUS_ADDR, status));
        p
    }

    #[test]
    fn reports_pass_with_result_text() {
        let rom = rom_with_program(&signature_program(0, b"Passed"));
        let mut nes = Nes::from_rom(&rom).unwrap();
        match run(&mut nes, 10) {
            Outcome::Finished { code, text, .. } => {
                assert_eq!(code, 0);
                assert_eq!(text, "Passed");
            }
            other => panic!("預期 Finished，得到 {other:?}"),
        }
    }

    #[test]
    fn reports_failure_code() {
        let rom = rom_with_program(&signature_program(3, b"bad"));
        let mut nes = Nes::from_rom(&rom).unwrap();
        assert!(matches!(
            run(&mut nes, 10),
            Outcome::Finished { code: 3, .. }
        ));
    }

    #[test]
    fn running_status_times_out() {
        let rom = rom_with_program(&signature_program(STATUS_RUNNING, b"..."));
        let mut nes = Nes::from_rom(&rom).unwrap();
        assert!(matches!(run(&mut nes, 5), Outcome::Timeout { .. }));
    }

    #[test]
    fn screen_verdict_parsing() {
        assert_eq!(parse_screen_verdict("  $01"), ScreenVerdict::Code(1));
        assert!(parse_screen_verdict("  $01").is_pass());
        assert_eq!(
            parse_screen_verdict(
                "x
  $03"
            ),
            ScreenVerdict::Code(3)
        );
        assert!(!parse_screen_verdict("  $03").is_pass());
        assert_eq!(
            parse_screen_verdict(
                "SPRITE HIT BASICS
PASSED"
            ),
            ScreenVerdict::Passed
        );
        assert_eq!(parse_screen_verdict("FAILED"), ScreenVerdict::Failed);
        assert_eq!(parse_screen_verdict("hello"), ScreenVerdict::Unknown);
    }

    #[test]
    fn missing_signature_is_reported_with_screen_text() {
        let rom = rom_with_program(&[]);
        let mut nes = Nes::from_rom(&rom).unwrap();
        assert!(matches!(run(&mut nes, 3), Outcome::NoSignature { .. }));
    }
}
