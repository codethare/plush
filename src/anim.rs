//! Entrance and exit motion for a card.
//!
//! A card arrives by sliding in from the nearest screen edge while fading up and growing
//! from just under its resting size, and it leaves by one of two cheaper motions, chosen by
//! what ended it. All of it is expressed as a [`Pose`], which is all the renderer needs:
//! everything is drawn from a rasterised card, so a frame is a transform and an opacity.
//!
//! Which edge that is comes from the configured position, and the motion lives here rather
//! than in the surface because the surface itself never moves. The compositor only honours
//! margins on the edges you are anchored to, and starting off-screen would need a negative
//! margin, which the protocol never promises. So the motion is drawn inside the buffer,
//! which works the same on every compositor.
//!
//! Arriving and leaving are deliberately not the same motion. An arrival asks for attention,
//! so it can afford to be long and to overshoot slightly; a departure gets out of the way,
//! so it is shorter, has no overshoot, and is shorter still when the user asked for it.

use std::time::{Duration, Instant};

/// How long a card takes to arrive.
const ENTER: Duration = Duration::from_millis(520);
/// How long a card takes to slide away once its time is up.
const EXIT_SLIDE: Duration = Duration::from_millis(260);
/// How long a card takes to fade away after a click. The user has already said what they
/// want, so there is nothing left for the motion to explain.
const EXIT_FADE: Duration = Duration::from_millis(180);

/// How far past its resting place an arriving card travels, as a share of the edge it comes
/// along, before settling back onto it. It is a share rather than a distance because the motion
/// is drawn in buffer pixels, and both edges it can travel along are bounded — so this stays
/// inside the shadow margin on the shortest panel (2.8 px) and on the widest (9.9 px), with the
/// margin at 26.
const OVERSHOOT: f32 = 0.025;

/// The recoil of the arrival's ease. Its peak above 1 is `4·p³ / (27·(p+1)²)`, and the
/// assertion below is that this value makes that peak `OVERSHOOT`. The vendored 1.70158 recoils
/// by a tenth of the distance, which on a 396 px buffer would travel 40 px past rest — further
/// than the shadow margin, so the panel would be clipped as it settled.
const ENTER_RECOIL: f32 = 0.825;
const _: () = {
    let peak = 4.0 * ENTER_RECOIL * ENTER_RECOIL * ENTER_RECOIL
        / (27.0 * (ENTER_RECOIL + 1.0) * (ENTER_RECOIL + 1.0));
    assert!((peak - OVERSHOOT).abs() < 0.0005);
};

/// Why a card is leaving, which is what decides how it leaves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exit {
    /// It timed out or was dropped to make room: it slides away along its edge while fading.
    Expired,
    /// It was clicked: it fades where it stands.
    Dismissed,
}

impl Exit {
    fn duration(self) -> Duration {
        match self {
            Self::Expired => EXIT_SLIDE,
            Self::Dismissed => EXIT_FADE,
        }
    }
}

/// What a frame needs to know about the motion: how far the card has moved and how visible it
/// is. A card never changes size, which is what keeps every frame a copy rather than a resample.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pose {
    /// 1 fully opaque, 0 gone.
    pub opacity: f32,
    /// Share of the edge distance between the card and its resting place, signed: positive is
    /// outside the edge, 0 is at rest, negative is past rest — which is the arrival's overshoot.
    /// A click never travels.
    pub travel: f32,
}

impl Pose {
    /// Standing still, fully drawn.
    pub const RESTING: Self = Self { opacity: 1.0, travel: 0.0 };
}

/// Where a card is in its life.
///
/// A card with animations turned off stays `Settled` from start to finish, which is
/// what makes the whole feature one boolean.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Anim {
    /// Standing still: it has arrived, or it was never animated.
    Settled,
    /// Sliding in from its edge, fading up and growing to its resting size.
    Entering(Instant),
    /// Sliding or fading out. The surface is dropped once this finishes.
    Leaving(Instant, Exit),
}

impl Anim {
    pub fn entering() -> Self {
        Self::Entering(Instant::now())
    }

    /// Turns an arrival into a departure, in the shape `exit` asked for. The clock restarts
    /// deliberately, so a card dismissed while it is still coming in leaves from wherever it
    /// had got to.
    pub fn leave(&mut self, exit: Exit) {
        *self = Self::Leaving(Instant::now(), exit);
    }

    pub fn leaving(&self) -> bool {
        matches!(self, Self::Leaving(..))
    }

    /// True while the card still wants frames drawn.
    pub fn running(&self) -> bool {
        !matches!(self, Self::Settled)
    }

    /// How long this phase runs for; `None` once the card is settled.
    fn duration(&self) -> Option<Duration> {
        match self {
            Self::Settled => None,
            Self::Entering(_) => Some(ENTER),
            Self::Leaving(_, exit) => Some(exit.duration()),
        }
    }

    pub fn finished(&self, now: Instant) -> bool {
        match self.duration() {
            None => true,
            Some(duration) => self.elapsed(now) >= duration,
        }
    }

    fn elapsed(&self, now: Instant) -> Duration {
        let start = match self {
            Self::Settled => return Duration::ZERO,
            Self::Entering(start) | Self::Leaving(start, _) => start,
        };
        now.saturating_duration_since(*start)
    }

    pub fn pose(&self, now: Instant) -> Pose {
        match self {
            Self::Settled => Pose::RESTING,
            Self::Entering(_) => {
                let fraction = self.fraction(now);
                Pose {
                    // The fade is monotone even though the travel is not: a fade that overshot
                    // would read as a flash rather than as a settle.
                    opacity: ease_out(fraction),
                    // Travels past the resting place and comes back. Overshoot only on the way
                    // in; a departure has nothing left to emphasise.
                    travel: 1.0 - ease_out_back(fraction),
                }
            }
            Self::Leaving(_, exit) => {
                let progress = ease_out(self.fraction(now));
                Pose {
                    opacity: 1.0 - progress,
                    travel: match exit {
                        Exit::Expired => progress,
                        Exit::Dismissed => 0.0,
                    },
                }
            }
        }
    }

    /// Share of this phase that has run, `0..=1`.
    fn fraction(&self, now: Instant) -> f32 {
        match self.duration() {
            None => 1.0,
            Some(duration) => {
                (self.elapsed(now).as_secs_f32() / duration.as_secs_f32()).clamp(0.0, 1.0)
            }
        }
    }
}

/// Ease out: leaves the edge quickly and settles gently. Run backwards it becomes an ease
/// in, which is what a departure wants. Monotone, so a fade never flickers and a travel
/// never reverses.
fn ease_out(fraction: f32) -> f32 {
    1.0 - (1.0 - fraction).powi(3)
}

/// Ease out with a recoil: passes 1 by `OVERSHOOT` of the distance and comes back to it. Only
/// ever used for the arrival's travel, where passing the resting place reads as a settle.
///
fn ease_out_back(fraction: f32) -> f32 {
    const C1: f32 = ENTER_RECOIL;
    const C3: f32 = C1 + 1.0;
    let t = fraction - 1.0;
    1.0 + C3 * t * t * t + C1 * t * t
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The peak of `ease_out_back`, which is how far past rest the arrival travels.
    fn overshoot_peak() -> f32 {
        let (mut peak, mut at) = (0.0, 0.0);
        for step in 0..=1000 {
            let fraction = step as f32 / 1000.0;
            let value = ease_out_back(fraction);
            if value > peak {
                peak = value;
                at = fraction;
            }
        }
        assert!(at > 0.0 && at < 1.0, "the overshoot is somewhere in the middle");
        peak
    }

    #[test]
    fn a_settled_card_is_at_rest() {
        let anim = Anim::Settled;
        assert_eq!(anim.pose(Instant::now()), Pose::RESTING);
        assert!(anim.finished(Instant::now()));
        assert!(!anim.running());
        assert!(!anim.leaving());
    }

    #[test]
    fn arriving_runs_from_off_screen_to_rest() {
        let start = Instant::now();
        let anim = Anim::Entering(start);

        let first = anim.pose(start);
        assert_eq!(first.travel, 1.0, "arrives from nothing");
        assert_eq!(first.opacity, 0.0, "invisibly");

        let last = anim.pose(start + ENTER);
        assert_eq!(last, Pose::RESTING, "settles exactly at rest");
        assert_eq!(anim.pose(start + ENTER * 10), Pose::RESTING, "and stays there");
        assert!(anim.running());
        assert!(!anim.finished(start));
        assert!(anim.finished(start + ENTER));
    }

    /// The arrival is the only motion that overshoots, and it does it by travelling past its
    /// resting place rather than by changing size: nothing is resampled, so a frame stays a
    /// plain copy.
    #[test]
    fn the_arrival_travels_past_its_resting_place() {
        let start = Instant::now();
        let anim = Anim::Entering(start);
        let mut peak: f32 = 0.0;
        let mut previous_opacity = 0.0;
        for step in 0..=200 {
            let pose = anim.pose(start + ENTER * step / 200);
            peak = peak.min(pose.travel);
            assert!(pose.opacity >= previous_opacity, "the fade never goes backwards");
            previous_opacity = pose.opacity;
        }
        assert!(peak < 0.0, "the card passes its resting place, reached {peak}");
        let expected = overshoot_peak() - 1.0;
        assert!((-peak - expected).abs() < 1e-3, "by the curve's own peak: {peak}");
        assert!((-peak - OVERSHOOT).abs() < 1e-3, "which is the share asked for: {peak}");
    }

    /// The overshoot has to fit inside the shadow margin the surface already carries, on the
    /// shortest edge it can travel along and on the widest, or the panel is clipped while it
    /// settles.
    #[test]
    fn the_overshoot_fits_inside_the_shadow_margin() {
        // The edges a card can travel along are the buffer's own: the widest panel's width,
        // and the shortest panel's height, both counting the shadow margin on either side.
        let margin = crate::render::SHADOW as f32;
        let wide = crate::render::surface(crate::render::PANEL_W, crate::render::MIN_H).0 as f32;
        let short = crate::render::surface(crate::render::PANEL_W, crate::render::MIN_H).1 as f32;
        for edge in [wide, short] {
            let travel = (overshoot_peak() - 1.0) * edge;
            assert!(travel < margin, "the overshoot would need {travel} px on a {edge} px edge");
        }
    }

    #[test]
    fn progress_never_goes_backwards_while_arriving() {
        let start = Instant::now();
        let anim = Anim::Entering(start);
        let mut previous = 0.0;
        for step in 0..=20 {
            let pose = anim.pose(start + ENTER * step / 20);
            assert!(pose.opacity >= previous, "{} went backwards at step {step}", pose.opacity);
            previous = pose.opacity;
        }
        assert_eq!(previous, 1.0);
    }

    #[test]
    fn leaving_is_not_arriving_backwards() {
        let start = Instant::now();
        let mut anim = Anim::Entering(start);
        anim.leave(Exit::Expired);
        // `leave` restarts the clock, so the departure ends a full duration after it.
        let departed = Instant::now();

        assert!(anim.leaving());
        assert_eq!(anim.pose(start), Pose::RESTING, "leaves from rest");
        assert!(!anim.finished(departed));
        assert!(anim.finished(departed + EXIT_SLIDE));
        // Gone, and a full edge away: a timeout leaves rather than dissolving in place.
        let gone = Pose { opacity: 0.0, travel: 1.0 };
        assert_eq!(anim.pose(departed + EXIT_SLIDE), gone);
        assert_eq!(anim.pose(departed + EXIT_SLIDE * 10), gone);
    }

    /// A panel that times out slides; a panel that was clicked only fades. Neither changes
    /// size, and neither overshoots.
    #[test]
    fn the_departure_follows_what_ended_the_panel() {
        for (exit, duration, travels) in
            [(Exit::Expired, EXIT_SLIDE, true), (Exit::Dismissed, EXIT_FADE, false)]
        {
            let mut anim = Anim::Entering(Instant::now());
            anim.leave(exit);
            let start = Instant::now();

            let mut previous = 1.0;
            for step in 0..=20 {
                let pose = anim.pose(start + duration * step / 20);
                assert!(pose.opacity <= previous, "{exit:?} brightens at step {step}");
                assert_eq!(pose.travel > 0.0, travels, "{exit:?} travels the wrong way");
                previous = pose.opacity;
            }
            assert_eq!(anim.pose(start + duration).opacity, 0.0, "{exit:?} ends invisible");
            assert!(anim.finished(start + duration), "{exit:?} runs for its own duration");
            assert!(!anim.finished(start + duration / 2), "{exit:?} is not over early");

            // And it is shorter than the arrival: a departure gets out of the way.
            assert!(
                duration < ENTER,
                "{exit:?} takes {duration:?} against the arrival's {ENTER:?}"
            );
        }
    }

    /// A click must be gone sooner than a timeout, which is the whole point of having two.
    #[test]
    fn a_click_leaves_sooner_than_a_timeout() {
        let mut clicked = Anim::Entering(Instant::now());
        clicked.leave(Exit::Dismissed);
        let mut timed_out = Anim::Entering(Instant::now());
        timed_out.leave(Exit::Expired);

        let departed = Instant::now();
        assert!(clicked.finished(departed + EXIT_FADE));
        assert!(!timed_out.finished(departed + EXIT_FADE));
        assert!(timed_out.finished(departed + EXIT_SLIDE));
    }

    /// The easing is what makes it feel like macOS rather than a linear wipe.
    #[test]
    fn the_motion_eases_out() {
        let start = Instant::now();
        let anim = Anim::Entering(start);
        let halfway = anim.pose(start + ENTER / 2).opacity;
        assert!(halfway > 0.8, "covers most of the distance early, got {halfway}");
        assert!(halfway < 1.0, "but not all of it, got {halfway}");
    }
}
