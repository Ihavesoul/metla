//! Shared scalar newtypes. Everything here is `Copy` and allocation-free.

/// Index into [`crate::arena::FileArena`]. `NONE` is a "no node" marker that
/// is never stored in the arena itself.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct NodeId(pub u32);

impl NodeId {
    pub const NONE: NodeId = NodeId(u32::MAX);

    #[inline]
    pub fn index(self) -> usize {
        self.0 as usize
    }

    #[inline]
    pub fn is_some(self) -> bool {
        self.0 != u32::MAX
    }
}

/// Byte count (apparent or physical — the call site decides).
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Default, Debug)]
pub struct Bytes(pub u64);

impl Bytes {
    #[inline]
    pub fn as_u64(self) -> u64 {
        self.0
    }
}

impl std::ops::Add for Bytes {
    type Output = Bytes;
    fn add(self, rhs: Bytes) -> Bytes {
        Bytes(self.0 + rhs.0)
    }
}

impl std::ops::Sub for Bytes {
    type Output = Bytes;
    fn sub(self, rhs: Bytes) -> Bytes {
        Bytes(self.0 - rhs.0)
    }
}

/// Seconds since the Unix epoch.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Default, Debug)]
pub struct UnixTime(pub i64);

impl UnixTime {
    /// Whole days elapsed between `self` and `now` (clamped at zero).
    #[inline]
    pub fn days_before(self, now: UnixTime) -> u32 {
        ((now.0 - self.0).max(0) / 86_400) as u32
    }

    /// `y, m, d` in UTC (Howard Hinnant's civil-from-days algorithm).
    pub fn civil(self) -> (i64, u32, u32) {
        let z = self.0.div_euclid(86_400) + 719_468;
        let era = z.div_euclid(146_097);
        let doe = (z - era * 146_097) as u64;
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let y = yoe as i64 + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
        let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
        (if m <= 2 { y + 1 } else { y }, m, d)
    }

    /// `(hh, mm, ss)` in UTC.
    pub fn hms(self) -> (u32, u32, u32) {
        let s = self.0.rem_euclid(86_400);
        ((s / 3600) as u32, ((s % 3600) / 60) as u32, (s % 60) as u32)
    }
}

/// What a directory entry is, without following symlinks.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum NodeKind {
    File,
    Dir,
    Symlink,
    /// FIFOs, sockets, devices — counted but never descended into.
    Other,
}

/// Bit flags stored on a node.
pub mod flags {
    /// Node (and its whole subtree) was deleted or archived away.
    pub const FREED: u8 = 1;
    /// Hardlink duplicate: the first-seen link carries the size, this one
    /// reports zero so disks are never double-counted.
    pub const HARDLINK: u8 = 2;
}
