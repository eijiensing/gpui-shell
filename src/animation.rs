use std::time::Instant;

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

/// Global animation driver shared by every island window.
///
/// The animation is advanced from the render path, once per presented frame, so
/// it is paced by the compositor's frame callbacks instead of a fixed timer.
/// Progress is measured against wall-clock time, which keeps the motion correct
/// when frames are dropped and keeps windows on different refresh rates in
/// sync.
pub struct Animation {
    animator: Animator,
    /// When the animator was last advanced; `None` until the first tick after a
    /// request, so idle time never produces a jump.
    last_tick: Option<Instant>,
    running: bool,
}

impl gpui::Global for Animation {}

impl Animation {
    pub fn new(initial: Appearance) -> Self {
        Self {
            animator: Animator::new(initial),
            last_tick: None,
            running: false,
        }
    }

    /// Marks the animation as in flight.
    ///
    /// The caller must also request a frame on at least one island window;
    /// `tick` then advances the animation from that window's render path.
    pub fn request(&mut self) {
        if !self.running {
            self.running = true;
            self.last_tick = None;
        }
    }

    /// Whether an animation is currently in flight.
    pub fn running(&self) -> bool {
        self.running
    }

    /// Advances towards `target` by the time elapsed since the previous tick.
    ///
    /// Returns the appearance to draw and whether the animation settled on this
    /// tick. The first call after [`Self::request`] keeps the current appearance
    /// (it uses a zero delta), so the starting state is presented before motion
    /// begins.
    pub fn tick(&mut self, target: Appearance, now: Instant) -> (Appearance, bool) {
        if !self.running {
            return (self.animator.value(), false);
        }

        let dt = self
            .last_tick
            .map(|last| now.saturating_duration_since(last).as_secs_f32())
            .unwrap_or(0.0);
        self.last_tick = Some(now);

        if self.animator.advance(target, dt) {
            self.animator.snap_to(target);
            self.running = false;
            (target, true)
        } else {
            (self.animator.value(), false)
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
    use std::time::Duration;

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

    #[test]
    fn first_tick_after_request_ignores_idle_time() {
        let mut animation = Animation::new(with_size(48.0, 12.0));
        animation.request();

        // However long the app sat idle before the request, the first frame
        // stays at the starting size instead of jumping ahead.
        let now = Instant::now() + Duration::from_secs(10);
        let (value, settled) = animation.tick(with_size(200.0, 100.0), now);

        assert!(!settled);
        assert_eq!(value.size, with_size(48.0, 12.0).size);
        assert!(animation.running());
    }

    #[test]
    fn ticks_are_paced_by_elapsed_time() {
        let mut animation = Animation::new(with_size(48.0, 12.0));
        let target = with_size(200.0, 100.0);
        animation.request();

        let start = Instant::now();
        animation.tick(target, start); // retargets with a zero delta
        let (value, settled) = animation.tick(
            target,
            start + Duration::from_secs_f32(RESIZE_DURATION / 2.0),
        );

        assert!(!settled);
        assert!((f32::from(value.size.width) - 124.0).abs() < 0.01);

        let (value, settled) =
            animation.tick(target, start + Duration::from_secs_f32(RESIZE_DURATION));

        assert!(settled);
        assert_eq!(value.size, target.size);
        assert!(!animation.running());
    }
}
