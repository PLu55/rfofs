/// Global panning mode, set at startup, uniform for all FOFs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PanMode {
    Mono,

    /// Equal-power stereo panning using azimuth only.
    Stereo,

    /// Ambisonics: order `n`, dimensions `m` (2 or 3), reverb order `k` (0 or 1).
    ///
    /// Notation: ambOnDmRk  e.g. amb O1 D3 R1 = first-order 3D with reverb channels.
    /// When k == 0 the reverb channels are omitted (plain ambisonics).
    Ambisonic {
        order: u8,
        dims: u8,   // 2 = horizontal only, 3 = full sphere
        reverb: u8, // 0 = no reverb channels, 1 = with reverb channels
    },
}

impl PanMode {
    /// Number of output channels this mode produces.
    pub fn channel_count(&self) -> usize {
        match self {
            PanMode::Mono => 1,
            PanMode::Stereo => 2,
            PanMode::Ambisonic { order, dims, reverb } => {
                let n = *order as usize;
                let direct = match dims {
                    2 => 2 * n + 1,          // horizontal: (2n+1) channels
                    _ => (n + 1) * (n + 1),  // 3D: (n+1)² channels
                };
                let rev = if *reverb == 1 { direct } else { 0 };
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
        let mut reverb = 0u8;

        for token in rest.split_whitespace() {
            let upper = token.to_ascii_uppercase();
            if let Some(n) = upper.strip_prefix('O') {
                order = n.parse().ok();
            } else if let Some(m) = upper.strip_prefix('D') {
                dims = m.parse().ok();
            } else if let Some(k) = upper.strip_prefix('R') {
                reverb = k.parse().unwrap_or(0);
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

        PanMode::Ambisonic { order, dims, reverb: _ } => {
            // B-format encoding.  For order 1:
            //   W = s / sqrt(2)
            //   X = s · cos(elev) · cos(azm)
            //   Y = s · cos(elev) · sin(azm)
            //   Z = s · sin(elev)          [3D only]
            //
            // Higher orders: spherical harmonic encoding (stub — extend as needed).
            encode_ambisonics(s, azm, elev, order, dims, out);
        }
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
            Some(PanMode::Ambisonic { order: 1, dims: 2, reverb: 0 })
        );
    }

    #[test]
    fn parse_amb_o3_d3_r1() {
        assert_eq!(
            PanMode::parse("amb O3 D3 R1"),
            Some(PanMode::Ambisonic { order: 3, dims: 3, reverb: 1 })
        );
    }

    #[test]
    fn channel_count_stereo() {
        assert_eq!(PanMode::Stereo.channel_count(), 2);
    }

    #[test]
    fn channel_count_amb_o1_d3() {
        // (1+1)² = 4
        assert_eq!(
            PanMode::Ambisonic { order: 1, dims: 3, reverb: 0 }.channel_count(),
            4
        );
    }

    #[test]
    fn channel_count_amb_o1_d3_r1() {
        // 4 direct + 4 reverb = 8
        assert_eq!(
            PanMode::Ambisonic { order: 1, dims: 3, reverb: 1 }.channel_count(),
            8
        );
    }
}
