//! Easing curves for time-based animations.
//!
//! An easing maps normalized time (`0.0..=1.0`) to normalized progress. Most
//! curves stay within `0.0..=1.0`, while back and elastic curves intentionally
//! overshoot for a springier feel.

use std::f32::consts::{FRAC_PI_2, PI, TAU};

/// Constants for the "back" family (a small overshoot before settling).
const BACK_OVERSHOOT: f32 = 1.70158;
const BACK_OVERSHOOT_IN_OUT: f32 = BACK_OVERSHOOT * 1.525;
const BACK_OVERSHOOT_OUT: f32 = BACK_OVERSHOOT + 1.0;

/// Oscillation periods for the "elastic" family.
const ELASTIC_PERIOD: f32 = TAU / 3.0;
const ELASTIC_PERIOD_IN_OUT: f32 = TAU / 4.5;

/// Parameters of the classic four-segment bounce curve.
const BOUNCE_SCALE: f32 = 7.5625;
const BOUNCE_DIVISOR: f32 = 2.75;

/// A standard easing curve.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Easing {
    /// Constant speed.
    #[default]
    Linear,
    EaseInSine,
    EaseOutSine,
    EaseInOutSine,
    EaseInQuad,
    EaseOutQuad,
    EaseInOutQuad,
    EaseInCubic,
    EaseOutCubic,
    EaseInOutCubic,
    EaseInQuart,
    EaseOutQuart,
    EaseInOutQuart,
    EaseInQuint,
    EaseOutQuint,
    EaseInOutQuint,
    EaseInExpo,
    EaseOutExpo,
    EaseInOutExpo,
    EaseInCirc,
    EaseOutCirc,
    EaseInOutCirc,
    EaseInBack,
    EaseOutBack,
    EaseInOutBack,
    EaseInElastic,
    EaseOutElastic,
    EaseInOutElastic,
    EaseInBounce,
    EaseOutBounce,
    EaseInOutBounce,
}

impl Easing {
    /// Maps normalized `time` on `0.0..=1.0` to eased progress.
    ///
    /// Input outside `0.0..=1.0` is clamped, but the result is not: curves
    /// such as [`Easing::EaseOutBack`] overshoot on purpose.
    pub fn apply(self, time: f32) -> f32 {
        let t = time.clamp(0.0, 1.0);

        match self {
            Self::Linear => t,
            Self::EaseInSine => 1.0 - (t * FRAC_PI_2).cos(),
            Self::EaseOutSine => (t * FRAC_PI_2).sin(),
            Self::EaseInOutSine => -((PI * t).cos() - 1.0) / 2.0,
            Self::EaseInQuad => t * t,
            Self::EaseOutQuad => 1.0 - (1.0 - t).powi(2),
            Self::EaseInOutQuad => in_out_power(t, 2),
            Self::EaseInCubic => t.powi(3),
            Self::EaseOutCubic => 1.0 - (1.0 - t).powi(3),
            Self::EaseInOutCubic => in_out_power(t, 3),
            Self::EaseInQuart => t.powi(4),
            Self::EaseOutQuart => 1.0 - (1.0 - t).powi(4),
            Self::EaseInOutQuart => in_out_power(t, 4),
            Self::EaseInQuint => t.powi(5),
            Self::EaseOutQuint => 1.0 - (1.0 - t).powi(5),
            Self::EaseInOutQuint => in_out_power(t, 5),
            Self::EaseInExpo => {
                if t == 0.0 {
                    0.0
                } else {
                    2.0_f32.powf(10.0 * t - 10.0)
                }
            }
            Self::EaseOutExpo => {
                if t == 1.0 {
                    1.0
                } else {
                    1.0 - 2.0_f32.powf(-10.0 * t)
                }
            }
            Self::EaseInOutExpo => {
                if t == 0.0 {
                    0.0
                } else if t == 1.0 {
                    1.0
                } else if t < 0.5 {
                    2.0_f32.powf(20.0 * t - 10.0) / 2.0
                } else {
                    (2.0 - 2.0_f32.powf(-20.0 * t + 10.0)) / 2.0
                }
            }
            Self::EaseInCirc => 1.0 - (1.0 - t * t).sqrt(),
            Self::EaseOutCirc => (1.0 - (t - 1.0).powi(2)).sqrt(),
            Self::EaseInOutCirc => {
                if t < 0.5 {
                    (1.0 - (1.0 - (2.0 * t).powi(2)).sqrt()) / 2.0
                } else {
                    ((1.0 - (-2.0 * t + 2.0).powi(2)).sqrt() + 1.0) / 2.0
                }
            }
            Self::EaseInBack => BACK_OVERSHOOT_OUT * t.powi(3) - BACK_OVERSHOOT * t.powi(2),
            Self::EaseOutBack => {
                1.0 + BACK_OVERSHOOT_OUT * (t - 1.0).powi(3) + BACK_OVERSHOOT * (t - 1.0).powi(2)
            }
            Self::EaseInOutBack => {
                if t < 0.5 {
                    (2.0 * t).powi(2)
                        * ((BACK_OVERSHOOT_IN_OUT + 1.0) * 2.0 * t - BACK_OVERSHOOT_IN_OUT)
                        / 2.0
                } else {
                    ((2.0 * t - 2.0).powi(2)
                        * ((BACK_OVERSHOOT_IN_OUT + 1.0) * (2.0 * t - 2.0) + BACK_OVERSHOOT_IN_OUT)
                        + 2.0)
                        / 2.0
                }
            }
            Self::EaseInElastic => {
                if t == 0.0 {
                    0.0
                } else if t == 1.0 {
                    1.0
                } else {
                    -2.0_f32.powf(10.0 * t - 10.0) * ((10.0 * t - 10.75) * ELASTIC_PERIOD).sin()
                }
            }
            Self::EaseOutElastic => {
                if t == 0.0 {
                    0.0
                } else if t == 1.0 {
                    1.0
                } else {
                    2.0_f32.powf(-10.0 * t) * ((10.0 * t - 0.75) * ELASTIC_PERIOD).sin() + 1.0
                }
            }
            Self::EaseInOutElastic => {
                if t == 0.0 {
                    0.0
                } else if t == 1.0 {
                    1.0
                } else if t < 0.5 {
                    -(2.0_f32.powf(20.0 * t - 10.0)
                        * ((20.0 * t - 11.125) * ELASTIC_PERIOD_IN_OUT).sin())
                        / 2.0
                } else {
                    (2.0_f32.powf(-20.0 * t + 10.0)
                        * ((20.0 * t - 11.125) * ELASTIC_PERIOD_IN_OUT).sin())
                        / 2.0
                        + 1.0
                }
            }
            Self::EaseInBounce => 1.0 - ease_out_bounce(1.0 - t),
            Self::EaseOutBounce => ease_out_bounce(t),
            Self::EaseInOutBounce => {
                if t < 0.5 {
                    (1.0 - ease_out_bounce(1.0 - 2.0 * t)) / 2.0
                } else {
                    (1.0 + ease_out_bounce(2.0 * t - 1.0)) / 2.0
                }
            }
        }
    }
}

/// Ease-in-out for a polynomial of the given `power`, expressed symmetrically
/// around the midpoint so both halves join smoothly.
fn in_out_power(t: f32, power: i32) -> f32 {
    if t < 0.5 {
        (2.0 * t).powi(power) / 2.0
    } else {
        1.0 - (-2.0 * t + 2.0).powi(power) / 2.0
    }
}

/// The classic "bounce out" curve, used to build the in and in-out variants.
fn ease_out_bounce(t: f32) -> f32 {
    if t < 1.0 / BOUNCE_DIVISOR {
        BOUNCE_SCALE * t * t
    } else if t < 2.0 / BOUNCE_DIVISOR {
        let t = t - 1.5 / BOUNCE_DIVISOR;
        BOUNCE_SCALE * t * t + 0.75
    } else if t < 2.5 / BOUNCE_DIVISOR {
        let t = t - 2.25 / BOUNCE_DIVISOR;
        BOUNCE_SCALE * t * t + 0.9375
    } else {
        let t = t - 2.625 / BOUNCE_DIVISOR;
        BOUNCE_SCALE * t * t + 0.984375
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [Easing; 31] = [
        Easing::Linear,
        Easing::EaseInSine,
        Easing::EaseOutSine,
        Easing::EaseInOutSine,
        Easing::EaseInQuad,
        Easing::EaseOutQuad,
        Easing::EaseInOutQuad,
        Easing::EaseInCubic,
        Easing::EaseOutCubic,
        Easing::EaseInOutCubic,
        Easing::EaseInQuart,
        Easing::EaseOutQuart,
        Easing::EaseInOutQuart,
        Easing::EaseInQuint,
        Easing::EaseOutQuint,
        Easing::EaseInOutQuint,
        Easing::EaseInExpo,
        Easing::EaseOutExpo,
        Easing::EaseInOutExpo,
        Easing::EaseInCirc,
        Easing::EaseOutCirc,
        Easing::EaseInOutCirc,
        Easing::EaseInBack,
        Easing::EaseOutBack,
        Easing::EaseInOutBack,
        Easing::EaseInElastic,
        Easing::EaseOutElastic,
        Easing::EaseInOutElastic,
        Easing::EaseInBounce,
        Easing::EaseOutBounce,
        Easing::EaseInOutBounce,
    ];

    #[test]
    fn every_curve_has_the_expected_endpoints() {
        for easing in ALL {
            assert!(
                (easing.apply(0.0) - 0.0).abs() < 1e-5,
                "{easing:?} should start at 0, got {}",
                easing.apply(0.0)
            );
            assert!(
                (easing.apply(1.0) - 1.0).abs() < 1e-5,
                "{easing:?} should end at 1, got {}",
                easing.apply(1.0)
            );
        }
    }

    #[test]
    fn input_outside_the_unit_interval_is_clamped() {
        for easing in ALL {
            assert_eq!(easing.apply(-1.0), easing.apply(0.0));
            assert_eq!(easing.apply(2.0), easing.apply(1.0));
        }
    }

    #[test]
    fn ease_in_out_sine_is_symmetric() {
        for step in 0..=10 {
            let t = step as f32 / 10.0;
            let forward = Easing::EaseInOutSine.apply(t);
            let reverse = Easing::EaseInOutSine.apply(1.0 - t);
            assert!((forward + reverse - 1.0).abs() < 1e-5);
        }
        assert!((Easing::EaseInOutSine.apply(0.5) - 0.5).abs() < 1e-5);
    }
}
