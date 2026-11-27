//! The window.
//!
//! Two screens: outside a meeting (a name, a look at the microphone, start one or join
//! one) and inside (the people round the fire, the way to bring someone in, the sound).
//! The look is "Ember": warm dark, one ember colour for what matters.

use crate::check::{Finding, Found, MicCheck};
use crate::picture::{Coding, Feed, Slot};
use crate::session::{Order, Person, Session, Stage, View};
use crate::show::Screen;
use crate::words;
use eframe::egui::{
    self, Align2, CornerRadius, FontId, Pos2, Rect, RichText, Sense, Stroke, Ui, Vec2,
};
use palaver_media::picture::Size;
use palaver_media::video::Settings;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// The colours of the Ember look.
mod ember {
    use eframe::egui::Color32;

    pub const GROUND: Color32 = Color32::from_rgb(0x1A, 0x16, 0x13);
    pub const PANEL: Color32 = Color32::from_rgb(0x2A, 0x23, 0x1E);
    pub const LINE: Color32 = Color32::from_rgb(0x4A, 0x3F, 0x36);
    pub const TEXT: Color32 = Color32::from_rgb(0xED, 0xE4, 0xD3);
    pub const QUIET: Color32 = Color32::from_rgb(0xB5, 0xA9, 0x94);
    pub const DIM: Color32 = Color32::from_rgb(0x6B, 0x5A, 0x4B);
    pub const EMBER: Color32 = Color32::from_rgb(0xF0, 0x69, 0x2B);
    pub const EMBER_LOW: Color32 = Color32::from_rgb(0x8A, 0x4A, 0x2A);
    pub const GOOD: Color32 = Color32::from_rgb(0x9D, 0xB8, 0x9A);
}

/// How often the window is drawn again when nothing is touched: the meters move.
const REDRAW: Duration = Duration::from_millis(100);

/// A picture of the window to take by itself: for looking at the window without a person
/// at it (a developer's tool, and the way these screens are checked).
#[derive(Debug, Clone)]
pub struct Shot {
    /// Where the PNG goes.
    pub path: String,
    /// If given, start a meeting under this name first, so the picture is of a meeting.
    pub name: Option<String>,
}

/// What the window is opened with.
#[derive(Debug, Clone, Default)]
pub struct Setup {
    pub shot: Option<Shot>,
    /// The name to begin with.
    pub name: Option<String>,
    /// A file to carry the ticket between machines that share a folder and not a
    /// clipboard. Whoever starts a meeting writes its ticket there; whoever opens the
    /// window finds the ticket in the field, ready to join.
    pub ticket_file: Option<String>,
    /// The delays to run with.
    pub tuning: crate::live::Tuning,
    /// Show a moving pattern as this machine's own picture, in place of a camera.
    pub pattern: bool,
    /// With the pattern: also show it as it would be sent, encoded at this many bits a
    /// second and decoded again.
    pub coded: Option<u32>,
}

/// How long the window is left to settle before its picture is taken.
const SETTLE: Duration = Duration::from_secs(3);

/// Whether the details stand beside the meeting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Drawer {
    Shut,
    /// Open in the window as it was, taking their width from the meeting.
    Open,
    /// Open, in a window made wider for them, to be made narrower again.
    OpenWider,
}

/// One picture on the screen: where its pictures come from, and the newest of them as
/// the screen holds it.
struct Tile {
    slot: Arc<Slot>,
    /// The count of the picture on the screen.
    seen: u64,
    texture: Option<egui::TextureHandle>,
    /// Kept from one picture to the next, to save making room each time.
    rgba: Vec<u8>,
    /// The size of the picture on the screen, and how long it took to make ready.
    shown: Option<(u32, u32, Duration)>,
}

impl Tile {
    const fn new(slot: Arc<Slot>) -> Self {
        Self {
            slot,
            seen: 0,
            texture: None,
            rgba: Vec::new(),
            shown: None,
        }
    }

    /// Put the newest picture on the screen, if there is a newer one than is there.
    fn refresh(&mut self, context: &egui::Context) {
        let Some((seen, picture)) = self.slot.newer_than(self.seen) else {
            return;
        };
        self.seen = seen;
        let began = Instant::now();
        picture.to_rgba(&mut self.rgba);
        let size = picture.size();
        let image = egui::ColorImage::from_rgba_unmultiplied(
            [size.width() as usize, size.height() as usize],
            &self.rgba,
        );
        let options = egui::TextureOptions::LINEAR;
        match &mut self.texture {
            Some(texture) => texture.set(image, options),
            None => self.texture = Some(context.load_texture("picture", image, options)),
        }
        self.shown = Some((size.width(), size.height(), began.elapsed()));
    }

    /// Draw the picture `tall` high under the name `name`, if there is one yet.
    fn draw(&mut self, ui: &mut Ui, name: &str, tall: f32) {
        self.refresh(ui.ctx());
        if let (Some(texture), Some((width, height, _))) = (&self.texture, self.shown) {
            let size = Vec2::new(words::tile_width(width, height, tall), tall);
            ui.add(
                egui::Image::new((texture.id(), size))
                    .corner_radius(CornerRadius::same(4))
                    .alt_text(name),
            );
        }
    }
}

/// How large this machine's own pattern is, and how high its picture is drawn.
const PATTERN: (u32, u32) = (640, 360);
const OWN_TALL: f32 = 90.0;
/// How high someone else's picture is drawn.
const FAR_TALL: f32 = 180.0;
/// The rate this machine's picture is coded at for the meeting, unless told otherwise.
const BITRATE: u32 = 500_000;

/// The window's own state: what has been typed, and what is being listened to.
pub struct Window {
    session: Session,
    /// The microphone check; running only outside a meeting.
    check: Option<MicCheck>,
    name: String,
    ticket: String,
    came_back: String,
    ask_outside: bool,
    /// Whether the strength of the noise suppression is left to the ear, so that a
    /// noisy microphone will be given more.
    noise_by_ear: bool,
    /// The meter falls slowly from its last peak, so that it can be read.
    meter: f32,
    copied: Option<&'static str>,
    shot: Option<Shot>,
    /// This machine's own picture, if it has one, and what makes it.
    own: Option<Tile>,
    feed: Option<Feed>,
    /// The same picture after encoding and decoding, and what that costs.
    sent: Option<(Tile, Arc<Coding>)>,
    /// The picture of someone else in the meeting.
    far: Option<Tile>,
    /// Whether the details stand beside the meeting.
    details: Drawer,
    ticket_file: Option<String>,
    /// The ticket last written to the file, so that it is written once.
    written: Option<String>,
    opened: Instant,
    /// Set once the picture is saved: the window then closes.
    shot_done: Arc<AtomicBool>,
    shot_asked: bool,
}

impl Window {
    /// Give `context` the Ember look.
    pub fn dress(context: &egui::Context) {
        let mut visuals = egui::Visuals::dark();
        visuals.panel_fill = ember::GROUND;
        visuals.window_fill = ember::PANEL;
        visuals.extreme_bg_color = ember::PANEL;
        visuals.override_text_color = Some(ember::TEXT);
        visuals.selection.bg_fill = ember::EMBER_LOW;
        visuals.hyperlink_color = ember::EMBER;
        for widget in [
            &mut visuals.widgets.inactive,
            &mut visuals.widgets.hovered,
            &mut visuals.widgets.active,
            &mut visuals.widgets.noninteractive,
        ] {
            widget.bg_fill = ember::PANEL;
            widget.weak_bg_fill = ember::PANEL;
            widget.bg_stroke = Stroke::new(1.0, ember::LINE);
            widget.corner_radius = CornerRadius::same(4);
        }
        visuals.widgets.hovered.bg_stroke = Stroke::new(1.0, ember::EMBER);
        visuals.widgets.active.bg_stroke = Stroke::new(1.5, ember::EMBER);
        context.set_visuals(visuals);
        context.global_style_mut(|style| {
            style.spacing.item_spacing = Vec2::new(10.0, 10.0);
            style.spacing.button_padding = Vec2::new(14.0, 7.0);
        });
    }

    fn new(context: &egui::Context, setup: Setup) -> Self {
        Self::dress(context);
        let (pattern, coded) = (setup.pattern, setup.coded);
        // A new picture is a reason to draw again, at once.
        let waking = |slot: &Slot| {
            let context = context.clone();
            slot.on_new(move || context.request_repaint());
        };
        let camera = pattern.then(|| Arc::new(Slot::default()));
        let screen = Screen::new(camera.clone(), PATTERN.1, coded.unwrap_or(BITRATE));
        waking(&screen.far);
        let far = Arc::clone(&screen.far);
        let session = Session::start(setup.tuning, Some(screen));
        let mut window = Self::over(session, Some(MicCheck::start()), setup);
        window.watch_far(far);
        if let (Some(slot), Some(size)) = (camera, Size::new(PATTERN.0, PATTERN.1)) {
            waking(&slot);
            window.feed = Some(match coded {
                Some(bitrate) => {
                    let (back, coding) = (Arc::new(Slot::default()), Arc::new(Coding::default()));
                    window.watch_sent(Arc::clone(&back), Arc::clone(&coding));
                    let settings = Settings::new(size, bitrate);
                    Feed::coded(Arc::clone(&slot), back, coding, settings, 0)
                }
                None => Feed::pattern(Arc::clone(&slot), size, 0),
            });
            window.watch(slot);
        }
        window
    }

    /// Show what is put into `slot` as this machine's own picture.
    pub fn watch(&mut self, slot: Arc<Slot>) {
        self.own = Some(Tile::new(slot));
    }

    /// Show what is put into `slot` as the picture of someone else, while the session
    /// says whose it is.
    pub fn watch_far(&mut self, slot: Arc<Slot>) {
        self.far = Some(Tile::new(slot));
    }

    /// Show what is put into `slot` beside it, as the picture after encoding and
    /// decoding, with what `coding` says that costs in the details.
    pub fn watch_sent(&mut self, slot: Arc<Slot>, coding: Arc<Coding>) {
        self.sent = Some((Tile::new(slot), coding));
    }

    /// A window over a session that already exists, listening to the microphone or not.
    #[must_use]
    pub fn over(session: Session, check: Option<MicCheck>, setup: Setup) -> Self {
        Self {
            session,
            check,
            name: setup.name.unwrap_or_default(),
            // A ticket left in the file is put in the field, to join by.
            ticket: setup
                .ticket_file
                .as_ref()
                .and_then(|path| std::fs::read_to_string(path).ok())
                .map(|text| text.trim().to_owned())
                .unwrap_or_default(),
            came_back: String::new(),
            ask_outside: true,
            noise_by_ear: setup.tuning.noise.is_none(),
            meter: 0.0,
            copied: None,
            own: None,
            far: None,
            feed: None,
            sent: None,
            // Closed until asked for; open in a picture, so that the picture shows them.
            details: if setup.shot.is_some() {
                Drawer::Open
            } else {
                Drawer::Shut
            },
            shot: setup.shot,
            ticket_file: setup.ticket_file,
            written: None,
            opened: Instant::now(),
            shot_done: Arc::new(AtomicBool::new(false)),
            shot_asked: false,
        }
    }

    /// Leave the meeting's ticket in the file, if one was named, for another machine to
    /// pick up.
    fn leave_ticket(&mut self, view: &View) {
        let (Some(path), Some(ticket)) = (&self.ticket_file, &view.ticket) else {
            return;
        };
        if self.written.as_ref() != Some(ticket) {
            self.written = Some(ticket.clone());
            if let Err(error) = std::fs::write(path, format!("{ticket}\n")) {
                eprintln!("{path}: {error}");
            }
        }
    }

    /// Take the picture that was asked for, once the window has settled, and close.
    fn picture(&mut self, context: &egui::Context) {
        let Some(shot) = self.shot.clone() else {
            return;
        };
        if let Some(name) = shot.name.filter(|_| self.name.is_empty()) {
            self.name = name;
            self.ask_outside = false;
            // With a ticket left in the file, the picture is of that meeting.
            let ticket = Some(self.ticket.trim().to_owned()).filter(|ticket| !ticket.is_empty());
            self.start(ticket);
        }
        if self.shot_done.load(Ordering::Relaxed) {
            context.send_viewport_cmd(egui::ViewportCommand::Close);
        } else if !self.shot_asked && self.opened.elapsed() >= SETTLE {
            self.shot_asked = true;
            let done = Arc::clone(&self.shot_done);
            context.request_screenshot(move |image| {
                let rgba: Vec<u8> = image
                    .pixels
                    .iter()
                    .flat_map(egui::Color32::to_srgba_unmultiplied)
                    .collect();
                match crate::png::encode(image.size[0], image.size[1], &rgba) {
                    Some(png) => {
                        if let Err(error) = std::fs::write(&shot.path, png) {
                            eprintln!("{}: {error}", shot.path);
                        }
                    }
                    None => eprintln!("the picture could not be made"),
                }
                done.store(true, Ordering::Relaxed);
            });
        }
    }

    /// Where the meter stands now that the level is `level`.
    fn metered(&mut self, level: u8) -> f32 {
        self.meter = words::meter_after(self.meter, level);
        self.meter
    }

    fn outside(&mut self, ui: &mut Ui, view: &View) {
        ui.add_space(8.0);
        ui.label(
            RichText::new("PALAVER")
                .size(34.0)
                .strong()
                .color(ember::TEXT),
        );
        ui.label(
            RichText::new("No servers. The people in the meeting are the network.")
                .color(ember::QUIET),
        );
        ui.add_space(18.0);

        // The field is named by its label, for those who are read the screen.
        let label = ui.label(RichText::new("YOUR NAME").small().color(ember::DIM));
        ui.add(egui::TextEdit::singleline(&mut self.name).desired_width(320.0))
            .labelled_by(label.id);

        ui.add_space(8.0);
        ui.label(RichText::new("MICROPHONE").small().color(ember::DIM));
        let (found, level, finding, noisy) =
            self.check
                .as_ref()
                .map_or((Found::Opening, 0, Finding::TooSoon, false), |check| {
                    (
                        check.found(),
                        check.loudest(),
                        check.finding(),
                        check.noisy(),
                    )
                });
        match found {
            Found::Opening => {
                ui.label(RichText::new("opening…").color(ember::QUIET));
            }
            Found::Listening(device) => {
                ui.label(RichText::new(&device).color(ember::QUIET));
                let share = self.metered(level);
                meter(ui, share);
                let colour = match finding {
                    Finding::Good => ember::GOOD,
                    _ if words::is_trouble(finding) => ember::EMBER,
                    _ => ember::DIM,
                };
                ui.label(
                    RichText::new(words::microphone_hint(finding, words::System::HERE))
                        .small()
                        .color(colour),
                );
                settings_link(ui, finding, &device);
                if let Some(hint) = words::noisy_hint(noisy, self.noise_by_ear) {
                    ui.label(RichText::new(hint).small().color(ember::QUIET));
                }
            }
            Found::Failed(why) => {
                ui.label(RichText::new(why).color(ember::EMBER));
            }
        }

        ui.add_space(8.0);
        ui.checkbox(
            &mut self.ask_outside,
            "Find my public address (for people outside this network; asks a public server)",
        );

        ui.add_space(8.0);
        let named = !self.name.trim().is_empty();
        let starting = view.stage == Stage::Starting;
        let start = egui::Button::new(RichText::new("Start a meeting").color(ember::GROUND))
            .fill(ember::EMBER);
        if ui.add_enabled(named && !starting, start).clicked() {
            self.start(None);
        }

        ui.add_space(8.0);
        ui.label(RichText::new("OR JOIN ONE").small().color(ember::DIM));
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.ticket)
                    .hint_text("paste a ticket: palaver:…")
                    .desired_width(320.0),
            );
            let ready = named && !starting && !self.ticket.trim().is_empty();
            if ui.add_enabled(ready, egui::Button::new("Join")).clicked() {
                let ticket = self.ticket.clone();
                self.start(Some(ticket));
            }
        });
        if !named {
            ui.label(RichText::new("A name first.").small().color(ember::DIM));
        }
        if starting {
            ui.label(RichText::new("starting…").color(ember::QUIET));
        }
        if let Some(error) = &view.error {
            ui.label(RichText::new(error).color(ember::EMBER));
        }
    }

    /// The ticket that goes back when nobody answers, from either end.
    fn getting_in(&mut self, ui: &mut Ui, view: &View) {
        if let Some(reply) = &view.reply {
            ui.separator();
            ui.label(RichText::new("Nobody can reach this machine yet.").color(ember::EMBER));
            ui.label(
                RichText::new(
                    "Send this ticket back to whoever invited you. When they paste it in, \
                     their machine reaches for yours.",
                )
                .small()
                .color(ember::QUIET),
            );
            if ui.button("Copy the ticket to send back").clicked() {
                ui.ctx().copy_text(reply.clone());
                self.copied = Some("The ticket to send back is on the clipboard.");
            }
        }

        ui.separator();
        ui.label(
            RichText::new("SOMEONE CANNOT GET IN? PASTE WHAT THEY SENT BACK")
                .small()
                .color(ember::DIM),
        );
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.came_back)
                    .hint_text("palaver:…")
                    .desired_width(320.0),
            );
            if ui
                .add_enabled(
                    !self.came_back.trim().is_empty(),
                    egui::Button::new("Reach"),
                )
                .clicked()
            {
                self.session
                    .order(Order::Reach(std::mem::take(&mut self.came_back)));
            }
        });
        if let Some(notice) = &view.notice {
            ui.label(RichText::new(notice).small().color(ember::QUIET));
        }
    }

    fn start(&mut self, ticket: Option<String>) {
        // The meeting opens the microphone for itself.
        self.check = None;
        self.meter = 0.0;
        self.session.order(Order::Start {
            name: self.name.trim().to_owned(),
            ticket,
            ask_outside: self.ask_outside,
        });
    }

    fn inside(&mut self, ui: &mut Ui, view: &View) {
        ui.horizontal(|ui| {
            ui.label(RichText::new("PALAVER").size(20.0).strong());
            let elapsed = view.since.map_or(0, |since| since.elapsed().as_secs());
            ui.label(
                RichText::new(words::header(view.people.len(), elapsed))
                    .monospace()
                    .color(ember::QUIET),
            );
        });
        ui.separator();

        // The circle leaves room for what is drawn under it.
        let below = if view.far.is_some() && self.far.is_some() {
            220.0 + FAR_TALL
        } else {
            210.0
        };
        let side = ui
            .available_width()
            .min((ui.available_height() - below).max(160.0));
        circle(ui, side, &view.people, view.sound.voices);
        if let (Some(far), Some(name)) = (&mut self.far, &view.far) {
            far.draw(ui, &words::far_name(name), FAR_TALL);
        }
        ui.horizontal(|ui| {
            if let Some(own) = &mut self.own {
                own.draw(ui, "Your picture", OWN_TALL);
            }
            if let Some((sent, _)) = &mut self.sent {
                sent.draw(ui, "Your picture, as sent", OWN_TALL);
            }
        });

        let line = words::links_line(view.links, view.network);
        ui.label(RichText::new(line).small().monospace().color(ember::DIM));

        let share = self.metered(if view.muted { 0 } else { view.sound.loudest });
        meter(ui, share);
        if let Some(warning) = words::microphone_warning(view.sound.microphone, words::System::HERE)
        {
            ui.label(RichText::new(warning).small().color(ember::EMBER));
            settings_link(ui, view.sound.microphone, &view.microphone);
        }

        ui.horizontal(|ui| {
            let label = if view.muted { "Unmute" } else { "Mute" };
            if ui.button(label).clicked() {
                self.session.order(Order::Mute(!view.muted));
            }
            if let Some(ticket) = &view.ticket {
                let bring =
                    egui::Button::new(RichText::new("Bring someone in").color(ember::GROUND))
                        .fill(ember::EMBER);
                if ui.add(bring).clicked() {
                    ui.ctx().copy_text(ticket.clone());
                    self.copied = Some("The ticket is on the clipboard. Send it to them.");
                }
            }
            if ui.button("Details").clicked() {
                self.toggle_details(ui.ctx());
            }
            if ui.button("Leave").clicked() {
                if self.details != Drawer::Shut {
                    self.toggle_details(ui.ctx());
                }
                self.session.order(Order::Leave);
                self.copied = None;
                self.check = Some(MicCheck::start());
            }
        });
        if let Some(copied) = self.copied {
            ui.label(RichText::new(copied).small().color(ember::GOOD));
        }

        self.getting_in(ui, view);
    }

    /// Open the details beside the meeting, or close them. Where there is room on the
    /// screen the window grows by their width, so that the meeting keeps its own, and
    /// shrinks again; where there is not, they take their width from the meeting.
    fn toggle_details(&mut self, context: &egui::Context) {
        let opening = self.details == Drawer::Shut;
        let grew = std::mem::replace(
            &mut self.details,
            if opening { Drawer::Open } else { Drawer::Shut },
        ) == Drawer::OpenWider;
        let (inner, screen, fills) = context.input(|input| {
            let viewport = input.viewport();
            (
                viewport.inner_rect,
                viewport.monitor_size.map(|size| size.x),
                viewport.maximized.unwrap_or(false) || viewport.fullscreen.unwrap_or(false),
            )
        });
        let Some(inner) = inner else {
            return;
        };
        let width = if opening {
            let grown = words::grown_width(inner.width(), DETAILS_WIDTH, screen, fills);
            if grown.is_some() {
                self.details = Drawer::OpenWider;
            }
            grown
        } else {
            grew.then(|| (inner.width() - DETAILS_WIDTH).max(MIN_WIDTH))
        };
        if let Some(width) = width {
            context.send_viewport_cmd(egui::ViewportCommand::InnerSize(Vec2::new(
                width,
                inner.height(),
            )));
        }
    }

    /// The details: this machine, each link and each voice, as they are now and as they
    /// have been over the last two minutes.
    fn details(ui: &mut Ui, view: &View, pictures: Vec<String>) {
        let small = |text: String| RichText::new(text).small().monospace().color(ember::QUIET);
        let heading = |ui: &mut Ui, text: &str| {
            ui.add_space(6.0);
            ui.label(RichText::new(text).small().color(ember::DIM));
        };
        ui.label(RichText::new("DETAILS").strong().color(ember::TEXT));
        ui.label(
            RichText::new("Each line is drawn over the last two minutes.")
                .small()
                .color(ember::DIM),
        );

        heading(ui, "THIS MACHINE");
        ui.label(small(format!("{} → {}", view.microphone, view.speaker)));
        ui.label(small(words::sound_line(
            view.details.sound_gap,
            view.details.sound_burst,
            view.sound.starved,
            view.sound.dropped,
        )));
        ui.label(small(words::network_line(view.details.network_late)));
        if let Some(line) = words::noise_line(view.sound.noise_high) {
            ui.label(small(String::from(line)));
        }
        for line in pictures {
            ui.label(small(line));
        }
        plot(
            ui,
            "sound thread's longest wait, ms",
            &view.details.sound_gap_past,
            50,
        );

        heading(ui, "LINKS");
        for link in &view.details.links {
            ui.label(small(words::link_line(
                &link.name, link.rtt, link.sent, link.lost,
            )));
            plot(ui, "round trip, ms", &link.rtt_past, 50);
            plot(ui, "packets lost a second", &link.lost_past, 5);
        }
        if view.details.links.is_empty() {
            ui.label(small(String::from("no links yet")));
        }

        heading(ui, "VOICES");
        for voice in &view.details.voices {
            let line = words::voice_line(
                &voice.name,
                voice.delay,
                view.details.speaker_lead,
                &voice.stats,
            );
            ui.label(small(words::resting(line, voice.playing)));
            plot(ui, "heard after arriving, ms", &voice.delay_past, 100);
            plot(ui, "frames lost or late a second", &voice.trouble_past, 5);
        }
        if view.details.voices.is_empty() {
            ui.label(small(String::from("no voices are being played")));
        }
    }
}

/// How wide the details are, beside the meeting.
const DETAILS_WIDTH: f32 = 400.0;
/// The least width the window is given.
const MIN_WIDTH: f32 = 420.0;

/// How much of a plot's width is kept for the numbers of its scale.
const SCALE_WIDTH: f32 = 34.0;

/// One number over the last two minutes: a caption, and under it a line against a scale,
/// with the value under the pointer said beside it. `least` is the least the top of the
/// scale may be, so that a small number is not drawn as a large one.
fn plot(ui: &mut Ui, what: &str, past: &[Option<u32>], least: u32) {
    let top = words::plot_top(past, least);
    ui.label(
        RichText::new(words::plot_caption(what, past))
            .small()
            .color(ember::DIM),
    );
    let size = Vec2::new(ui.available_width().min(360.0), 44.0);
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
    let runs = words::plot_runs(past, top, crate::history::KEPT);
    for run in &runs {
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
    // Under the pointer: a line down the plot, and the value there, written beside the
    // line on whichever side has the room.
    let pointed = ui
        .ctx()
        .pointer_hover_pos()
        .filter(|at| field.contains(*at) && ui.rect_contains_pointer(field))
        .and_then(|at| {
            let across = (at.x - inner.left()) / inner.width().max(1.0);
            words::plot_at(past, crate::history::KEPT, across).map(|found| (at.x, found))
        });
    if let Some((x, (value, ago))) = pointed {
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
}

/// Open the system's settings at one of the addresses the words give.
fn open_settings(link: words::SettingsLink) {
    let opener = match words::System::HERE {
        words::System::Windows => "explorer.exe",
        words::System::Mac => "open",
        words::System::Other => return,
    };
    if let Err(error) = std::process::Command::new(opener).arg(link.address).spawn() {
        eprintln!("{opener}: {error}");
    }
}

/// How to put right a microphone found wanting: the steps, with the link into the
/// system's settings among their words; or where there are no steps to give, the link
/// alone, where the system has an address for its settings.
fn settings_link(ui: &mut Ui, finding: Finding, device: &str) {
    let here = words::System::HERE;
    if let Some(steps) = words::microphone_steps(finding, here, device) {
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            ui.label(RichText::new(steps.before).small().color(ember::QUIET));
            if ui.link(RichText::new(steps.link.label).small()).clicked() {
                open_settings(steps.link);
            }
            ui.label(RichText::new(steps.after).small().color(ember::QUIET));
        });
    } else if let Some(link) = words::settings_for(finding, here)
        && ui.link(RichText::new(link.label).small()).clicked()
    {
        open_settings(link);
    }
}

/// A level meter: a dark bar with an ember fill.
fn meter(ui: &mut Ui, share: f32) {
    let (bar, _) = ui.allocate_exact_size(Vec2::new(320.0, 8.0), Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(bar, CornerRadius::same(2), ember::PANEL);
    let lit = Rect::from_min_size(bar.min, Vec2::new(bar.width() * share, bar.height()));
    painter.rect_filled(lit, CornerRadius::same(2), ember::EMBER);
}

/// The people in a circle round the fire, which burns higher the more voices are heard.
#[expect(
    clippy::cast_precision_loss,
    reason = "a count of people, as an angle and a size on the screen"
)]
fn circle(ui: &mut Ui, side: f32, people: &[Person], voices: usize) {
    let (area, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), side), Sense::hover());
    let painter = ui.painter();
    let centre = area.center();
    let ring = side * 0.36;
    let disc = (side * 0.085).clamp(18.0, 34.0);

    // The fire.
    let fire = 6.0 + 5.0 * voices.min(4) as f32;
    painter.circle_filled(centre, fire + 6.0, ember::EMBER_LOW);
    painter.circle_filled(centre, fire, ember::EMBER);

    if people.is_empty() {
        painter.text(
            centre + Vec2::new(0.0, fire + 26.0),
            Align2::CENTER_CENTER,
            "getting in…",
            FontId::proportional(14.0),
            ember::QUIET,
        );
        return;
    }
    for (place, person) in people.iter().enumerate() {
        let (across, down) = words::seat(place, people.len());
        let at = Pos2::new(centre.x + ring * across, centre.y + ring * down);
        let edge = if person.me {
            Stroke::new(2.0, ember::EMBER)
        } else if person.linked {
            Stroke::new(1.5, ember::QUIET)
        } else {
            Stroke::new(1.0, ember::LINE)
        };
        painter.circle(at, disc, ember::PANEL, edge);
        painter.text(
            at,
            Align2::CENTER_CENTER,
            words::initials(&person.name),
            FontId::proportional(disc * 0.7),
            ember::TEXT,
        );
        let label = if person.me {
            "You"
        } else {
            person.name.as_str()
        };
        painter.text(
            at + Vec2::new(0.0, disc + 12.0),
            Align2::CENTER_CENTER,
            label,
            FontId::proportional(13.0),
            ember::QUIET,
        );
    }
    if people.len() == 1 {
        painter.text(
            centre + Vec2::new(0.0, fire + 26.0),
            Align2::CENTER_CENTER,
            "nobody else is here yet",
            FontId::proportional(14.0),
            ember::QUIET,
        );
    }
}

impl Window {
    /// Draw the window as things stand, and act on what was clicked.
    pub fn draw(&mut self, ui: &mut Ui) {
        let view = self.session.view();
        self.leave_ticket(&view);
        let frame = egui::Frame::new()
            .fill(ember::GROUND)
            .inner_margin(egui::Margin::symmetric(28, 20));
        let own = self
            .own
            .as_ref()
            .and_then(|tile| tile.shown)
            .map(|(width, height, cost)| words::picture_line(width, height, cost));
        let sent = self.sent.as_ref().map(|(_, coding)| {
            let cost = coding.cost();
            cost.failed.map_or_else(
                || words::coding_line(cost.bitrate, cost.encode, cost.decode, cost.let_go),
                |why| format!("as sent: {why}"),
            )
        });
        let mut pictures: Vec<String> = own.into_iter().chain(sent).collect();
        if self.own.is_some() {
            pictures.push(words::picture_sent_line(view.details.picture_sent));
        }
        if let Some(name) = &view.far {
            pictures.push(words::picture_shown_line(name, view.details.picture_shown));
        }
        if let Some(why) = &view.details.picture_trouble {
            pictures.push(format!("pictures: {why}"));
        }
        if self.details != Drawer::Shut && view.stage == Stage::In {
            let beside = egui::Frame::new()
                .fill(ember::PANEL)
                .inner_margin(egui::Margin::symmetric(16, 20));
            // In a window that could not grow for them, they leave the meeting some room.
            let width = DETAILS_WIDTH.min((ui.available_width() - 240.0).max(200.0));
            egui::Panel::right("details")
                .exact_size(width)
                .resizable(false)
                .frame(beside)
                .show(ui, |ui| {
                    egui::ScrollArea::vertical()
                        .id_salt("details")
                        .show(ui, |ui| Self::details(ui, &view, pictures));
                });
        }
        egui::CentralPanel::default().frame(frame).show(ui, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| match view.stage {
                Stage::Out | Stage::Starting => self.outside(ui, &view),
                Stage::In => self.inside(ui, &view),
            });
        });
        self.picture(ui.ctx());
    }
}

impl eframe::App for Window {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        self.draw(ui);
        // The meters move whether or not anything is touched.
        ui.ctx().request_repaint_after(REDRAW);
    }
}

/// Open the window and run until it is closed.
///
/// # Errors
///
/// When the system gives no window: no display, or no way to draw on it.
///
/// With a shot in `setup`, the window takes a picture of itself after a few seconds and
/// closes.
pub fn run(setup: Setup) -> Result<(), String> {
    // A picture shows the details open, and is wider to hold them.
    let width = if setup.shot.is_some() {
        520.0 + DETAILS_WIDTH
    } else {
        520.0
    };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Palaver")
            .with_inner_size([width, 720.0])
            .with_min_inner_size([MIN_WIDTH, 560.0]),
        ..eframe::NativeOptions::default()
    };
    eframe::run_native(
        "Palaver",
        options,
        Box::new(move |creation| Ok(Box::new(Window::new(&creation.egui_ctx, setup)))),
    )
    .map_err(|error| error.to_string())
}
