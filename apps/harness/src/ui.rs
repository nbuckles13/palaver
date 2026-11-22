//! The window.
//!
//! Two screens: outside a meeting (a name, a look at the microphone, start one or join
//! one) and inside (the people round the fire, the way to bring someone in, the sound).
//! The look is "Ember": warm dark, one ember colour for what matters.

use crate::check::{Found, MicCheck};
use crate::session::{Order, Person, Session, Stage, View};
use eframe::egui::{
    self, Align2, CornerRadius, FontId, Pos2, Rect, RichText, Sense, Stroke, Ui, Vec2,
};
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

/// The quietest and the loudest level the meter shows, on the scale of
/// [`palaver_media::level`]: above the hiss of a quiet room, and close to full scale.
const METER_FLOOR: u8 = 72;
const METER_TOP: u8 = 120;

/// A picture of the window to take by itself: for looking at the window without a person
/// at it (a developer's tool, and the way these screens are checked).
#[derive(Debug, Clone)]
pub struct Shot {
    /// Where the PNG goes.
    pub path: String,
    /// If given, start a meeting under this name first, so the picture is of a meeting.
    pub name: Option<String>,
}

/// How long the window is left to settle before its picture is taken.
const SETTLE: Duration = Duration::from_secs(3);

/// The window's own state: what has been typed, and what is being listened to.
pub struct Window {
    session: Session,
    /// The microphone check; running only outside a meeting.
    check: Option<MicCheck>,
    name: String,
    ticket: String,
    came_back: String,
    ask_outside: bool,
    /// The meter falls slowly from its last peak, so that it can be read.
    meter: f32,
    copied: Option<&'static str>,
    shot: Option<Shot>,
    opened: Instant,
    /// Set once the picture is saved: the window then closes.
    shot_done: Arc<AtomicBool>,
    shot_asked: bool,
}

impl Window {
    fn new(context: &egui::Context, shot: Option<Shot>) -> Self {
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
        Self {
            session: Session::start(),
            check: Some(MicCheck::start()),
            name: String::new(),
            ticket: String::new(),
            came_back: String::new(),
            ask_outside: true,
            meter: 0.0,
            copied: None,
            shot,
            opened: Instant::now(),
            shot_done: Arc::new(AtomicBool::new(false)),
            shot_asked: false,
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
            self.start(None);
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

    /// A level as a share of the meter, with a slow fall from the last peak.
    fn metered(&mut self, level: u8) -> f32 {
        let span = f32::from(METER_TOP.saturating_sub(METER_FLOOR));
        let now = f32::from(level.saturating_sub(METER_FLOOR)) / span;
        self.meter = now.clamp(0.0, 1.0).max(self.meter - 0.06);
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

        ui.label(RichText::new("YOUR NAME").small().color(ember::DIM));
        ui.add(egui::TextEdit::singleline(&mut self.name).desired_width(320.0));

        ui.add_space(8.0);
        ui.label(RichText::new("MICROPHONE").small().color(ember::DIM));
        let (found, level) = self.check.as_ref().map_or((Found::Opening, 0), |check| {
            (check.found(), check.loudest())
        });
        match found {
            Found::Opening => {
                ui.label(RichText::new("opening…").color(ember::QUIET));
            }
            Found::Listening(device) => {
                ui.label(RichText::new(device).color(ember::QUIET));
                let share = self.metered(level);
                meter(ui, share);
                ui.label(
                    RichText::new("Say something. The bar should move.")
                        .small()
                        .color(ember::DIM),
                );
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
            let here = view.people.len().max(1);
            let elapsed = view.since.map_or(0, |since| since.elapsed().as_secs());
            ui.label(
                RichText::new(format!(
                    "{here:02} here · {:02}:{:02}:{:02}",
                    elapsed / 3600,
                    elapsed / 60 % 60,
                    elapsed % 60
                ))
                .monospace()
                .color(ember::QUIET),
            );
        });
        ui.separator();

        let side = ui
            .available_width()
            .min((ui.available_height() - 210.0).max(160.0));
        circle(ui, side, &view.people, view.sound.voices);

        let mut line = match view.links {
            0 => String::from("no links yet · no server"),
            1 => String::from("1 link · direct · no server"),
            links => format!("{links} links · direct · no server"),
        };
        if let Some(network) = view.network.filter(|network| *network != "unknown") {
            line.push_str(" · ");
            line.push_str(network);
        }
        ui.label(RichText::new(line).small().monospace().color(ember::DIM));

        let share = self.metered(if view.muted { 0 } else { view.sound.loudest });
        meter(ui, share);

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
            if ui.button("Leave").clicked() {
                self.session.order(Order::Leave);
                self.copied = None;
                self.check = Some(MicCheck::start());
            }
        });
        if let Some(copied) = self.copied {
            ui.label(RichText::new(copied).small().color(ember::GOOD));
        }

        self.getting_in(ui, view);
        ui.label(
            RichText::new(format!("{} → {}", view.microphone, view.speaker))
                .small()
                .monospace()
                .color(ember::DIM),
        );
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

/// The first two letters of a name, as they stand on a person's disc.
fn initials(name: &str) -> String {
    let mut letters = name.trim().chars();
    let first: String = letters
        .next()
        .into_iter()
        .flat_map(char::to_uppercase)
        .collect();
    let second: String = letters.next().into_iter().collect();
    format!("{first}{second}")
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
        // The first sits at the top; the rest go round clockwise.
        let angle = std::f32::consts::TAU * place as f32 / people.len() as f32
            - std::f32::consts::FRAC_PI_2;
        let at = Pos2::new(centre.x + ring * angle.cos(), centre.y + ring * angle.sin());
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
            initials(&person.name),
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

impl eframe::App for Window {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let view = self.session.view();
        let frame = egui::Frame::new()
            .fill(ember::GROUND)
            .inner_margin(egui::Margin::symmetric(28, 20));
        egui::CentralPanel::default().frame(frame).show(ui, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| match view.stage {
                Stage::Out | Stage::Starting => self.outside(ui, &view),
                Stage::In => self.inside(ui, &view),
            });
        });
        self.picture(ui.ctx());
        ui.ctx().request_repaint_after(REDRAW);
    }
}

/// Open the window and run until it is closed.
///
/// # Errors
///
/// When the system gives no window: no display, or no way to draw on it.
///
/// With `shot`, the window takes a picture of itself after a few seconds and closes.
pub fn run(shot: Option<Shot>) -> Result<(), String> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Palaver")
            .with_inner_size([520.0, 720.0])
            .with_min_inner_size([420.0, 560.0]),
        ..eframe::NativeOptions::default()
    };
    eframe::run_native(
        "Palaver",
        options,
        Box::new(move |creation| Ok(Box::new(Window::new(&creation.egui_ctx, shot)))),
    )
    .map_err(|error| error.to_string())
}
