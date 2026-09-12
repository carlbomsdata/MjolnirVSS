//! Property tests for the extent algebra and the checked arithmetic helpers.
//!
//! The invariants asserted here are the ones the restore path relies on. If
//! `split_chunks` could ever emit a piece that leaves its range, a recovery
//! would write bytes into a neighbouring partition.

use mjolnir_core::extents::{ByteRange, ExtentList};
use mjolnir_core::math;
use proptest::prelude::*;

/// Ranges are kept well below `u64::MAX` so the generator explores realistic
/// disk geometry instead of spending every case on overflow, which has its own
/// dedicated tests.
fn range_strategy() -> impl Strategy<Value = ByteRange> {
    (0u64..1_000_000u64, 0u64..10_000u64)
        .prop_map(|(offset, length)| ByteRange::new(offset, length))
}

fn list_strategy() -> impl Strategy<Value = ExtentList> {
    proptest::collection::vec(range_strategy(), 0..40)
        .prop_map(|v| ExtentList::from_unsorted(v).expect("bounded inputs cannot overflow"))
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// The central invariant: the result is sorted, non empty, and no two
    /// ranges touch or overlap.
    #[test]
    fn merged_list_is_sorted_disjoint_and_minimal(list in list_strategy()) {
        let ranges = list.ranges();
        for w in ranges.windows(2) {
            let (a, b) = (w[0], w[1]);
            let a_end = a.end().unwrap();
            prop_assert!(a.offset < b.offset, "not sorted: {a:?} then {b:?}");
            prop_assert!(a_end < b.offset, "touching or overlapping: {a:?} then {b:?}");
        }
        for r in ranges {
            prop_assert!(!r.is_empty(), "empty range survived merging");
        }
    }

    /// Merging never invents or loses coverage: every input byte is still
    /// covered, and no byte outside the input becomes covered.
    #[test]
    fn merging_preserves_coverage(input in proptest::collection::vec(range_strategy(), 0..25)) {
        let list = ExtentList::from_unsorted(input.clone()).unwrap();

        // Every byte of every input range is covered by the result.
        for r in &input {
            for probe in [r.offset, r.offset + r.length / 2, r.offset + r.length.saturating_sub(1)] {
                if r.length > 0 {
                    prop_assert!(list.contains(probe), "lost coverage of {probe} from {r:?}");
                }
            }
        }

        // Nothing outside the union is covered. Checked by walking the gaps.
        let limit = 1_100_000u64;
        for gap in list.gaps_within(limit).unwrap() {
            if gap.length == 0 {
                continue;
            }
            let probe = gap.offset + gap.length / 2;
            let covered_by_input = input
                .iter()
                .any(|r| r.length > 0 && probe >= r.offset && probe < r.offset + r.length);
            prop_assert!(!covered_by_input, "gap at {probe} but input covered it");
            prop_assert!(!list.contains(probe));
        }
    }

    /// Splitting reproduces exactly the same byte coverage, in pieces that
    /// never exceed the chunk size and never leave their parent range.
    #[test]
    fn split_preserves_coverage_and_bounds(
        list in list_strategy(),
        chunk_size in prop::sample::select(vec![512u32, 4096, 65536, 1 << 20]),
    ) {
        let pieces = list.split_chunks(chunk_size).unwrap();

        let total: u64 = pieces.iter().map(|p| p.length).sum();
        prop_assert_eq!(total, list.total_bytes().unwrap());

        for p in &pieces {
            prop_assert!(p.length > 0);
            prop_assert!(p.length <= u64::from(chunk_size));
            // Each piece lies inside exactly one source range.
            let inside = list.ranges().iter().any(|r| {
                p.offset >= r.offset && p.end().unwrap() <= r.end().unwrap()
            });
            prop_assert!(inside, "piece {p:?} escaped its range");
        }

        // Pieces are contiguous within a range and strictly ordered overall.
        for w in pieces.windows(2) {
            prop_assert!(w[0].end().unwrap() <= w[1].offset);
        }
    }

    /// Cut points are stable: a piece that does not start at a range start
    /// begins on a multiple of the chunk size. This is what lets a future
    /// incremental backup match chunks against an existing store.
    #[test]
    fn split_cut_points_are_globally_aligned(
        list in list_strategy(),
        chunk_size in prop::sample::select(vec![512u32, 4096, 1 << 20]),
    ) {
        let starts: Vec<u64> = list.ranges().iter().map(|r| r.offset).collect();
        for p in list.split_chunks(chunk_size).unwrap() {
            let at_range_start = starts.contains(&p.offset);
            let globally_aligned = p.offset % u64::from(chunk_size) == 0;
            prop_assert!(at_range_start || globally_aligned, "unstable cut at {}", p.offset);
        }
    }

    /// Aligning outward may only grow coverage, never shrink it, and the result
    /// is aligned on both edges.
    #[test]
    fn alignment_only_grows(
        list in list_strategy(),
        granularity in prop::sample::select(vec![512u64, 4096, 65536]),
    ) {
        let aligned = list.align_outward(granularity).unwrap();
        prop_assert!(aligned.total_bytes().unwrap() >= list.total_bytes().unwrap());
        for r in aligned.ranges() {
            prop_assert_eq!(r.offset % granularity, 0);
            prop_assert_eq!(r.end().unwrap() % granularity, 0);
        }
        for r in list.ranges() {
            prop_assert!(aligned.contains(r.offset), "alignment dropped {r:?}");
        }
    }

    /// Clamping never produces a byte at or past the limit.
    #[test]
    fn clamp_respects_the_limit(list in list_strategy(), limit in 0u64..1_200_000u64) {
        let clamped = list.clamp_to(limit).unwrap();
        for r in clamped.ranges() {
            prop_assert!(r.end().unwrap() <= limit);
        }
        prop_assert!(clamped.total_bytes().unwrap() <= list.total_bytes().unwrap());
    }

    /// Gaps and ranges together tile `[0, limit)` exactly once.
    #[test]
    fn gaps_and_ranges_tile_the_space(list in list_strategy(), limit in 0u64..1_200_000u64) {
        let clamped = list.clamp_to(limit).unwrap();
        let gaps = clamped.gaps_within(limit).unwrap();
        let covered = clamped.total_bytes().unwrap();
        let gapped: u64 = gaps.iter().map(|g| g.length).sum();
        prop_assert_eq!(covered + gapped, limit);
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2048))]

    /// Checked addition agrees with 128 bit arithmetic on every input, and
    /// fails exactly when the true result does not fit.
    #[test]
    fn checked_add_matches_wide_arithmetic(a: u64, b: u64) {
        let wide = u128::from(a) + u128::from(b);
        match math::add_u64("t", a, b) {
            Ok(v) => prop_assert_eq!(u128::from(v), wide),
            Err(_) => prop_assert!(wide > u128::from(u64::MAX)),
        }
    }

    #[test]
    fn checked_mul_matches_wide_arithmetic(a: u64, b: u64) {
        let wide = u128::from(a) * u128::from(b);
        match math::mul_u64("t", a, b) {
            Ok(v) => prop_assert_eq!(u128::from(v), wide),
            Err(_) => prop_assert!(wide > u128::from(u64::MAX)),
        }
    }

    /// `ensure_within` accepts a range if and only if it truly fits, computed
    /// in 128 bits so the check itself cannot overflow.
    #[test]
    fn ensure_within_matches_wide_arithmetic(offset: u64, length: u64, limit: u64) {
        let fits = u128::from(offset) + u128::from(length) <= u128::from(limit);
        prop_assert_eq!(math::ensure_within("t", offset, length, limit).is_ok(), fits);
    }

    /// Chunk counting never under counts: the chunks always cover the length.
    #[test]
    fn chunk_count_covers_the_length(length in 0u64..(1u64 << 48), chunk_size in 1u32..(1 << 24)) {
        let n = math::chunk_count("t", length, chunk_size).unwrap();
        let covered = u128::from(n) * u128::from(chunk_size);
        prop_assert!(covered >= u128::from(length));
        // And never over counts by a whole chunk.
        prop_assert!(covered < u128::from(length) + u128::from(chunk_size));
    }

    /// Rounding up lands on a multiple and never moves backwards.
    #[test]
    fn round_up_lands_on_a_multiple(value in 0u64..(1u64 << 60), granularity in 1u64..(1 << 20)) {
        let rounded = math::round_up("t", value, granularity).unwrap();
        prop_assert!(rounded >= value);
        prop_assert_eq!(rounded % granularity, 0);
        prop_assert!(rounded - value < granularity);
    }

    /// Overlap detection agrees with a direct 128 bit interval test.
    #[test]
    fn overlap_matches_interval_arithmetic(a_off: u64, a_len: u64, b_off: u64, b_len: u64) {
        let expected = if a_len == 0 || b_len == 0 {
            false
        } else {
            let a_end = u128::from(a_off) + u128::from(a_len);
            let b_end = u128::from(b_off) + u128::from(b_len);
            u128::from(a_off) < b_end && u128::from(b_off) < a_end
        };
        if let Ok(got) = math::ranges_overlap(a_off, a_len, b_off, b_len) {
            prop_assert_eq!(got, expected);
        }
    }
}
