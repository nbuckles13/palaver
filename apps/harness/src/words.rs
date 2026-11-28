//! What the window says and where it puts things, apart from the drawing of them.
//!
//! Everything here is a plain function of what is known, so that it is tested without a
//! window. The drawing code in [`crate::ui`] decides nothing but colours and layout.

use crate::check::Finding;
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

/// The system the program runs on, for where it names the system's own settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum System {
    Windows,
    Mac,
    Other,
}

impl System {
    /// The one this program was built for.
    pub const HERE: Self = if cfg!(target_os = "windows") {
        Self::Windows
    } else if cfg!(target_os = "macos") {
        Self::Mac
    } else {
        Self::Other
    };
}

/// What to say of the microphone while it is being tried, under its meter. How a
/// microphone is judged is the same everywhere; where to look to put it right is not.
#[must_use]
pub const fn microphone_hint(finding: Finding, system: System) -> &'static str {
    match (finding, system) {
        (Finding::TooSoon | Finding::NothingSaid, _) => "Say something. The bar should move.",
        (Finding::Good, _) => "You are heard well.",
        (Finding::Faint, _) => {
            "You are heard, but faintly. Move closer to the microphone, or raise its level in the system's sound settings."
        }
        (Finding::NoSignal, System::Mac) => {
            "Nothing at all is coming from this microphone. It may be muted or switched off, or Palaver may not be allowed the microphone (System Settings, Privacy & Security, Microphone)."
        }
        (Finding::NoSignal, System::Windows) => {
            "Nothing at all is coming from this microphone. It may be muted or switched off, or apps may not be allowed the microphone (Settings, Privacy & security, Microphone)."
        }
        (Finding::NoSignal, System::Other) => {
            "Nothing at all is coming from this microphone. It may be muted or switched off, on the device or in the system's sound settings."
        }
        (Finding::Processed, System::Windows) => {
            "Windows is cleaning this microphone before Palaver gets it (Voice Focus or Voice Clarity). You may be heard faintly and cut off between words; turning that off often sounds better. If the microphone then hisses, turn it back on."
        }
        (Finding::Processed, System::Mac) => {
            "The system is cleaning this microphone before Palaver gets it (Voice Isolation, under Mic Mode in the menu bar). You may be heard faintly and cut off between words; Standard often sounds better."
        }
        (Finding::Processed, System::Other) => {
            "Something is cleaning this microphone before Palaver gets it (noise suppression in the system or the device). You may be heard faintly and cut off between words; turning that off often sounds better. If the microphone then hisses, turn it back on."
        }
    }
}

/// What to say of the microphone during a meeting: only of what stays wrong whether or
/// not anyone is speaking, so that it does not come and go with each word.
#[must_use]
pub const fn microphone_warning(finding: Finding, system: System) -> Option<&'static str> {
    match finding {
        Finding::NoSignal | Finding::Processed => Some(microphone_hint(finding, system)),
        Finding::TooSoon | Finding::NothingSaid | Finding::Faint | Finding::Good => None,
    }
}

/// A way into the system's own settings: what the link says, and the address the system
/// opens them by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SettingsLink {
    pub label: &'static str,
    pub address: &'static str,
}

/// The steps to put a microphone right in the system's settings: a sentence with a link
/// in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Steps {
    pub before: &'static str,
    /// The words of the sentence that open the settings when clicked.
    pub link: SettingsLink,
    pub after: String,
}

/// The steps to put a microphone right, where they are known well enough to give: on
/// Windows, where its cleaning is switched off for the microphone called `device`.
#[must_use]
pub fn microphone_steps(finding: Finding, system: System, device: &str) -> Option<Steps> {
    match (finding, system) {
        (Finding::Processed, System::Windows) => Some(Steps {
            before: "To turn it off: ",
            link: SettingsLink {
                label: "open the sound settings",
                address: "ms-settings:sound",
            },
            after: format!(
                ", choose \u{201c}{device}\u{201d} under Input, and under Advanced settings set Audio enhancements to Off. That turns Voice Focus off as well."
            ),
        }),
        _ => None,
    }
}

/// Where in the system's settings a microphone found wanting can be put right, where
/// the system has an address for it. `None` for a microphone with nothing wrong, and on
/// systems with no one place for it.
#[must_use]
pub const fn settings_for(finding: Finding, system: System) -> Option<SettingsLink> {
    match (finding, system) {
        (Finding::NoSignal | Finding::Processed | Finding::Faint, System::Windows) => {
            Some(SettingsLink {
                label: "Open the sound settings",
                address: "ms-settings:sound",
            })
        }
        // The Mac's Voice Isolation is in its menu bar, which has no address.
        (Finding::NoSignal | Finding::Faint, System::Mac) => Some(SettingsLink {
            label: "Open the sound settings",
            address: "x-apple.systempreferences:com.apple.preference.sound?input",
        }),
        _ => None,
    }
}

/// Whether a finding is of something wrong, to be shown as a warning.
#[must_use]
pub const fn is_trouble(finding: Finding) -> bool {
    matches!(
        finding,
        Finding::NoSignal | Finding::Processed | Finding::Faint
    )
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

/// What is said of a noisy microphone while it is being tried: that more noise
/// suppression will be added, where that is left to Palaver, and nothing where the
/// strength was fixed by whoever started it.
#[must_use]
pub const fn noisy_hint(noisy: bool, by_ear: bool) -> Option<&'static str> {
    if noisy && by_ear {
        Some(
            "Palaver hears a lot of background noise from this microphone, and will add extra noise suppression to compensate.",
        )
    } else {
        None
    }
}

/// What the details say of this machine's picture as it would be sent: the rate, and
/// what coding costs.
#[must_use]
pub fn coding_line(bitrate: u64, encode: Duration, decode: Duration, let_go: u32) -> String {
    let tenths = |span: Duration| {
        let tenths = span.as_micros() / 100;
        format!("{}.{}", tenths / 10, tenths % 10)
    };
    let line = format!(
        "as sent: {} kbit/s · {} ms to encode · {} ms to decode",
        bitrate / 1000,
        tenths(encode),
        tenths(decode)
    );
    if let_go > 0 {
        format!("{line} · {let_go} let go of late")
    } else {
        line
    }
}

/// What the button says that shows this machine's own picture, or hides it.
#[must_use]
pub const fn self_view(shown: bool) -> &'static str {
    if shown {
        "Hide my picture"
    } else {
        "My picture"
    }
}

/// How the others' pictures are laid out in `width`: how many to a row, and how wide
/// and high each is. One is large, up to four are two to a row, more are three.
#[must_use]
#[expect(
    clippy::cast_precision_loss,
    reason = "a count of at most three, as a size on the screen"
)]
pub fn tiles(count: usize, width: f32) -> (usize, f32, f32) {
    let across: usize = match count {
        0 | 1 => 1,
        2..=4 => 2,
        _ => 3,
    };
    let gaps = 10.0 * (across - 1) as f32;
    let wide = ((width - gaps) / across as f32).clamp(80.0, 320.0);
    (across, wide, wide * 9.0 / 16.0)
}

/// How high `count` pictures stand, `across` to a row and each `tall` over its name.
#[must_use]
#[expect(
    clippy::cast_precision_loss,
    reason = "a count of rows, as a size on the screen"
)]
pub fn tiles_tall(count: usize, across: usize, tall: f32) -> f32 {
    count.div_ceil(across.max(1)) as f32 * (tall + 36.0)
}

/// A length of time in milliseconds, to a tenth.
#[must_use]
pub fn tenths(time: Duration) -> String {
    let tenths = time.as_micros() / 100;
    format!("{}.{} ms", tenths / 10, tenths % 10)
}

/// How much of `most` a length of time is: nought to one, and one for anything over.
#[must_use]
pub fn share(value: Duration, most: Duration) -> f32 {
    if most.is_zero() {
        return 1.0;
    }
    (value.as_secs_f32() / most.as_secs_f32()).clamp(0.0, 1.0)
}

/// The share of the packets sent on a link that were lost, to a tenth of a percent.
#[must_use]
pub fn lost_share(sent: u64, lost: u64) -> String {
    let tenths = lost.saturating_mul(1000) / sent.max(1);
    format!("{}.{}%", tenths / 10, tenths % 10)
}

/// The most frames the microphone had waiting at once: one is right.
#[must_use]
pub fn burst_line(burst: usize) -> String {
    format!("microphone: {burst} at once at most (1 is right)")
}

/// What the plan in force comes to: who conducts, how many are here, and the links.
#[must_use]
pub fn plan_line(conductor: Option<&str>, here: usize, links: usize, sound: usize) -> String {
    let conducts = conductor.map_or_else(String::new, |name| format!("{name} conducts · "));
    format!("{conducts}{here} here · {links} links, sound on {sound}")
}

/// Where each of `count` people is drawn on a map, across and down, each from nought to
/// one.
///
/// `me` is at the bottom; everyone else stands a row higher for each link sound takes
/// to reach them, those of a row spread evenly across. `links` are pairs of people by
/// number, with whether sound travels along each. Someone sound does not reach is put
/// by the spare links, and failing that in a row above everyone.
#[must_use]
#[expect(
    clippy::cast_precision_loss,
    reason = "counts of people in a meeting, as places on the screen"
)]
pub fn map_places(count: usize, links: &[(usize, usize, bool)], me: usize) -> Vec<(f32, f32)> {
    let mut row: Vec<Option<usize>> = vec![None; count];
    if let Some(mine) = row.get_mut(me) {
        *mine = Some(0);
    }
    // Rows by sound's links first, then by the spare ones for whoever is left.
    for sound_only in [true, false] {
        loop {
            let mut reached = false;
            for (one, other, sound) in links {
                if sound_only && !sound {
                    continue;
                }
                let rows = (
                    row.get(*one).copied().flatten(),
                    row.get(*other).copied().flatten(),
                );
                let (found, from) = match rows {
                    (Some(from), None) => (*other, from),
                    (None, Some(from)) => (*one, from),
                    _ => continue,
                };
                if let Some(place) = row.get_mut(found) {
                    *place = Some(from + 1);
                    reached = true;
                }
            }
            if !reached {
                break;
            }
        }
    }
    let top = row.iter().flatten().max().copied().unwrap_or(0);
    let rows: Vec<usize> = row.iter().map(|place| place.unwrap_or(top + 1)).collect();
    let most = rows.iter().max().copied().unwrap_or(0).max(1);
    let mut seen = vec![0usize; most + 1];
    let in_row = |which: usize| rows.iter().filter(|row| **row == which).count();
    rows.iter()
        .map(|which| {
            let place = seen.get(*which).copied().unwrap_or(0);
            if let Some(count) = seen.get_mut(*which) {
                *count += 1;
            }
            let across = (place + 1) as f32 / (in_row(*which) + 1) as f32;
            (across, 1.0 - *which as f32 / most as f32)
        })
        .collect()
}

/// What someone else's picture is called.
#[must_use]
pub fn far_name(name: &str) -> String {
    format!("{name}'s picture")
}

/// A length on the screen in its pixels, for a screen with `scale` of them to a point.
#[must_use]
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "a length on a screen, which is neither negative nor large"
)]
pub fn pixels(points: f32, scale: f32) -> u32 {
    (points * scale).round().max(0.0) as u32
}

/// How large a picture is drawn in a tile `tall` high: as wide as keeps its shape.
#[must_use]
#[expect(
    clippy::cast_precision_loss,
    reason = "a picture's size in pixels, as a size on the screen"
)]
pub fn tile_width(width: u32, height: u32, tall: f32) -> f32 {
    tall * width as f32 / height.max(1) as f32
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

/// A voice's line, with a word added when it is not being played and the line is of how
/// it last was.
#[must_use]
pub fn resting(line: String, playing: bool) -> String {
    if playing {
        line
    } else {
        format!("{line} · not playing now")
    }
}

/// The top of a plot's scale: the least of 1, 2 or 5 times a power of ten that holds
/// everything in `past`, and `least` too, so that small numbers are not drawn large.
#[must_use]
pub fn plot_top(past: &[Option<u32>], least: u32) -> u32 {
    let most = past.iter().flatten().copied().max().unwrap_or(0).max(least);
    let mut ten = 1u32;
    loop {
        for step in [1u32, 2, 5] {
            let top = ten.saturating_mul(step);
            if top >= most || top == u32::MAX {
                return top;
            }
        }
        ten = ten.saturating_mul(10);
    }
}

/// The lines of a plot, in a square from (0, 0) at the top left to (1, 1).
///
/// One run of points for each unbroken stretch of values, the newest at the right edge,
/// and a full `kept` of them reaching the left.
#[must_use]
#[expect(
    clippy::cast_precision_loss,
    reason = "a few hundred values and what they measure, as places on the screen"
)]
pub fn plot_runs(past: &[Option<u32>], top: u32, kept: usize) -> Vec<Vec<(f32, f32)>> {
    let across = kept.saturating_sub(1).max(1) as f32;
    let first = kept.saturating_sub(past.len());
    let mut runs = Vec::new();
    let mut run = Vec::new();
    for (place, value) in past.iter().enumerate() {
        match value {
            Some(value) => {
                let x = (first.saturating_add(place)) as f32 / across;
                let y = 1.0 - (*value as f32 / top.max(1) as f32).clamp(0.0, 1.0);
                run.push((x.clamp(0.0, 1.0), y));
            }
            None => {
                if !run.is_empty() {
                    runs.push(std::mem::take(&mut run));
                }
            }
        }
    }
    if !run.is_empty() {
        runs.push(run);
    }
    runs
}

/// What stands over a plot: what it is of, its value now, and the most it has been.
#[must_use]
pub fn plot_caption(what: &str, past: &[Option<u32>]) -> String {
    let now = past
        .last()
        .copied()
        .flatten()
        .map_or_else(|| String::from("–"), |now| now.to_string());
    let most = past.iter().flatten().copied().max().unwrap_or(0);
    format!("{what} · now {now} · most {most}")
}

/// The value of a plot under a pointer `across` its width (0 at the left edge, 1 at the
/// right), and how many values ago it was. `None` where nothing is drawn.
#[must_use]
#[expect(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "a place among a few hundred values, from a share of a width kept within 0 and 1"
)]
pub fn plot_at(past: &[Option<u32>], kept: usize, across: f32) -> Option<(u32, usize)> {
    let last = kept.saturating_sub(1);
    let slot = (across.clamp(0.0, 1.0) * last as f32).round() as usize;
    let place = slot.checked_sub(kept.saturating_sub(past.len()))?;
    let value = past.get(place).copied().flatten()?;
    Some((value, last.saturating_sub(slot)))
}

/// What is said of the value under the pointer: a value is taken each second.
#[must_use]
pub fn plot_pointed(value: u32, ago: usize) -> String {
    match ago {
        0 => format!("{value} · now"),
        ago => format!("{value} · {ago} s ago"),
    }
}

/// How wide the window should become to hold something `extra` wide beside what it shows.
///
/// `None` if it should stay as it is and give up some of its own room: when it already
/// fills the screen, or would not fit on it.
#[must_use]
pub fn grown_width(inner: f32, extra: f32, screen: Option<f32>, fills_screen: bool) -> Option<f32> {
    let wanted = inner + extra;
    let fits = screen.is_none_or(|screen| wanted <= screen);
    (!fills_screen && fits).then_some(wanted)
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
    fn the_microphone_is_warned_of_in_a_meeting_only_for_what_stays_wrong() {
        let here = System::Windows;
        for finding in [Finding::NoSignal, Finding::Processed] {
            assert_eq!(
                microphone_warning(finding, here),
                Some(microphone_hint(finding, here))
            );
            assert!(is_trouble(finding));
        }
        // A faint voice is said while trying the microphone, and not over a meeting,
        // where it would come and go with each word.
        assert!(is_trouble(Finding::Faint));
        for finding in [
            Finding::TooSoon,
            Finding::NothingSaid,
            Finding::Faint,
            Finding::Good,
        ] {
            assert_eq!(microphone_warning(finding, here), None);
        }
    }

    #[test]
    fn the_steps_on_windows_name_the_microphone_and_the_setting() {
        let steps =
            microphone_steps(Finding::Processed, System::Windows, "Headset Microphone").unwrap();
        assert_eq!(
            format!("{}{}{}", steps.before, steps.link.label, steps.after),
            "To turn it off: open the sound settings, choose \u{201c}Headset Microphone\u{201d} under Input, and under Advanced settings set Audio enhancements to Off. That turns Voice Focus off as well."
        );
        assert_eq!(steps.link.address, "ms-settings:sound");
        assert_eq!(
            microphone_steps(Finding::Processed, System::Mac, "a microphone"),
            None
        );
        assert_eq!(
            microphone_steps(Finding::Faint, System::Windows, "a microphone"),
            None
        );
    }

    #[test]
    fn a_way_into_the_systems_settings_is_offered_where_there_is_one() {
        for finding in [Finding::NoSignal, Finding::Processed, Finding::Faint] {
            let link = settings_for(finding, System::Windows).unwrap();
            assert_eq!(link.address, "ms-settings:sound");
            assert_eq!(settings_for(finding, System::Other), None);
        }
        assert!(
            settings_for(Finding::Faint, System::Mac)
                .is_some_and(|link| link.address.starts_with("x-apple.systempreferences:"))
        );
        assert_eq!(settings_for(Finding::Processed, System::Mac), None);
        // Nothing wrong, nothing to open.
        for finding in [Finding::TooSoon, Finding::NothingSaid, Finding::Good] {
            assert_eq!(settings_for(finding, System::Windows), None);
        }
    }

    #[test]
    fn where_to_put_a_microphone_right_is_said_for_the_system_it_is_on() {
        let processed = |system| microphone_hint(Finding::Processed, system);
        assert!(processed(System::Windows).contains("Voice Focus"));
        assert!(processed(System::Mac).contains("Voice Isolation"));
        assert!(processed(System::Other).contains("noise suppression"));
        assert!(microphone_hint(Finding::NoSignal, System::Mac).contains("Privacy"));
        // What needs no settings is said the same everywhere.
        for system in [System::Windows, System::Mac, System::Other] {
            assert_eq!(
                microphone_hint(Finding::NothingSaid, system),
                "Say something. The bar should move."
            );
        }
    }

    #[test]
    fn a_plots_scale_is_a_round_number_that_holds_everything() {
        assert_eq!(plot_top(&[Some(3), None, Some(7)], 0), 10);
        assert_eq!(plot_top(&[Some(11)], 0), 20);
        assert_eq!(plot_top(&[Some(21)], 0), 50);
        assert_eq!(plot_top(&[Some(500)], 0), 500);
        assert_eq!(plot_top(&[Some(501)], 0), 1000);
        // Nothing to draw, or only small numbers, and the least still stands.
        assert_eq!(plot_top(&[], 5), 5);
        assert_eq!(plot_top(&[Some(1)], 50), 50);
        assert_eq!(plot_top(&[None], 0), 1);
        assert_eq!(plot_top(&[Some(u32::MAX)], 0), u32::MAX);
    }

    #[test]
    fn a_plot_is_drawn_from_the_right_and_broken_where_there_was_nothing() {
        // Five kept, three known: they stand in the last three places.
        let runs = plot_runs(&[Some(0), Some(5), Some(10)], 10, 5);
        assert_eq!(runs, [vec![(0.5, 1.0), (0.75, 0.5), (1.0, 0.0)]]);
        // A gap makes two runs; what is over the top is drawn at the top.
        let runs = plot_runs(&[Some(10), None, None, Some(0), Some(30)], 10, 5);
        assert_eq!(runs, [vec![(0.0, 0.0)], vec![(0.75, 1.0), (1.0, 0.0)]]);
        assert!(plot_runs(&[None, None], 10, 5).is_empty());
    }

    #[test]
    fn a_plots_caption_gives_now_and_the_most() {
        assert_eq!(
            plot_caption("round trip, ms", &[Some(7), Some(12), Some(9)]),
            "round trip, ms · now 9 · most 12"
        );
        assert_eq!(
            plot_caption("heard after, ms", &[Some(70), None]),
            "heard after, ms · now – · most 70"
        );
        assert_eq!(
            resting(String::from("Bo: heard"), false),
            "Bo: heard · not playing now"
        );
        assert_eq!(resting(String::from("Bo: heard"), true), "Bo: heard");
    }

    #[test]
    fn the_value_under_the_pointer_is_found_by_how_far_across_it_is() {
        // Five kept, three known: they stand in the last three places.
        let past = [Some(4), None, Some(9)];
        assert_eq!(plot_at(&past, 5, 1.0), Some((9, 0)));
        assert_eq!(plot_at(&past, 5, 0.5), Some((4, 2)));
        // A gap, and the places before anything was kept, have nothing under them.
        assert_eq!(plot_at(&past, 5, 0.75), None);
        assert_eq!(plot_at(&past, 5, 0.0), None);
        // The nearest value is the one meant, and the edges are not overrun.
        assert_eq!(plot_at(&past, 5, 0.9), Some((9, 0)));
        assert_eq!(plot_at(&past, 5, 7.0), Some((9, 0)));
        assert_eq!(plot_pointed(9, 0), "9 · now");
        assert_eq!(plot_pointed(4, 2), "4 · 2 s ago");
    }

    #[test]
    fn the_window_grows_for_the_details_only_where_there_is_room() {
        assert_eq!(grown_width(520.0, 400.0, Some(1920.0), false), Some(920.0));
        assert_eq!(grown_width(520.0, 400.0, None, false), Some(920.0));
        // A window that fills its screen, or would run off it, keeps its size.
        assert_eq!(grown_width(1280.0, 400.0, Some(1280.0), false), None);
        assert_eq!(grown_width(1000.0, 400.0, Some(1280.0), false), None);
        assert_eq!(grown_width(520.0, 400.0, Some(1920.0), true), None);
    }

    #[test]
    fn a_coding_line_gives_the_rate_and_the_costs() {
        assert_eq!(
            coding_line(
                487_300,
                Duration::from_micros(3140),
                Duration::from_micros(870),
                0
            ),
            "as sent: 487 kbit/s · 3.1 ms to encode · 0.8 ms to decode"
        );
        assert_eq!(
            coding_line(
                90_000,
                Duration::from_millis(40),
                Duration::from_millis(2),
                4
            ),
            "as sent: 90 kbit/s · 40.0 ms to encode · 2.0 ms to decode · 4 let go of late"
        );
    }

    #[test]
    fn the_others_pictures_are_smaller_the_more_there_are() {
        // One is as large as it may be; a narrow window makes it smaller.
        assert_eq!(tiles(1, 464.0), (1, 320.0, 180.0));
        assert_eq!(tiles(1, 160.0), (1, 160.0, 90.0));
        // Two to a row up to four, then three.
        assert_eq!(tiles(2, 330.0), (2, 160.0, 90.0));
        assert_eq!(tiles(4, 330.0).0, 2);
        assert_eq!(tiles(5, 500.0), (3, 160.0, 90.0));
        // Nobody's is no division by nothing.
        assert_eq!(tiles(0, 464.0).0, 1);
        // Five at three to a row are two rows, each with its name under it.
        assert!((tiles_tall(5, 3, 90.0) - 252.0).abs() < 0.001);
        assert!(tiles_tall(0, 1, 180.0).abs() < 0.001);
    }

    #[test]
    fn times_and_shares_are_said_shortly() {
        assert_eq!(tenths(Duration::from_micros(3140)), "3.1 ms");
        assert_eq!(tenths(Duration::ZERO), "0.0 ms");
        let ms = Duration::from_millis;
        assert!((share(ms(5), ms(20)) - 0.25).abs() < 0.001);
        assert!((share(ms(50), ms(20)) - 1.0).abs() < 0.001);
        assert!((share(ms(5), Duration::ZERO) - 1.0).abs() < 0.001);
        assert_eq!(lost_share(4000, 30), "0.7%");
        assert_eq!(lost_share(0, 0), "0.0%");
        assert_eq!(
            plan_line(Some("Ana"), 8, 11, 7),
            "Ana conducts · 8 here · 11 links, sound on 7"
        );
        assert_eq!(plan_line(None, 1, 0, 0), "1 here · 0 links, sound on 0");
    }

    #[test]
    fn a_map_puts_me_at_the_bottom_and_the_rest_by_how_far_sound_goes() {
        // 0 is me; 1 and 2 hear me directly; 3 through 1; 4 only has a spare link to 2.
        let links = [
            (0, 1, true),
            (2, 0, true),
            (1, 3, true),
            (1, 2, false),
            (2, 4, false),
        ];
        let places = map_places(5, &links, 0);
        let near = |got: (f32, f32), want: (f32, f32)| {
            (got.0 - want.0).abs() < 0.001 && (got.1 - want.1).abs() < 0.001
        };
        assert!(near(places[0], (0.5, 1.0)), "{places:?}");
        // The two beside me share a row, a third and two thirds across.
        assert!(near(places[1], (1.0 / 3.0, 0.5)), "{places:?}");
        assert!(near(places[2], (2.0 / 3.0, 0.5)), "{places:?}");
        // Two links away, by sound or by a spare link.
        assert!(near(places[3], (1.0 / 3.0, 0.0)), "{places:?}");
        assert!(near(places[4], (2.0 / 3.0, 0.0)), "{places:?}");
        // Alone is in the middle at the bottom; nobody is nothing.
        assert!(near(map_places(1, &[], 0)[0], (0.5, 1.0)));
        assert!(map_places(0, &[], 0).is_empty());
        // Someone no link reaches is put above the rest.
        let apart = map_places(3, &[(0, 1, true)], 0);
        assert!(near(apart[2], (0.5, 0.0)), "{apart:?}");
    }

    #[test]
    fn a_tile_keeps_the_shape_of_its_picture() {
        assert!((tile_width(640, 360, 90.0) - 160.0).abs() < 0.001);
        assert!((tile_width(480, 640, 80.0) - 60.0).abs() < 0.001);
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
