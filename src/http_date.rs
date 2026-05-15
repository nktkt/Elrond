//! Minimal IMF-fixdate (RFC 7231 §7.1.1.1) formatter. No date *parser* yet —
//! `If-Modified-Since` validation against `Last-Modified` is therefore only
//! exact-string today; v0.5.0 leans on ETag-based revalidation, which is the
//! more reliable mechanism anyway.

/// Format a Unix-epoch second count as an HTTP date:
/// `Sun, 06 Nov 1994 08:49:37 GMT`.
pub fn format(secs: u64) -> String {
    let days = secs / 86400;
    let tod = secs % 86400;
    let h = tod / 3600;
    let m = (tod % 3600) / 60;
    let s = tod % 60;
    // 1970-01-01 was a Thursday.
    let weekday = ((days + 4) % 7) as usize;
    let (year, month, day) = days_to_date(days);
    let wd = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"][weekday];
    let mo = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ][month - 1];
    format!("{wd}, {day:02} {mo} {year:04} {h:02}:{m:02}:{s:02} GMT")
}

fn days_to_date(days: u64) -> (u32, usize, u32) {
    let mut days = days as i64;
    let mut year: i64 = 1970;
    loop {
        let dy: i64 = if is_leap(year) { 366 } else { 365 };
        if days < dy {
            break;
        }
        days -= dy;
        year += 1;
    }
    let months: [i64; 12] = if is_leap(year) {
        [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    } else {
        [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    };
    let mut m = 0usize;
    for &dm in &months {
        if days < dm {
            break;
        }
        days -= dm;
        m += 1;
    }
    (year as u32, m + 1, days as u32 + 1)
}

fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_format() {
        // 1970-01-01T00:00:00Z is a Thursday.
        assert_eq!(format(0), "Thu, 01 Jan 1970 00:00:00 GMT");
    }

    #[test]
    fn known_dates() {
        // 2001-09-09T01:46:40Z is a Sunday (a well-known epoch milestone:
        // 1_000_000_000 seconds since the Unix epoch).
        assert_eq!(format(1_000_000_000), "Sun, 09 Sep 2001 01:46:40 GMT");
        // 2024-02-29T12:34:56Z — leap-day correctness check.
        assert_eq!(format(1_709_209_896), "Thu, 29 Feb 2024 12:31:36 GMT");
    }
}
