//! The settle rule: when the damage that follows a key makes a frame, and how long it took.

/// A settled frame: the time to the last damage before the window stayed quiet, and the
/// longest quiet gap between two damages of the frame, both in milliseconds.
#[derive(Debug, PartialEq)]
pub struct Frame {
    pub ms: f64,
    pub gap_ms: f64,
}

/// What a key, or a start, drew.
#[derive(Debug, PartialEq)]
pub enum Outcome {
    Frame(Frame),
    /// No damage came within the limit.
    Nothing,
    /// Damage kept coming until the cap.
    Unsettled,
}

/// Decides a frame from the times of its damage, in milliseconds since the key, in the order
/// they came, at `now`. The frame ends at the first damage after which the window stayed quiet
/// for `settle`; that wait is not counted. A first damage past `limit` makes the key draw
/// nothing. Returns the outcome and the count of damage that came after it, which is late:
/// the settle rule cut a frame, or the viewer draws on its own. None while the frame is open.
pub fn decide(
    damage: &[f64],
    now: f64,
    settle: f64,
    limit: f64,
    cap: f64,
) -> Option<(Outcome, usize)> {
    match damage.first() {
        None if now >= limit => return Some((Outcome::Nothing, 0)),
        Some(&first) if first > limit => return Some((Outcome::Nothing, damage.len())),
        _ => {}
    }
    let quiet_after = damage.windows(2).position(|w| w[1] - w[0] >= settle);
    let quiet_at_end = || {
        damage
            .last()
            .filter(|&&last| now - last >= settle)
            .map(|_| damage.len() - 1)
    };
    if let Some(end) = quiet_after.or_else(quiet_at_end) {
        let frame = Frame {
            ms: damage[end],
            gap_ms: damage[..=end]
                .windows(2)
                .map(|w| w[1] - w[0])
                .fold(0.0, f64::max),
        };
        return Some((Outcome::Frame(frame), damage.len() - end - 1));
    }
    (now >= cap).then_some((Outcome::Unsettled, 0))
}

/// The next time, in milliseconds since the key, at which `decide` may answer.
pub fn next_check(damage: &[f64], settle: f64, limit: f64, cap: f64) -> f64 {
    damage.last().map_or(limit, |last| last + settle).min(cap)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SETTLE: f64 = 500.0;
    const LIMIT: f64 = 5000.0;
    const CAP: f64 = 120_000.0;

    fn frame(ms: f64, gap_ms: f64, late: usize) -> Option<(Outcome, usize)> {
        Some((Outcome::Frame(Frame { ms, gap_ms }), late))
    }

    #[test]
    fn the_frame_ends_at_the_last_damage_before_the_quiet() {
        // the bar at once, the image once decoded, then nothing
        let damage = [3.0, 4.0, 180.0, 181.5];
        assert_eq!(decide(&damage, 600.0, SETTLE, LIMIT, CAP), None);
        assert_eq!(
            decide(&damage, 681.5, SETTLE, LIMIT, CAP),
            frame(181.5, 176.0, 0)
        );
        assert_eq!(next_check(&damage, SETTLE, LIMIT, CAP), 681.5);
    }

    #[test]
    fn a_second_step_later_than_the_settle_time_is_late() {
        assert_eq!(
            decide(&[3.0], 503.0, SETTLE, LIMIT, CAP),
            frame(3.0, 0.0, 0)
        );
        // both taken in at once: the frame still ends at the first quiet, and the rest is late
        let damage = [3.0, 800.0, 801.0];
        assert_eq!(
            decide(&damage, 1300.0, SETTLE, LIMIT, CAP),
            frame(3.0, 0.0, 2)
        );
        assert_eq!(
            decide(&damage, 900.0, SETTLE, LIMIT, CAP),
            frame(3.0, 0.0, 2)
        );
    }

    #[test]
    fn no_damage_within_the_limit_is_nothing_never_zero() {
        assert_eq!(decide(&[], 4999.0, SETTLE, LIMIT, CAP), None);
        assert_eq!(
            decide(&[], LIMIT, SETTLE, LIMIT, CAP),
            Some((Outcome::Nothing, 0))
        );
        assert_eq!(next_check(&[], SETTLE, LIMIT, CAP), LIMIT);
        // damage first taken in past the limit drew too late, and is all late
        let damage = [5001.0, 5002.0];
        assert_eq!(
            decide(&damage, 5003.0, SETTLE, LIMIT, CAP),
            Some((Outcome::Nothing, 2))
        );
    }

    #[test]
    fn damage_after_the_limit_still_waits_for_the_quiet() {
        // the limit only bounds the wait for the first damage
        let damage = [4900.0, 5200.0];
        assert_eq!(decide(&damage, 5600.0, SETTLE, LIMIT, CAP), None);
        assert_eq!(
            decide(&damage, 5700.0, SETTLE, LIMIT, CAP),
            frame(5200.0, 300.0, 0)
        );
    }

    #[test]
    fn a_window_that_never_stays_quiet_is_unsettled_at_the_cap() {
        let damage: Vec<f64> = (0..1200).map(|i| f64::from(i) * 100.0).collect();
        assert_eq!(decide(&damage, 119_950.0, SETTLE, LIMIT, CAP), None);
        assert_eq!(
            decide(&damage, CAP, SETTLE, LIMIT, CAP),
            Some((Outcome::Unsettled, 0))
        );
        assert_eq!(next_check(&damage, SETTLE, LIMIT, CAP), CAP);
    }
}
