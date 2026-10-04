use serde::{Deserialize, Serialize};

/// Bounded presets: clients select a level, never arbitrary blast parameters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum BowPower {
    Low,
    #[default]
    Standard,
    High,
    Extreme,
}

impl BowPower {
    pub const ALL: [Self; 4] = [Self::Low, Self::Standard, Self::High, Self::Extreme];

    /// Minimum draw fraction a release must reach to fire at this power. The
    /// server validates a client's claimed power against its held draw.
    pub fn min_charge(self) -> f32 {
        match self {
            Self::Low => 0.15,
            Self::Standard => 0.35,
            Self::High => 0.6,
            Self::Extreme => 0.85,
        }
    }

    /// Strongest power a draw fraction has earned, or `None` below the minimum.
    pub fn from_charge(fraction: f32) -> Option<Self> {
        Self::ALL
            .into_iter()
            .rev()
            .find(|power| fraction >= power.min_charge())
    }

    pub fn next(self) -> Self {
        match self {
            Self::Low => Self::Standard,
            Self::Standard => Self::High,
            Self::High => Self::Extreme,
            Self::Extreme => Self::Low,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Low => "0.5x",
            Self::Standard => "1x",
            Self::High => "2x",
            Self::Extreme => "4x",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ClientMessage, MAX_FRAME, decode, encode};

    #[test]
    fn power_presets_cycle_and_round_trip_in_shots() {
        let mut power = BowPower::Low;
        for expected in BowPower::ALL {
            assert_eq!(power, expected);
            let bytes = encode(
                &ClientMessage::FireBow {
                    request: 1,
                    yaw: 0.0,
                    pitch: 0.0,
                    power,
                },
                MAX_FRAME,
            )
            .unwrap();
            let ClientMessage::FireBow { power: decoded, .. } = decode(&bytes, MAX_FRAME).unwrap()
            else {
                panic!("wrong message")
            };
            assert_eq!(decoded, power);
            power = power.next();
        }
        assert_eq!(power, BowPower::Low);
        assert!(decode::<BowPower>(&[4], MAX_FRAME).is_err());
    }
}
