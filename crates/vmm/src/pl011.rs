//! A PL011 UART: enough for the Linux amba-pl011 driver to probe it and run a console both ways.
use crate::{
    console::Console,
    mmio::{Irq, Mmio},
};
use std::sync::Mutex;

const RX: u32 = 1 << 4;
const TX: u32 = 1 << 5;
const RX_TIMEOUT: u32 = 1 << 6;
/// PrimeCell peripheral and cell IDs at 0xfe0, which the AMBA bus reads to find a driver.
const IDS: [u8; 8] = [0x11, 0x10, 0x14, 0x00, 0x0d, 0xf0, 0x05, 0xb1];

pub struct Pl011 {
    console: Console,
    irq: Irq,
    line: u32,
    registers: Mutex<Registers>,
}

#[derive(Default)]
struct Registers {
    ibrd: u32,
    fbrd: u32,
    lcr: u32,
    cr: u32,
    ifls: u32,
    imsc: u32,
}

impl Pl011 {
    pub fn new(console: Console, irq: Irq, line: u32) -> Self {
        Self {
            console,
            irq,
            line,
            registers: Mutex::default(),
        }
    }

    fn raw_status(&self) -> u32 {
        // The transmitter never buffers, so it always asks for more, as an empty FIFO does.
        TX | if self.console.has_input() {
            RX | RX_TIMEOUT
        } else {
            0
        }
    }

    /// Drives the line from the current state; called after anything that may change it.
    pub fn update(&self) {
        let imsc = self.registers.lock().unwrap().imsc;
        (self.irq)(self.line, self.raw_status() & imsc != 0);
    }
}

impl Mmio for Pl011 {
    fn read(&self, offset: u64, data: &mut [u8]) {
        let registers = self.registers.lock().unwrap();
        let value = match offset {
            0x000 => self.console.read().map_or(0, u32::from),
            // TX FIFO empty, and RX FIFO empty when there is no input.
            0x018 => 0x80 | if self.console.has_input() { 0 } else { 0x10 },
            0x024 => registers.ibrd,
            0x028 => registers.fbrd,
            0x02c => registers.lcr,
            0x030 => registers.cr,
            0x034 => registers.ifls,
            0x038 => registers.imsc,
            0x03c => self.raw_status(),
            0x040 => self.raw_status() & registers.imsc,
            0xfe0..=0xffc => IDS[((offset - 0xfe0) / 4) as usize % 8].into(),
            _ => 0,
        };
        drop(registers);
        for (byte, value) in data.iter_mut().zip(value.to_le_bytes()) {
            *byte = value;
        }
        self.update();
    }

    fn write(&self, offset: u64, data: &[u8]) {
        let mut bytes = [0; 4];
        let length = data.len().min(4);
        bytes[..length].copy_from_slice(&data[..length]);
        let value = u32::from_le_bytes(bytes);
        {
            let mut registers = self.registers.lock().unwrap();
            match offset {
                0x000 => self.console.output(value as u8),
                0x024 => registers.ibrd = value,
                0x028 => registers.fbrd = value,
                0x02c => registers.lcr = value,
                0x030 => registers.cr = value,
                0x034 => registers.ifls = value,
                0x038 => registers.imsc = value,
                _ => (),
            }
        }
        self.update();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    #[test]
    fn identifies_itself_and_carries_bytes_with_interrupts() {
        let console = Console::default();
        let level = Arc::new(AtomicBool::new(false));
        let seen = level.clone();
        let irq: Irq = Arc::new(move |_, on| seen.store(on, Ordering::SeqCst));
        let uart = Pl011::new(console.clone(), irq, 1);
        let read = |offset| {
            let mut data = [0; 4];
            uart.read(offset, &mut data);
            u32::from_le_bytes(data)
        };
        assert_eq!(read(0xfe0), 0x11);
        assert_eq!(read(0xffc), 0xb1);
        assert_eq!(read(0x018) & 0x10, 0x10);
        uart.write(0x038, &RX.to_le_bytes());
        assert!(!level.load(Ordering::SeqCst));
        console.input(b"a");
        uart.update();
        assert!(level.load(Ordering::SeqCst));
        assert_eq!(read(0x018) & 0x10, 0);
        assert_eq!(read(0x000), u32::from(b'a'));
        assert!(!level.load(Ordering::SeqCst));
    }
}
