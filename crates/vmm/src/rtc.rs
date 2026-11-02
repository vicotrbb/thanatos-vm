//! The real-time clock the guest reads once at boot: without it, it believes it is 1970 and no
//! TLS certificate or apt signature verifies. A PL031 on aarch64, CMOS registers on x86_64.
use crate::mmio::Mmio;
use std::{
    sync::atomic::{AtomicU8, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// PrimeCell peripheral and cell IDs at 0xfe0, which the AMBA bus reads to find a driver.
const PL031_IDS: [u8; 8] = [0x31, 0x10, 0x14, 0x00, 0x0d, 0xf0, 0x05, 0xb1];

/// shortcut: the guest's writes to the clock are ignored, keep an offset if a guest must set it.
pub struct Pl031;

impl Mmio for Pl031 {
    fn read(&self, offset: u64, data: &mut [u8]) {
        let value = match offset {
            0x000 => now() as u32,
            0xfe0..=0xffc => PL031_IDS[((offset - 0xfe0) / 4) as usize % 8].into(),
            _ => 0,
        };
        for (byte, value) in data.iter_mut().zip(value.to_le_bytes()) {
            *byte = value;
        }
    }

    fn write(&self, _: u64, _: &[u8]) {}
}

/// Days-since-epoch to (year, month, day), Howard Hinnant's civil-from-days.
fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(month <= 2), month, day)
}

fn bcd(value: u64) -> u8 {
    (value / 10 * 16 + value % 10) as u8
}

fn cmos_register(index: u8, time: u64) -> u8 {
    let (year, month, day) = civil((time / 86400) as i64);
    match index {
        0x00 => bcd(time % 60),
        0x02 => bcd(time / 60 % 60),
        0x04 => bcd(time / 3600 % 24),
        0x06 => bcd((time / 86400 + 4) % 7 + 1),
        0x07 => bcd(day.into()),
        0x08 => bcd(month.into()),
        0x09 => bcd(year as u64 % 100),
        0x32 => bcd(year as u64 / 100),
        0x0b => 0x02, // 24-hour, BCD: the only form Linux on x86 trusts
        0x0d => 0x80, // the battery is fine
        _ => 0,
    }
}

/// Ports 0x70 (index) and 0x71 (data).
#[derive(Default)]
pub struct Cmos(AtomicU8);

impl Cmos {
    pub fn select(&self, index: u8) {
        self.0.store(index & 0x7f, Ordering::Relaxed);
    }

    pub fn read(&self) -> u8 {
        cmos_register(self.0.load(Ordering::Relaxed), now())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cmos_encodes_a_known_moment() {
        // 2026-11-02 15:44:41 UTC, a Monday.
        let time = 1_793_634_281;
        let read = |index| cmos_register(index, time);
        assert_eq!(
            [read(0x32), read(9), read(8), read(7)],
            [0x20, 0x26, 0x11, 0x02]
        );
        assert_eq!([read(4), read(2), read(0)], [0x15, 0x44, 0x41]);
        assert_eq!(read(6), 2);
    }

    #[test]
    fn pl031_reports_the_time_and_its_identity() {
        let mut word = [0; 4];
        Pl031.read(0xfe0, &mut word);
        assert_eq!(word[0], 0x31);
        Pl031.read(0, &mut word);
        assert!(u64::from(u32::from_le_bytes(word)).abs_diff(now()) < 5);
    }
}
