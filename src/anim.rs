//! Entrance and exit motion for a card.
//!
//! A card arrives by sliding in from the nearest screen edge while fading up, and leaves
//! by running the same motion backwards. Both are expressed as one `progress` in
//! `0..=1`, which is all the renderer needs: 0 is a full side off screen and invisible,
//! 1 is settled.
//!
//! Which edge that is comes from the configured position, and the motion lives here
//! rather than in the surface because the surface itself never moves. The compositor only
//! honours margins on the edges you are anchored to, and starting off-screen would need a
//! negative margin, which the protocol never promises. So the slide is drawn inside the
//! buffer, which works the same on every compositor.

use std::time::{Duration, Instant};

/// How long a card takes to arrive, or to leave.
const DURATION: Duration = Duration::from_millis(220);

/// Where a card is in its life.
///
/// A card with animations turned off stays `Settled` from start to finish, which is
/// what makes the whole feature one boolean.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Anim {
    /// Standing still: it has arrived, or it was never animated.
    Settled,
    /// Sliding in from the right.
    Entering(Instant),
    /// Sliding back out to the right. The surface is dropped once this finishes.
    Leaving(Instant),
}

impl Anim {
    pub fn entering() -> Self {
        Self::Entering(Instant::now())
    }

    /// Turns an arrival into a departure. The clock restarts deliberately, so a card
    /// dismissed while it is still coming in leaves from wherever it had got to.
    pub fn leave(&mut self) {
        *self = Self::Leaving(Instant::now());
    }

    pub fn leaving(&self) -> bool {
        matches!(self, Self::Leaving(_))
    }

    /// True while the card still wants frames drawn.
    pub fn running(&self) -> bool {
        !matches!(self, Self::Settled)
    }

    pub fn finished(&self, now: Instant) -> bool {
        match self {
            Self::Settled => true,
            Self::Entering(start) | Self::Leaving(start) => {
                now.saturating_duration_since(*start) >= DURATION
            }
        }
    }

    /// 0 is fully off to the right and invisible, 1 is settled.
    pub fn progress(&self, now: Instant) -> f32 {
        let (arriving, start) = match self {
            Self::Settled => return 1.0,
            Self::Entering(start) => (true, start),
            Self::Leaving(start) => (false, start),
        };
        let fraction = (now.saturating_duration_since(*start).as_secs_f32()
            / DURATION.as_secs_f32())
        .clamp(0.0, 1.0);
        // Ease out, so the card leaves the edge quickly and settles gently. Run
        // backwards it becomes an ease in, which is what a departure wants.
        let eased = 1.0 - (1.0 - fraction).powi(3);
        if arriving {
            eased
        } else {
            1.0 - eased
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_settled_card_is_at_rest() {
        let anim = Anim::Settled;
        assert_eq!(anim.progress(Instant::now()), 1.0);
        assert!(anim.finished(Instant::now()));
        assert!(!anim.running());
        assert!(!anim.leaving());
    }

    #[test]
    fn arriving_runs_from_off_screen_to_rest() {
        let start = Instant::now();
        let anim = Anim::Entering(start);

        assert_eq!(anim.progress(start), 0.0, "arrives from nothing");
        assert_eq!(anim.progress(start + DURATION), 1.0, "settles exactly at rest");
        assert_eq!(anim.progress(start + DURATION * 10), 1.0, "and stays there");
        assert!(anim.running());
        assert!(!anim.finished(start));
        assert!(anim.finished(start + DURATION));
    }

    #[test]
    fn progress_never_goes_backwards_while_arriving() {
        let start = Instant::now();
        let anim = Anim::Entering(start);
        let mut previous = 0.0;
        for step in 0..=20 {
            let progress = anim.progress(start + DURATION * step / 20);
            assert!(progress >= previous, "{progress} went backwards at step {step}");
            previous = progress;
        }
        assert_eq!(previous, 1.0);
    }

    #[test]
    fn leaving_is_arriving_backwards() {
        let start = Instant::now();
        let mut anim = Anim::Entering(start);
        anim.leave();
        // `leave` restarts the clock, so the departure ends a full duration after it.
        let departed = Instant::now();

        assert!(anim.leaving());
        assert_eq!(anim.progress(start), 1.0, "leaves from rest");
        assert!(!anim.finished(departed));
        assert!(anim.finished(departed + DURATION));
        assert_eq!(anim.progress(departed + DURATION), 0.0, "and ends invisible");
        assert_eq!(anim.progress(departed + DURATION * 10), 0.0, "and stays there");
    }

    /// The easing is what makes it feel like macOS rather than a linear wipe.
    #[test]
    fn the_motion_eases_out() {
        let start = Instant::now();
        let anim = Anim::Entering(start);
        let halfway = anim.progress(start + DURATION / 2);
        assert!(halfway > 0.8, "covers most of the distance early, got {halfway}");
        assert!(halfway < 1.0, "but not all of it, got {halfway}");
    }
}
