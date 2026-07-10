//! Lookup-table sine approximation for the carrier oscillator hot path.
//!
//! The table is generated at compile time by `build.rs` and baked into the
//! binary's read-only data — no runtime initialization or heap allocation.

// Table contents (`TABLE_BITS`, `SIN_TABLE`) are generated at compile time by
// build.rs and baked into .rodata, so lookups have no runtime init/check cost.
include!(concat!(env!("OUT_DIR"), "/sin_table.rs"));

const TABLE_SIZE: usize = 1 << TABLE_BITS;
const TABLE_MASK: usize = TABLE_SIZE - 1;
const QUARTER_SIZE: usize = 1 << QUARTER_BITS;

/// Fast sine approximation via a nearest-neighbor lookup table.
///
/// `phase` is the fraction of a full cycle (i.e. `radians / TAU`), matching
/// `FofState::carrier_phase`'s convention — wrapped into `[0, 1)` internally,
/// so callers don't need to multiply by `TAU` first.
#[inline]
pub fn fast_sin(phase: f32) -> f32 {
    let phase = phase - phase.floor();
    let idx = (phase * TABLE_SIZE as f32 + 0.5) as usize & TABLE_MASK;
    SIN_TABLE[idx]
}

/// Fast sine approximation via a quarter-wave lookup table.
///
/// Only `[0, π/2]` is tabulated; the rest of the cycle is reconstructed from
/// `sin`'s quadrant symmetry, so `QUARTER_TABLE` gives the same angular
/// resolution as a full-cycle table 4x its size — a smaller table that's
/// more likely to stay resident in L1 under real workloads.
///
/// `phase` uses the same `[0, 1)`-per-cycle convention as [`fast_sin`].
#[inline]
pub fn fast_sin_quarter(phase: f32) -> f32 {
    let phase = phase - phase.floor();
    let x = phase * 4.0; // [0, 4): which quadrant, and position within it
    let quadrant = x as u32;
    let r = x - quadrant as f32; // fraction into the quadrant, [0, 1)
    let quadrant = quadrant & 3; // defensive: guard against fp rounding to 4

    let fwd = ((r * QUARTER_SIZE as f32 + 0.5) as usize).min(QUARTER_SIZE);
    let idx = if quadrant & 1 == 0 { fwd } else { QUARTER_SIZE - fwd };
    let value = QUARTER_TABLE[idx];

    if quadrant & 2 == 0 { value } else { -value }
}

/// The sine implementation used by the FOF synthesis hot path (`src/fof.rs`).
///
/// By default this is the LUT-based [`fast_sin`]. Building with the
/// `std-sin` Cargo feature switches every call site over to `f32::sin`
/// instead — useful for isolating how much of the synthesis cost is the LUT
/// vs. everything else, or for chasing down a suspected LUT-quantization
/// artifact.
///
/// `phase` uses the same "fraction of a cycle" convention as [`fast_sin`].
#[cfg(not(feature = "std-sin"))]
#[inline]
pub fn active_sin(phase: f32) -> f32 {
    fast_sin(phase)
}

/// See the `not(feature = "std-sin")` variant's doc comment.
#[cfg(feature = "std-sin")]
#[inline]
pub fn active_sin(phase: f32) -> f32 {
    (phase * std::f32::consts::TAU).sin()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_std_sin_closely() {
        // Worst case for a rounded nearest-neighbor lookup: half a table step
        // in phase, times sin's max slope (1). Some margin on top for the
        // table itself being built from (imprecise) f32 trig.
        let bound = std::f32::consts::PI / TABLE_SIZE as f32 * 1.5;

        let mut max_err = 0.0f32;
        for i in 0..10_000 {
            let phase = i as f32 / 10_000.0;
            let expected = (phase * std::f32::consts::TAU).sin();
            let actual = fast_sin(phase);
            max_err = max_err.max((expected - actual).abs());
        }
        assert!(max_err < bound, "max error too large: {max_err} (bound {bound})");
    }

    #[test]
    fn wraps_negative_and_large_phase() {
        let a = fast_sin(0.25);
        let b = fast_sin(-0.75);
        let c = fast_sin(4.25);
        assert!((a - b).abs() < 1e-6);
        assert!((a - c).abs() < 1e-6);
    }

    #[test]
    fn quarter_matches_std_sin_closely() {
        // Quarter-wave resolution is (π/2)/QUARTER_SIZE per step, i.e. 1/4 the
        // full table's angular step for the same QUARTER_SIZE — bound scales
        // accordingly (see `matches_std_sin_closely`).
        let bound = std::f32::consts::PI / (4.0 * QUARTER_SIZE as f32) * 1.5;

        let mut max_err = 0.0f32;
        for i in 0..10_000 {
            let phase = i as f32 / 10_000.0;
            let expected = (phase * std::f32::consts::TAU).sin();
            let actual = fast_sin_quarter(phase);
            max_err = max_err.max((expected - actual).abs());
        }
        assert!(max_err < bound, "max error too large: {max_err} (bound {bound})");
    }

    #[test]
    fn quarter_wraps_negative_and_large_phase() {
        let a = fast_sin_quarter(0.25);
        let b = fast_sin_quarter(-0.75);
        let c = fast_sin_quarter(4.25);
        assert!((a - b).abs() < 1e-6);
        assert!((a - c).abs() < 1e-6);
    }

    #[test]
    fn quarter_matches_full_table_family() {
        // Both LUT variants should agree with each other to within the
        // looser of their two quantization bounds.
        let bound = std::f32::consts::PI / QUARTER_SIZE as f32;
        for i in 0..1000 {
            let phase = i as f32 / 1000.0;
            let a = fast_sin(phase);
            let b = fast_sin_quarter(phase);
            assert!((a - b).abs() < bound, "phase {phase}: {a} vs {b}");
        }
    }
}
