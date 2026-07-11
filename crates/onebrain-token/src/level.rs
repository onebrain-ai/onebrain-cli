//! Optimization-level ladder: `off` → `conservative` → `balanced` → `aggressive`.
//!
//! Each rung is a strict superset of the previous one (see the v3.4.10
//! design §2). Ordering matters: `OptLevel` derives `PartialOrd`/`Ord` so
//! [`crate::guard::run_funnel`] can gate transforms with `min_level() <= level`.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

/// The four rungs of the optimization ladder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub enum OptLevel {
    /// Default: byte-for-byte today's behavior — safe when no level is set.
    #[default]
    Off,
    Conservative,
    Balanced,
    Aggressive,
}

// Serde impls route through `Display`/`FromStr` so the wire format and the
// CLI/config string format (`off|conservative|balanced|aggressive`) can
// never drift from each other — one source of truth.
impl Serialize for OptLevel {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for OptLevel {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

impl OptLevel {
    /// All levels, in ascending order — the single source of truth for
    /// "valid values" listings and iteration.
    pub const ALL: [OptLevel; 4] = [
        OptLevel::Off,
        OptLevel::Conservative,
        OptLevel::Balanced,
        OptLevel::Aggressive,
    ];
}

impl fmt::Display for OptLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            OptLevel::Off => "off",
            OptLevel::Conservative => "conservative",
            OptLevel::Balanced => "balanced",
            OptLevel::Aggressive => "aggressive",
        };
        f.write_str(s)
    }
}

/// Returned by [`OptLevel::from_str`] on unrecognized input. Carries the
/// original input plus the full valid-values list so callers can render a
/// helpful error without re-deriving the list themselves.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
#[error("invalid opt level {input:?}; valid values: off, conservative, balanced, aggressive")]
pub struct InvalidOptLevel {
    pub input: String,
}

impl FromStr for OptLevel {
    type Err = InvalidOptLevel;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "off" => Ok(OptLevel::Off),
            "conservative" => Ok(OptLevel::Conservative),
            "balanced" => Ok(OptLevel::Balanced),
            "aggressive" => Ok(OptLevel::Aggressive),
            _ => Err(InvalidOptLevel {
                input: s.to_string(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_every_level_through_display_and_from_str() {
        for level in OptLevel::ALL {
            let rendered = level.to_string();
            let parsed: OptLevel = rendered.parse().expect("valid level string must parse");
            assert_eq!(parsed, level);
        }
    }

    #[test]
    fn exact_strings_match_spec() {
        assert_eq!(OptLevel::Off.to_string(), "off");
        assert_eq!(OptLevel::Conservative.to_string(), "conservative");
        assert_eq!(OptLevel::Balanced.to_string(), "balanced");
        assert_eq!(OptLevel::Aggressive.to_string(), "aggressive");
    }

    #[test]
    fn bad_input_lists_valid_values() {
        let err = "yolo".parse::<OptLevel>().unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("yolo"));
        for valid in ["off", "conservative", "balanced", "aggressive"] {
            assert!(
                msg.contains(valid),
                "error message {msg:?} must list {valid:?}"
            );
        }
    }

    #[test]
    fn ordering_is_ascending_ladder() {
        assert!(OptLevel::Off < OptLevel::Conservative);
        assert!(OptLevel::Conservative < OptLevel::Balanced);
        assert!(OptLevel::Balanced < OptLevel::Aggressive);
    }
}
