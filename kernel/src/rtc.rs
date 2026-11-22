//! Minimal MC146818 CMOS real-time clock driver.
//!
//! The RTC keeps wall-clock date and time in battery-backed registers on the
//! classic PC platform. `GTos` reads the cached registers directly; it does not
//! program alarms or periodic interrupts. The result is converted to Unix
//! seconds with a civil-date algorithm, so userspace gets a familiar epoch.

use core::sync::atomic::{AtomicU64, Ordering};

use x86_64::instructions::port::Port;

const CMOS_INDEX: u16 = 0x70;
const CMOS_DATA: u16 = 0x71;

const REG_SECONDS: u8 = 0x00;
const REG_MINUTES: u8 = 0x02;
const REG_HOURS: u8 = 0x04;
const REG_DAY: u8 = 0x07;
const REG_MONTH: u8 = 0x08;
const REG_YEAR: u8 = 0x09;
const REG_STATUS_B: u8 = 0x0b;
const REG_CENTURY: u8 = 0x32;

const STATUS_B_24H: u8 = 1 << 2;
const STATUS_B_BINARY: u8 = 1 << 0;

/// Last successfully read wall-clock value, or 0 before the first read.
static CACHED_SECONDS: AtomicU64 = AtomicU64::new(0);

/// Reads one CMOS register, disabling NMI for the short access as the PC
/// architecture requires.
fn read_register(register: u8) -> u8 {
    // SAFETY: 0x70 and 0x71 are the CMOS index/data ports; only the RTC and NMI
    // mask logic respond to them, and reading data has no side effects.
    unsafe {
        let mut index = Port::<u8>::new(CMOS_INDEX);
        let mut data = Port::<u8>::new(CMOS_DATA);
        index.write(0x80 | register);
        let value = data.read();
        // Writing a register index without the NMI-disable bit re-enables NMI.
        index.write(register);
        value
    }
}

/// Converts a BCD-encoded byte, returning `None` for invalid nibbles.
fn from_bcd(value: u8) -> Option<u8> {
    let high = value >> 4;
    let low = value & 0x0f;
    if high > 9 || low > 9 {
        return None;
    }
    Some(high * 10 + low)
}

/// Number of days from 1970-01-01 to the given civil date.
///
/// Howard Hinnant's days-from-civil algorithm, valid for proleptic Gregorian
/// dates. The intermediate `i64` arithmetic cannot overflow for RTC years.
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let shifted_month = i64::from(month) + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * shifted_month + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// Reads the wall clock and returns Unix seconds, or `None` for invalid data.
#[must_use]
pub fn unix_seconds() -> Option<u64> {
    let status_b = read_register(REG_STATUS_B);
    let binary = status_b & STATUS_B_BINARY != 0;
    let decode = |register: u8| -> Option<u8> {
        let value = read_register(register);
        if binary { Some(value) } else { from_bcd(value) }
    };

    let second = decode(REG_SECONDS)?;
    let minute = decode(REG_MINUTES)?;
    let raw_hour = decode(REG_HOURS)?;
    let day = decode(REG_DAY)?;
    let month = decode(REG_MONTH)?;
    let raw_year = decode(REG_YEAR)?;

    if second > 59 || minute > 59 || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }

    let hour = if raw_hour <= 23 || status_b & STATUS_B_24H != 0 {
        // Many emulated RTCs keep 24-hour values even when the mode bit says
        // 12-hour, so accept a valid 24-hour reading first.
        raw_hour
    } else {
        // In true 12-hour mode the RTC stores 1..12 with bit 7 set for PM.
        let is_pm = raw_hour & 0x80 != 0;
        let hour_12 = raw_hour & 0x7f;
        if !(1..=12).contains(&hour_12) {
            return None;
        }
        if hour_12 == 12 {
            if is_pm { 12 } else { 0 }
        } else {
            hour_12 + if is_pm { 12 } else { 0 }
        }
    };
    if hour > 23 {
        return None;
    }

    let century = decode(REG_CENTURY).unwrap_or(0);
    let year_2digit = u64::from(raw_year);
    let year = if century > 0 {
        i64::from(century) * 100 + year_2digit as i64
    } else if raw_year < 70 {
        // The usual pivot: two-digit years below 70 belong to this century.
        2000 + year_2digit as i64
    } else {
        1900 + year_2digit as i64
    };

    let days = days_from_civil(year, u32::from(month), u32::from(day));
    let total = days
        .checked_mul(86_400)?
        .checked_add(i64::from(hour) * 3600)?
        .checked_add(i64::from(minute) * 60)?
        .checked_add(i64::from(second))?;
    let seconds = u64::try_from(total).ok()?;
    CACHED_SECONDS.store(seconds, Ordering::Relaxed);
    Some(seconds)
}

/// Returns the last good wall-clock reading without touching the CMOS ports.
#[must_use]
pub fn cached_seconds() -> u64 {
    CACHED_SECONDS.load(Ordering::Relaxed)
}
