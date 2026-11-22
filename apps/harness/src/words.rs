//! What the window says and where it puts things, apart from the drawing of them.
//!
//! Everything here is a plain function of what is known, so that it is tested without a
//! window. The drawing code in [`crate::ui`] decides nothing but colours and layout.

use palaver_media::jitter::Stats;
use std::time::Duration;

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

/// A length of time as whole milliseconds.
#[must_use]
pub fn millis(time: Duration) -> String {
    format!("{} ms", time.as_millis())
}

/// How the sound thread is keeping up.
///
/// Its longest wait between turns and the most frames the microphone had waiting at
/// once; then how much sound the speaker wanted and did not get since the meeting's
/// sound began, and how much it was too slow to take.
#[must_use]
pub fn sound_line(gap: Duration, burst: usize, starved: Duration, dropped: Duration) -> String {
    format!(
        "sound thread: longest wait {} · most frames at once {burst} · speaker ran dry for {} · fell behind by {}",
        millis(gap),
        millis(starved),
        millis(dropped)
    )
}

/// How the network thread is keeping up.
#[must_use]
pub fn network_line(late: Duration) -> String {
    format!("network thread: timers up to {} late", millis(late))
}

/// What the speaker's own buffer adds to what waits for it: the device is asked to work
/// in pieces of this length.
pub const SPEAKER_PIECE: Duration = Duration::from_millis(10);

/// How long after a voice's frame arrives it is heard: its jitter buffer's delay, what
/// waits for the speaker, and the speaker's own piece.
#[must_use]
pub const fn heard_after(delay: Duration, speaker_lead: Duration) -> Duration {
    delay
        .saturating_add(speaker_lead)
        .saturating_add(SPEAKER_PIECE)
}

/// One voice being played.
///
/// How long after arriving it is heard, the delay its buffer aims for, and what became
/// of its frames. "Lost" are frames that could not be rebuilt; the fade at the start of
/// each pause is not among them.
#[must_use]
pub fn voice_line(name: &str, delay: Duration, speaker_lead: Duration, stats: &Stats) -> String {
    format!(
        "{name}: heard {} after arriving (buffer {}) · played {} · rebuilt {} · lost {} · skipped {} · late {}",
        millis(heard_after(delay, speaker_lead)),
        millis(delay),
        stats.played,
        stats.recovered_fec.saturating_add(stats.recovered_dred),
        stats.lost,
        stats.skipped,
        stats.late
    )
}

/// How the connection to one of the others is faring: the round trip, and how many of
/// the packets sent to them were lost on the way, in tenths of a percent.
#[must_use]
pub fn link_line(name: &str, rtt: Duration, sent: u64, lost: u64) -> String {
    let tenths = lost.saturating_mul(1000).checked_div(sent).unwrap_or(0);
    format!(
        "link to {name}: round trip {} · lost {lost} of {sent} packets sent ({}.{}%)",
        millis(rtt),
        tenths / 10,
        tenths % 10
    )
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
    fn the_details_say_how_the_threads_and_each_voice_are_faring() {
        assert_eq!(
            sound_line(
                Duration::from_micros(14_900),
                2,
                Duration::from_millis(20),
                Duration::ZERO
            ),
            "sound thread: longest wait 14 ms · most frames at once 2 · speaker ran dry for 20 ms · fell behind by 0 ms"
        );
        assert_eq!(
            network_line(Duration::from_millis(3)),
            "network thread: timers up to 3 ms late"
        );
        let stats = Stats {
            played: 1200,
            recovered_fec: 3,
            recovered_dred: 1,
            concealed: 12,
            lost: 2,
            skipped: 2,
            late: 5,
            ..Stats::default()
        };
        assert_eq!(
            voice_line(
                "Bo",
                Duration::from_millis(60),
                Duration::from_millis(45),
                &stats
            ),
            "Bo: heard 115 ms after arriving (buffer 60 ms) · played 1200 · rebuilt 4 · lost 2 · skipped 2 · late 5"
        );
    }

    #[test]
    fn a_link_line_gives_the_round_trip_and_the_share_of_packets_lost() {
        assert_eq!(
            link_line("Bo", Duration::from_millis(12), 4000, 30),
            "link to Bo: round trip 12 ms · lost 30 of 4000 packets sent (0.7%)"
        );
        // Nothing sent yet is nothing lost.
        assert_eq!(
            link_line("Bo", Duration::ZERO, 0, 0),
            "link to Bo: round trip 0 ms · lost 0 of 0 packets sent (0.0%)"
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
