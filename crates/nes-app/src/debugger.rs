//! Debugger 面板的分頁內容：CPU、PPU 暫存器、調色盤、OAM、pattern table、nametable。
//!
//! 這個模組只負責「顯示」：資料來自 emu 執行緒送來的 `DebugSnapshot` 與
//! `PpuViews`（見 `emu.rs`）。控制項（暫停／單步／trace）在 `app.rs`。
//!
//! PPU 影像（pattern table / nametable）要畫 6 張圖，所以只在「面板開著且目前分頁
//! 是 pattern table / nametable」時才向 emu 執行緒要求（`EmuCommand::SetDebugViews`），
//! 其他情況零成本。

use crossbeam_channel::Sender;
use eframe::egui;
use nes_core::ppu::SYSTEM_PALETTE;
use nes_core::{DebugSnapshot, PpuImage, PpuViews};

use crate::commands::EmuCommand;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Cpu,
    Ppu,
    Palette,
    Oam,
    Patterns,
    Nametables,
}

impl Tab {
    const ALL: [(Tab, &'static str); 6] = [
        (Tab::Cpu, "CPU"),
        (Tab::Ppu, "PPU"),
        (Tab::Palette, "調色盤"),
        (Tab::Oam, "OAM"),
        (Tab::Patterns, "Pattern"),
        (Tab::Nametables, "Nametable"),
    ];

    /// 這個分頁要不要 `PpuViews`。
    pub fn needs_views(self) -> bool {
        matches!(self, Tab::Patterns | Tab::Nametables)
    }
}

/// 6 張 PPU 影像的 texture：0–1 是 pattern table，2–5 是 nametable。
const VIEW_TEXTURES: usize = 6;

pub struct DebuggerUi {
    tab: Tab,
    /// pattern table 用的調色盤（0–3 背景、4–7 精靈）。
    view_palette: u8,
    /// 目前已向 emu 執行緒要求的 `SetDebugViews` 內容（避免每幀重複送）。
    views_requested: Option<u8>,
    views_output: triple_buffer::Output<Option<PpuViews>>,
    textures: [Option<egui::TextureHandle>; VIEW_TEXTURES],
}

impl DebuggerUi {
    pub fn new(views_output: triple_buffer::Output<Option<PpuViews>>) -> Self {
        Self {
            tab: Tab::Cpu,
            view_palette: 0,
            views_requested: None,
            views_output,
            textures: Default::default(),
        }
    }

    /// 每個 UI 幀呼叫：依「面板是否開著、分頁是否需要影像」決定要不要（以及用哪個
    /// 調色盤）向 emu 執行緒要求 PPU 影像，只在需求改變時才送指令；需要時把新影像
    /// 上傳成 texture。
    pub fn sync_views(
        &mut self,
        ctx: &egui::Context,
        panel_open: bool,
        cmd_tx: &Sender<EmuCommand>,
    ) {
        let wanted = (panel_open && self.tab.needs_views()).then_some(self.view_palette);
        if wanted != self.views_requested {
            self.views_requested = wanted;
            let _ = cmd_tx.send(EmuCommand::SetDebugViews(wanted));
        }
        if wanted.is_none() {
            return;
        }
        // 只在 emu 執行緒真的送來新影像時才重新上傳 texture。
        if self.views_output.update()
            && let Some(views) = self.views_output.output_buffer()
        {
            let images = views.pattern_tables.iter().chain(views.nametables.iter());
            for (slot, image) in self.textures.iter_mut().zip(images) {
                upload(ctx, slot, image);
            }
        }
    }

    pub fn show(&mut self, ui: &mut egui::Ui, snapshot: Option<&DebugSnapshot>) {
        ui.horizontal_wrapped(|ui| {
            for (tab, label) in Tab::ALL {
                ui.selectable_value(&mut self.tab, tab, label);
            }
        });
        ui.separator();

        let Some(snap) = snapshot else {
            ui.label("尚未取得資料");
            return;
        };

        egui::ScrollArea::vertical().show(ui, |ui| match self.tab {
            Tab::Cpu => cpu_tab(ui, snap),
            Tab::Ppu => ppu_tab(ui, snap),
            Tab::Palette => palette_tab(ui, snap),
            Tab::Oam => oam_tab(ui, snap),
            Tab::Patterns => self.patterns_tab(ui),
            Tab::Nametables => self.nametables_tab(ui),
        });
    }

    fn patterns_tab(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            ui.label("調色盤：");
            for p in 0..8u8 {
                let label = if p < 4 {
                    format!("BG{p}")
                } else {
                    format!("SP{}", p - 4)
                };
                ui.selectable_value(&mut self.view_palette, p, label);
            }
        });
        let size = ui.available_width().min(256.0);
        for (i, name) in ["$0000", "$1000"].into_iter().enumerate() {
            ui.label(format!("Pattern table {name}"));
            show_texture(ui, self.textures[i].as_ref(), egui::vec2(size, size));
        }
    }

    fn nametables_tab(&mut self, ui: &mut egui::Ui) {
        let spacing = ui.spacing().item_spacing.x;
        let width = ((ui.available_width() - spacing) / 2.0).clamp(64.0, 256.0);
        let size = egui::vec2(width, width * 240.0 / 256.0);
        egui::Grid::new("nametables").show(ui, |ui| {
            for row in 0..2 {
                for col in 0..2 {
                    let i = row * 2 + col;
                    ui.vertical(|ui| {
                        ui.label(format!("${:04X}", 0x2000 + i * 0x400));
                        show_texture(ui, self.textures[2 + i].as_ref(), size);
                    });
                }
                ui.end_row();
            }
        });
    }
}

fn upload(ctx: &egui::Context, slot: &mut Option<egui::TextureHandle>, image: &PpuImage) {
    let color_image =
        egui::ColorImage::from_rgba_unmultiplied([image.width, image.height], &image.rgba);
    match slot {
        Some(texture) => texture.set(color_image, egui::TextureOptions::NEAREST),
        None => {
            *slot = Some(ctx.load_texture(
                format!("ppu-view-{}x{}", image.width, image.height),
                color_image,
                egui::TextureOptions::NEAREST,
            ));
        }
    }
}

fn show_texture(ui: &mut egui::Ui, texture: Option<&egui::TextureHandle>, size: egui::Vec2) {
    match texture {
        Some(texture) => {
            ui.add(egui::Image::new(texture).fit_to_exact_size(size));
        }
        None => {
            ui.label("（等待影像…）");
        }
    }
}

fn cpu_tab(ui: &mut egui::Ui, snap: &DebugSnapshot) {
    ui.label(format!("PC: {:#06X}", snap.cpu_pc));
    ui.label(format!(
        "A: {:#04X}  X: {:#04X}  Y: {:#04X}",
        snap.cpu_a, snap.cpu_x, snap.cpu_y
    ));
    ui.label(format!(
        "SP: {:#04X}  Status: {:#04X}",
        snap.cpu_sp, snap.cpu_status
    ));
    ui.label(format!("CPU cycles: {}", snap.cpu_cycles));
    ui.label(format!("下一條指令: {}", snap.cpu_disassembly));
    if snap.cpu_jammed {
        ui.colored_label(egui::Color32::RED, "CPU JAMMED");
    }
    ui.separator();
    ui.label(format!("APU frame counter: {}", snap.apu_frame_counter));
}

fn flag(value: u8, bit: u8) -> u8 {
    (value >> bit) & 1
}

fn ppu_tab(ui: &mut egui::Ui, snap: &DebugSnapshot) {
    ui.label(format!(
        "掃描線: {}  dot: {}  PPU 幀: {}",
        snap.ppu_scanline, snap.ppu_cycle, snap.ppu_frame
    ));
    ui.separator();

    let c = snap.ppu_ctrl;
    ui.label(format!("PPUCTRL   ${c:02X}"));
    ui.monospace(format!(
        "NMI:{} 精靈8x16:{} BG表:{} 精靈表:{} 遞增32:{} NT:{}",
        flag(c, 7),
        flag(c, 5),
        flag(c, 4),
        flag(c, 3),
        flag(c, 2),
        c & 3
    ));
    let m = snap.ppu_mask;
    ui.label(format!("PPUMASK   ${m:02X}"));
    ui.monospace(format!(
        "灰階:{} BG左:{} 精靈左:{} BG:{} 精靈:{} 強調 RGB:{}{}{}",
        flag(m, 0),
        flag(m, 1),
        flag(m, 2),
        flag(m, 3),
        flag(m, 4),
        flag(m, 5),
        flag(m, 6),
        flag(m, 7)
    ));
    let s = snap.ppu_status;
    ui.label(format!("PPUSTATUS ${s:02X}"));
    ui.monospace(format!(
        "VBlank:{} Sprite0:{} Overflow:{}",
        flag(s, 7),
        flag(s, 6),
        flag(s, 5)
    ));
    ui.label(format!("OAMADDR   ${:02X}", snap.ppu_oam_addr));
    ui.separator();

    let v = snap.ppu_v;
    ui.label(format!("v: ${v:04X}  t: ${:04X}", snap.ppu_t));
    ui.monospace(format!(
        "v → coarseX:{} coarseY:{} NT:{} fineY:{}",
        v & 0x1F,
        (v >> 5) & 0x1F,
        (v >> 10) & 3,
        (v >> 12) & 7
    ));
    ui.label(format!(
        "fine X: {}  w: {}",
        snap.ppu_fine_x,
        u8::from(snap.ppu_w)
    ));
}

fn palette_index(i: usize) -> usize {
    if i & 3 == 0 { i & 0x0F } else { i }
}

fn palette_tab(ui: &mut egui::Ui, snap: &DebugSnapshot) {
    for group in 0..8usize {
        ui.horizontal(|ui| {
            let name = if group < 4 {
                format!("BG{group}")
            } else {
                format!("SP{}", group - 4)
            };
            ui.monospace(format!("{name:<4}"));
            for i in 0..4usize {
                let value = snap.palette_ram[palette_index(group * 4 + i)] & 0x3F;
                let [r, g, b] = SYSTEM_PALETTE[value as usize];
                let (rect, response) =
                    ui.allocate_exact_size(egui::vec2(28.0, 22.0), egui::Sense::hover());
                ui.painter()
                    .rect_filled(rect, 0.0, egui::Color32::from_rgb(r, g, b));
                response.on_hover_text(format!("${:02X}: #{r:02X}{g:02X}{b:02X}", value));
            }
            let raw: Vec<String> = (0..4)
                .map(|i| format!("{:02X}", snap.palette_ram[palette_index(group * 4 + i)]))
                .collect();
            ui.monospace(raw.join(" "));
        });
    }
}

fn oam_tab(ui: &mut egui::Ui, snap: &DebugSnapshot) {
    egui::Grid::new("oam").striped(true).show(ui, |ui| {
        for header in ["#", "Y", "Tile", "Attr", "X"] {
            ui.strong(header);
        }
        ui.end_row();
        for (n, sprite) in snap.oam.as_chunks::<4>().0.iter().enumerate() {
            ui.monospace(format!("{n:02}"));
            ui.monospace(format!("{:02X}", sprite[0]));
            ui.monospace(format!("{:02X}", sprite[1]));
            ui.monospace(format!("{:02X}", sprite[2]));
            ui.monospace(format!("{:02X}", sprite[3]));
            ui.end_row();
        }
    });
}
