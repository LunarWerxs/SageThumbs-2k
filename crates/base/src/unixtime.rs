//! Unix seconds: the current time, and a time as a local calendar date for display. The one
//! FILETIME conversion behind every surface that shows a date (the licence line, the rename
//! verb's `{date}`, an upload's expiry), so none carries its own copy of the epoch arithmetic.

use windows::Win32::Foundation::{FILETIME, SYSTEMTIME};
use windows::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};

/// Now, in Unix seconds. A clock reading before 1970 gives 0, which every caller treats as
/// "no time has passed" - the safe direction for an expiry or a licence period.
pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// `unix_secs` in local time, or `None` when the FILETIME conversion fails.
fn local_systemtime(unix_secs: u64) -> Option<SYSTEMTIME> {
    // FILETIME ticks are 100 ns since 1601-01-01; the Unix epoch is 11_644_473_600 s later.
    let ticks = unix_secs
        .saturating_add(11_644_473_600)
        .saturating_mul(10_000_000);
    let ft = FILETIME {
        dwLowDateTime: (ticks & 0xFFFF_FFFF) as u32,
        dwHighDateTime: (ticks >> 32) as u32,
    };
    let mut utc = SYSTEMTIME::default();
    // SAFETY: both calls only read and write the stack structs passed to them.
    unsafe { FileTimeToSystemTime(&ft, &mut utc) }.ok()?;
    let mut local = utc;
    unsafe {
        let _ = SystemTimeToTzSpecificLocalTime(None, &utc, &mut local);
    }
    Some(local)
}

/// `unix_secs` as local `"YYYY-MM-DD"`, or `None` when the conversion fails.
pub fn local_date(unix_secs: u64) -> Option<String> {
    local_systemtime(unix_secs).map(|t| format!("{:04}-{:02}-{:02}", t.wYear, t.wMonth, t.wDay))
}

/// `unix_secs` as local `"YYYY-MM-DD HH:MM"`, the shape the Quick preview's info card uses
/// for a file's modified time (empty when the conversion fails).
pub fn local_datetime(unix_secs: u64) -> String {
    local_systemtime(unix_secs).map_or_else(String::new, |t| {
        format!(
            "{:04}-{:02}-{:02} {:02}:{:02}",
            t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_date_and_its_date_time_agree_on_the_day() {
        // 2025-09-16 12:00 UTC: the 16th everywhere from UTC-12 to UTC+11, the 17th from
        // UTC+12 on, so the day itself depends on the machine; that both shapes agree does not.
        let secs = 1_758_024_000;
        let date = local_date(secs).unwrap();
        assert!(date == "2025-09-16" || date == "2025-09-17", "{date}");
        assert!(local_datetime(secs).starts_with(&date));
        assert_eq!(local_datetime(secs).len(), "YYYY-MM-DD HH:MM".len());
    }

    #[test]
    fn a_time_past_what_filetime_holds_is_refused_rather_than_wrapped() {
        // The saturating arithmetic pins the tick count at u64::MAX, which FileTimeToSystemTime
        // rejects (anything at or above 0x8000_0000_0000_0000 is invalid).
        assert_eq!(local_date(u64::MAX), None);
        assert_eq!(local_datetime(u64::MAX), "");
    }

    #[test]
    fn now_is_after_this_code_was_written() {
        assert!(now() > 1_758_000_000);
    }
}
