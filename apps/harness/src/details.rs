//! The details beside the meeting: how the sound, the pictures, the network and this
//! machine are faring, a tab for each.
//!
//! Each tab is a table of the numbers as they stand, with a total where one means
//! something, and under it the lines of whichever row is chosen, over the last two
//! minutes. Nothing here measures anything: it draws what the session keeps.

use crate::history::{KEPT, LinkLine, PictureLine, SentLine, VoiceLine};
use crate::session::{Map, View};
use crate::ui::ember;
use crate::words;
use eframe::egui::{
    self, Align2, CornerRadius, FontId, Pos2, Rect, RichText, Sense, Stroke, Ui, Vec2,
};
use palaver_proto::ParticipantId;
use std::time::Duration;

/// The parts the details are in.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Tab {
    #[default]
    Audio,
    Video,
    Network,
    System,
}

impl Tab {
    const ALL: [Self; 4] = [Self::Audio, Self::Video, Self::Network, Self::System];

    /// The tab called `name`, whatever its letters' case.
    #[must_use]
    pub fn named(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|tab| tab.name().eq_ignore_ascii_case(name))
    }

    const fn name(self) -> &'static str {
        match self {
            Self::Audio => "Audio",
            Self::Video => "Video",
            Self::Network => "Network",
            Self::System => "System",
        }
    }
}

/// Which tab is open, and which row of each is chosen to draw the lines of.
#[derive(Debug, Clone, Default)]
pub struct Chosen {
    pub tab: Tab,
    voice: Option<String>,
    picture: Option<ParticipantId>,
    link: Option<String>,
}

impl Chosen {
    /// Nothing chosen yet, with `tab` open.
    #[must_use]
    pub fn on(tab: Tab) -> Self {
        Self {
            tab,
            ..Self::default()
        }
    }
}

/// What the window itself knows of this machine's own picture.
#[derive(Debug, Clone, Default)]
pub struct Own {
    /// How large it is, and how long one takes to make ready for the screen, if it
    /// has been drawn.
    pub drawn: Option<(u32, u32, Duration)>,
    /// What coding it in a loop on this machine costs, where that is being tried.
    pub looped: Option<String>,
}

fn dim(text: &str) -> RichText {
    RichText::new(text).small().color(ember::DIM)
}

fn number(text: impl Into<String>) -> RichText {
    RichText::new(text).small().monospace().color(ember::QUIET)
}

fn strong(text: impl Into<String>) -> RichText {
    RichText::new(text).small().monospace().color(ember::TEXT)
}

fn heading(ui: &mut Ui, text: &str) {
    ui.add_space(8.0);
    ui.label(RichText::new(text).small().strong().color(ember::TEXT));
}

/// A table: its headings, then whatever `rows` puts in, a cell at a time.
fn table(ui: &mut Ui, id: &str, heads: &[&str], rows: impl FnOnce(&mut Ui)) {
    egui::Grid::new(id)
        .num_columns(heads.len())
        .spacing(Vec2::new(12.0, 4.0))
        .show(ui, |ui| {
            for head in heads {
                ui.label(dim(head));
            }
            ui.end_row();
            rows(ui);
        });
}

/// The first cell of a row: its name, which chooses the row when clicked.
fn name_cell(ui: &mut Ui, name: &str, chosen: bool) -> bool {
    let text = if chosen { strong(name) } else { number(name) };
    ui.add(egui::Label::new(text).sense(Sense::click()))
        .clicked()
}

/// The last row of a table: `cells` under the columns they total, the rest left empty.
fn total_row(ui: &mut Ui, cells: &[Option<String>]) {
    ui.label(strong("Total"));
    for cell in cells {
        match cell {
            Some(text) => ui.label(strong(text.clone())),
            None => ui.label(""),
        };
    }
    ui.end_row();
}

/// A number over the last two minutes, small enough for a cell of a table.
fn spark(ui: &mut Ui, past: &[Option<u32>], least: u32) {
    let (area, _) = ui.allocate_exact_size(Vec2::new(64.0, 14.0), Sense::hover());
    let top = words::plot_top(past, least);
    let inner = area.shrink(1.0);
    let place = |(x, y): (f32, f32)| {
        Pos2::new(
            inner.left() + x * inner.width(),
            inner.top() + y * inner.height(),
        )
    };
    for run in &words::plot_runs(past, top, KEPT) {
        let points: Vec<Pos2> = run.iter().copied().map(place).collect();
        if points.len() > 1 {
            ui.painter().line(points, Stroke::new(1.0, ember::QUIET));
        }
    }
}

/// A cell of a row of boxes: what a number is, and the number.
fn boxed(ui: &mut Ui, label: &str, value: &str) {
    egui::Frame::new()
        .stroke(Stroke::new(1.0, ember::LINE))
        .corner_radius(CornerRadius::same(4))
        .inner_margin(egui::Margin::symmetric(8, 6))
        .show(ui, |ui| {
            ui.vertical(|ui| {
                ui.label(dim(label));
                ui.label(RichText::new(value).monospace().color(ember::TEXT));
            });
        });
}

/// A number against the most it may be: a bar, filled as far as it has come.
fn gauge(ui: &mut Ui, label: &str, value: Duration, most: Duration) {
    ui.label(number(label));
    ui.label(strong(words::tenths(value)));
    let (area, _) = ui.allocate_exact_size(Vec2::new(90.0, 6.0), Sense::hover());
    let share = words::share(value, most);
    let colour = if share >= 1.0 {
        ember::EMBER
    } else {
        ember::QUIET
    };
    ui.painter()
        .rect_filled(area, CornerRadius::same(2), ember::GROUND);
    let filled = Rect::from_min_size(area.min, Vec2::new(area.width() * share, area.height()));
    ui.painter()
        .rect_filled(filled, CornerRadius::same(2), colour);
    ui.label(dim(&format!("of {}", words::millis(most))));
    ui.end_row();
}

fn size_cell(size: Option<(u32, u32)>) -> String {
    size.map_or_else(
        || String::from("—"),
        |(width, height)| format!("{width} × {height}"),
    )
}

/// Draw the details as things stand.
pub fn draw(ui: &mut Ui, view: &View, chosen: &mut Chosen, own: &Own) {
    ui.horizontal(|ui| {
        for tab in Tab::ALL {
            if ui.selectable_label(chosen.tab == tab, tab.name()).clicked() {
                chosen.tab = tab;
            }
        }
    });
    ui.separator();
    match chosen.tab {
        Tab::Audio => audio(ui, view, chosen),
        Tab::Video => video(ui, view, chosen, own),
        Tab::Network => network(ui, view, chosen),
        Tab::System => system(ui, view, own),
    }
}

fn audio(ui: &mut Ui, view: &View, chosen: &mut Chosen) {
    heading(ui, "Your microphone and speaker");
    ui.horizontal_wrapped(|ui| {
        let noise = if view.sound.noise_high { "High" } else { "Low" };
        boxed(ui, "Noise removal", noise);
        boxed(
            ui,
            "Speaker holds",
            &words::millis(view.details.speaker_lead),
        );
        boxed(ui, "Ran dry", &words::millis(view.sound.starved));
        boxed(ui, "Fell behind", &words::millis(view.sound.dropped));
    });

    heading(ui, "Voices");
    let voices = &view.details.voices;
    if voices.is_empty() {
        ui.label(dim("No voices are being played."));
        return;
    }
    let picked = chosen
        .voice
        .clone()
        .filter(|name| voices.iter().any(|voice| voice.name == *name))
        .or_else(|| voices.first().map(|voice| voice.name.clone()));
    let heads = [
        "Voice",
        "Heard after",
        "Buffer",
        "Lost",
        "Late",
        "Rebuilt",
        "",
    ];
    table(ui, "voices", &heads, |ui| {
        for voice in voices {
            voice_row(
                ui,
                voice,
                view.details.speaker_lead,
                picked.as_ref(),
                chosen,
            );
        }
        let sum = |of: fn(&VoiceLine) -> u64| -> Option<String> {
            Some(voices.iter().map(of).sum::<u64>().to_string())
        };
        total_row(
            ui,
            &[
                None,
                None,
                sum(|voice| voice.stats.lost),
                sum(|voice| voice.stats.late),
                sum(rebuilt),
            ],
        );
    });
    if let Some(voice) = voices
        .iter()
        .find(|voice| Some(&voice.name) == picked.as_ref())
    {
        heading(ui, &format!("{} over two minutes", voice.name));
        plot(ui, "heard after arriving, ms", &voice.delay_past, 100);
        plot(ui, "frames lost or late a second", &voice.trouble_past, 5);
    }
}

/// Frames of a voice that were lost on the way and made again from what came after.
const fn rebuilt(voice: &VoiceLine) -> u64 {
    voice
        .stats
        .recovered_fec
        .saturating_add(voice.stats.recovered_dred)
}

fn voice_row(
    ui: &mut Ui,
    voice: &VoiceLine,
    speaker_lead: Duration,
    picked: Option<&String>,
    chosen: &mut Chosen,
) {
    let name = words::resting(voice.name.clone(), voice.playing);
    if name_cell(ui, &name, Some(&voice.name) == picked) {
        chosen.voice = Some(voice.name.clone());
    }
    let heard = words::heard_after(voice.delay, speaker_lead);
    ui.label(number(words::millis(heard)));
    ui.label(number(words::millis(voice.delay)));
    ui.label(number(voice.stats.lost.to_string()));
    ui.label(number(voice.stats.late.to_string()));
    ui.label(number(rebuilt(voice).to_string()));
    spark(ui, &voice.delay_past, 100);
    ui.end_row();
}

fn video(ui: &mut Ui, view: &View, chosen: &mut Chosen, own: &Own) {
    if let Some(why) = &view.details.picture_trouble {
        ui.label(RichText::new(format!("Pictures stopped: {why}")).color(ember::EMBER));
    }
    heading(ui, "You send");
    let sent = &view.details.sent;
    if sent.is_empty() {
        ui.label(dim("This machine offers no camera."));
    } else {
        let heads = ["Size", "kbit/s", "a second", "Encode", ""];
        table(ui, "sent", &heads, |ui| {
            for line in sent {
                sent_row(ui, line);
            }
            let rate: u32 = sent.iter().map(|line| line.flowed.rate).sum();
            let cost: Duration = sent.iter().map(|line| line.flowed.cost).sum();
            total_row(
                ui,
                &[Some(rate.to_string()), None, Some(words::tenths(cost))],
            );
        });
    }
    if let Some(looped) = &own.looped {
        ui.label(dim(looped));
    }

    heading(ui, "You are shown");
    let shown = &view.details.shown;
    if shown.is_empty() {
        ui.label(dim("Nobody else has a picture."));
        return;
    }
    let picked = chosen
        .picture
        .clone()
        .filter(|id| shown.iter().any(|line| line.id == *id))
        .or_else(|| shown.first().map(|line| line.id.clone()));
    let heads = [
        "Picture", "Size", "kbit/s", "a second", "Decode", "Keys", "",
    ];
    table(ui, "shown", &heads, |ui| {
        for line in shown {
            let is_picked = Some(&line.id) == picked.as_ref();
            if name_cell(ui, &line.name, is_picked) {
                chosen.picture = Some(line.id.clone());
            }
            ui.label(number(size_cell(line.size)));
            ui.label(number(line.flowed.rate.to_string()));
            ui.label(number(line.flowed.pace.to_string()));
            ui.label(number(words::tenths(line.flowed.cost)));
            ui.label(number(line.keys.to_string()));
            spark(ui, &line.rate_past, 100);
            ui.end_row();
        }
        let rate: u32 = shown.iter().map(|line| line.flowed.rate).sum();
        let cost: Duration = shown.iter().map(|line| line.flowed.cost).sum();
        let keys: u64 = shown.iter().map(|line| line.keys).sum();
        total_row(
            ui,
            &[
                None,
                Some(rate.to_string()),
                None,
                Some(words::tenths(cost)),
                Some(keys.to_string()),
            ],
        );
    });
    if let Some(line) = shown.iter().find(|line| Some(&line.id) == picked.as_ref()) {
        picture_plots(ui, line);
    }
}

fn sent_row(ui: &mut Ui, line: &SentLine) {
    let watched = line.flowed.pace > 0;
    let dash = || String::from("—");
    ui.label(number(size_cell(line.size)));
    if watched {
        ui.label(number(line.flowed.rate.to_string()));
        ui.label(number(line.flowed.pace.to_string()));
        ui.label(number(words::tenths(line.flowed.cost)));
        spark(ui, &line.rate_past, 100);
    } else {
        ui.label(number(dash()));
        ui.label(number(dash()));
        ui.label(number(dash()));
        ui.label(dim("nobody watching"));
    }
    ui.end_row();
}

fn picture_plots(ui: &mut Ui, line: &PictureLine) {
    heading(ui, &format!("{} over two minutes", line.name));
    plot(ui, "kbit/s", &line.rate_past, 100);
    plot(ui, "pictures a second", &line.pace_past, 30);
}

fn network(ui: &mut Ui, view: &View, chosen: &mut Chosen) {
    let map = &view.details.map;
    ui.add_space(4.0);
    ui.label(number(words::plan_line(
        map.conductor(),
        map.places.len(),
        map.links.len(),
        map.links.iter().filter(|link| link.2).count(),
    )));
    if let Some(kind) = view.network {
        ui.label(dim(&format!("This machine is {kind}.")));
    }
    small_map(ui, map);

    heading(ui, "Your links");
    let links = &view.details.links;
    if links.is_empty() {
        ui.label(dim("No links yet."));
        return;
    }
    let picked = chosen
        .link
        .clone()
        .filter(|name| links.iter().any(|link| link.name == *name))
        .or_else(|| links.first().map(|link| link.name.clone()));
    let heads = ["Link to", "Round trip", "Lost", "Up", "Down", ""];
    table(ui, "links", &heads, |ui| {
        for link in links {
            if name_cell(ui, &link.name, Some(&link.name) == picked.as_ref()) {
                chosen.link = Some(link.name.clone());
            }
            ui.label(number(words::millis(link.rtt)));
            ui.label(number(words::lost_share(link.sent, link.lost)));
            ui.label(number(link.up.to_string()));
            ui.label(number(link.down.to_string()));
            spark(ui, &link.rtt_past, 50);
            ui.end_row();
        }
        let up: u32 = links.iter().map(|link| link.up).sum();
        let down: u32 = links.iter().map(|link| link.down).sum();
        total_row(
            ui,
            &[None, None, Some(up.to_string()), Some(down.to_string())],
        );
    });
    ui.label(dim("Up and down are in kbit/s."));
    if let Some(link) = links
        .iter()
        .find(|link| Some(&link.name) == picked.as_ref())
    {
        link_plots(ui, link);
    }
}

fn link_plots(ui: &mut Ui, link: &LinkLine) {
    heading(ui, &format!("The link to {} over two minutes", link.name));
    plot(ui, "round trip, ms", &link.rtt_past, 50);
    plot(ui, "packets lost a second", &link.lost_past, 5);
    plot(ui, "up, kbit/s", &link.up_past, 100);
    plot(ui, "down, kbit/s", &link.down_past, 100);
}

/// The meeting's wiring, small: a dot for each node, this machine at the bottom.
fn small_map(ui: &mut Ui, map: &Map) {
    if map.places.is_empty() {
        return;
    }
    let size = Vec2::new(ui.available_width().min(380.0), 150.0);
    let (area, _) = ui.allocate_exact_size(size, Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(area, CornerRadius::same(4), ember::GROUND);
    let me = map.places.iter().position(|place| place.me).unwrap_or(0);
    let places = words::map_places(map.places.len(), &map.links, me);
    let inner = area.shrink2(Vec2::new(30.0, 22.0));
    let at = |place: usize| {
        let (across, down) = places.get(place).copied().unwrap_or((0.5, 0.5));
        Pos2::new(
            inner.left() + across * inner.width(),
            inner.top() + down * inner.height(),
        )
    };
    for (one, other, sound) in &map.links {
        let stroke = if *sound {
            Stroke::new(1.5, ember::QUIET)
        } else {
            Stroke::new(1.0, ember::LINE)
        };
        painter.line_segment([at(*one), at(*other)], stroke);
    }
    for (number, place) in map.places.iter().enumerate() {
        let centre = at(number);
        if place.me {
            painter.circle(centre, 6.0, ember::GROUND, Stroke::new(2.0, ember::EMBER));
        } else if place.conducts {
            painter.circle_filled(centre, 6.0, ember::TEXT);
        } else {
            painter.circle(centre, 6.0, ember::PANEL, Stroke::new(1.5, ember::QUIET));
        }
        let name = if place.me { "You" } else { place.name.as_str() };
        painter.text(
            centre + Vec2::new(0.0, 9.0),
            Align2::CENTER_TOP,
            name,
            FontId::proportional(10.0),
            ember::QUIET,
        );
    }
    ui.label(dim(
        "Sound travels along the brighter lines; the filled dot conducts.",
    ));
}

fn system(ui: &mut Ui, view: &View, own: &Own) {
    heading(ui, "Devices");
    let camera = own.drawn.map_or_else(
        || String::from("none"),
        |(width, height, _)| format!("a moving pattern, {width} × {height}"),
    );
    table(ui, "devices", &["", ""], |ui| {
        for (what, name) in [
            ("Microphone", view.microphone.as_str()),
            ("Speaker", view.speaker.as_str()),
            ("Camera", camera.as_str()),
        ] {
            ui.label(dim(what));
            ui.label(number(name));
            ui.end_row();
        }
    });

    heading(ui, "How this machine is keeping up");
    // The speaker takes sound every 10 ms; a wait of two frames of sound is heard.
    let frame = Duration::from_millis(40);
    let picture = Duration::from_millis(33);
    let encode: Duration = view.details.sent.iter().map(|l| l.flowed.cost).sum();
    let decode: Duration = view.details.shown.iter().map(|l| l.flowed.cost).sum();
    table(ui, "keeping-up", &["", "", "", ""], |ui| {
        gauge(ui, "Sound thread waits", view.details.sound_gap, frame);
        gauge(
            ui,
            "Network timers late",
            view.details.network_late,
            Duration::from_millis(10),
        );
        gauge(ui, "Encoding a picture", encode, picture);
        gauge(ui, "Decoding the pictures", decode, picture);
        if let Some((_, _, cost)) = own.drawn {
            gauge(ui, "Drawing your picture", cost, picture);
        }
    });
    ui.label(number(words::burst_line(view.details.sound_burst)));

    heading(ui, "The sound thread over two minutes");
    plot(ui, "longest wait, ms", &view.details.sound_gap_past, 50);
}

/// How much of a plot's width is kept for the numbers of its scale.
const SCALE_WIDTH: f32 = 34.0;

/// One number over the last two minutes: a caption, and under it a line against a scale,
/// with the value under the pointer said beside it. `least` is the least the top of the
/// scale may be, so that a small number is not drawn as a large one.
fn plot(ui: &mut Ui, what: &str, past: &[Option<u32>], least: u32) {
    let top = words::plot_top(past, least);
    ui.label(dim(&words::plot_caption(what, past)));
    let size = Vec2::new(ui.available_width().min(380.0), 44.0);
    let (area, _) = ui.allocate_exact_size(size, Sense::hover());
    let painter = ui.painter();
    // A little room above and below, so that a line at either edge is seen whole.
    let field = Rect::from_min_max(
        Pos2::new(area.left() + SCALE_WIDTH, area.top()),
        area.right_bottom(),
    );
    let inner = field.shrink2(Vec2::new(2.0, 4.0));
    painter.rect_filled(field, CornerRadius::same(2), ember::GROUND);
    // The scale: its top, its middle and nought, each with a faint line across.
    let font = FontId::monospace(9.0);
    // The middle of an odd top is not a whole number, and is left unmarked.
    let middle = top.is_multiple_of(2).then_some(top / 2);
    for (share, mark) in [(0.0, Some(top)), (0.5, middle), (1.0, Some(0))] {
        let y = inner.top() + share * inner.height();
        if let Some(mark) = mark {
            painter.text(
                Pos2::new(field.left() - 5.0, y),
                Align2::RIGHT_CENTER,
                mark.to_string(),
                font.clone(),
                ember::QUIET,
            );
        }
        painter.line_segment(
            [Pos2::new(inner.left(), y), Pos2::new(inner.right(), y)],
            Stroke::new(1.0, ember::PANEL),
        );
    }
    let place = |(x, y): (f32, f32)| {
        Pos2::new(
            inner.left() + x * inner.width(),
            inner.top() + y * inner.height(),
        )
    };
    for run in &words::plot_runs(past, top, KEPT) {
        let points: Vec<Pos2> = run.iter().copied().map(place).collect();
        match points.as_slice() {
            [alone] => {
                painter.circle_filled(*alone, 1.5, ember::EMBER);
            }
            _ => {
                painter.line(points, Stroke::new(1.5, ember::EMBER));
            }
        }
    }
    pointed(ui, past, field, inner);
}

/// Under the pointer: a line down the plot, and the value there, written beside the
/// line on whichever side has the room.
fn pointed(ui: &Ui, past: &[Option<u32>], field: Rect, inner: Rect) {
    let found = ui
        .ctx()
        .pointer_hover_pos()
        .filter(|at| field.contains(*at) && ui.rect_contains_pointer(field))
        .and_then(|at| {
            let across = (at.x - inner.left()) / inner.width().max(1.0);
            words::plot_at(past, KEPT, across).map(|found| (at.x, found))
        });
    let Some((x, (value, ago))) = found else {
        return;
    };
    let painter = ui.painter();
    painter.line_segment(
        [Pos2::new(x, inner.top()), Pos2::new(x, inner.bottom())],
        Stroke::new(1.0, ember::QUIET),
    );
    let (anchor, beside) = if x > field.center().x {
        (Align2::RIGHT_TOP, x - 5.0)
    } else {
        (Align2::LEFT_TOP, x + 5.0)
    };
    let words = words::plot_pointed(value, ago);
    let said = painter.layout_no_wrap(words, FontId::monospace(10.0), ember::TEXT);
    let at = anchor.anchor_size(Pos2::new(beside, field.top() + 2.0), said.size());
    painter.rect_filled(at.expand(2.0), CornerRadius::same(2), ember::PANEL);
    painter.galley(at.min, said, ember::TEXT);
}
