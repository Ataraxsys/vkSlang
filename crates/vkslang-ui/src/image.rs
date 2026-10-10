//! The Picture page and the "Set up this game" assistant.
//!
//! One large view (the result on the screen, or the captured game with its
//! frame and grid) and three short cards: the game, its shape, its size.
//! Opening the page analyses the game by itself: a full-resolution capture
//! gives where it is and how big its pixels are. Every change is applied
//! live.

use crate::detect::{self, Pixels};
use crate::i18n::Lang;
use crate::plan::{parse_area, Plan, Shape, Size};
use crate::{App, SourceEdit};
use eframe::egui;
use std::time::{Duration, Instant};
use vkslang_ipc::{Request, SourceSize};

/// A full-resolution picture of the game, before the preset.
pub struct Capture {
    pub texture: egui::TextureHandle,
    pub size: [u32; 2],
    /// Size of the output it was taken from.
    pub base: [u32; 2],
    pub rgba: Vec<u8>,
}

/// What the large view shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum View {
    /// The screen as it will look.
    Result,
    /// The captured game, to correct the frame or check the grid.
    Capture,
}

/// What dragging on the captured game does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tool {
    /// Moves or resizes the game's frame.
    Frame,
    /// Shows the pixel grid; dragging shifts it onto the game's blocks.
    Grid,
}

/// How sure the analysis is.
#[derive(Debug, Clone, PartialEq)]
enum Found {
    /// Zone and pixels measured.
    Measured,
    /// The zone is known, the pixels are not (smoothed picture).
    ZoneOnly,
    /// Nothing usable (black picture, no capture).
    Nothing,
}

const STEPS: usize = 4;

pub struct ImageState {
    pub capture: Option<Capture>,
    /// Last capture published by the layer that was read, even if it could
    /// not be: a broken file is not read again on every repaint.
    read_id: Option<u64>,
    requested: Option<Instant>,
    pub plan: Option<Plan>,
    /// Analyse the next capture as soon as it arrives.
    analyse_next: bool,
    found: Option<Found>,
    view: View,
    tool: Tool,
    /// Which edges of the frame the current drag moves (none: all of it).
    drag: Option<[bool; 4]>,
    /// Zoom of the captured game; 0 fits it.
    zoom: f32,
    lock: bool,
    /// Current step of the assistant, when open.
    pub wizard: Option<usize>,
    search: String,
    profile_name: String,
    star: bool,
}

impl Default for ImageState {
    fn default() -> Self {
        ImageState {
            capture: None,
            read_id: None,
            requested: None,
            plan: None,
            analyse_next: true,
            found: None,
            view: View::Result,
            tool: Tool::Frame,
            drag: None,
            zoom: 0.0,
            lock: true,
            wizard: None,
            search: String::new(),
            profile_name: String::new(),
            star: true,
        }
    }
}

/// Reads the raw capture written by the layer.
fn read_capture(path: &str) -> Option<(Vec<u8>, [u32; 2])> {
    let data = std::fs::read(path).ok()?;
    if data.len() < 12 || data[..4] != vkslang_ipc::CAPTURE_MAGIC {
        return None;
    }
    let width = u32::from_le_bytes(data[4..8].try_into().ok()?);
    let height = u32::from_le_bytes(data[8..12].try_into().ok()?);
    let len = width as usize * height as usize * 4;
    Some((data.get(12..12 + len)?.to_vec(), [width, height]))
}

/// `8` or `8.33`: whole numbers without decimals.
fn num(v: f32) -> String {
    if (v - v.round()).abs() < 0.005 {
        format!("{}", v.round())
    } else {
        format!("{v:.2}")
    }
}

/// A large selectable block: a title and one line saying what it does.
fn choice(ui: &mut egui::Ui, selected: bool, title: &str, detail: &str) -> bool {
    let visuals = ui.visuals();
    let fill = if selected { visuals.selection.bg_fill } else { visuals.faint_bg_color };
    let stroke = if selected { visuals.selection.stroke } else { visuals.widgets.noninteractive.bg_stroke };
    let response = egui::Frame::new()
        .fill(fill)
        .stroke(stroke)
        .corner_radius(6.0)
        .inner_margin(egui::Margin::symmetric(10, 6))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.strong(title);
            ui.weak(detail);
        })
        .response
        .interact(egui::Sense::click());
    response.on_hover_cursor(egui::CursorIcon::PointingHand).clicked()
}

/// A card: a numbered title over its contents.
fn card(ui: &mut egui::Ui, number: &str, title: &str, add: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::group(ui.style()).corner_radius(8.0).inner_margin(10).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new(number).strong().size(18.0).color(ui.visuals().selection.bg_fill));
            ui.label(egui::RichText::new(title).strong().size(16.0));
        });
        ui.add_space(4.0);
        add(ui);
    });
    ui.add_space(6.0);
}

impl App {
    fn screen(&self) -> Option<[u32; 2]> {
        self.state.as_ref().and_then(|s| s.outputs.first()).map(|o| o.size)
    }

    pub fn request_capture(&mut self) {
        self.image.requested = Some(Instant::now());
        // Full resolution: a downscaled picture blends the game's pixels and
        // hides their grid.
        self.send(Request::Capture { max_width: 8192 });
    }

    /// Takes a new picture and analyses it as soon as it arrives.
    fn analyse(&mut self) {
        self.image.analyse_next = true;
        self.image.found = None;
        self.request_capture();
    }

    /// Picks up a capture published by the layer.
    pub fn poll_capture(&mut self, ctx: &egui::Context) {
        let Some(c) = self.state.as_ref().and_then(|s| s.capture.clone()) else { return };
        if self.image.read_id == Some(c.id) {
            return;
        }
        self.image.read_id = Some(c.id);
        let Some((rgba, size)) = read_capture(&c.path) else {
            self.error(self.lang.t("capture illisible", "unreadable capture"));
            return;
        };
        let image = egui::ColorImage::from_rgba_unmultiplied([size[0] as usize, size[1] as usize], &rgba);
        let texture = ctx.load_texture("vkslang-capture", image, egui::TextureOptions::NEAREST);
        self.image.capture = Some(Capture { texture, size, base: c.base, rgba });
        if self.image.plan.is_none() {
            self.image.plan = self.plan_from_settings();
        }
        if std::mem::take(&mut self.image.analyse_next) {
            self.detect();
        }
    }

    /// The plan the current settings amount to, as a starting point.
    fn plan_from_settings(&self) -> Option<Plan> {
        let screen = self.screen()?;
        let s = &self.state.as_ref()?.source;
        // An explicit area is where it says; `4:3` and the like are centred,
        // and the layer reports their size.
        let zone = parse_area(&s.rect).unwrap_or_else(|| {
            let [w, h] =
                self.state.as_ref().and_then(|s| s.outputs.first()).map_or(screen, |o| o.picture).map(|v| v.max(1));
            [(screen[0].saturating_sub(w)) / 2, (screen[1].saturating_sub(h)) / 2, w, h]
        });
        let pixel = match s.res {
            SourceSize::Fixed { size: [w, h] } => [zone[2] as f32 / w as f32, zone[3] as f32 / h as f32],
            SourceSize::Divide { by } => [by, by],
            SourceSize::Native => [1.0, 1.0],
        };
        let in_place = s.display == s.rect || s.display == "full" && s.rect == "full";
        Some(Plan {
            zone,
            pixel,
            dup: s.duplicate,
            shape: Shape::AsShown,
            size: if in_place { Size::InPlace } else { Size::Fit },
        })
    }

    /// Sends the plan to the layer, and keeps the advanced settings in step.
    fn apply_plan(&mut self) {
        let (Some(plan), Some(screen), Some(state)) = (self.image.plan, self.screen(), self.state.as_ref()) else {
            return;
        };
        let source = plan.settings(screen, &state.source);
        self.source = Some(SourceEdit::from(&source));
        self.send(Request::SetSource { source });
    }

    /// Finds the game on the capture: its zone, then its pixel grid.
    fn detect(&mut self) {
        let Some(c) = self.image.capture.as_ref() else {
            self.image.found = Some(Found::Nothing);
            return;
        };
        let pixels = Pixels { data: &c.rgba, width: c.size[0], height: c.size[1] };
        let full_size = c.size == c.base;
        let Some(lit) = detect::content_bounds(&pixels) else {
            self.image.found = Some(Found::Nothing);
            return;
        };
        let scale = c.base[0] as f32 / c.size[0] as f32;
        let mut zone = lit.map(|v| (v as f32 * scale).round() as u32);
        // A downscaled capture blends pixels: the grid cannot be read there.
        let grid =
            if full_size { detect::pitch(&pixels, lit, true).zip(detect::pitch(&pixels, lit, false)) } else { None };
        // A coarse grid on a picture with few details is divided down to a
        // resolution of the time; its lines are still on the game's grid.
        let grid = grid.map(|(mut h, mut v)| {
            for (pitch, length, horizontal) in [(&mut h, lit[2], true), (&mut v, lit[3], false)] {
                let size = detect::refine_to_standard(pitch.size, length, horizontal);
                pitch.offset = pitch.offset.rem_euclid(size);
                pitch.size = size;
            }
            (h, v)
        });
        let base = c.base;
        let previous = self.image.plan;
        let mut plan =
            previous.unwrap_or(Plan { zone, pixel: [1.0, 1.0], dup: [1, 1], shape: Shape::Crt, size: Size::Integer });
        match grid {
            Some((h, v)) if h.size > 1.0 || v.size > 1.0 => {
                plan.pixel = [h.size, v.size];
                // Start on the grid and keep only whole game pixels.
                for (axis, pitch) in [(0, h), (1, v)] {
                    let offset = pitch.offset.round().min(zone[axis + 2] as f32 - 1.0).max(0.0);
                    let count = ((zone[axis + 2] as f32 - offset) / pitch.size).floor().max(1.0);
                    zone[axis] += offset as u32;
                    zone[axis + 2] = (count * pitch.size).round() as u32;
                }
                // The game's own black borders are invisible against the bars.
                plan.zone = detect::grow_to_standard(zone, plan.pixel, base);
                self.image.found = Some(Found::Measured);
            }
            _ => {
                plan.zone = zone;
                self.image.found = Some(Found::ZoneOnly);
            }
        }
        // A first analysis picks the look most retro games want.
        if previous.is_none_or(|p| p.shape == Shape::AsShown && p.size == Size::InPlace) {
            plan.shape = Shape::Crt;
            plan.size = Size::Integer;
        }
        self.image.plan = Some(plan);
        self.apply_plan();
    }

    /// One sentence on what reaches the screen.
    fn summary(&self) -> Option<String> {
        let (plan, screen) = (self.image.plan?, self.screen()?);
        let [gw, gh] = plan.game();
        let [iw, ih] = plan.input();
        let [_, _, dw, dh] = plan.display(screen);
        let (sx, sy) = (dw as f32 / iw as f32, dh as f32 / ih as f32);
        let uneven = (sx - sx.round()).abs() > 0.01 || (sy - sy.round()).abs() > 0.01;
        let dup = plan.dup != [1, 1];
        Some(match self.lang {
            Lang::Fr => {
                let mut s = format!("Le jeu fait {gw} × {gh}.");
                if dup {
                    s += &format!(" Dupliqué, le shader travaille sur {iw} × {ih}.");
                }
                s += &format!(" Chaque pixel devient {} × {} pixels d'écran : image de {dw} × {dh}.", num(sx), num(sy));
                if uneven {
                    s += " Pixels de tailles inégales : choisis « Nette » pour des pixels identiques.";
                }
                s
            }
            Lang::En => {
                let mut s = format!("The game is {gw} × {gh}.");
                if dup {
                    s += &format!(" Duplicated, the shader works on {iw} × {ih}.");
                }
                s += &format!(" Each pixel becomes {} × {} screen pixels: a {dw} × {dh} picture.", num(sx), num(sy));
                if uneven {
                    s += " Pixels of uneven sizes: pick \"Sharp\" for identical pixels.";
                }
                s
            }
        })
    }

    // ------------------------------------------------------------- cards

    /// Card 1: what the game is.
    fn card_game(&mut self, ui: &mut egui::Ui) {
        let l = self.lang;
        let elapsed = self.image.requested.map(|t| t.elapsed());
        let waiting = self.image.analyse_next && elapsed.is_some_and(|e| e < Duration::from_secs(3));
        let stuck = self.image.analyse_next && elapsed.is_some_and(|e| e >= Duration::from_secs(3));
        let found = self.image.found.clone();
        let plan = self.image.plan;
        let mut analyse = false;
        card(ui, "1", l.t("Le jeu", "The game"), |ui| {
            match (&found, plan) {
                _ if waiting => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(l.t("Analyse de l'image…", "Analysing the picture…"));
                    });
                }
                _ if stuck => {
                    ui.colored_label(
                        egui::Color32::from_rgb(255, 170, 60),
                        l.t(
                            "Pas d'image : il faut un preset chargé et « Shader actif » coché.",
                            "No picture: a preset must be loaded and \"Shader on\" ticked.",
                        ),
                    );
                }
                (Some(Found::Measured), Some(plan)) => {
                    let [w, h] = plan.game();
                    ui.label(egui::RichText::new(format!("✔ {w} × {h}")).size(22.0).strong());
                    ui.weak(match l {
                        Lang::Fr => format!(
                            "pixels de {} × {} à l'écran, mesurés sur l'image",
                            num(plan.pixel[0]),
                            num(plan.pixel[1])
                        ),
                        Lang::En => format!(
                            "pixels of {} × {} on screen, measured on the picture",
                            num(plan.pixel[0]),
                            num(plan.pixel[1])
                        ),
                    });
                }
                (Some(Found::ZoneOnly), Some(plan)) => {
                    let [w, h] = plan.game();
                    ui.label(egui::RichText::new(format!("⚠ {w} × {h} ?")).size(22.0).strong());
                    ui.weak(l.t(
                        "Le jeu est trouvé, mais pas ses pixels (image lissée ou trop simple). Indique sa \
                         résolution dans « Corriger » ou vérifie la grille.",
                        "The game is found, not its pixels (smoothed or too plain a picture). Give its resolution \
                         under \"Correct\" or check the grid.",
                    ));
                }
                (Some(Found::Nothing), _) => {
                    ui.weak(l.t(
                        "Image noire : va sur un écran du jeu bien rempli et réanalyse.",
                        "Black picture: go to a busy screen of the game and analyse again.",
                    ));
                }
                (_, Some(plan)) => {
                    let [w, h] = plan.game();
                    ui.label(egui::RichText::new(format!("{w} × {h}")).size(22.0).strong());
                    ui.weak(l.t("d'après les réglages actuels", "from the current settings"));
                }
                (_, None) => {
                    ui.weak(l.t("Pas encore analysé.", "Not analysed yet."));
                }
            }
            ui.add_space(4.0);
            analyse = ui
                .button(l.t("🔍 Analyser l'image", "🔍 Analyse the picture"))
                .on_hover_text(l.t(
                    "Reprend une photo du jeu et retrouve sa zone et ses pixels. Utile après un changement de mode.",
                    "Takes a new picture of the game and finds its zone and pixels again. Useful after a mode change.",
                ))
                .clicked();
            egui::CollapsingHeader::new(l.t("Corriger", "Correct"))
                .id_salt("correct-game")
                .show(ui, |ui| self.correct_game(ui));
        });
        if analyse {
            self.analyse();
        }
    }

    /// Fine corrections of what the analysis found.
    fn correct_game(&mut self, ui: &mut egui::Ui) {
        let l = self.lang;
        let Some(mut plan) = self.image.plan else { return };
        let before = plan;
        ui.horizontal_wrapped(|ui| {
            ui.label(l.t("Résolution", "Resolution"));
            let mut game = plan.game();
            let mut edited = ui.add(egui::DragValue::new(&mut game[0]).range(1..=4096)).changed();
            ui.label("×");
            edited |= ui.add(egui::DragValue::new(&mut game[1]).range(1..=4096)).changed();
            if edited {
                plan.pixel = [plan.zone[2] as f32 / game[0] as f32, plan.zone[3] as f32 / game[1] as f32];
            }
        });
        ui.horizontal_wrapped(|ui| {
            for (w, h) in [(320, 200), (320, 240), (640, 200), (640, 400), (640, 480)] {
                if ui.small_button(format!("{w}×{h}")).clicked() {
                    plan.pixel = [plan.zone[2] as f32 / w as f32, plan.zone[3] as f32 / h as f32];
                }
            }
        });
        ui.horizontal_wrapped(|ui| {
            ui.label(l.t("Pixel à l'écran", "Pixel on screen"));
            let start = plan.pixel;
            for (axis, label) in [(0usize, "↔ "), (1usize, "↕ ")] {
                ui.add(egui::DragValue::new(&mut plan.pixel[axis]).speed(0.01).range(0.5..=64.0).prefix(label));
            }
            if self.image.lock && start != plan.pixel {
                if start[0] != plan.pixel[0] {
                    plan.pixel[1] = start[1] * plan.pixel[0] / start[0];
                } else {
                    plan.pixel[0] = start[0] * plan.pixel[1] / start[1];
                }
            }
            ui.checkbox(&mut self.image.lock, "🔒").on_hover_text(l.t("Garder le rapport", "Keep the ratio"));
        });
        ui.horizontal_wrapped(|ui| {
            if ui.button(l.t("✋ Cadre sur l'image", "✋ Frame on the picture")).clicked() {
                self.image.view = View::Capture;
                self.image.tool = Tool::Frame;
                self.image.zoom = 0.0;
            }
            if ui.button(l.t("📐 Vérifier la grille", "📐 Check the grid")).clicked() {
                self.image.view = View::Capture;
                self.image.tool = Tool::Grid;
                self.image.zoom = 4.0;
            }
            if ui.button(l.t("Tout l'écran", "Whole screen")).clicked() {
                if let Some([w, h]) = self.screen() {
                    plan.zone = [0, 0, w, h];
                }
            }
        });
        let [x, y, w, h] = plan.zone;
        ui.weak(match l {
            Lang::Fr => format!("cadre du jeu : {w} × {h} en {x}, {y}"),
            Lang::En => format!("game frame: {w} × {h} at {x}, {y}"),
        });
        if plan != before {
            self.image.plan = Some(plan);
            self.apply_plan();
        }
    }

    /// Card 2: the shape it is drawn with.
    fn card_shape(&mut self, ui: &mut egui::Ui) {
        let l = self.lang;
        let Some(mut plan) = self.image.plan else { return };
        let before = plan;
        card(ui, "2", l.t("Forme", "Shape"), |ui| {
            if choice(
                ui,
                plan.shape == Shape::Crt,
                l.t("Écran 4:3 d'époque", "4:3 monitor of the time"),
                l.t("comme sur un moniteur CRT : le 320×200 étiré en 4:3", "as on a CRT: 320×200 stretched to 4:3"),
            ) {
                plan.shape = Shape::Crt;
            }
            if choice(
                ui,
                plan.shape == Shape::Square,
                l.t("Pixels carrés", "Square pixels"),
                l.t("chaque pixel du jeu dessiné carré", "every game pixel drawn square"),
            ) {
                plan.shape = Shape::Square;
            }
            if choice(
                ui,
                plan.shape == Shape::AsShown,
                l.t("Tel qu'à l'écran", "As shown now"),
                l.t("la forme que l'émulateur lui donne", "the shape the emulator gives it"),
            ) {
                plan.shape = Shape::AsShown;
            }
            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                ui.label(l.t("Dupliquer", "Duplicate")).on_hover_text(l.t(
                    "Répète chaque pixel du jeu : ↕ 2 rend le jeu deux fois plus haut, ↔ 2 deux fois plus large. \
                     Le shader travaille ensuite sur ce jeu dupliqué.",
                    "Repeats each game pixel: ↕ 2 makes the game twice as tall, ↔ 2 twice as wide. The shader \
                     then works on that duplicated game.",
                ));
                ui.add(egui::DragValue::new(&mut plan.dup[0]).range(1..=8).prefix("↔ ×"));
                ui.add(egui::DragValue::new(&mut plan.dup[1]).range(1..=8).prefix("↕ ×"));
            });
        });
        if plan != before {
            self.image.plan = Some(plan);
            self.apply_plan();
        }
    }

    /// Card 3: how big.
    fn card_size(&mut self, ui: &mut egui::Ui) {
        let l = self.lang;
        let Some(mut plan) = self.image.plan else { return };
        let before = plan;
        card(ui, "3", l.t("Taille", "Size"), |ui| {
            if choice(
                ui,
                plan.size == Size::Integer,
                l.t("Nette", "Sharp"),
                l.t(
                    "tous les pixels et scanlines de la même taille, le plus grand possible",
                    "every pixel and scanline the same size, as large as possible",
                ),
            ) {
                plan.size = Size::Integer;
            }
            if choice(
                ui,
                plan.size == Size::Fit,
                l.t("Plein écran", "Fill the screen"),
                l.t("aussi grand que l'écran, pixels un peu inégaux", "as large as the screen, pixels slightly uneven"),
            ) {
                plan.size = Size::Fit;
            }
            if choice(
                ui,
                plan.size == Size::InPlace,
                l.t("À sa place", "Where it is"),
                l.t("dans le cadre actuel du jeu", "inside the game's current frame"),
            ) {
                plan.size = Size::InPlace;
            }
        });
        if plan != before {
            self.image.plan = Some(plan);
            self.apply_plan();
        }
    }

    // -------------------------------------------------------------- views

    /// The large view: result or captured game, switchable.
    fn view(&mut self, ui: &mut egui::Ui) {
        let l = self.lang;
        ui.horizontal(|ui| {
            ui.selectable_value(
                &mut self.image.view,
                View::Result,
                egui::RichText::new(l.t("Résultat", "Result")).size(16.0),
            );
            ui.selectable_value(
                &mut self.image.view,
                View::Capture,
                egui::RichText::new(l.t("Image du jeu", "Game picture")).size(16.0),
            );
            if self.image.view == View::Capture {
                ui.separator();
                ui.selectable_value(&mut self.image.tool, Tool::Frame, l.t("✋ cadre", "✋ frame"));
                ui.selectable_value(&mut self.image.tool, Tool::Grid, l.t("📐 grille", "📐 grid"));
                ui.separator();
                let fit = self.image.zoom == 0.0;
                if ui.selectable_label(fit, l.t("ajusté", "fit")).clicked() {
                    self.image.zoom = 0.0;
                }
                for z in [1.0, 2.0, 4.0, 8.0] {
                    if ui.selectable_label(!fit && self.image.zoom == z, format!("×{z}")).clicked() {
                        self.image.zoom = z;
                    }
                }
            }
        });
        if self.image.capture.is_none() {
            ui.add_space(40.0);
            ui.vertical_centered(|ui| {
                ui.weak(l.t("L'image du jeu apparaîtra ici.", "The game's picture shows up here."));
            });
            return;
        }
        match self.image.view {
            View::Result => self.result_view(ui),
            View::Capture => {
                let height = ui.available_height().max(200.0);
                self.canvas(ui, height);
                ui.weak(match self.image.tool {
                    Tool::Frame => l.t(
                        "Fais glisser le cadre orange : dedans pour le déplacer, près d'un bord pour le redimensionner.",
                        "Drag the orange frame: inside to move it, near an edge to resize it.",
                    ),
                    Tool::Grid => l.t(
                        "Chaque case doit contenir un seul bloc de couleur. Fais glisser pour caler la grille.",
                        "Every cell must hold a single block of colour. Drag to lay the grid on the blocks.",
                    ),
                });
            }
        }
    }

    /// The screen as it will look: black screen, game where it will be drawn.
    fn result_view(&mut self, ui: &mut egui::Ui) {
        let summary = self.summary();
        let (Some(plan), Some(screen), Some(c)) = (self.image.plan, self.screen(), self.image.capture.as_ref()) else {
            return;
        };
        let space = ui.available_size() - egui::vec2(0.0, 48.0);
        let scale = (space.x / screen[0] as f32).min(space.y / screen[1] as f32).max(0.01);
        let (rect, _) = ui
            .allocate_exact_size(egui::vec2(screen[0] as f32 * scale, screen[1] as f32 * scale), egui::Sense::hover());
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 4.0, egui::Color32::BLACK);
        let [dx, dy, dw, dh] = plan.display(screen);
        let target = egui::Rect::from_min_size(
            rect.min + egui::vec2(dx as f32 * scale, dy as f32 * scale),
            egui::vec2(dw as f32 * scale, dh as f32 * scale),
        );
        let [zx, zy, zw, zh] = plan.zone.map(|v| v as f32);
        let (bw, bh) = (c.base[0] as f32, c.base[1] as f32);
        let uv = egui::Rect::from_min_max(egui::pos2(zx / bw, zy / bh), egui::pos2((zx + zw) / bw, (zy + zh) / bh));
        painter.image(c.texture.id(), target, uv, egui::Color32::WHITE);
        painter.rect_stroke(rect, 4.0, egui::Stroke::new(1.0, egui::Color32::DARK_GRAY), egui::StrokeKind::Inside);
        let label = format!("{dw} × {dh}");
        let font = egui::FontId::proportional(14.0);
        let anchor = target.center_bottom() + egui::vec2(0.0, -6.0);
        let galley = painter.layout_no_wrap(label.clone(), font.clone(), egui::Color32::WHITE);
        let back = egui::Rect::from_center_size(
            anchor - egui::vec2(0.0, galley.size().y / 2.0),
            galley.size() + egui::vec2(10.0, 4.0),
        );
        painter.rect_filled(back, 4.0, egui::Color32::from_black_alpha(170));
        painter.text(anchor, egui::Align2::CENTER_BOTTOM, label, font, egui::Color32::WHITE);
        if let Some(summary) = summary {
            ui.add_space(6.0);
            ui.label(summary);
        }
    }

    /// The captured game with its frame or grid over it.
    fn canvas(&mut self, ui: &mut egui::Ui, max_height: f32) {
        let l = self.lang;
        let Some(c) = self.image.capture.as_ref() else { return };
        let (texture, base) = (c.texture.id(), c.base);
        let fit = (ui.available_width() / base[0] as f32).min((max_height - 30.0) / base[1] as f32);
        // ×N: N canvas points per screen pixel.
        let zoom = if self.image.zoom == 0.0 { fit } else { self.image.zoom };
        let size = egui::vec2(base[0] as f32 * zoom, base[1] as f32 * zoom);
        egui::ScrollArea::both().max_height(max_height - 30.0).id_salt("capture").show(ui, |ui| {
            let (rect, response) = ui.allocate_exact_size(size, egui::Sense::drag());
            let painter = ui.painter_at(rect);
            painter.image(
                texture,
                rect,
                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                egui::Color32::WHITE,
            );
            let Some(plan) = self.image.plan.as_mut() else { return };
            let to_canvas = |x: f32, y: f32| rect.min + egui::vec2(x * zoom, y * zoom);
            let [zx, zy, zw, zh] = plan.zone.map(|v| v as f32);
            let zone = egui::Rect::from_min_max(to_canvas(zx, zy), to_canvas(zx + zw, zy + zh));
            let shade = egui::Color32::from_black_alpha(150);
            for outside in [
                egui::Rect::from_min_max(rect.min, egui::pos2(rect.right(), zone.top())),
                egui::Rect::from_min_max(egui::pos2(rect.left(), zone.bottom()), rect.max),
                egui::Rect::from_min_max(egui::pos2(rect.left(), zone.top()), egui::pos2(zone.left(), zone.bottom())),
                egui::Rect::from_min_max(egui::pos2(zone.right(), zone.top()), egui::pos2(rect.right(), zone.bottom())),
            ] {
                painter.rect_filled(outside, 0.0, shade);
            }
            let orange = egui::Color32::from_rgb(255, 170, 60);
            painter.rect_stroke(zone, 0.0, egui::Stroke::new(2.0, orange), egui::StrokeKind::Middle);

            let mut moved = false;
            match self.image.tool {
                Tool::Frame => {
                    for corner in [zone.left_top(), zone.right_top(), zone.left_bottom(), zone.right_bottom()] {
                        painter.circle_filled(corner, 5.0, orange);
                    }
                    let margin = 12.0;
                    if response.drag_started() {
                        let p = response.interact_pointer_pos().unwrap_or(zone.center());
                        self.image.drag = Some([
                            (p.x - zone.left()).abs() < margin,
                            (p.y - zone.top()).abs() < margin,
                            (p.x - zone.right()).abs() < margin,
                            (p.y - zone.bottom()).abs() < margin,
                        ]);
                    }
                    if response.dragged() {
                        let d = response.drag_delta() / zoom;
                        let edges = self.image.drag.unwrap_or([false; 4]);
                        let (mut x0, mut y0, mut x1, mut y1) = (zx, zy, zx + zw, zy + zh);
                        if edges == [false; 4] {
                            (x0, x1, y0, y1) = (x0 + d.x, x1 + d.x, y0 + d.y, y1 + d.y);
                        } else {
                            x0 += if edges[0] { d.x } else { 0.0 };
                            y0 += if edges[1] { d.y } else { 0.0 };
                            x1 += if edges[2] { d.x } else { 0.0 };
                            y1 += if edges[3] { d.y } else { 0.0 };
                        }
                        let (w, h) = (base[0] as f32, base[1] as f32);
                        let x0 = x0.round().clamp(0.0, w - 8.0);
                        let y0 = y0.round().clamp(0.0, h - 8.0);
                        let x1 = x1.round().clamp(x0 + 8.0, w);
                        let y1 = y1.round().clamp(y0 + 8.0, h);
                        plan.zone = [x0 as u32, y0 as u32, (x1 - x0) as u32, (y1 - y0) as u32];
                    }
                    if response.drag_stopped() {
                        self.image.drag = None;
                        moved = true;
                    }
                }
                Tool::Grid => {
                    let step = egui::vec2(plan.pixel[0] * zoom, plan.pixel[1] * zoom);
                    if step.min_elem() >= 3.0 {
                        let line = egui::Stroke::new(1.0, egui::Color32::from_rgba_unmultiplied(255, 60, 60, 170));
                        // Only the lines in sight: at ×4 a 4K capture is huge.
                        let visible = ui.clip_rect().intersect(zone);
                        let mut x = zone.left() + ((visible.left() - zone.left()) / step.x).floor() * step.x;
                        while x <= visible.right() + 0.5 {
                            painter.line_segment([egui::pos2(x, visible.top()), egui::pos2(x, visible.bottom())], line);
                            x += step.x;
                        }
                        let mut y = zone.top() + ((visible.top() - zone.top()) / step.y).floor() * step.y;
                        while y <= visible.bottom() + 0.5 {
                            painter.line_segment([egui::pos2(visible.left(), y), egui::pos2(visible.right(), y)], line);
                            y += step.y;
                        }
                    } else {
                        painter.text(
                            zone.center(),
                            egui::Align2::CENTER_CENTER,
                            l.t("Zoome (×4) pour voir la grille", "Zoom in (×4) to see the grid"),
                            egui::FontId::proportional(18.0),
                            orange,
                        );
                    }
                    // Dragging shifts the frame, grid included, onto the
                    // game's blocks.
                    if response.dragged() {
                        let d = response.drag_delta() / zoom;
                        let max_x = base[0].saturating_sub(plan.zone[2]) as f32;
                        let max_y = base[1].saturating_sub(plan.zone[3]) as f32;
                        plan.zone[0] = (plan.zone[0] as f32 + d.x).round().clamp(0.0, max_x) as u32;
                        plan.zone[1] = (plan.zone[1] as f32 + d.y).round().clamp(0.0, max_y) as u32;
                    }
                    if response.drag_stopped() {
                        moved = true;
                    }
                }
            }
            if moved {
                self.apply_plan();
            }
        });
    }

    // --------------------------------------------------------------- page

    /// The Picture page.
    pub fn image_tab(&mut self, ui: &mut egui::Ui) {
        let l = self.lang;
        // Opening the page analyses the game once, by itself.
        if self.image.requested.is_none() {
            self.analyse();
        }
        egui::Panel::right("picture-cards").resizable(true).default_size(360.0).show(ui, |ui| {
            egui::ScrollArea::vertical().id_salt("picture-cards").show(ui, |ui| {
                self.card_game(ui);
                self.card_shape(ui);
                self.card_size(ui);
                egui::CollapsingHeader::new(l.t("Réglages avancés", "Advanced settings")).id_salt("advanced").show(
                    ui,
                    |ui| {
                        ui.weak(l.t(
                            "Les réglages bruts de la couche. Les cartes du dessus les remplissent pour toi.",
                            "The layer's raw settings. The cards above fill them in for you.",
                        ));
                        self.source_panel(ui);
                    },
                );
            });
        });
        self.view(ui);
    }

    // ---------------------------------------------------------- assistant

    /// The "Set up this game" assistant: the same cards, one at a time.
    pub fn wizard_window(&mut self, ctx: &egui::Context) {
        let l = self.lang;
        let Some(step) = self.image.wizard else { return };
        if self.client.is_none() {
            self.image.wizard = None;
            return;
        }
        let titles = [
            l.t("1. Le jeu", "1. The game"),
            l.t("2. Forme et taille", "2. Shape and size"),
            l.t("3. Shader", "3. Shader"),
            l.t("4. Profil", "4. Profile"),
        ];
        let mut open = true;
        let mut next = step;
        egui::Window::new(l.t("Configurer ce jeu", "Set up this game"))
            .open(&mut open)
            .default_size([1000.0, 720.0])
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    for (i, title) in titles.iter().enumerate() {
                        if ui.selectable_label(i == step, egui::RichText::new(*title).size(15.0)).clicked() {
                            next = i;
                        }
                    }
                });
                ui.separator();
                egui::Panel::bottom("wizard-nav").show(ui, |ui| {
                    ui.horizontal(|ui| {
                        if ui.add_enabled(step > 0, egui::Button::new(l.t("◀ Précédent", "◀ Back"))).clicked() {
                            next = step - 1;
                        }
                        if step + 1 < STEPS && ui.button(l.t("Suivant ▶", "Next ▶")).clicked() {
                            next = step + 1;
                        }
                    });
                });
                match step {
                    0 => {
                        egui::Panel::right("wizard-game").default_size(340.0).show(ui, |ui| {
                            ui.label(l.t(
                                "Va sur un écran du jeu bien rempli (pas un écran noir). vkSlang le photographie \
                                 avant le shader et trouve tout seul où il est et la taille de ses pixels.",
                                "Go to a busy screen of the game (not a black one). vkSlang photographs it before \
                                 the shader and finds by itself where it is and how big its pixels are.",
                            ));
                            ui.add_space(6.0);
                            self.card_game(ui);
                        });
                        self.view(ui);
                    }
                    1 => {
                        egui::Panel::right("wizard-shape").default_size(340.0).show(ui, |ui| {
                            egui::ScrollArea::vertical().id_salt("wizard-shape").show(ui, |ui| {
                                self.card_shape(ui);
                                self.card_size(ui);
                            });
                        });
                        self.image.view = View::Result;
                        self.view(ui);
                    }
                    2 => self.wizard_shader(ui),
                    _ => self.wizard_profile(ui),
                }
            });
        if !open {
            self.image.wizard = None;
            return;
        }
        if next != step {
            self.image.wizard = Some(next);
        }
    }

    /// Opens the assistant, analysing the game afresh.
    pub fn open_wizard(&mut self) {
        self.image.plan = None;
        self.image.profile_name = self.state.as_ref().map(|s| s.process.clone()).unwrap_or_default();
        self.image.view = View::Result;
        self.image.wizard = Some(0);
        self.analyse();
    }

    fn wizard_shader(&mut self, ui: &mut egui::Ui) {
        let l = self.lang;
        ui.label(l.t(
            "Choisis un preset : il s'applique dès le clic. Les réglages fins sont dans l'onglet Shader.",
            "Pick a preset: it applies on click. Fine tuning is in the Shader tab.",
        ));
        ui.add(
            egui::TextEdit::singleline(&mut self.image.search)
                .hint_text(l.t("Rechercher (ex. crt guest)", "Search (e.g. crt guest)"))
                .desired_width(f32::INFINITY),
        );
        let words: Vec<String> = self.image.search.to_lowercase().split_whitespace().map(String::from).collect();
        let running: Vec<String> = self.state.as_ref().map(|s| s.presets.clone()).unwrap_or_default();
        let root = std::path::PathBuf::from(&self.shader_root);
        let mut clicked = None;
        egui::ScrollArea::vertical().id_salt("wizard-presets").show(ui, |ui| {
            let matches = self.presets.iter().filter(|p| {
                let s = p.to_string_lossy().to_lowercase();
                words.iter().all(|w| s.contains(w.as_str()))
            });
            for rel in matches.take(300) {
                let abs = root.join(rel);
                let running = running.iter().any(|p| p == abs.to_string_lossy().as_ref());
                if ui.selectable_label(running, rel.to_string_lossy()).clicked() {
                    clicked = Some(abs);
                }
            }
        });
        if let Some(path) = clicked {
            self.chain = vec![crate::ChainEntry { path: path.clone(), enabled: true }];
            self.send(Request::SetEnabled { enabled: true });
            self.send(Request::LoadPresets { paths: vec![path.display().to_string()] });
        }
    }

    fn wizard_profile(&mut self, ui: &mut egui::Ui) {
        let l = self.lang;
        let Some(state) = self.state.clone() else { return };
        ui.label(l.t(
            "Enregistre tout ça sous un nom. Coché, le profil se charge tout seul au prochain lancement de ce programme.",
            "Save all this under a name. Ticked, the profile loads by itself next time this program starts.",
        ));
        ui.horizontal(|ui| {
            ui.label(l.t("Nom", "Name"));
            ui.text_edit_singleline(&mut self.image.profile_name);
        });
        ui.checkbox(
            &mut self.image.star,
            match l {
                Lang::Fr => format!("Charger automatiquement pour {}", state.process),
                Lang::En => format!("Load automatically for {}", state.process),
            },
        );
        let named = !self.image.profile_name.trim().is_empty();
        if ui.add_enabled(named, egui::Button::new(l.t("💾 Enregistrer et terminer", "💾 Save and finish"))).clicked()
        {
            let p = crate::profile::Profile::from_state(&self.image.profile_name, &state);
            match crate::profile::save(&p) {
                Ok(_) => {
                    if self.image.star {
                        if let Err(e) = crate::save::set_default_profile(&state.process, Some(&p.name)) {
                            self.error(format!("vkSlang.conf: {e}"));
                            return;
                        }
                        self.default_profile = None;
                    }
                    self.reload_profiles();
                    self.info(match l {
                        Lang::Fr => format!("profil « {} » enregistré", p.name),
                        Lang::En => format!("profile \"{}\" saved", p.name),
                    });
                    self.image.wizard = None;
                }
                Err(e) => self.error(format!("{e}")),
            }
        }
    }
}
