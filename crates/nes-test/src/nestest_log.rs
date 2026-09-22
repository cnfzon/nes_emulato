//! nestest.log 格式的逐行解析器。
//!
//! 用來把「我們自己 `Nes::trace()` 產生的一行」跟「官方 nestest.log 的
//! 一行」都拆成同一組欄位，逐欄位比對，而不是要求兩行字串完全相等——這樣
//! `trace()` 的欄位間距即使跟官方 log 有一點點不同，也不影響比對結果。

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedLine {
    pub pc: u16,
    pub bytes: Vec<u8>,
    pub disasm: String,
    pub a: u8,
    pub x: u8,
    pub y: u8,
    pub p: u8,
    pub sp: u8,
    pub ppu_scanline: u16,
    pub ppu_cycle: u16,
    pub cyc: u64,
}

pub fn parse_line(line: &str) -> Option<ParsedLine> {
    let line = line.trim_end();
    if line.len() < 6 {
        return None;
    }
    let pc = u16::from_str_radix(&line[0..4], 16).ok()?;

    let a_idx = line.find("A:")?;
    let rest = line.get(6..a_idx)?;
    let tokens: Vec<&str> = rest.split_whitespace().collect();

    // Bytes 欄位永遠是「剛好 2 個 hex 字元」的 token；反組譯文字（mnemonic
    // 至少 3 個字元，運算元帶 `$`/`#`/`,` 等符號）不會符合這個形狀，用這個
    // 當「bytes 欄位在哪裡結束」的判斷依據。
    let mut bytes = Vec::new();
    let mut disasm_start = 0;
    for tok in &tokens {
        if tok.len() == 2 && tok.chars().all(|c| c.is_ascii_hexdigit()) {
            bytes.push(u8::from_str_radix(tok, 16).ok()?);
            disasm_start += 1;
        } else {
            break;
        }
    }
    let disasm = tokens[disasm_start..].join(" ");

    let a = extract_hex_u8(line, "A:")?;
    let x = extract_hex_u8(line, "X:")?;
    let y = extract_hex_u8(line, "Y:")?;
    let p = extract_hex_u8(line, "P:")?;
    let sp = extract_hex_u8(line, "SP:")?;

    let ppu_str = after(line, "PPU:")?;
    let comma = ppu_str.find(',')?;
    let ppu_scanline = ppu_str[..comma].trim().parse::<u16>().ok()?;
    // 欄位是右對齊、空白補位（例如 "  0, 21"），逗號後面可能還有一個前導
    // 空白，要先 trim_start 再找結尾的空白，不然會把數字切成空字串。
    let after_comma = ppu_str[comma + 1..].trim_start();
    let end = after_comma.find(' ').unwrap_or(after_comma.len());
    let ppu_cycle = after_comma[..end].parse::<u16>().ok()?;

    let cyc_str = after(line, "CYC:")?;
    let cyc = cyc_str.trim().parse::<u64>().ok()?;

    Some(ParsedLine {
        pc,
        bytes,
        disasm,
        a,
        x,
        y,
        p,
        sp,
        ppu_scanline,
        ppu_cycle,
        cyc,
    })
}

fn after<'a>(line: &'a str, marker: &str) -> Option<&'a str> {
    let idx = line.find(marker)?;
    Some(&line[idx + marker.len()..])
}

fn extract_hex_u8(line: &str, marker: &str) -> Option<u8> {
    let rest = after(line, marker)?;
    if rest.len() < 2 {
        return None;
    }
    u8::from_str_radix(&rest[0..2], 16).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_representative_line() {
        let line = "C000  4C F5 C5  JMP $C5F5                       A:00 X:00 Y:00 P:24 SP:FD PPU:  0, 21 CYC:7";
        let parsed = parse_line(line).unwrap();
        assert_eq!(parsed.pc, 0xC000);
        assert_eq!(parsed.bytes, vec![0x4C, 0xF5, 0xC5]);
        assert_eq!(parsed.disasm, "JMP $C5F5");
        assert_eq!(parsed.a, 0);
        assert_eq!(parsed.p, 0x24);
        assert_eq!(parsed.sp, 0xFD);
        assert_eq!(parsed.ppu_scanline, 0);
        assert_eq!(parsed.ppu_cycle, 21);
        assert_eq!(parsed.cyc, 7);
    }

    #[test]
    fn parses_large_ppu_values_with_no_space_after_comma() {
        let line = "C66E  60        RTS                             A:00 X:FF Y:15 P:27 SP:FD PPU:233,209 CYC:26554";
        let parsed = parse_line(line).unwrap();
        assert_eq!(parsed.ppu_scanline, 233);
        assert_eq!(parsed.ppu_cycle, 209);
        assert_eq!(parsed.cyc, 26554);
    }

    #[test]
    fn one_byte_instruction_has_empty_operand() {
        let line = "C72D  EA        NOP                             A:00 X:00 Y:00 P:26 SP:FB PPU:  0, 81 CYC:27";
        let parsed = parse_line(line).unwrap();
        assert_eq!(parsed.bytes, vec![0xEA]);
        assert_eq!(parsed.disasm, "NOP");
    }

    #[test]
    fn rejects_garbage_input() {
        assert!(parse_line("not a log line").is_none());
    }
}
