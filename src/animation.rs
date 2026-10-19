use gpui::{SpringConfig, SpringState};

use crate::appearance::{Appearance, CHANNELS};
use crate::easing::Easing;

/// An underdamped spring for the color channels: `ζ = damping / (2·√(stiffness·mass)) ≈ 0.45`,
/// which gives a visible bounce before settling. Raise `damping` toward `2·√k`
/// for a calmer, critically damped feel.
const SPRING: SpringConfig = SpringConfig::new(640.0, 32.0, 1.0);

/// How long the island takes to move between sizes.
const RESIZE_DURATION: f32 = 0.2;

/// The curve the size follows while resizing.
const RESIZE_EASING: Easing = Easing::EaseInOutSine;

/// The first two channels are the island size; the remainder are colors.
const SIZE_CHANNELS: usize = 2;
const COLOR_CHANNELS: usize = CHANNELS - SIZE_CHANNELS;

/// Settling tolerance for the color channels, which live in `0..1`.
const COLOR_EPSILON: f32 = 0.002;

/// Drives an [`Appearance`] with an eased tween for the size and one analytic
/// spring per color channel.
///
/// The resize is a fixed-duration [`Easing::EaseInOutSine`] tween. Retargeting
/// mid-flight restarts the tween from the current size, so a new resize never
/// jumps. `SpringConfig::step` is frame-rate independent and preserves
/// velocity, so the color springs still bounce smoothly when retargeted.
pub struct Animator {
    size: SizeTween,
    colors: [SpringState; COLOR_CHANNELS],
}

impl Animator {
    pub fn new(initial: Appearance) -> Self {
        let channels = initial.to_channels();
        let mut colors = [SpringState::default(); COLOR_CHANNELS];
        for (state, position) in colors.iter_mut().zip(&channels[SIZE_CHANNELS..]) {
            state.position = *position;
        }
        Self {
            size: SizeTween::settled([channels[0], channels[1]]),
            colors,
        }
    }

    /// The appearance at the animator's current position.
    pub fn value(&self) -> Appearance {
        let mut channels = [0.0; CHANNELS];
        channels[..SIZE_CHANNELS].copy_from_slice(&self.size.current);
        for (channel, state) in channels[SIZE_CHANNELS..].iter_mut().zip(&self.colors) {
            *channel = state.position;
        }
        Appearance::from_channels(channels)
    }

    /// Advances the size tween and every color spring towards `target` by `dt`
    /// seconds and returns whether all of them have settled.
    pub fn advance(&mut self, target: Appearance, dt: f32) -> bool {
        let target = target.to_channels();

        let mut settled = self.size.advance([target[0], target[1]], dt);

        for (index, state) in self.colors.iter_mut().enumerate() {
            let channel = index + SIZE_CHANNELS;
            *state = SPRING.step(*state, target[channel], dt);
            settled &= SPRING.is_settled(*state, target[channel], COLOR_EPSILON);
        }

        settled
    }

    /// Jumps immediately to `target`, clearing any velocity.
    pub fn snap_to(&mut self, target: Appearance) {
        let channels = target.to_channels();
        self.size = SizeTween::settled([channels[0], channels[1]]);
        for (state, position) in self.colors.iter_mut().zip(&channels[SIZE_CHANNELS..]) {
            *state = SpringState {
                position: *position,
                velocity: 0.0,
            };
        }
    }
}

/// A fixed-duration, eased interpolation between two sizes.
struct SizeTween {
    /// The size currently drawn.
    current: [f32; 2],
    /// The size the tween started from.
    start: [f32; 2],
    /// The size the tween is heading towards.
    target: [f32; 2],
    /// Seconds elapsed since the current tween started.
    elapsed: f32,
}

impl SizeTween {
    /// Creates a tween already resting at `size`.
    fn settled(size: [f32; 2]) -> Self {
        Self {
            current: size,
            start: size,
            target: size,
            elapsed: RESIZE_DURATION,
        }
    }

    /// Advances towards `target`, returning whether the tween has finished.
    fn advance(&mut self, target: [f32; 2], dt: f32) -> bool {
        if target != self.target {
            // Retarget from wherever the tween is now so a resize made
            // mid-flight continues smoothly instead of jumping.
            self.start = self.current;
            self.target = target;
            self.elapsed = 0.0;
        }

        if self.elapsed >= RESIZE_DURATION {
            self.current = self.target;
            return true;
        }

        self.elapsed = (self.elapsed + dt).min(RESIZE_DURATION);
        let progress = RESIZE_EASING.apply(self.elapsed / RESIZE_DURATION);
        self.current = [
            lerp(self.start[0], self.target[0], progress),
            lerp(self.start[1], self.target[1], progress),
        ];

        self.elapsed >= RESIZE_DURATION
    }
}

fn lerp(from: f32, to: f32, progress: f32) -> f32 {
    from + (to - from) * progress
}

#[cfg(test)]
mod tests {
    use gpui::{Size, px};

    use super::*;

    fn with_size(width: f32, height: f32) -> Appearance {
        Appearance {
            size: Size::new(px(width), px(height)),
            ..Appearance::default()
        }
    }

    #[test]
    fn resize_follows_ease_in_out_sine() {
        let mut animator = Animator::new(with_size(48.0, 12.0));
        let target = with_size(200.0, 100.0);

        // The first tick retargets without moving.
        assert!(!animator.advance(target, 0.0));
        assert_eq!(f32::from(animator.value().size.width), 48.0);

        // Halfway through an ease-in-out curve the size is exactly halfway.
        assert!(!animator.advance(target, RESIZE_DURATION / 2.0));
        assert!((f32::from(animator.value().size.width) - 124.0).abs() < 0.01);
        assert!((f32::from(animator.value().size.height) - 56.0).abs() < 0.01);
    }

    #[test]
    fn resize_settles_after_its_duration() {
        let mut animator = Animator::new(with_size(48.0, 12.0));
        let target = with_size(200.0, 100.0);

        assert!(!animator.advance(target, RESIZE_DURATION / 2.0));
        assert!(animator.advance(target, RESIZE_DURATION / 2.0));
        assert_eq!(animator.value().size, target.size);
    }

    #[test]
    fn retargeting_mid_flight_starts_from_the_current_size() {
        let mut animator = Animator::new(with_size(48.0, 12.0));
        let first = with_size(200.0, 100.0);
        animator.advance(first, RESIZE_DURATION / 2.0);
        let midway = animator.value().size;

        let second = with_size(100.0, 50.0);
        assert!(!animator.advance(second, 0.0));
        assert_eq!(animator.value().size, midway);
    }
}
