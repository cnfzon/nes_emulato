//! 6502 CPU 暫存器狀態。
//!
//! `Cpu` 擁有 `Bus`（`Nes` 擁有 `Cpu`），所有記憶體存取都透過
//! `self.bus.read` / `self.bus.write` 進行，因此之後實作定址模式時不需要
//! 改動所有權結構。

use crate::bus::Bus;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Cpu {
    pub a: u8,
    pub x: u8,
    pub y: u8,
    pub sp: u8,
    pub pc: u16,
    pub status: u8,
    pub cycles: u64,
    bus: Bus,
}

impl Cpu {
    pub fn new(bus: Bus) -> Self {
        Self {
            a: 0,
            x: 0,
            y: 0,
            sp: 0xFD,
            pc: 0,
            status: 0x24,
            cycles: 0,
            bus,
        }
    }

    pub fn bus(&self) -> &Bus {
        &self.bus
    }

    pub fn bus_mut(&mut self) -> &mut Bus {
        &mut self.bus
    }

    /// 執行一條指令：fetch → decode → execute，回傳消耗的 CPU 週期數。
    ///
    /// TODO Phase 1: 照 bugzmanov 教材的架構實作定址模式表 + opcode 表，
    /// 讓 `run_frame` 改成「跑到這一幀的 CPU 週期預算用完為止」而不是現在
    /// 這樣完全不呼叫 CPU。目前回傳 `0` 是正確行為（因為根本還沒有指令可
    /// 執行），不是 stub 佔位錯誤。
    pub fn step(&mut self) -> u8 {
        0
    }
}
