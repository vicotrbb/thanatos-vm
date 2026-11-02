//! A 16550 UART: enough for the Linux 8250 driver to probe it and run a console both ways.
use crate::console::Console;

const DLAB: u8 = 0x80;
const RX_INTERRUPT: u8 = 0x01;
const THR_EMPTY_INTERRUPT: u8 = 0x02;
const LOOPBACK: u8 = 0x10;

pub struct Serial {
    console: Console,
    ier: u8,
    lcr: u8,
    mcr: u8,
    scr: u8,
    divisor: [u8; 2],
    thr_empty_pending: bool,
}

impl Serial {
    pub fn new(console: Console) -> Self {
        Self {
            console,
            ier: 0,
            lcr: 0,
            mcr: 0,
            scr: 0,
            divisor: [0; 2],
            thr_empty_pending: false,
        }
    }

    pub fn write(&mut self, offset: u16, value: u8) {
        let dlab = self.lcr & DLAB != 0;
        match offset {
            0 | 1 if dlab => self.divisor[offset as usize] = value,
            0 => {
                if self.mcr & LOOPBACK == 0 {
                    self.console.output(value);
                }
                self.thr_empty_pending = self.ier & THR_EMPTY_INTERRUPT != 0;
            }
            1 => {
                self.ier = value & 0x0f;
                self.thr_empty_pending = self.ier & THR_EMPTY_INTERRUPT != 0;
            }
            3 => self.lcr = value,
            4 => self.mcr = value,
            7 => self.scr = value,
            _ => (),
        }
    }

    pub fn read(&mut self, offset: u16) -> u8 {
        let dlab = self.lcr & DLAB != 0;
        match offset {
            0 | 1 if dlab => self.divisor[offset as usize],
            0 => self.console.read().unwrap_or(0),
            1 => self.ier,
            2 if self.rx_pending() => 0x04,
            // Reading IIR acknowledges a pending transmit-empty interrupt, as on hardware.
            2 if std::mem::take(&mut self.thr_empty_pending) => 0x02,
            2 => 0x01,
            3 => self.lcr,
            4 => self.mcr,
            // Transmitter always empty.
            5 => 0x60 | u8::from(self.console.has_input()),
            6 => self.modem_status(),
            7 => self.scr,
            _ => 0,
        }
    }

    /// The level the UART drives on its interrupt line.
    pub fn line(&self) -> bool {
        self.thr_empty_pending || self.rx_pending()
    }

    fn rx_pending(&self) -> bool {
        self.ier & RX_INTERRUPT != 0 && self.console.has_input()
    }

    fn modem_status(&self) -> u8 {
        if self.mcr & LOOPBACK == 0 {
            return 0xb0; // DCD, DSR and CTS asserted
        }
        // Loopback wires DTR->DSR, RTS->CTS, OUT1->RI, OUT2->DCD; the driver's probe checks this.
        let mcr = self.mcr;
        (mcr & 0x01) << 5 | (mcr & 0x02) << 3 | (mcr & 0x0c) << 4
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct Out(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for Out {
        fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend(data);
            Ok(data.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn passes_the_8250_probe_and_carries_bytes_both_ways() {
        let console = Console::default();
        let out = Out::default();
        console.add_output(out.clone());
        let mut serial = Serial::new(console.clone());
        serial.write(1, 0x0f);
        assert_eq!(serial.read(1), 0x0f);
        serial.write(1, 0);
        serial.write(4, LOOPBACK | 0x0a);
        assert_eq!(serial.read(6) & 0xf0, 0x90);
        serial.write(4, 0);
        serial.write(1, THR_EMPTY_INTERRUPT);
        assert!(serial.line());
        assert_eq!(serial.read(2), 0x02);
        assert!(!serial.line());
        serial.write(0, b'o');
        assert_eq!(serial.read(2), 0x02);
        assert_eq!(serial.read(2), 0x01);
        serial.write(1, RX_INTERRUPT);
        console.input(b"k");
        assert!(serial.line());
        assert_eq!(serial.read(2), 0x04);
        assert_eq!(serial.read(5) & 1, 1);
        assert_eq!(serial.read(0), b'k');
        assert!(!serial.line());
        assert_eq!(*out.0.lock().unwrap(), b"o");
    }
}
