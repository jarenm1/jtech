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
