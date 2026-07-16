/// Global panning mode, set at startup, uniform for all FOFs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PanMode {
    Mono,

    /// Equal-power stereo panning using azimuth only.
    Stereo,

    /// Ambisonics: order `n`, dimensions `m` (2 or 3), optional reverb bus of
    /// order `k` (0 or 1).
    ///
    /// Notation: ambOnDmRk  e.g. amb O1 D3 R1 = first-order 3D plus a
    /// first-order reverb bus; amb O2 D3 R0 = second-order 3D plus a mono
    /// reverb bus. When the `R` token is omitted entirely there is no
    /// reverb bus at all (channel count is just the direct signal's).
    ///
    /// The reverb bus's *size* is fixed by `k` alone — it does not scale
    /// with the main signal's `order` (`k == 0` is always 1 mono channel,
    /// `k == 1` is always the 4 (3D) / 3 (2D) first-order channels, even if
    /// `order` is higher). See `encode_ambisonics`/`pan_sample` for what
    /// feeds it: a distance-independent send, muted entirely below
    /// distance 1.0.
    Ambisonic {
        order: u8,
        dims: u8,           // 2 = horizontal only, 3 = full sphere
        reverb: Option<u8>, // None = no reverb bus; Some(0) = mono; Some(1) = first-order
    },
}

impl PanMode {
    /// Number of output channels this mode produces.
    pub fn channel_count(&self) -> usize {
        match self {
            PanMode::Mono => 1,
            PanMode::Stereo => 2,
            PanMode::Ambisonic { order, dims, reverb } => {
                let direct = ambisonic_direct_channel_count(*order, *dims);
                let rev = match reverb {
                    None => 0,
                    Some(0) => 1, // mono reverb bus
                    Some(_) => ambisonic_direct_channel_count(1, *dims), // first-order reverb bus
                };
                direct + rev
            }
        }
    }

    /// Parse the ambOnDmRk notation string, e.g. "amb O2 D3 R1" or "amb O1 D2".
    /// Returns None if the string is not recognised.
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim();
        if s.eq_ignore_ascii_case("mono") {
            return Some(PanMode::Mono);
        }
        if s.eq_ignore_ascii_case("stereo") {
            return Some(PanMode::Stereo);
        }
        // Expect prefix "amb" then tokens like O<n> D<m> [R<k>]
        let rest = s.strip_prefix("amb")
            .or_else(|| s.strip_prefix("Amb"))
            .or_else(|| s.strip_prefix("AMB"))?
            .trim();

        let mut order = None::<u8>;
        let mut dims  = None::<u8>;
        let mut reverb = None::<u8>;

        for token in rest.split_whitespace() {
            let upper = token.to_ascii_uppercase();
            if let Some(n) = upper.strip_prefix('O') {
                order = n.parse().ok();
            } else if let Some(m) = upper.strip_prefix('D') {
                dims = m.parse().ok();
            } else if let Some(k) = upper.strip_prefix('R') {
                reverb = Some(k.parse().unwrap_or(0));
            }
        }

        Some(PanMode::Ambisonic {
            order:  order?,
            dims:   dims?,
            reverb,
        })
    }
}

// ─────────────────────────────────────────────────────────────────────────────

/// Compute per-channel gain weights for a FOF at the given spatial position.
///
/// `gains` must have length `mode.channel_count()` and be pre-zeroed.
/// Multiply each output sample by `gains[ch]` and accumulate into the channel.
#[inline]
pub fn pan_gains(azm: f32, elev: f32, distance: f32, mode: PanMode, gains: &mut [f32]) {
    pan_sample(1.0, azm, elev, distance, mode, gains);
}

/// Apply panning to a mono FOF sample, accumulating into `out`.
///
/// `out` must have exactly `mode.channel_count()` channels worth of space
/// for this sample position.  The caller provides a flat slice per-channel:
/// `out[ch]` is the buffer for channel `ch`.
#[inline]
fn pan_sample(
    sample: f32,
    azm: f32,
    elev: f32,
    distance: f32,
    mode: PanMode,
    out: &mut [f32],
) {
    // Distance attenuation — simple 1/d model, clamped.
    let dist_gain = if distance > 0.0 { 1.0 / distance.max(0.01) } else { 1.0 };
    let s = sample * dist_gain;

    match mode {
        PanMode::Mono => {
            out[0] += s;
        }

        PanMode::Stereo => {
            // Equal-power pan: azm = 0 → centre, ±π/2 → hard L/R
            let angle = (azm.clamp(-std::f32::consts::FRAC_PI_2,
                                    std::f32::consts::FRAC_PI_2)
                         + std::f32::consts::FRAC_PI_2)
                        * 0.5; // map to [0, π/2]
            let (sin_a, cos_a) = angle.sin_cos();
            out[0] += s * cos_a; // left
            out[1] += s * sin_a; // right
        }

        PanMode::Ambisonic { order, dims, reverb } => {
            // B-format encoding.  For order 1:
            //   W = s / sqrt(2)
            //   X = s · cos(elev) · cos(azm)
            //   Y = s · cos(elev) · sin(azm)
            //   Z = s · sin(elev)          [3D only]
            //
            // Higher orders: spherical harmonic encoding (stub — extend as needed).
            let direct = ambisonic_direct_channel_count(order, dims);
            let (direct_out, rev_out) = out.split_at_mut(direct);
            encode_ambisonics(s, azm, elev, order, dims, direct_out);

            // Reverb bus: a distance-independent send (`sample`, not the
            // distance-attenuated `s`) — its level doesn't fall off with
            // distance the way the direct signal does — muted entirely for
            // sources closer than the reference distance. See
            // `PanMode::Ambisonic`'s doc comment.
            if let Some(reverb_order) = reverb
                && distance >= 1.0
            {
                if reverb_order == 0 {
                    if let Some(mono) = rev_out.first_mut() {
                        *mono += sample * std::f32::consts::FRAC_1_SQRT_2;
                    }
                } else {
                    if reverb_order > 1 {
                        log::warn!(
                            "Ambisonic reverb order > 1 not yet implemented; only order-1 reverb encoded."
                        );
                    }
                    encode_ambisonics(sample, azm, elev, 1, dims, rev_out);
                }
            }
        }
    }
}

/// Direct (non-reverb) ambisonic channel count for `order`/`dims` — shared
/// by [`PanMode::channel_count`] and [`pan_sample`] so they can't drift
/// apart on the direct/reverb split point.
fn ambisonic_direct_channel_count(order: u8, dims: u8) -> usize {
    let n = order as usize;
    match dims {
        2 => 2 * n + 1,         // horizontal: (2n+1) channels
        _ => (n + 1) * (n + 1), // 3D: (n+1)² channels
    }
}

/// Encode a sample into ambisonics B-format channels.
/// `out` slice is ordered [W, X, Y, (Z), higher-order ...].
fn encode_ambisonics(s: f32, azm: f32, elev: f32, order: u8, dims: u8, out: &mut [f32]) {
    let cos_e = elev.cos();
    let sin_e = elev.sin();
    let cos_a = azm.cos();
    let sin_a = azm.sin();

    // Order 0
    if out.is_empty() { return; }
    out[0] += s * std::f32::consts::FRAC_1_SQRT_2; // W

    if order >= 1 && out.len() > 1 {
        // Order 1
        out[1] += s * cos_e * cos_a; // X
        if out.len() > 2 {
            out[2] += s * cos_e * sin_a; // Y
        }
        if dims == 3 && out.len() > 3 {
            out[3] += s * sin_e;          // Z
        }
    }

    // Orders 2+ are stubbed — real spherical harmonic coefficients to be added.
    // The channel layout follows ACN/SN3D convention.
    if order >= 2 {
        log::warn!("Ambisonic order > 1 not yet implemented; only order-1 encoded.");
    }
}

// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_mono() {
        assert_eq!(PanMode::parse("mono"), Some(PanMode::Mono));
    }

    #[test]
    fn parse_stereo() {
        assert_eq!(PanMode::parse("stereo"), Some(PanMode::Stereo));
    }

    #[test]
    fn parse_amb_o1_d2() {
        assert_eq!(
            PanMode::parse("amb O1 D2"),
            Some(PanMode::Ambisonic { order: 1, dims: 2, reverb: None })
        );
    }

    #[test]
    fn parse_amb_o1_d3_r0() {
        assert_eq!(
            PanMode::parse("amb O1 D3 R0"),
            Some(PanMode::Ambisonic { order: 1, dims: 3, reverb: Some(0) })
        );
    }

    #[test]
    fn parse_amb_o3_d3_r1() {
        assert_eq!(
            PanMode::parse("amb O3 D3 R1"),
            Some(PanMode::Ambisonic { order: 3, dims: 3, reverb: Some(1) })
        );
    }

    #[test]
    fn channel_count_stereo() {
        assert_eq!(PanMode::Stereo.channel_count(), 2);
    }

    #[test]
    fn channel_count_amb_o1_d3() {
        // (1+1)² = 4, no reverb bus (no R token at all)
        assert_eq!(
            PanMode::Ambisonic { order: 1, dims: 3, reverb: None }.channel_count(),
            4
        );
    }

    #[test]
    fn channel_count_amb_o1_d3_r0() {
        // 4 direct + 1 mono reverb = 5
        assert_eq!(
            PanMode::Ambisonic { order: 1, dims: 3, reverb: Some(0) }.channel_count(),
            5
        );
    }

    #[test]
    fn channel_count_amb_o1_d3_r1() {
        // 4 direct + 4 (first-order) reverb = 8
        assert_eq!(
            PanMode::Ambisonic { order: 1, dims: 3, reverb: Some(1) }.channel_count(),
            8
        );
    }

    #[test]
    fn channel_count_reverb_bus_size_independent_of_main_order() {
        // Main order 2 -> 9 direct channels, but an R1 reverb bus is always
        // first-order (4 channels), never mirroring the main order.
        assert_eq!(
            PanMode::Ambisonic { order: 2, dims: 3, reverb: Some(1) }.channel_count(),
            13
        );
    }

    #[test]
    fn reverb_bus_muted_below_reference_distance() {
        let mode = PanMode::Ambisonic { order: 1, dims: 3, reverb: Some(1) };
        let mut gains = vec![0.0; mode.channel_count()];
        pan_gains(0.0, 0.0, 0.5, mode, &mut gains); // distance < 1.0
        assert_eq!(&gains[4..8], &[0.0; 4]);
    }

    #[test]
    fn reverb_bus_active_at_reference_distance() {
        let mode = PanMode::Ambisonic { order: 1, dims: 3, reverb: Some(1) };
        let mut gains = vec![0.0; mode.channel_count()];
        pan_gains(0.0, 0.0, 1.0, mode, &mut gains); // distance == 1.0, not muted
        assert!(gains[4] != 0.0);
    }

    #[test]
    fn reverb_bus_mono_is_constant_level_regardless_of_distance() {
        let mode = PanMode::Ambisonic { order: 1, dims: 3, reverb: Some(0) };
        let mut near = vec![0.0; mode.channel_count()];
        let mut far = vec![0.0; mode.channel_count()];
        pan_gains(0.0, 0.0, 1.0, mode, &mut near);
        pan_gains(0.0, 0.0, 100.0, mode, &mut far);
        assert_eq!(near[4], far[4]);
        assert!((near[4] - std::f32::consts::FRAC_1_SQRT_2).abs() < 1e-6);
    }

    #[test]
    fn reverb_bus_first_order_matches_direct_at_reference_distance() {
        // Main is already first-order, and at distance == 1.0 the direct
        // signal is undistanced too, so the reverb bus exactly matches the
        // direct channels ("just a copy if main is first order").
        let mode = PanMode::Ambisonic { order: 1, dims: 3, reverb: Some(1) };
        let mut gains = vec![0.0; mode.channel_count()];
        pan_gains(0.3, 0.2, 1.0, mode, &mut gains);
        assert_eq!(gains[0..4], gains[4..8]);
    }

    #[test]
    fn reverb_bus_first_order_independent_of_distance_beyond_reference() {
        let mode = PanMode::Ambisonic { order: 1, dims: 3, reverb: Some(1) };
        let mut near = vec![0.0; mode.channel_count()];
        let mut far = vec![0.0; mode.channel_count()];
        pan_gains(0.3, 0.2, 1.0, mode, &mut near);
        pan_gains(0.3, 0.2, 50.0, mode, &mut far);
        // Direct channels shrink with distance...
        assert!(near[0] > far[0]);
        // ...but the reverb bus doesn't.
        assert_eq!(near[4..8], far[4..8]);
    }

    #[test]
    fn reverb_bus_first_order_fixed_size_even_with_higher_main_order() {
        // order 2 -> 9 direct channels, reverb bus is still just 4.
        let mode = PanMode::Ambisonic { order: 2, dims: 3, reverb: Some(1) };
        let mut gains = vec![0.0; mode.channel_count()];
        pan_gains(0.0, 0.0, 1.0, mode, &mut gains);
        assert_eq!(gains.len(), 13);
        assert!(gains[9] != 0.0); // reverb W landed right after the 9 direct channels
    }
}
