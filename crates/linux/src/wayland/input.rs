//! Which seat input comes from, and what a pointer frame's scrolling is.

use nsxiv_ng_session::Scroll;

/// One step of a wheel in `value120` units.
const STEP: i32 = 120;

/// What the axis events of one pointer frame added up to.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(super) struct Axes {
    /// The distances in logical units, positive right and down.
    pub dx: f64,
    pub dy: f64,
    /// The vertical wheel's steps, positive down, from a seat before version 8.
    pub discrete: i32,
    /// The vertical wheel's steps in 120ths, from a seat of version 8 or later.
    pub value120: i32,
    /// The distances came from a touchpad or another continuous source.
    pub smooth: bool,
}

/// The scroll gesture of one frame: a touchpad's distances; else a wheel's steps; else, from
/// a wheel that gives no steps, its bare distance. `rest` keeps the 120ths of a step that a
/// high-resolution wheel has turned without finishing it. The grid scrolls by the vertical
/// distance alone, so a touchpad's sideways move there is none.
pub(super) fn scroll(axes: Axes, rest: &mut i32, grid: bool) -> Option<Scroll> {
    let Axes {
        dx,
        dy,
        discrete,
        value120,
        smooth,
    } = axes;
    if smooth {
        let moved = dy != 0.0 || (dx != 0.0 && !grid);
        return moved.then_some(Scroll::Distance { dx, dy });
    }
    if value120 != 0 {
        *rest += value120;
        let steps = *rest / STEP;
        *rest -= steps * STEP;
        return (steps != 0).then_some(Scroll::Steps(steps));
    }
    if discrete != 0 {
        Some(Scroll::Steps(discrete))
    } else if dy != 0.0 {
        Some(Scroll::WheelDistance(dy))
    } else {
        None
    }
}

/// The seat input comes from: the one it came from while that seat is there, with devices or
/// without, else the first with a keyboard or a pointer, as one user drives the viewer.
/// `seats` pairs each seat with whether it has either.
pub(super) fn input_seat<S: Clone + PartialEq>(
    current: Option<&S>,
    seats: &[(S, bool)],
) -> Option<S> {
    match current {
        Some(seat) if seats.iter().any(|(s, _)| s == seat) => Some(seat.clone()),
        _ => seats
            .iter()
            .find(|(_, input)| *input)
            .map(|(seat, _)| seat.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wheel(discrete: i32, value120: i32, dy: f64) -> Axes {
        Axes {
            dy,
            discrete,
            value120,
            ..Axes::default()
        }
    }

    fn touchpad(dx: f64, dy: f64) -> Axes {
        Axes {
            dx,
            dy,
            smooth: true,
            ..Axes::default()
        }
    }

    #[test]
    fn a_touchpad_gives_distances_a_wheel_steps_or_its_bare_distance() {
        let mut rest = 0;
        // down and right are positive on the wire and in the session alike
        assert_eq!(
            scroll(touchpad(3.0, -7.5), &mut rest, false),
            Some(Scroll::Distance { dx: 3.0, dy: -7.5 })
        );
        // a seat of version 5 to 7 gives steps with the distance
        assert_eq!(
            scroll(wheel(-2, 0, -30.0), &mut rest, false),
            Some(Scroll::Steps(-2))
        );
        // a seat before version 5 gives the distance alone
        assert_eq!(
            scroll(wheel(0, 0, 15.0), &mut rest, false),
            Some(Scroll::WheelDistance(15.0))
        );
        assert_eq!(scroll(Axes::default(), &mut rest, false), None);
        // a tilted wheel's sideways turn has no vertical step
        let tilt = Axes {
            dx: 15.0,
            ..Axes::default()
        };
        assert_eq!(scroll(tilt, &mut rest, false), None);
    }

    #[test]
    fn a_high_resolution_wheel_steps_once_it_turned_a_whole_step() {
        let mut rest = 0;
        assert_eq!(
            scroll(wheel(0, 120, 15.0), &mut rest, false),
            Some(Scroll::Steps(1))
        );
        assert_eq!(
            scroll(wheel(0, 60, 7.5), &mut rest, false),
            None,
            "half a step"
        );
        assert_eq!(rest, 60);
        assert_eq!(
            scroll(wheel(0, 300, 37.5), &mut rest, false),
            Some(Scroll::Steps(3))
        );
        assert_eq!(rest, 0);
        assert_eq!(scroll(wheel(0, -90, -11.25), &mut rest, false), None);
        assert_eq!(
            scroll(wheel(0, -30, -3.75), &mut rest, false),
            Some(Scroll::Steps(-1)),
            "up"
        );
    }

    #[test]
    fn in_the_grid_a_touchpad_scrolls_only_by_its_vertical_distance() {
        let mut rest = 0;
        assert_eq!(scroll(touchpad(5.0, 0.0), &mut rest, true), None);
        assert_eq!(
            scroll(touchpad(5.0, 2.0), &mut rest, true),
            Some(Scroll::Distance { dx: 5.0, dy: 2.0 })
        );
        assert_eq!(
            scroll(touchpad(5.0, 0.0), &mut rest, false),
            Some(Scroll::Distance { dx: 5.0, dy: 0.0 }),
            "the image pans sideways"
        );
        assert_eq!(scroll(touchpad(0.0, 0.0), &mut rest, false), None);
    }

    #[test]
    fn input_comes_from_one_seat_while_it_is_there() {
        assert_eq!(input_seat(None, &[("a", false), ("b", true)]), Some("b"));
        assert_eq!(input_seat(None, &[("a", false)]), None, "no devices");
        assert_eq!(
            input_seat(Some(&"b"), &[("a", true), ("b", true)]),
            Some("b"),
            "another seat with devices takes nothing over"
        );
        assert_eq!(
            input_seat(Some(&"b"), &[("a", true), ("b", false)]),
            Some("b"),
            "a seat that lost its devices stays, as they may come back"
        );
        assert_eq!(
            input_seat(Some(&"b"), &[("a", false), ("c", true)]),
            Some("c"),
            "a seat gone gives way to the next with devices"
        );
    }
}
