//! Pure Rust DateTime formatting behind a bounded C ABI. Luau userdata and
//! errors stay in the native binding; this module only converts values and bytes.
use std::panic::{catch_unwind, AssertUnwindSafe};
#[cfg(not(feature = "rust-host"))]
use std::sync::Once;

const MAX_FORMAT_BYTES: usize = 4096;
const MAX_LOCALE_BYTES: usize = 32;
const MAX_OUTPUT_BYTES: usize = 65_536;
const INVALID_ARGUMENT: i32 = -1;
const INVALID_FORMAT: i32 = -2;
const UNSUPPORTED_LOCALE: i32 = -3;
const OUTPUT_TOO_LARGE: i32 = -4;
const INVALID_DATETIME: i32 = -5;

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DateTimeParts {
    pub year: i64,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
    pub millisecond: u32,
    pub offset_seconds: i32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DateTimeFormatError {
    InvalidFormat,
    UnsupportedLocale,
    OutputTooLarge,
    InvalidDateTime,
}

impl DateTimeParts {
    fn valid(self) -> bool {
        (1400..=9999).contains(&self.year)
            && (1..=12).contains(&self.month)
            && (1..=days_in_month(self.year, self.month)).contains(&self.day)
            && self.hour < 24
            && self.minute < 60
            && self.second < 60
            && self.millisecond < 1000
    }
}

pub fn format_datetime(
    parts: DateTimeParts,
    format: &[u8],
    locale: &[u8],
) -> Result<Vec<u8>, DateTimeFormatError> {
    if !parts.valid() {
        return Err(DateTimeFormatError::InvalidDateTime);
    }
    if format.len() > MAX_FORMAT_BYTES {
        return Err(DateTimeFormatError::InvalidFormat);
    }
    if locale.len() > MAX_LOCALE_BYTES || !english_locale(locale) {
        return Err(DateTimeFormatError::UnsupportedLocale);
    }
    let days = days_from_civil(parts.year, parts.month, parts.day);
    let weekday = (days + 4).rem_euclid(7) as usize;
    let year_day = days - days_from_civil(parts.year, 1, 1);
    let unix = days * 86_400
        + i64::from(parts.hour) * 3600
        + i64::from(parts.minute) * 60
        + i64::from(parts.second)
        - i64::from(parts.offset_seconds);
    let mut out = Vec::with_capacity(format.len().min(256));
    let mut at = 0;
    while at < format.len() {
        if format[at] != b'%' {
            out.push(format[at]);
            at += 1;
        } else {
            at += 1;
            if at == format.len() {
                return Err(DateTimeFormatError::InvalidFormat);
            }
            append_token(&mut out, format[at], parts, weekday, year_day, unix)?;
            at += 1;
        }
        if out.len() > MAX_OUTPUT_BYTES {
            return Err(DateTimeFormatError::OutputTooLarge);
        }
    }
    Ok(out)
}

fn english_locale(locale: &[u8]) -> bool {
    locale.eq_ignore_ascii_case(b"en")
        || locale.eq_ignore_ascii_case(b"en-us")
        || locale.eq_ignore_ascii_case(b"en_us")
}

fn days_in_month(year: i64, month: u32) -> u32 {
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => 0,
    }
}

fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = (if year >= 0 { year } else { year - 399 }) / 400;
    let yoe = year - era * 400;
    let month = i64::from(month);
    let day = i64::from(day);
    let doy = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_year_from_days(days: i64) -> i64 {
    let days = days + 719_468;
    let era = (if days >= 0 { days } else { days - 146_096 }) / 146_097;
    let doe = days - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let month = (5 * doy + 2) / 153;
    year + i64::from(month >= 10)
}

fn iso_week(days: i64, weekday: usize) -> (i64, i64) {
    let iso_weekday = if weekday == 0 { 7 } else { weekday } as i64;
    let thursday = days + 4 - iso_weekday;
    let year = civil_year_from_days(thursday);
    let january_fourth = days_from_civil(year, 1, 4);
    let january_fourth_weekday = (january_fourth + 4).rem_euclid(7) as i64;
    let january_fourth_iso = if january_fourth_weekday == 0 {
        7
    } else {
        january_fourth_weekday
    };
    let week_one_monday = january_fourth - (january_fourth_iso - 1);
    (year, (thursday - week_one_monday) / 7 + 1)
}

fn append_token(
    out: &mut Vec<u8>,
    token: u8,
    parts: DateTimeParts,
    weekday: usize,
    year_day: i64,
    unix: i64,
) -> Result<(), DateTimeFormatError> {
    const WEEKDAYS: [&[u8]; 7] = [
        b"Sunday",
        b"Monday",
        b"Tuesday",
        b"Wednesday",
        b"Thursday",
        b"Friday",
        b"Saturday",
    ];
    const WEEKDAY_ABBR: [&[u8]; 7] = [b"Sun", b"Mon", b"Tue", b"Wed", b"Thu", b"Fri", b"Sat"];
    const MONTHS: [&[u8]; 12] = [
        b"January",
        b"February",
        b"March",
        b"April",
        b"May",
        b"June",
        b"July",
        b"August",
        b"September",
        b"October",
        b"November",
        b"December",
    ];
    const MONTH_ABBR: [&[u8]; 12] = [
        b"Jan", b"Feb", b"Mar", b"Apr", b"May", b"Jun", b"Jul", b"Aug", b"Sep", b"Oct", b"Nov",
        b"Dec",
    ];
    let month = parts.month as usize - 1;
    let hour12 = match parts.hour % 12 {
        0 => 12,
        value => value,
    };
    let period = if parts.hour < 12 {
        b"AM".as_slice()
    } else {
        b"PM".as_slice()
    };
    let text = match token {
        b'a' => WEEKDAY_ABBR[weekday].to_vec(),
        b'A' => WEEKDAYS[weekday].to_vec(),
        b'b' | b'h' => MONTH_ABBR[month].to_vec(),
        b'B' => MONTHS[month].to_vec(),
        b'C' => format!("{:02}", parts.year / 100).into_bytes(),
        b'd' => format!("{:02}", parts.day).into_bytes(),
        b'D' => format!(
            "{:02}/{:02}/{:02}",
            parts.month,
            parts.day,
            parts.year % 100
        )
        .into_bytes(),
        b'e' => format!("{:2}", parts.day).into_bytes(),
        b'F' => format!("{:04}-{:02}-{:02}", parts.year, parts.month, parts.day).into_bytes(),
        b'g' => {
            let (year, _) = iso_week(days_from_civil(parts.year, parts.month, parts.day), weekday);
            format!("{:02}", year % 100).into_bytes()
        }
        b'G' => {
            let (year, _) = iso_week(days_from_civil(parts.year, parts.month, parts.day), weekday);
            format!("{:04}", year).into_bytes()
        }
        b'V' => {
            let (_, week) = iso_week(days_from_civil(parts.year, parts.month, parts.day), weekday);
            format!("{:02}", week).into_bytes()
        }
        b'H' => format!("{:02}", parts.hour).into_bytes(),
        b'I' => format!("{:02}", hour12).into_bytes(),
        b'j' => format!("{:03}", year_day + 1).into_bytes(),
        b'L' => format!("{:03}", parts.millisecond).into_bytes(),
        b'm' => format!("{:02}", parts.month).into_bytes(),
        b'M' => format!("{:02}", parts.minute).into_bytes(),
        b'n' => b"\n".to_vec(),
        b'p' => period.to_vec(),
        b'P' => period.iter().map(u8::to_ascii_lowercase).collect(),
        b'r' => format!(
            "{:02}:{:02}:{:02} {}",
            hour12,
            parts.minute,
            parts.second,
            String::from_utf8_lossy(period)
        )
        .into_bytes(),
        b'R' => format!("{:02}:{:02}", parts.hour, parts.minute).into_bytes(),
        b's' => unix.to_string().into_bytes(),
        b'S' => format!("{:02}", parts.second).into_bytes(),
        b't' => b"\t".to_vec(),
        b'T' | b'X' => {
            format!("{:02}:{:02}:{:02}", parts.hour, parts.minute, parts.second).into_bytes()
        }
        b'u' => (if weekday == 0 { 7 } else { weekday })
            .to_string()
            .into_bytes(),
        b'U' => format!("{:02}", (year_day + 7 - weekday as i64) / 7).into_bytes(),
        b'w' => weekday.to_string().into_bytes(),
        b'W' => format!("{:02}", (year_day + 7 - (weekday + 6) as i64 % 7) / 7).into_bytes(),
        b'x' => format!(
            "{:02}/{:02}/{:02}",
            parts.month,
            parts.day,
            parts.year % 100
        )
        .into_bytes(),
        b'y' => format!("{:02}", parts.year % 100).into_bytes(),
        b'Y' => format!("{:04}", parts.year).into_bytes(),
        b'z' => {
            let offset = parts.offset_seconds;
            let sign = if offset < 0 { '-' } else { '+' };
            let magnitude = offset.unsigned_abs();
            format!("{sign}{:02}{:02}", magnitude / 3600, magnitude % 3600 / 60).into_bytes()
        }
        b'Z' if parts.offset_seconds == 0 => b"UTC".to_vec(),
        b'%' => b"%".to_vec(),
        b'c' => {
            let mut value = Vec::new();
            append_token(&mut value, b'a', parts, weekday, year_day, unix)?;
            value.push(b' ');
            append_token(&mut value, b'b', parts, weekday, year_day, unix)?;
            value.push(b' ');
            append_token(&mut value, b'e', parts, weekday, year_day, unix)?;
            value.push(b' ');
            append_token(&mut value, b'T', parts, weekday, year_day, unix)?;
            value.push(b' ');
            append_token(&mut value, b'Y', parts, weekday, year_day, unix)?;
            value
        }
        _ => return Err(DateTimeFormatError::InvalidFormat),
    };
    out.extend_from_slice(&text);
    Ok(())
}

#[no_mangle]
pub unsafe extern "C" fn rove_datetime_format(
    parts: *const DateTimeParts,
    format: *const u8,
    format_len: usize,
    locale: *const u8,
    locale_len: usize,
    out: *mut u8,
    out_capacity: usize,
) -> i32 {
    catch_unwind(AssertUnwindSafe(|| {
        if parts.is_null()
            || (format_len > 0 && format.is_null())
            || (locale_len > 0 && locale.is_null())
            || (out_capacity > 0 && out.is_null())
        {
            return INVALID_ARGUMENT;
        }
        let format = if format_len == 0 {
            &[]
        } else {
            unsafe { std::slice::from_raw_parts(format, format_len) }
        };
        let locale = if locale_len == 0 {
            &[]
        } else {
            unsafe { std::slice::from_raw_parts(locale, locale_len) }
        };
        let result = match format_datetime(unsafe { *parts }, format, locale) {
            Ok(value) => value,
            Err(DateTimeFormatError::InvalidFormat) => return INVALID_FORMAT,
            Err(DateTimeFormatError::UnsupportedLocale) => return UNSUPPORTED_LOCALE,
            Err(DateTimeFormatError::OutputTooLarge) => return OUTPUT_TOO_LARGE,
            Err(DateTimeFormatError::InvalidDateTime) => return INVALID_DATETIME,
        };
        if !out.is_null() && out_capacity >= result.len() {
            unsafe { std::ptr::copy_nonoverlapping(result.as_ptr(), out, result.len()) };
        }
        result.len() as i32
    }))
    .unwrap_or(INVALID_ARGUMENT)
}

#[cfg(not(feature = "rust-host"))]
#[repr(C)]
struct DateTimeApi {
    version: u32,
    format: unsafe extern "C" fn(
        *const DateTimeParts,
        *const u8,
        usize,
        *const u8,
        usize,
        *mut u8,
        usize,
    ) -> i32,
}
#[cfg(not(feature = "rust-host"))]
static API: DateTimeApi = DateTimeApi {
    version: 1,
    format: rove_datetime_format,
};
#[cfg(not(feature = "rust-host"))]
unsafe extern "C" {
    fn local_luau_set_rust_datetime_api(api: *const DateTimeApi) -> i32;
}
#[cfg(not(feature = "rust-host"))]
pub fn install() {
    static INIT: Once = Once::new();
    INIT.call_once(|| assert_eq!(unsafe { local_luau_set_rust_datetime_api(&API) }, 1));
}

#[cfg(test)]
mod tests {
    use super::*;
    fn parts() -> DateTimeParts {
        DateTimeParts {
            year: 2020,
            month: 1,
            day: 2,
            hour: 15,
            minute: 4,
            second: 5,
            millisecond: 6,
            offset_seconds: 0,
        }
    }
    #[test]
    fn formats_numeric_named_and_composite_fields() {
        assert_eq!(
            format_datetime(parts(), b"%Y-%m-%d %H:%M:%S.%L", b"en").unwrap(),
            b"2020-01-02 15:04:05.006"
        );
        assert_eq!(
            format_datetime(parts(), b"%A %a %B %b %e %j %I %p %r", b"en-US").unwrap(),
            b"Thursday Thu January Jan  2 002 03 PM 03:04:05 PM"
        );
        assert_eq!(
            format_datetime(parts(), b"%D %F %T %x %X %c %%", b"en").unwrap(),
            b"01/02/20 2020-01-02 15:04:05 01/02/20 15:04:05 Thu Jan  2 15:04:05 2020 %"
        );
    }
    #[test]
    fn iso_weeks_use_thursday_based_week_years() {
        let boundary = days_from_civil(2020, 12, 31);
        assert_eq!(civil_year_from_days(boundary), 2020);
        assert_eq!(iso_week(boundary, 4), (2020, 53));
        for (year, month, day, expected) in [
            (2021, 1, 1, "2020 20 53"),
            (2019, 12, 30, "2020 20 01"),
            (2018, 12, 31, "2019 19 01"),
            (2020, 12, 31, "2020 20 53"),
            (2022, 1, 3, "2022 22 01"),
            (2021, 1, 4, "2021 21 01"),
            (2016, 1, 1, "2015 15 53"),
            (2010, 1, 1, "2009 09 53"),
        ] {
            let date = DateTimeParts {
                year,
                month,
                day,
                ..parts()
            };
            assert_eq!(
                format_datetime(date, b"%G %g %V", b"en").unwrap(),
                expected.as_bytes()
            );
        }
    }

    #[test]
    fn handles_epoch_offsets_weekdays_and_negative_dates() {
        let epoch = DateTimeParts {
            year: 1970,
            month: 1,
            day: 1,
            hour: 0,
            minute: 0,
            second: 0,
            millisecond: 0,
            offset_seconds: 0,
        };
        assert_eq!(
            format_datetime(epoch, b"%s %u %w %U %W %z %Z", b"en").unwrap(),
            b"0 4 4 00 00 +0000 UTC"
        );
        let negative = DateTimeParts {
            year: 1969,
            month: 12,
            day: 31,
            hour: 23,
            minute: 59,
            second: 59,
            millisecond: 999,
            offset_seconds: 0,
        };
        assert_eq!(format_datetime(negative, b"%s", b"en").unwrap(), b"-1");
        let local = DateTimeParts {
            offset_seconds: 20_700,
            ..parts()
        };
        assert_eq!(format_datetime(local, b"%z", b"en").unwrap(), b"+0545");
    }
    #[test]
    fn rejects_invalid_inputs_and_bounds_output() {
        assert_eq!(
            format_datetime(parts(), b"%Q", b"en"),
            Err(DateTimeFormatError::InvalidFormat)
        );
        assert_eq!(
            format_datetime(parts(), b"%", b"en"),
            Err(DateTimeFormatError::InvalidFormat)
        );
        assert_eq!(
            format_datetime(parts(), b"%Y", b"zz"),
            Err(DateTimeFormatError::UnsupportedLocale)
        );
        assert_eq!(format_datetime(parts(), b"%Y", b"en"), Ok(b"2020".to_vec()));
        assert_eq!(
            format_datetime(parts(), &vec![b'x'; MAX_OUTPUT_BYTES + 1], b"en"),
            Err(DateTimeFormatError::InvalidFormat)
        );
        let long = format!("{:4096}", "x");
        assert!(format_datetime(parts(), long.as_bytes(), b"en").is_ok());
    }
}
