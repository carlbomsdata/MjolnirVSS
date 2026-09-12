//! Checked arithmetic for sectors, offsets, lengths and counts.
//!
//! Every number that reaches these helpers comes from a manifest, a partition
//! table or a device, and is therefore untrusted. Silent wrapping would turn a
//! corrupt backup into an out-of-bounds write against a physical disk, so all
//! of it goes through here and every failure names the operands.

use std::fmt;

/// Why a checked operation failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArithErrorKind {
    /// The result does not fit in the destination type.
    Overflow,
    /// The result would be negative.
    Underflow,
    /// The divisor was zero.
    DivideByZero,
    /// The value does not fit in a narrower type it has to be converted to.
    OutOfRange,
    /// The value is not a whole multiple of the required granularity.
    NotAligned,
}

impl ArithErrorKind {
    const fn describe(self) -> &'static str {
        match self {
            ArithErrorKind::Overflow => "overflows",
            ArithErrorKind::Underflow => "underflows",
            ArithErrorKind::DivideByZero => "divides by zero",
            ArithErrorKind::OutOfRange => "is out of range",
            ArithErrorKind::NotAligned => "is not aligned",
        }
    }
}

/// An arithmetic operation that could not be carried out safely.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArithError {
    /// What was being computed, for the operator facing message.
    pub what: &'static str,
    /// Left operand.
    pub lhs: u128,
    /// Right operand.
    pub rhs: u128,
    /// The kind of failure.
    pub kind: ArithErrorKind,
}

impl fmt::Display for ArithError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} {} ({} and {})",
            self.what,
            self.kind.describe(),
            self.lhs,
            self.rhs
        )
    }
}

impl std::error::Error for ArithError {}

/// Result alias for the helpers in this module.
pub type ArithResult<T> = Result<T, ArithError>;

fn err(what: &'static str, lhs: u128, rhs: u128, kind: ArithErrorKind) -> ArithError {
    ArithError {
        what,
        lhs,
        rhs,
        kind,
    }
}

/// `a + b`, refusing to wrap.
pub fn add_u64(what: &'static str, a: u64, b: u64) -> ArithResult<u64> {
    a.checked_add(b)
        .ok_or_else(|| err(what, u128::from(a), u128::from(b), ArithErrorKind::Overflow))
}

/// `a - b`, refusing to go negative.
pub fn sub_u64(what: &'static str, a: u64, b: u64) -> ArithResult<u64> {
    a.checked_sub(b).ok_or_else(|| {
        err(
            what,
            u128::from(a),
            u128::from(b),
            ArithErrorKind::Underflow,
        )
    })
}

/// `a * b`, refusing to wrap.
pub fn mul_u64(what: &'static str, a: u64, b: u64) -> ArithResult<u64> {
    a.checked_mul(b)
        .ok_or_else(|| err(what, u128::from(a), u128::from(b), ArithErrorKind::Overflow))
}

/// `a / b`, refusing to divide by zero.
pub fn div_u64(what: &'static str, a: u64, b: u64) -> ArithResult<u64> {
    a.checked_div(b).ok_or_else(|| {
        err(
            what,
            u128::from(a),
            u128::from(b),
            ArithErrorKind::DivideByZero,
        )
    })
}

/// The end offset of a range, that is `offset + length`.
///
/// Used everywhere a segment, partition or extent is bounds checked.
pub fn range_end(what: &'static str, offset: u64, length: u64) -> ArithResult<u64> {
    add_u64(what, offset, length)
}

/// Checks that `offset + length` stays inside `limit`.
pub fn ensure_within(what: &'static str, offset: u64, length: u64, limit: u64) -> ArithResult<()> {
    let end = range_end(what, offset, length)?;
    if end > limit {
        return Err(err(
            what,
            u128::from(end),
            u128::from(limit),
            ArithErrorKind::OutOfRange,
        ));
    }
    Ok(())
}

/// Narrows a `u64` to a `usize`, refusing to truncate.
pub fn to_usize(what: &'static str, value: u64) -> ArithResult<usize> {
    usize::try_from(value).map_err(|_| {
        err(
            what,
            u128::from(value),
            usize::MAX as u128,
            ArithErrorKind::OutOfRange,
        )
    })
}

/// Narrows a `u64` to a `u32`, refusing to truncate.
pub fn to_u32(what: &'static str, value: u64) -> ArithResult<u32> {
    u32::try_from(value).map_err(|_| {
        err(
            what,
            u128::from(value),
            u128::from(u32::MAX),
            ArithErrorKind::OutOfRange,
        )
    })
}

/// Converts a sector count to a byte count.
pub fn sectors_to_bytes(what: &'static str, sectors: u64, sector_size: u32) -> ArithResult<u64> {
    if sector_size == 0 {
        return Err(err(
            what,
            u128::from(sectors),
            0,
            ArithErrorKind::DivideByZero,
        ));
    }
    mul_u64(what, sectors, u64::from(sector_size))
}

/// Converts a byte count to a sector count, refusing a partial sector.
pub fn bytes_to_sectors(what: &'static str, bytes: u64, sector_size: u32) -> ArithResult<u64> {
    if sector_size == 0 {
        return Err(err(
            what,
            u128::from(bytes),
            0,
            ArithErrorKind::DivideByZero,
        ));
    }
    let size = u64::from(sector_size);
    if bytes % size != 0 {
        return Err(err(
            what,
            u128::from(bytes),
            u128::from(size),
            ArithErrorKind::NotAligned,
        ));
    }
    div_u64(what, bytes, size)
}

/// Checks that `value` is a whole multiple of `granularity`.
pub fn ensure_aligned(what: &'static str, value: u64, granularity: u64) -> ArithResult<()> {
    if granularity == 0 {
        return Err(err(
            what,
            u128::from(value),
            0,
            ArithErrorKind::DivideByZero,
        ));
    }
    if value % granularity != 0 {
        return Err(err(
            what,
            u128::from(value),
            u128::from(granularity),
            ArithErrorKind::NotAligned,
        ));
    }
    Ok(())
}

/// Number of chunks needed to cover `length` bytes at `chunk_size` bytes each.
pub fn chunk_count(what: &'static str, length: u64, chunk_size: u32) -> ArithResult<u64> {
    if chunk_size == 0 {
        return Err(err(
            what,
            u128::from(length),
            0,
            ArithErrorKind::DivideByZero,
        ));
    }
    let size = u64::from(chunk_size);
    let full = div_u64(what, length, size)?;
    if length % size == 0 {
        Ok(full)
    } else {
        add_u64(what, full, 1)
    }
}

/// Rounds `value` up to the next multiple of `granularity`.
pub fn round_up(what: &'static str, value: u64, granularity: u64) -> ArithResult<u64> {
    if granularity == 0 {
        return Err(err(
            what,
            u128::from(value),
            0,
            ArithErrorKind::DivideByZero,
        ));
    }
    let rem = value % granularity;
    if rem == 0 {
        return Ok(value);
    }
    add_u64(what, value, granularity - rem)
}

/// Whether the half open ranges `[a_off, a_off + a_len)` and
/// `[b_off, b_off + b_len)` share any byte.
///
/// Zero length ranges never overlap anything, which is what the callers want:
/// an empty segment covers no bytes.
pub fn ranges_overlap(a_off: u64, a_len: u64, b_off: u64, b_len: u64) -> ArithResult<bool> {
    if a_len == 0 || b_len == 0 {
        return Ok(false);
    }
    let a_end = range_end("overlap check", a_off, a_len)?;
    let b_end = range_end("overlap check", b_off, b_len)?;
    Ok(a_off < b_end && b_off < a_end)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_detects_overflow() {
        assert_eq!(add_u64("t", 1, 2).unwrap(), 3);
        let e = add_u64("disk offset", u64::MAX, 1).unwrap_err();
        assert_eq!(e.kind, ArithErrorKind::Overflow);
        assert!(e.to_string().contains("disk offset"));
    }

    #[test]
    fn sub_detects_underflow() {
        assert_eq!(sub_u64("t", 5, 3).unwrap(), 2);
        assert_eq!(
            sub_u64("t", 3, 5).unwrap_err().kind,
            ArithErrorKind::Underflow
        );
    }

    #[test]
    fn mul_detects_overflow() {
        assert_eq!(mul_u64("t", 1 << 20, 1 << 20).unwrap(), 1 << 40);
        assert_eq!(
            mul_u64("t", u64::MAX, 2).unwrap_err().kind,
            ArithErrorKind::Overflow
        );
    }

    #[test]
    fn ensure_within_rejects_past_end() {
        assert!(ensure_within("t", 0, 10, 10).is_ok());
        assert_eq!(
            ensure_within("t", 1, 10, 10).unwrap_err().kind,
            ArithErrorKind::OutOfRange
        );
        // Overflow must be reported as overflow, not silently accepted.
        assert_eq!(
            ensure_within("t", u64::MAX, 1, u64::MAX).unwrap_err().kind,
            ArithErrorKind::Overflow
        );
    }

    #[test]
    fn sector_conversions_round_trip() {
        assert_eq!(sectors_to_bytes("t", 2048, 512).unwrap(), 1_048_576);
        assert_eq!(bytes_to_sectors("t", 1_048_576, 512).unwrap(), 2048);
        assert_eq!(
            bytes_to_sectors("t", 513, 512).unwrap_err().kind,
            ArithErrorKind::NotAligned
        );
        assert_eq!(
            sectors_to_bytes("t", 1, 0).unwrap_err().kind,
            ArithErrorKind::DivideByZero
        );
        assert_eq!(
            sectors_to_bytes("t", u64::MAX, 512).unwrap_err().kind,
            ArithErrorKind::Overflow
        );
    }

    #[test]
    fn chunk_count_covers_partial_tail() {
        assert_eq!(chunk_count("t", 0, 4096).unwrap(), 0);
        assert_eq!(chunk_count("t", 4096, 4096).unwrap(), 1);
        assert_eq!(chunk_count("t", 4097, 4096).unwrap(), 2);
        assert_eq!(
            chunk_count("t", 1, 0).unwrap_err().kind,
            ArithErrorKind::DivideByZero
        );
    }

    #[test]
    fn round_up_is_idempotent_on_multiples() {
        assert_eq!(round_up("t", 0, 512).unwrap(), 0);
        assert_eq!(round_up("t", 512, 512).unwrap(), 512);
        assert_eq!(round_up("t", 513, 512).unwrap(), 1024);
        assert_eq!(
            round_up("t", u64::MAX, 512).unwrap_err().kind,
            ArithErrorKind::Overflow
        );
    }

    #[test]
    fn overlap_is_half_open() {
        // Touching ranges do not overlap.
        assert!(!ranges_overlap(0, 10, 10, 10).unwrap());
        assert!(ranges_overlap(0, 11, 10, 10).unwrap());
        assert!(!ranges_overlap(0, 0, 0, 10).unwrap());
        assert!(ranges_overlap(5, 1, 0, 10).unwrap());
    }

    #[test]
    fn narrowing_refuses_to_truncate() {
        assert_eq!(to_u32("t", 42).unwrap(), 42);
        assert_eq!(
            to_u32("t", u64::from(u32::MAX) + 1).unwrap_err().kind,
            ArithErrorKind::OutOfRange
        );
        assert_eq!(to_usize("t", 42).unwrap(), 42);
    }
}
