//! What the window says and where it puts things, apart from the drawing of them.
//!
//! Everything here is a plain function of what is known, so that it is tested without a
//! window. The drawing code in [`crate::ui`] decides nothing but colours and layout.

/// The quietest and the loudest level the meter shows, on the scale of
/// [`palaver_media::level`]: above the hiss of a quiet room, and close to full scale.
pub const METER_FLOOR: u8 = 72;
pub const METER_TOP: u8 = 120;
/// How far the meter falls each time it is drawn, as a share of its length. It rises at
/// once and falls slowly, so that a peak can be read.
pub const METER_FALL: f32 = 0.06;

/// A level as a share of the meter's length, from 0 to 1.
#[must_use]
pub fn meter_share(level: u8) -> f32 {
    let span = f32::from(METER_TOP.saturating_sub(METER_FLOOR));
    (f32::from(level.saturating_sub(METER_FLOOR)) / span).clamp(0.0, 1.0)
}

/// Where the meter stands, given where it stood and the level now.
#[must_use]
pub fn meter_after(stood: f32, level: u8) -> f32 {
    meter_share(level).max(stood - METER_FALL).max(0.0)
}

/// The line under the title: how many are here and for how long.
#[must_use]
pub fn header(here: usize, seconds: u64) -> String {
    format!(
        "{:02} here · {:02}:{:02}:{:02}",
        here.max(1),
        seconds / 3600,
        seconds / 60 % 60,
        seconds % 60
    )
}

/// The line about the connections: how many, of what kind, and the network if known.
#[must_use]
pub fn links_line(links: usize, network: Option<&str>) -> String {
    let mut line = match links {
        0 => String::from("no links yet · no server"),
        1 => String::from("1 link · direct · no server"),
        links => format!("{links} links · direct · no server"),
    };
    if let Some(network) = network.filter(|network| *network != "unknown") {
        line.push_str(" · ");
        line.push_str(network);
    }
    line
}

/// The first two letters of a name, as they stand on a person's disc.
#[must_use]
pub fn initials(name: &str) -> String {
    let mut letters = name.trim().chars();
    let first: String = letters
        .next()
        .into_iter()
        .flat_map(char::to_uppercase)
        .collect();
    let second: String = letters.next().into_iter().collect();
    format!("{first}{second}")
}

/// Where the person in `place` of `people` sits on a circle of radius one about the
/// origin, as across and down. The first sits at the top; the rest go round clockwise.
#[must_use]
#[expect(
    clippy::cast_precision_loss,
    reason = "a count of people, as an angle on the screen"
)]
pub fn seat(place: usize, people: usize) -> (f32, f32) {
    let angle =
        std::f32::consts::TAU * place as f32 / people.max(1) as f32 - std::f32::consts::FRAC_PI_2;
    (angle.cos(), angle.sin())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_meter_is_empty_in_a_quiet_room_and_full_near_full_scale() {
        assert!(meter_share(0).abs() < f32::EPSILON);
        assert!(meter_share(METER_FLOOR).abs() < f32::EPSILON);
        assert!((meter_share(96) - 0.5).abs() < 1e-6);
        assert!((meter_share(METER_TOP) - 1.0).abs() < f32::EPSILON);
        assert!((meter_share(127) - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn the_meter_rises_at_once_and_falls_slowly_to_nothing() {
        assert!((meter_after(0.1, METER_TOP) - 1.0).abs() < f32::EPSILON);
        let mut stood = 1.0;
        let mut draws = 0;
        for _ in 0..100 {
            if stood <= 0.0 {
                break;
            }
            let next = meter_after(stood, 0);
            assert!(next < stood && stood - next <= METER_FALL + 1e-6);
            stood = next;
            draws += 1;
        }
        // About a second and a half at ten draws a second.
        assert!((16..=18).contains(&draws), "{draws}");
        assert!(meter_after(0.0, 0).abs() < f32::EPSILON);
    }

    #[test]
    fn the_header_counts_people_and_time() {
        assert_eq!(header(8, 724), "08 here · 00:12:04");
        assert_eq!(header(1, 3600 + 61), "01 here · 01:01:01");
        // Before the roster is known there is still one here: this node.
        assert_eq!(header(0, 0), "01 here · 00:00:00");
    }

    #[test]
    fn the_links_line_says_how_many_and_leaves_out_what_is_not_known() {
        assert_eq!(links_line(0, None), "no links yet · no server");
        assert_eq!(
            links_line(1, Some("unknown")),
            "1 link · direct · no server"
        );
        assert_eq!(
            links_line(3, Some("behind a router")),
            "3 links · direct · no server · behind a router"
        );
    }

    #[test]
    fn initials_are_the_first_two_letters_with_the_first_made_capital() {
        assert_eq!(initials("chidi"), "Ch");
        assert_eq!(initials("  Bo "), "Bo");
        assert_eq!(initials("é"), "É");
        assert_eq!(initials(""), "");
    }

    #[test]
    fn the_first_sits_at_the_top_and_the_rest_go_round_clockwise() {
        let near =
            |a: (f32, f32), b: (f32, f32)| (a.0 - b.0).abs() < 1e-5 && (a.1 - b.1).abs() < 1e-5;
        // On a screen, down is the positive direction.
        assert!(near(seat(0, 4), (0.0, -1.0)));
        assert!(near(seat(1, 4), (1.0, 0.0)));
        assert!(near(seat(2, 4), (0.0, 1.0)));
        assert!(near(seat(3, 4), (-1.0, 0.0)));
        assert!(near(seat(0, 1), (0.0, -1.0)));
        // Nobody: no division by nothing.
        assert!(near(seat(0, 0), (0.0, -1.0)));
    }
}
