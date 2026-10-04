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

    /// Minimum draw fraction a release must reach to fire at this power.
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MAX_FRAME, decode, encode};

    #[test]
    fn power_presets_map_from_draw_fractions_and_round_trip() {
        assert_eq!(BowPower::from_charge(0.0), None);
        assert_eq!(BowPower::from_charge(0.14), None);
        assert_eq!(BowPower::from_charge(0.15), Some(BowPower::Low));
        assert_eq!(BowPower::from_charge(0.5), Some(BowPower::Standard));
        assert_eq!(BowPower::from_charge(0.7), Some(BowPower::High));
        assert_eq!(BowPower::from_charge(1.0), Some(BowPower::Extreme));
        for power in BowPower::ALL {
            let bytes = encode(&power, MAX_FRAME).unwrap();
            assert_eq!(decode::<BowPower>(&bytes, MAX_FRAME).unwrap(), power);
        }
        assert!(decode::<BowPower>(&[4], MAX_FRAME).is_err());
    }
}
