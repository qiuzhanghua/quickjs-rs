//! Reading the platform's time-zone database.
//!
//! `Date`'s local-time half needs to know the UTC offset at an arbitrary instant,
//! including instants in the past where the zone had a different offset. The C
//! reference delegates this to `localtime_r` + `tm_gmtoff`; this crate declares no
//! external dependencies and is named `quickjs-pure`, so the database is read here
//! directly instead of through libc.
//!
//! The format is RFC 8536 "TZif". Only the parts needed for offsets are kept:
//! transitions, the offset each one switches to, and leap seconds (ignored —
//! POSIX time, which is what `Date` uses, does not count them).
//!
//! A v2+ file stores the same data twice: a 32-bit block for old readers and then
//! a 64-bit block. The 64-bit block wins when present, because the 32-bit one
//! cannot express the extremes `Date` allows (±8.64e15 ms ≈ ±273790 years).

use std::cell::RefCell;
use std::path::{Path, PathBuf};

pub struct Zone {
    /// `(transition time in seconds since the epoch, offset in seconds east of
    /// UTC in effect from that transition)`, ascending.
    transitions: Vec<(i64, i32)>,
    /// The offset before the first transition.
    initial: i32,
}

impl Zone {
    /// Offset in seconds east of UTC at `time` (seconds since the epoch).
    pub fn offset_at(&self, time: i64) -> i32 {
        match self.transitions.binary_search_by_key(&time, |&(at, _)| at) {
            // A transition exactly at `time` is already in force.
            Ok(index) => self.transitions[index].1,
            Err(0) => self.initial,
            Err(index) => self.transitions[index - 1].1,
        }
    }
}

/// Offset in seconds east of UTC for `time` (seconds since the epoch) in the
/// process's local zone, or `None` when no database could be read.
pub fn local_offset(time: i64) -> Option<i32> {
    zone().map(|zone| zone.offset_at(time))
}

// The zone cannot change during a call but can between calls (`TZ=… qjs`), so the
// name is part of the cache key. Re-reading the file on every `Date` method would
// be correct too, just needlessly slow.
thread_local! {
    static CACHE: RefCell<Option<(String, Option<&'static Zone>)>> = const { RefCell::new(None) };
}

fn zone() -> Option<&'static Zone> {
    let (key, path) = source();
    CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if let Some((cached_key, zone)) = cache.as_ref() {
            if *cached_key == key {
                return *zone;
            }
        }
        // Leaked deliberately: the zone lives as long as the process, which is what
        // lets the cache hand out a `&'static Zone` without an arena.
        let parsed = path
            .and_then(|path| std::fs::read(path).ok())
            .and_then(|data| {
                let zone = parse(&data)?;
                Some(&*Box::leak(Box::new(zone)))
            });
        *cache = Some((key, parsed));
        parsed
    })
}

/// The zone name to key the cache by, and the file to read it from.
fn source() -> (String, Option<PathBuf>) {
    match std::env::var("TZ") {
        // An empty `TZ` means UTC, per POSIX.
        Ok(name) if name.is_empty() => (String::new(), None),
        Ok(name) => {
            let name = name.trim_start_matches(':').to_string();
            let path = Path::new(&name);
            let path = if path.is_absolute() {
                path.to_path_buf()
            } else {
                zone_dir().join(&name)
            };
            (name, Some(path))
        }
        // No `TZ`: the system default, which is a symlink into the database.
        Err(_) => ("/etc/localtime".to_string(), Some("/etc/localtime".into())),
    }
}

fn zone_dir() -> PathBuf {
    for candidate in ["/usr/share/zoneinfo", "/usr/lib/zoneinfo", "/etc/zoneinfo"] {
        if Path::new(candidate).is_dir() {
            return PathBuf::from(candidate);
        }
    }
    PathBuf::from("/usr/share/zoneinfo")
}

/// Parse a TZif file, preferring the 64-bit block when one is present.
fn parse(data: &[u8]) -> Option<Zone> {
    let header = Header::read(data, 0)?;
    if header.version >= b'2' {
        let second = header.end(4);
        let header64 = Header::read(data, second)?;
        // A file that claims v2+ but carries no 64-bit block falls back to the
        // 32-bit one rather than failing outright.
        if header64.time_size() == 8 {
            return header64.zone(data);
        }
    }
    header.zone(data)
}

struct Header {
    version: u8,
    isutcnt: usize,
    isstdcnt: usize,
    leapcnt: usize,
    timecnt: usize,
    typecnt: usize,
    charcnt: usize,
    /// Offset of the header itself.
    start: usize,
}

impl Header {
    fn read(data: &[u8], start: usize) -> Option<Header> {
        let raw = data.get(start..start + 44)?;
        if &raw[0..4] != b"TZif" {
            return None;
        }
        let version = raw[4];
        let word = |index: usize| -> usize {
            let at = 20 + index * 4;
            u32::from_be_bytes([raw[at], raw[at + 1], raw[at + 2], raw[at + 3]]) as usize
        };
        let header = Header {
            version,
            isutcnt: word(0),
            isstdcnt: word(1),
            leapcnt: word(2),
            timecnt: word(3),
            typecnt: word(4),
            charcnt: word(5),
            start,
        };
        // A type table with no entries leaves every offset undefined.
        if header.typecnt == 0 || header.typecnt > 256 {
            return None;
        }
        Some(header)
    }

    fn time_size(&self) -> usize {
        if self.version >= b'2' { 8 } else { 4 }
    }

    /// Offset of the block that follows this header.
    fn end(&self, time_size: usize) -> usize {
        self.start
            + 44
            + self.timecnt * time_size
            + self.timecnt
            + self.typecnt * 6
            + self.charcnt
            + self.leapcnt * (time_size + 4)
            + self.isstdcnt
            + self.isutcnt
    }

    fn zone(&self, data: &[u8]) -> Option<Zone> {
        let time_size = self.time_size();
        let mut at = self.start + 44;

        let mut times = Vec::with_capacity(self.timecnt);
        for _ in 0..self.timecnt {
            let raw = data.get(at..at + time_size)?;
            at += time_size;
            times.push(match time_size {
                4 => i32::from_be_bytes([raw[0], raw[1], raw[2], raw[3]]) as i64,
                _ => i64::from_be_bytes([
                    raw[0], raw[1], raw[2], raw[3], raw[4], raw[5], raw[6], raw[7],
                ]),
            });
        }

        // One type index per transition.
        let indices = data.get(at..at + self.timecnt)?;
        at += self.timecnt;

        // Each type is: 4-byte offset, 1-byte isdst, 1-byte abbreviation index.
        let mut offsets = Vec::with_capacity(self.typecnt);
        let mut first_standard = None;
        for index in 0..self.typecnt {
            let raw = data.get(at + index * 6..at + index * 6 + 6)?;
            let offset = i32::from_be_bytes([raw[0], raw[1], raw[2], raw[3]]);
            let is_dst = raw[4] != 0;
            offsets.push(offset);
            if !is_dst && first_standard.is_none() {
                first_standard = Some(offset);
            }
        }

        let mut transitions = Vec::with_capacity(times.len());
        for (time, &index) in times.iter().zip(indices) {
            let offset = *offsets.get(index as usize)?;
            transitions.push((*time, offset));
        }
        transitions.sort_by_key(|&(at, _)| at);

        Some(Zone {
            initial: first_standard.or_else(|| offsets.first().copied())?,
            transitions,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The parse itself, against a hand-built file, so the test does not depend on
    /// which zones this machine happens to have installed.
    #[test]
    fn parses_a_v1_file() {
        let mut data = Vec::new();
        data.extend_from_slice(b"TZif");
        data.push(1); // version 1: 32-bit times
        data.extend_from_slice(&[0; 15]);
        for count in [0u32, 0, 0, 2, 2, 0] {
            data.extend_from_slice(&count.to_be_bytes());
        }
        // Two transitions.
        data.extend_from_slice(&100i32.to_be_bytes());
        data.extend_from_slice(&200i32.to_be_bytes());
        // Their type indices.
        data.extend_from_slice(&[0, 1]);
        // Two types: 0 = no DST at +0, 1 = DST at +3600.
        for (offset, is_dst) in [(0i32, 0u8), (3600, 1)] {
            data.extend_from_slice(&offset.to_be_bytes());
            data.push(is_dst);
            data.push(0); // abbreviation index
        }

        let zone = parse(&data).expect("should parse");
        assert_eq!(zone.offset_at(50), 0, "before the first transition");
        assert_eq!(zone.offset_at(100), 0, "a transition is already in force");
        assert_eq!(zone.offset_at(199), 0);
        assert_eq!(zone.offset_at(200), 3600);
        assert_eq!(zone.offset_at(1 << 40), 3600);
    }

    #[test]
    fn rejects_a_file_that_is_not_tzif() {
        assert!(parse(b"not a zone file at all").is_none());
        assert!(parse(&[]).is_none());
    }

    /// The real database, when this machine has one.
    #[test]
    fn reads_the_installed_database() {
        let Some(offset) = local_offset(0) else {
            eprintln!("no time-zone database on this machine; skipping");
            return;
        };
        // Whatever the zone is, the offset must be a plausible whole-second value.
        assert!(offset.abs() < 15 * 3600, "implausible offset {offset}");
    }
}
