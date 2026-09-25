//! 行為指紋（[`Nes::behavior_fingerprint`]）的測試。規格見 `docs/architecture.md` §18.2。

use serde::Deserialize;
use serde_json::Value;

use crate::test_support::{
    ApuProbe, Asm, apu_probe_rom, build_mapper_rom, dummy_read_probe_rom, input_probe_rom,
    mmc1_churn_rom, rendering_rom,
};
use crate::{Buttons, FrameInput, Nes, StatusFlags};

/// 與 `lib.rs` 的 `behavior_fingerprint_is_pinned_to_the_version_numbers` 相同的固定輸入。
fn inputs_for(n: u64) -> Vec<FrameInput> {
    (0..n)
        .map(|i| {
            FrameInput::new(
                if i % 3 == 0 {
                    Buttons::LEFT
                } else {
                    Buttons::empty()
                },
                if i % 5 == 0 {
                    Buttons::A
                } else {
                    Buttons::empty()
                },
            )
        })
        .collect()
}

fn idle_mapper_rom(mapper: u8, prg_banks: usize, chr_banks_8k: usize) -> Vec<u8> {
    let mut code = Asm::new(0xE000);
    let forever = code.pc();
    code.jmp(forever);
    build_mapper_rom(mapper, prg_banks, chr_banks_8k, &code, &[])
}

fn ran(rom: &[u8], frames: u64) -> Nes {
    let mut nes = Nes::from_rom(rom).unwrap();
    for input in inputs_for(frames) {
        nes.run_frame(input);
    }
    nes
}

/// 不同機制的合成 ROM：NROM（渲染）、MMC1、UxROM（CHR-RAM）、CNROM、APU、輸入探針。
fn all_kinds_of_rom() -> Vec<(&'static str, Vec<u8>)> {
    vec![
        ("NROM 渲染", rendering_rom()),
        ("MMC1", mmc1_churn_rom()),
        ("UxROM", idle_mapper_rom(2, 8, 0)),
        ("CNROM", idle_mapper_rom(3, 2, 4)),
        ("APU 探針", apu_probe_rom(ApuProbe::DEFAULT)),
        ("輸入探針", input_probe_rom()),
    ]
}

/// (a) 存檔 → 讀檔之後指紋不變；包含「一幀中間」的狀態（單步除錯停下來的位置）。
#[test]
fn fingerprint_survives_save_and_load() {
    for (name, rom) in all_kinds_of_rom() {
        let mut nes = ran(&rom, 20);
        for _ in 0..3000 {
            nes.step_instruction(); // 停在一幀中間
        }
        let before = nes.behavior_fingerprint();
        let saved = nes.save_state();

        // 讀進一個「已經走到別處」的實例，指紋必須回到存檔當時的值。
        let mut other = ran(&rom, 7);
        assert_ne!(
            other.behavior_fingerprint(),
            before,
            "{name}：前提：兩者不同"
        );
        other.load_state(&saved).unwrap();
        assert_eq!(other.behavior_fingerprint(), before, "{name}");

        // 原地存檔再讀檔。
        nes.load_state(&saved).unwrap();
        assert_eq!(nes.behavior_fingerprint(), before, "{name}");
    }
}

/// (b) 輸出（畫面與音訊）開啟、關閉、中途切換，跑相同輸入，指紋逐幀相同。
/// 這正是指紋排除輸出的理由：rollback 重跑時會關閉輸出。
#[test]
fn fingerprint_is_independent_of_the_output_switch() {
    for (name, rom) in all_kinds_of_rom() {
        let mut on = Nes::from_rom(&rom).unwrap();
        let mut off = Nes::from_rom(&rom).unwrap();
        let mut toggled = Nes::from_rom(&rom).unwrap();
        off.set_output_enabled(false);
        for (frame, input) in inputs_for(80).into_iter().enumerate() {
            toggled.set_output_enabled(!(20..50).contains(&frame));
            on.run_frame(input);
            off.run_frame(input);
            toggled.run_frame(input);
            let fp = on.behavior_fingerprint();
            assert_eq!(
                fp,
                off.behavior_fingerprint(),
                "{name}：第 {frame} 幀（關閉）"
            );
            assert_eq!(
                fp,
                toggled.behavior_fingerprint(),
                "{name}：第 {frame} 幀（切換）"
            );
        }
    }
}

/// 沒有任何輸入差異的兩個實例指紋相同；不同的輸入序列則指紋不同。
#[test]
fn fingerprint_follows_the_input_sequence() {
    let rom = input_probe_rom();
    let a = ran(&rom, 30);
    let b = ran(&rom, 30);
    assert_eq!(a.behavior_fingerprint(), b.behavior_fingerprint());

    let mut c = Nes::from_rom(&rom).unwrap();
    for (i, mut input) in inputs_for(30).into_iter().enumerate() {
        if i == 17 {
            input.p2 |= Buttons::B;
        }
        c.run_frame(input);
    }
    assert_ne!(a.behavior_fingerprint(), c.behavior_fingerprint());
}

/// (c-1) 依類別各竄改一個欄位，指紋都會改變：CPU、RAM、PPU、mapper、APU（外加卡帶 RAM、搖桿）。
#[test]
fn tampering_one_field_per_category_changes_the_fingerprint() {
    fn changes(base: &Nes, what: &str, tamper: impl Fn(&mut Nes)) {
        let mut t = base.clone();
        tamper(&mut t);
        assert_ne!(
            t.behavior_fingerprint(),
            base.behavior_fingerprint(),
            "竄改「{what}」沒有改變指紋"
        );
    }

    let nrom = ran(&rendering_rom(), 5);
    // CPU
    changes(&nrom, "CPU：A 暫存器", |n| n.cpu.a ^= 1);
    changes(&nrom, "CPU：PC", |n| n.cpu.pc ^= 1);
    changes(&nrom, "CPU：狀態旗標", |n| {
        n.cpu.status.toggle(StatusFlags::CARRY)
    });
    changes(&nrom, "CPU：JAM 旗標", |n| n.cpu.jammed ^= true);
    // RAM
    changes(&nrom, "RAM：$0123", |n| n.cpu.bus_mut().ram[0x123] ^= 1);
    changes(&nrom, "RAM：最後一個位元組", |n| {
        n.cpu.bus_mut().ram[0x7FF] ^= 1
    });
    // PPU
    changes(&nrom, "PPU：VRAM", |n| n.cpu.bus_mut().ppu.vram[5] ^= 1);
    changes(&nrom, "PPU：調色盤", |n| {
        n.cpu.bus_mut().ppu.palette[3] ^= 1
    });
    changes(&nrom, "PPU：OAM", |n| n.cpu.bus_mut().ppu.oam[200] ^= 1);
    changes(&nrom, "PPU：PPUCTRL", |n| n.cpu.bus_mut().ppu.ctrl ^= 1);
    changes(&nrom, "PPU：loopy v", |n| n.cpu.bus_mut().ppu.v ^= 1);
    changes(&nrom, "PPU：loopy t", |n| n.cpu.bus_mut().ppu.t ^= 1);
    changes(&nrom, "PPU：fine X", |n| n.cpu.bus_mut().ppu.fine_x ^= 1);
    changes(&nrom, "PPU：loopy w", |n| n.cpu.bus_mut().ppu.w ^= true);
    changes(&nrom, "PPU：dot", |n| n.cpu.bus_mut().ppu.cycle ^= 1);
    changes(&nrom, "PPU：奇偶幀", |n| {
        n.cpu.bus_mut().ppu.odd_frame ^= true
    });
    // 搖桿
    changes(&nrom, "搖桿：移位暫存器", |n| {
        n.cpu.bus_mut().joypads[1].shift ^= 1
    });
    // PRG-RAM
    changes(&nrom, "卡帶：PRG-RAM", |n| {
        n.cpu.bus_mut().cartridge.prg_ram[77] ^= 1
    });

    // mapper（MMC1 的暫存器、UxROM 的 bank）與 CHR-RAM
    let mmc1 = ran(&mmc1_churn_rom(), 5);
    changes(&mmc1, "mapper：MMC1 PRG bank 暫存器", |n| {
        let cart = &mut n.cpu.bus_mut().cartridge;
        let bank = cart.read_prg(0x8100);
        // 寫 5 次把 bank 暫存器改成另一個值。
        let target = (bank ^ 1) & 0x0F;
        for i in 0..5 {
            cart.write_prg(0xE000, (target >> i) & 1, false);
        }
    });
    changes(
        &mmc1,
        "mapper：MMC1 移位暫存器（寫到一半）",
        |n| n.cpu.bus_mut().cartridge.write_prg(0x8000, 1, false),
    );
    let ux = ran(&idle_mapper_rom(2, 8, 0), 5);
    changes(&ux, "mapper：UxROM bank", |n| {
        n.cpu.bus_mut().cartridge.write_prg(0x8000, 3, false)
    });
    changes(&ux, "卡帶：CHR-RAM", |n| {
        n.cpu.bus_mut().cartridge.chr_ram[1234] ^= 1
    });
    let cn = ran(&idle_mapper_rom(3, 2, 4), 5);
    changes(&cn, "mapper：CNROM CHR bank", |n| {
        n.cpu.bus_mut().cartridge.write_prg(0x8000, 2, false)
    });

    // APU
    let apu = ran(&apu_probe_rom(ApuProbe::DEFAULT), 5);
    changes(&apu, "APU：pulse 1 計時器週期", |n| {
        n.cpu.bus_mut().apu.pulse[0].timer_period ^= 1
    });
    changes(&apu, "APU：pulse 2 長度計數器", |n| {
        n.cpu.bus_mut().apu.pulse[1].length.counter ^= 1
    });
    changes(&apu, "APU：triangle 序列位置", |n| {
        n.cpu.bus_mut().apu.triangle.seq ^= 1
    });
    changes(&apu, "APU：noise 移位暫存器", |n| {
        n.cpu.bus_mut().apu.noise.shift ^= 1
    });
    changes(&apu, "APU：DMC 輸出電平", |n| {
        n.cpu.bus_mut().apu.dmc.output_level ^= 1
    });
    changes(&apu, "APU：DMC IRQ 旗標", |n| {
        n.cpu.bus_mut().apu.dmc.irq_flag ^= true
    });
    changes(&apu, "APU：frame counter 步驟", |n| {
        n.cpu.bus_mut().apu.frame.step ^= 1
    });
    changes(&apu, "APU：frame counter 模式", |n| {
        n.cpu.bus_mut().apu.frame.mode5 ^= true
    });
}

/// (c-2) 自動的完整性檢查：把存檔（serde）裡的**每一個**欄位竄改一次（數值 XOR 1、布林反轉），
/// 指紋都必須改變。將來新增的狀態欄位只要有進存檔，這個測試就會逼你把它納入指紋——漏掉的會
/// 在這裡失敗。
///
/// 只有兩類欄位刻意不納入：靜態的卡帶資料（iNES header 中繼資料、`rom_id`、NROM 由 header 決定的
/// bank 數；它們不會在模擬中改變，由 `rom_id` 識別），以及 `#[serde(skip)]` 的輸出（framebuffer、
/// 音訊管線，序列化時根本不存在）。大型陣列（RAM、VRAM、CHR-RAM……）只取頭、尾與中間的
/// 幾個元素（逐位元組雜湊由 c-1 的竄改保證）。
#[test]
fn every_serialized_state_field_is_covered_by_the_fingerprint() {
    /// 刻意不納入指紋的欄位路徑前綴（靜態、不隨模擬改變）。
    const STATIC_PREFIXES: [&str; 3] = [
        "cpu.bus.cartridge.info",
        "cpu.bus.cartridge.rom_id",
        "cpu.bus.cartridge.mapper.Nrom.prg_banks",
    ];

    #[derive(Clone)]
    enum Seg {
        Key(String),
        Idx(usize),
    }
    fn render(path: &[Seg]) -> String {
        let mut s = String::new();
        for seg in path {
            match seg {
                Seg::Key(k) => {
                    if !s.is_empty() {
                        s.push('.');
                    }
                    s.push_str(k);
                }
                Seg::Idx(i) => s.push_str(&format!("[{i}]")),
            }
        }
        s
    }
    fn collect(v: &Value, path: &mut Vec<Seg>, out: &mut Vec<Vec<Seg>>) {
        match v {
            Value::Object(map) => {
                for (k, child) in map {
                    path.push(Seg::Key(k.clone()));
                    collect(child, path, out);
                    path.pop();
                }
            }
            Value::Array(items) => {
                let indices: Vec<usize> = if items.len() > 64 {
                    vec![0, 1, items.len() / 2, items.len() - 1]
                } else {
                    (0..items.len()).collect()
                };
                for i in indices {
                    path.push(Seg::Idx(i));
                    collect(&items[i], path, out);
                    path.pop();
                }
            }
            _ => out.push(path.clone()),
        }
    }
    fn slot<'a>(root: &'a mut Value, path: &[Seg]) -> &'a mut Value {
        let mut cur = root;
        for seg in path {
            cur = match seg {
                Seg::Key(k) => cur.get_mut(k.as_str()).unwrap(),
                Seg::Idx(i) => cur.get_mut(*i).unwrap(),
            };
        }
        cur
    }
    /// 一個葉節點的「不同的值」候選。bitflags 在 JSON 是字串（旗標名稱以 ` | ` 相連）。
    fn candidates(v: &Value) -> Vec<Value> {
        match v {
            Value::Bool(b) => vec![Value::Bool(!b)],
            Value::Number(n) => vec![Value::from(n.as_u64().expect("狀態欄位都是非負整數") ^ 1)],
            Value::Null => vec![Value::from(1u8)],
            Value::String(s) if s.is_empty() => vec!["A".into(), "CARRY".into()],
            Value::String(_) => vec!["".into()],
            other => panic!("預期之外的葉節點 {other:?}"),
        }
    }

    let mut checked = 0usize;
    for (name, rom) in all_kinds_of_rom() {
        let nes = ran(&rom, 12);
        let base = nes.behavior_fingerprint();
        let mut json = serde_json::to_value(&nes).unwrap();

        let mut leaves = Vec::new();
        collect(&json, &mut Vec::new(), &mut leaves);
        assert!(
            leaves.len() > 150,
            "{name}：葉節點數量異常 {}",
            leaves.len()
        );

        let mut missed = Vec::new();
        for path in &leaves {
            let label = render(path);
            if STATIC_PREFIXES.iter().any(|p| label.starts_with(p)) {
                continue;
            }
            let original = slot(&mut json, path).clone();
            let mut covered = None;
            for candidate in candidates(&original) {
                *slot(&mut json, path) = candidate;
                if let Ok(mut changed) = Nes::deserialize(&json) {
                    // ROM 內容不進存檔（`#[serde(skip)]`），從原本的實例接回。
                    let cart = &mut changed.cpu.bus_mut().cartridge;
                    cart.prg_rom = nes.cpu.bus().cartridge.prg_rom.clone();
                    cart.chr_rom = nes.cpu.bus().cartridge.chr_rom.clone();
                    covered = Some(changed.behavior_fingerprint() != base);
                    break;
                }
            }
            *slot(&mut json, path) = original;
            match covered {
                Some(true) => checked += 1,
                Some(false) => missed.push(label),
                None => panic!("{name}：{label} 找不到可以還原成 Nes 的竄改值"),
            }
        }
        assert!(
            missed.is_empty(),
            "{name}：下列存檔欄位沒有納入行為指紋（請在對應的 fingerprint 方法寫入，並更新 \
             docs/architecture.md §18.2）：{missed:#?}"
        );
    }
    eprintln!("c′：共竄改並確認了 {checked} 個欄位");
    assert!(checked > 1000, "檢查過的欄位數量 {checked}");
}

/// (d) 回歸守衛：合成 ROM 在固定輸入下的行為指紋。這些值**不含**存檔格式或 header：存檔
/// 格式版本改變時它們不會動（見 (e) 與 §18.4）；只有模擬行為改變才會動——那時要依
/// `docs/architecture.md` §15 遞增 `CORE_BEHAVIOR_VERSION`，而不是只更新這裡的常數。
#[test]
fn fingerprint_is_pinned_for_synthetic_roms() {
    const FRAMES: u64 = 60;
    // 這五個值是在 `STATE_FORMAT_VERSION` 還是 2（存檔含 `rom_hash`）時記錄的，升到 3 之後
    // 沒有改動——那就是「與格式無關」的實證（見 (e) 與 §18.4）。
    const NROM: u64 = 0x5f315a76f9b81d04;
    const MMC1: u64 = 0x3a04bf962e35936d;
    const DUMMY_READ: u64 = 0x0a8ddb2282a8ab46;
    const APU_PROBE: u64 = 0x0683c3d5fc4c3b4b;
    const INPUT_PROBE_WITH_RESET: u64 = 0xf1d3718e4b0f7d98;

    let fp = |rom: &[u8]| ran(rom, FRAMES).behavior_fingerprint();
    // 輸入探針另外在第 30 幀按一次 reset，把 soft reset 的行為也釘住。
    let mut probe = Nes::from_rom(&input_probe_rom()).unwrap();
    for (i, input) in inputs_for(FRAMES).into_iter().enumerate() {
        probe.run_frame(if i == 30 { input.with_reset() } else { input });
    }

    let actual = (
        fp(&rendering_rom()),
        fp(&mmc1_churn_rom()),
        fp(&dummy_read_probe_rom()),
        fp(&apu_probe_rom(ApuProbe::DEFAULT)),
        probe.behavior_fingerprint(),
    );
    assert_eq!(
        actual,
        (NROM, MMC1, DUMMY_READ, APU_PROBE, INPUT_PROBE_WITH_RESET),
        "行為指紋改變：模擬行為變了嗎？見測試上方的說明"
    );
}

/// (e) 「與格式無關」的佐證：把整台 `Nes` 換成**完全不同的序列化格式**（serde_json 的文字，
/// 而不是 postcard 的位元組）往返一次，指紋不變。指紋是狀態「數值」的函數，不是任何編碼的位元組。
///
/// 這證明到的程度：指紋不依賴 postcard（或存檔 header）的編碼結果。**沒有**證明的部分：
/// 1. 無法證明「將來新增的欄位一定被納入」——那由 (c-2) 的自動完整性檢查負責；
/// 2. 指紋是手寫的欄位順序，所以它依賴「規格」（§18.2）而不是 Rust 的 struct 佈局；重排 struct 的欄位
///    不會改變它，但改寫入順序會（由 (d) 的釘住值抓到）。
///
/// 另一個歷史性的佐證：Phase 4a 把 `STATE_FORMAT_VERSION` 由 2 升到 3（`rom_hash` → `rom_id`），
/// 存檔位元組與 `state_hash` 全都變了，而 (d) 釘住的指紋在升版之前就已經記錄、升版後**逐位元不變**
/// （見 §18.4 的實測紀錄）。
#[test]
fn fingerprint_does_not_depend_on_the_serialization_format() {
    for (name, rom) in all_kinds_of_rom() {
        let nes = ran(&rom, 25);
        let via_postcard = nes.save_state();

        let json = serde_json::to_string(&nes).unwrap();
        let mut restored: Nes = serde_json::from_str(&json).unwrap();
        let cart = &mut restored.cpu.bus_mut().cartridge;
        cart.prg_rom = nes.cpu.bus().cartridge.prg_rom.clone();
        cart.chr_rom = nes.cpu.bus().cartridge.chr_rom.clone();

        assert_eq!(
            restored.behavior_fingerprint(),
            nes.behavior_fingerprint(),
            "{name}：經 JSON 往返後指紋不同"
        );
        // 前提：兩種編碼真的完全不同，且 JSON 往返後再存成 postcard 與原本的位元組相同。
        assert_ne!(json.as_bytes(), &via_postcard[..]);
        assert_eq!(restored.save_state(), via_postcard, "{name}");
    }
}
