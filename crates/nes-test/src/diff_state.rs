//! `diff-state`：解碼兩份存檔，以 `serde_json::Value` 為中介逐欄位比對。
//!
//! 只有**不同**的欄位會列出，格式 `欄位路徑: 左邊的值 vs 右邊的值`（例如 `ppu.v: 0x2104 vs 0x2105`）。
//! 路徑省略 `cpu.bus.` 前綴（匯流排底下的 `ppu`、`apu`、`ram`、`cartridge`…直接寫成頂層）。
//! 大型陣列（RAM、VRAM、OAM、CHR-RAM、PRG-RAM……）不傾印內容，只列出**不同的索引範圍**與首個差異。
//!
//! 比對的是「存檔裡的欄位」：`#[serde(skip)]` 的東西（framebuffer、音訊輸出管線、ROM 內容）本來就不在存檔
//! 裡；`cartridge.rom_id` 與 `info` 是靜態資料，仍會被比對（兩份存檔屬於不同 ROM 時會直接看到）。

use serde_json::Value;

/// 元素全是數字、且長度超過這個數量的陣列，改用「索引範圍」的摘要輸出。
const LARGE_ARRAY_LEN: usize = 16;
/// 最多列出幾段差異範圍。
const MAX_RANGES_SHOWN: usize = 8;

/// 比對兩個狀態，回傳每個差異的一行說明。
pub fn diff(a: &Value, b: &Value) -> Vec<String> {
    let mut out = Vec::new();
    walk(&mut String::new(), a, b, &mut out);
    out
}

fn display_path(path: &str) -> String {
    let path = path.strip_prefix("cpu.bus.").unwrap_or(path);
    if path.is_empty() {
        "(根)".to_string()
    } else {
        path.to_string()
    }
}

fn join(path: &str, key: &str) -> String {
    if path.is_empty() {
        key.to_string()
    } else {
        format!("{path}.{key}")
    }
}

/// 整數以十六進位顯示（超過 `0xFFFF` 的另外附十進位，例如 cycle 計數）；其他型別用 JSON 表示。
fn show(v: &Value) -> String {
    match v {
        Value::Number(n) => match n.as_u64() {
            Some(x) if x <= 0xFF => format!("{x:#04X}"),
            Some(x) if x <= 0xFFFF => format!("{x:#06X}"),
            Some(x) => format!("{x:#X} ({x})"),
            None => n.to_string(),
        },
        other => other.to_string(),
    }
}

fn is_large_numeric_array(items: &[Value]) -> bool {
    items.len() > LARGE_ARRAY_LEN && items.iter().all(Value::is_number)
}

fn walk(path: &mut String, a: &Value, b: &Value, out: &mut Vec<String>) {
    match (a, b) {
        (Value::Object(ma), Value::Object(mb)) => {
            // 鍵依字母排序（serde_json 預設用 BTreeMap）；兩邊各自獨有的鍵也要列出。
            for (key, va) in ma {
                let child = join(path, key);
                match mb.get(key) {
                    Some(vb) => walk(&mut child.clone(), va, vb, out),
                    None => out.push(format!("{}: 只存在於左邊", display_path(&child))),
                }
            }
            for key in mb.keys().filter(|k| !ma.contains_key(*k)) {
                out.push(format!("{}: 只存在於右邊", display_path(&join(path, key))));
            }
        }
        (Value::Array(xa), Value::Array(xb)) => {
            if xa.len() != xb.len() {
                out.push(format!(
                    "{}: 陣列長度 {} vs {}",
                    display_path(path),
                    xa.len(),
                    xb.len()
                ));
            } else if is_large_numeric_array(xa) && is_large_numeric_array(xb) {
                diff_large_array(path, xa, xb, out);
            } else {
                for (i, (va, vb)) in xa.iter().zip(xb).enumerate() {
                    walk(&mut format!("{path}[{i}]"), va, vb, out);
                }
            }
        }
        _ => {
            if a != b {
                out.push(format!(
                    "{}: {} vs {}",
                    display_path(path),
                    show(a),
                    show(b)
                ));
            }
        }
    }
}

fn diff_large_array(path: &str, xa: &[Value], xb: &[Value], out: &mut Vec<String>) {
    // 連續的不同索引合併成範圍。
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    let mut count = 0usize;
    for (i, (va, vb)) in xa.iter().zip(xb).enumerate() {
        if va != vb {
            count += 1;
            match ranges.last_mut() {
                Some((_, end)) if *end + 1 == i => *end = i,
                _ => ranges.push((i, i)),
            }
        }
    }
    let Some(&(first, _)) = ranges.first() else {
        return;
    };
    let shown: Vec<String> = ranges
        .iter()
        .take(MAX_RANGES_SHOWN)
        .map(|&(s, e)| {
            if s == e {
                format!("[{s:#06X}]")
            } else {
                format!("[{s:#06X}..={e:#06X}]")
            }
        })
        .collect();
    let more = ranges.len().saturating_sub(MAX_RANGES_SHOWN);
    out.push(format!(
        "{}: {} 個元素不同，分成 {} 段：{}{}；首個差異 [{first:#06X}]：{} vs {}",
        display_path(path),
        count,
        ranges.len(),
        shown.join(" "),
        if more > 0 {
            format!(" …另 {more} 段")
        } else {
            String::new()
        },
        show(&xa[first]),
        show(&xb[first]),
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn identical_values_have_no_differences() {
        let v = json!({"cpu": {"a": 1, "bus": {"ram": vec![0u8; 64]}}});
        assert!(diff(&v, &v.clone()).is_empty());
    }

    #[test]
    fn scalar_differences_show_path_and_both_values() {
        let a = json!({"cpu": {"pc": 0x8000, "bus": {"ppu": {"v": 0x2104, "w": false}}}});
        let b = json!({"cpu": {"pc": 0x8003, "bus": {"ppu": {"v": 0x2105, "w": true}}}});
        let lines = diff(&a, &b);
        assert_eq!(
            lines,
            [
                // 鍵依字母排序：`bus`（匯流排底下的 ppu）在 `pc` 之前。
                "ppu.v: 0x2104 vs 0x2105",
                "ppu.w: false vs true",
                "cpu.pc: 0x8000 vs 0x8003",
            ]
        );
    }

    #[test]
    fn large_values_also_show_decimal_and_bitflag_strings_are_quoted() {
        let a = json!({"cpu": {"status": "ZERO", "bus": {"total_cycles": 1_000_000}}});
        let b = json!({"cpu": {"status": "CARRY | ZERO", "bus": {"total_cycles": 1_000_004}}});
        let lines = diff(&a, &b);
        assert!(lines.contains(&"cpu.status: \"ZERO\" vs \"CARRY | ZERO\"".to_string()));
        assert!(
            lines.contains(&"total_cycles: 0xF4240 (1000000) vs 0xF4244 (1000004)".to_string())
        );
    }

    /// 大型陣列不傾印內容：只有索引範圍與首個差異。
    #[test]
    fn large_arrays_report_index_ranges_not_contents() {
        let mut ram_a = vec![0u8; 2048];
        let mut ram_b = ram_a.clone();
        ram_b[0x10..=0x13].fill(7);
        ram_b[0x7FF] = 1;
        ram_a[0x100] = 9;
        ram_b[0x100] = 9; // 相同的不列
        let a = json!({"cpu": {"bus": {"ram": ram_a}}});
        let b = json!({"cpu": {"bus": {"ram": ram_b}}});
        let lines = diff(&a, &b);
        assert_eq!(lines.len(), 1, "{lines:?}");
        let line = &lines[0];
        assert!(line.starts_with("ram: 5 個元素不同，分成 2 段："), "{line}");
        assert!(
            line.contains("[0x0010..=0x0013]") && line.contains("[0x07FF]"),
            "{line}"
        );
        assert!(line.contains("首個差異 [0x0010]：0x00 vs 0x07"), "{line}");
        // 輸出不會隨陣列大小成長。
        assert!(line.len() < 200, "{line}");
    }

    #[test]
    fn many_ranges_are_truncated() {
        let a = json!({"x": vec![0u8; 100]});
        let mut v = vec![0u8; 100];
        for i in (0..40).step_by(2) {
            v[i] = 1;
        }
        let b = json!({"x": v});
        let lines = diff(&a, &b);
        assert!(
            lines[0].contains("分成 20 段") && lines[0].contains("…另 12 段"),
            "{lines:?}"
        );
    }

    #[test]
    fn small_arrays_and_nested_structures_are_compared_element_by_element() {
        let a = json!({"apu": {"pulse": [{"seq": 1, "cnt": 5}, {"seq": 2, "cnt": 6}]}});
        let b = json!({"apu": {"pulse": [{"seq": 1, "cnt": 5}, {"seq": 3, "cnt": 6}]}});
        assert_eq!(diff(&a, &b), ["apu.pulse[1].seq: 0x02 vs 0x03"]);
    }

    #[test]
    fn keys_and_lengths_that_exist_on_one_side_only_are_reported() {
        let a = json!({"mapper": {"Nrom": {"prg_banks": 2}}, "list": [1, 2, 3]});
        let b = json!({"mapper": {"Mmc1": {"control": 12}}, "list": [1, 2]});
        let lines = diff(&a, &b);
        assert!(
            lines.contains(&"mapper.Nrom: 只存在於左邊".to_string()),
            "{lines:?}"
        );
        assert!(
            lines.contains(&"mapper.Mmc1: 只存在於右邊".to_string()),
            "{lines:?}"
        );
        assert!(
            lines.contains(&"list: 陣列長度 3 vs 2".to_string()),
            "{lines:?}"
        );
    }
}
