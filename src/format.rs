//! Human-readable formatting. All `Display` impls write straight into the
//! formatter — no intermediate heap strings.

use std::fmt;

use crate::types::UnixTime;

/// Binary units (du-style): 1024 → 1K, 1 K → 1M …
pub struct HumanBytes(pub u64);

impl fmt::Display for HumanBytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        const UNITS: [&str; 6] = ["B", "K", "M", "G", "T", "P"];
        if self.0 < 1024 {
            return write!(f, "{}B", self.0);
        }
        let mut v = self.0 as f64;
        let mut u = 0;
        while v >= 1024.0 && u < 5 {
            v /= 1024.0;
            u += 1;
        }
        if v >= 100.0 {
            write!(f, "{v:.0}{}", UNITS[u])
        } else {
            write!(f, "{v:.1}{}", UNITS[u])
        }
    }
}

/// Decimal counts: 1000 → 1K, 1K → 1M, 1M → 1B.
pub struct HumanCount(pub u64);

impl fmt::Display for HumanCount {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        const UNITS: [&str; 4] = ["", "K", "M", "B"];
        if self.0 < 1000 {
            return write!(f, "{}", self.0);
        }
        let mut v = self.0 as f64;
        let mut u = 0;
        while v >= 1000.0 && u < 3 {
            v /= 1000.0;
            u += 1;
        }
        if v >= 100.0 {
            write!(f, "{v:.0}{}", UNITS[u])
        } else {
            write!(f, "{v:.1}{}", UNITS[u])
        }
    }
}

/// `2026-10-10` (UTC).
pub struct Date(pub UnixTime);

impl fmt::Display for Date {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (y, m, d) = self.0.civil();
        write!(f, "{y:04}-{m:02}-{d:02}")
    }
}

/// Compact age: `today`, `37d`, `2.4y`.
pub struct Age {
    pub days: u32,
}

impl fmt::Display for Age {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.days < 1 {
            f.write_str("today")
        } else if self.days < 365 {
            write!(f, "{}d", self.days)
        } else {
            write!(f, "{:.1}y", self.days as f64 / 365.0)
        }
    }
}

/// Abbreviate `$HOME` to `~` in `path`, appending the result to `out`.
pub fn shorten_home(path: &std::path::Path, home: &std::path::Path, out: &mut String) {
    if let Ok(rest) = path.strip_prefix(home) {
        out.push('~');
        if rest.as_os_str().is_empty() {
            return;
        }
        out.push('/');
        out.push_str(&rest.to_string_lossy());
    } else {
        out.push_str(&path.to_string_lossy());
    }
}

/// `mm:ss` or `h:mm:ss` from seconds.
pub fn fmt_clock(total_secs: u64, out: &mut String) {
    let h = total_secs / 3600;
    let m = (total_secs % 3600) / 60;
    let s = total_secs % 60;
    if h > 0 {
        out.push_str(&format!("{h}:{m:02}:{s:02}"));
    } else {
        out.push_str(&format!("{m:02}:{s:02}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_bytes_units() {
        assert_eq!(format!("{}", HumanBytes(512)), "512B");
        assert_eq!(format!("{}", HumanBytes(1024)), "1.0K");
        assert_eq!(format!("{}", HumanBytes(1536)), "1.5K");
        assert_eq!(format!("{}", HumanBytes(1024 * 1024)), "1.0M");
        assert_eq!(format!("{}", HumanBytes(12_300_000_000)), "11.5G");
    }

    #[test]
    fn human_count_units() {
        assert_eq!(format!("{}", HumanCount(999)), "999");
        assert_eq!(format!("{}", HumanCount(1200)), "1.2K");
        assert_eq!(format!("{}", HumanCount(1_234_567)), "1.2M");
    }

    #[test]
    fn civil_date_known_values() {
        assert_eq!(UnixTime(0).civil(), (1970, 1, 1));
        assert_eq!(UnixTime(1_000_000_000).civil(), (2001, 9, 9));
        assert_eq!(UnixTime(1_760_000_000).civil(), (2025, 10, 9));
    }

    #[test]
    fn shorten_home_works() {
        let home = std::path::Path::new("/Users/x");
        let mut s = String::new();
        shorten_home(std::path::Path::new("/Users/x/Work/foo"), home, &mut s);
        assert_eq!(s, "~/Work/foo");
        s.clear();
        shorten_home(std::path::Path::new("/Users/x"), home, &mut s);
        assert_eq!(s, "~");
        s.clear();
        shorten_home(std::path::Path::new("/etc"), home, &mut s);
        assert_eq!(s, "/etc");
    }
}
