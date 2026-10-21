//! Two-level logging: events and errors on the console, mirrored to a file.
//!
//! The console carries what an operator glancing at the window wants to see:
//! connection events -- source connect/disconnect, consumer attach/detach, the
//! startup banner -- on stdout, and errors -- a config or bind failure, a
//! geometry mismatch, the safety valve firing -- on stderr. Both are also
//! appended, with UTC stamps, to the log file named by `log_path` when one is
//! configured. Nothing periodic is printed anywhere: the per-minute metrics
//! line has its own file (`metrics_path`), so a healthy shim's window stays
//! still between connection changes.
//!
//! No crate dependency: a `Mutex<Option<File>>` is const-constructible on the
//! declared MSRV, and the UTC stamp is thirty lines of civil-date arithmetic.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

static FILE: Mutex<Option<File>> = Mutex::new(None);

/// Opens (appending) the log file. Without a path, output reaches the console
/// only.
pub fn init(path: Option<&Path>) -> Result<(), String> {
    if let Some(p) = path {
        let f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(p)
            .map_err(|e| format!("cannot open log file {}: {}", p.display(), e))?;
        *FILE.lock().unwrap_or_else(|e| e.into_inner()) = Some(f);
    }
    Ok(())
}

/// Connection events and other one-off state changes: stdout and file.
pub fn info(msg: &str) {
    println!("{}", msg);
    write_file("INFO", msg);
}

/// Error: console and file.
pub fn error(msg: &str) {
    eprintln!("{}", msg);
    write_file("ERROR", msg);
}

fn write_file(level: &str, msg: &str) {
    let mut guard = FILE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(f) = guard.as_mut() {
        // A failing log write must never take the relay down; it is dropped.
        let _ = writeln!(f, "{} {} {}", stamp(), level, msg);
    }
}

/// UTC wall-clock stamp, `YYYY-MM-DD HH:MM:SS.mmm`. Civil-from-days per
/// Howard Hinnant's algorithm; valid for the whole Unix era.
fn stamp() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs() as i64;
    let ms = now.subsec_millis();
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
        y,
        m,
        d,
        sod / 3600,
        (sod / 60) % 60,
        sod % 60,
        ms
    )
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_dates_round_trip_known_epochs() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        // 2000-03-01 is day 11_017; the leap-day boundary is the classic trap.
        assert_eq!(civil_from_days(11_017), (2000, 3, 1));
        assert_eq!(civil_from_days(11_016), (2000, 2, 29));
        // 2026-10-21 = 20_747 days after the epoch.
        assert_eq!(civil_from_days(20_747), (2026, 10, 21));
    }

    #[test]
    fn stamp_has_the_documented_shape() {
        let s = stamp();
        assert_eq!(s.len(), 23, "{}", s);
        assert_eq!(&s[4..5], "-");
        assert_eq!(&s[10..11], " ");
        assert_eq!(&s[19..20], ".");
    }
}
