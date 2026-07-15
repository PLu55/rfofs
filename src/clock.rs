//! Selects which JACK time source drives the engine's absolute sample
//! clock each process cycle: the free-running `jack_frame_time()` counter,
//! or the shared JACK transport position (`jack_transport_query()`), which
//! can be started/stopped/relocated independently of wall-clock frame
//! count. Selectable from the CLI (`src/main.rs`) and mirrored as numeric
//! constants for `rfofs-client`'s FFI consumers.

/// Drive the clock from `jack_frame_time()` — a free-running estimate of
/// frames elapsed since the JACK server started. This is the default.
pub const RFOFS_CLOCK_JACK_FRAME_TIME: u32 = 1;
/// Drive the clock from `jack_transport_query()`'s reported frame — follows
/// the shared JACK transport, so FOF scheduling tracks transport
/// start/stop/relocate instead of raw uptime.
pub const RFOFS_CLOCK_JACK_TRANSPORT: u32 = 2;

/// Which JACK time source supplies `RfofsEngine`'s `sample_clock` each
/// block. See the module-level docs for what each mode means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClockMode {
    JackFrameTime,
    JackTransport,
}

impl ClockMode {
    /// The wire/CLI-numeric value for this mode (stable for compatibility
    /// with earlier systems — do not renumber).
    pub fn as_u32(self) -> u32 {
        match self {
            ClockMode::JackFrameTime => RFOFS_CLOCK_JACK_FRAME_TIME,
            ClockMode::JackTransport => RFOFS_CLOCK_JACK_TRANSPORT,
        }
    }

    /// Recover a mode from its numeric value. `None` if `v` isn't one of
    /// the two known constants.
    pub fn from_u32(v: u32) -> Option<Self> {
        match v {
            RFOFS_CLOCK_JACK_FRAME_TIME => Some(ClockMode::JackFrameTime),
            RFOFS_CLOCK_JACK_TRANSPORT => Some(ClockMode::JackTransport),
            _ => None,
        }
    }

    /// Parse a CLI argument: either a name (`"frame-time"`/`"transport"`)
    /// or the raw numeric value (`"1"`/`"2"`).
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "frame-time" | "jack-frame-time" => Some(ClockMode::JackFrameTime),
            "transport" | "jack-transport" => Some(ClockMode::JackTransport),
            _ => s.parse::<u32>().ok().and_then(Self::from_u32),
        }
    }
}

impl Default for ClockMode {
    fn default() -> Self {
        ClockMode::JackFrameTime
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numeric_values_are_stable_for_compatibility() {
        assert_eq!(RFOFS_CLOCK_JACK_FRAME_TIME, 1);
        assert_eq!(RFOFS_CLOCK_JACK_TRANSPORT, 2);
    }

    #[test]
    fn roundtrips_through_numeric_value() {
        for mode in [ClockMode::JackFrameTime, ClockMode::JackTransport] {
            assert_eq!(ClockMode::from_u32(mode.as_u32()), Some(mode));
        }
    }

    #[test]
    fn parses_names_and_numbers() {
        assert_eq!(ClockMode::parse("frame-time"), Some(ClockMode::JackFrameTime));
        assert_eq!(ClockMode::parse("jack-frame-time"), Some(ClockMode::JackFrameTime));
        assert_eq!(ClockMode::parse("transport"), Some(ClockMode::JackTransport));
        assert_eq!(ClockMode::parse("jack-transport"), Some(ClockMode::JackTransport));
        assert_eq!(ClockMode::parse("1"), Some(ClockMode::JackFrameTime));
        assert_eq!(ClockMode::parse("2"), Some(ClockMode::JackTransport));
        assert_eq!(ClockMode::parse("bogus"), None);
    }

    #[test]
    fn default_is_frame_time() {
        assert_eq!(ClockMode::default(), ClockMode::JackFrameTime);
    }
}
