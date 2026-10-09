use gpui::{SpringConfig, SpringState};

use crate::appearance::{Appearance, CHANNELS};

/// An underdamped spring: `ζ = damping / (2·√(stiffness·mass)) ≈ 0.45`, which
/// gives a visible bounce before settling. Raise `damping` toward `2·√k` for a
/// calmer, critically damped feel.
const SPRING: SpringConfig = SpringConfig::new(320.0, 16.0, 1.0);

/// Per-channel settling tolerances: pixels for the size, unit fractions for the
/// color channels (which live in `0..1`).
const EPSILONS: [f32; CHANNELS] = [
    0.05, 0.05, 0.002, 0.002, 0.002, 0.002, 0.002, 0.002, 0.002, 0.002,
];

/// Drives an [`Appearance`] with one analytic spring per scalar channel.
///
/// `SpringConfig::step` is frame-rate independent and preserves velocity, so a
/// retarget mid-flight bounces smoothly instead of restarting.
pub struct Animator {
    states: [SpringState; CHANNELS],
}

impl Animator {
    pub fn new(initial: Appearance) -> Self {
        let mut states = [SpringState::default(); CHANNELS];
        for (state, position) in states.iter_mut().zip(initial.to_channels()) {
            state.position = position;
        }
        Self { states }
    }

    /// The appearance at the springs' current position.
    pub fn value(&self) -> Appearance {
        let mut channels = [0.0; CHANNELS];
        for (channel, state) in channels.iter_mut().zip(&self.states) {
            *channel = state.position;
        }
        Appearance::from_channels(channels)
    }

    /// Advances every channel towards `target` by `dt` seconds and returns
    /// whether all of them have settled.
    pub fn advance(&mut self, target: Appearance, dt: f32) -> bool {
        let target = target.to_channels();
        let mut settled = true;
        for index in 0..CHANNELS {
            self.states[index] = SPRING.step(self.states[index], target[index], dt);
            settled &= SPRING.is_settled(self.states[index], target[index], EPSILONS[index]);
        }
        settled
    }

    /// Jumps immediately to `target`, clearing any velocity.
    pub fn snap_to(&mut self, target: Appearance) {
        for (state, position) in self.states.iter_mut().zip(target.to_channels()) {
            *state = SpringState {
                position,
                velocity: 0.0,
            };
        }
    }
}
