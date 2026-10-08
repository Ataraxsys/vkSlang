//! The Image tab and the "Set up this game" assistant.
//!
//! Both work on a [`Plan`]: where the game is, how big its pixels are, what
//! shape and size it is drawn with. A full-resolution capture lets the panel
//! find the first two by itself; every change is applied live.

use crate::detect::{self, Pixels};
use crate::plan::{parse_area, Plan, Shape, Size};
use crate::{App, SourceEdit};
use eframe::egui;
use std::time::{Duration, Instant};
use vkslang_ipc::{Request, SourceSize};

/// A full-resolution picture of the game, before the preset.
pub struct Capture {
    pub id: u64,
    pub texture: egui::TextureHandle,
    pub size: [u32; 2],
    /// Size of the output it was taken from.
    pub base: [u32; 2],
    pub rgba: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    /// Look only.
    View,
    /// Drag the game's zone.
    Zone,
    /// The game's pixel grid over the picture; dragging shifts it.
    Grid,
}

/// Steps of the assistant.
const STEPS: usize = 6;

pub struct ImageState {
    pub capture: Option<Capture>,
    /// Last capture published by the layer that was read, even if it could
    /// not be: a broken file is not read again on every repaint.
    read_id: Option<u64>,
    requested: Option<Instant>,
    pub plan: Option<Plan>,
    tool: Tool,
    /// Which edges of the zone the current drag moves (none: all of it).
    drag: Option<[bool; 4]>,
    /// Zoom of the picture; 0 fits it to the space available.
    zoom: f32,
    lock: bool,
    /// Outcome of the last detection, shown next to its button.
    found: Option<String>,
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
            tool: Tool::View,
            drag: None,
            zoom: 0.0,
            lock: true,
            found: None,
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
        self.image.capture = Some(Capture { id: c.id, texture, size, base: c.base, rgba });
        if self.image.plan.is_none() {
            self.image.plan = self.plan_from_settings();
        }
        // The assistant may already be past the capture: do what entering
        // that step would have done had the picture been there.
        match self.image.wizard {
            Some(1) => self.detect_zone(),
            Some(2) => self.detect_pixels(),
            _ => {}
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

    fn capture_pixels(&self) -> Option<(Pixels<'_>, f32)> {
        let c = self.image.capture.as_ref()?;
        let scale = c.base[0] as f32 / c.size[0] as f32;
        Some((Pixels { data: &c.rgba, width: c.size[0], height: c.size[1] }, scale))
    }

    fn detect_zone(&mut self) {
        let l = self.lang;
        let Some((pixels, scale)) = self.capture_pixels() else { return };
        let found = detect::content_bounds(&pixels).map(|r| r.map(|v| (v as f32 * scale).round() as u32));
        let screen = self.screen().unwrap_or([pixels.width, pixels.height]);
        match found {
            Some(zone) => {
                let plan = self.image.plan.get_or_insert(Plan {
                    zone,
                    pixel: [1.0, 1.0],
                    dup: [1, 1],
                    shape: Shape::AsShown,
                    size: Size::InPlace,
                });
                plan.zone = zone;
                let whole = zone == [0, 0, screen[0], screen[1]];
                self.image.found = Some(
                    if whole {
                        l.t(
                            "Aucune bande noire : le jeu occupe tout l'écran.",
                            "No black bars: the game fills the screen.",
                        )
                    } else {
                        l.t("Zone trouvée.", "Zone found.")
                    }
                    .to_string(),
                );
                self.apply_plan();
            }
            None => {
                self.image.found =
                    Some(l.t("L'image est entièrement noire.", "The picture is entirely black.").to_string());
            }
        }
    }

    fn detect_pixels(&mut self) {
        let l = self.lang;
        let Some(plan) = self.image.plan else { return };
        let Some((pixels, scale)) = self.capture_pixels() else { return };
        if (scale - 1.0).abs() > 0.01 {
            self.image.found = Some(
                l.t(
                    "Capture réduite : impossible de mesurer les pixels exactement. Utilise la grille.",
                    "Downscaled capture: pixels cannot be measured exactly. Use the grid.",
                )
                .to_string(),
            );
            return;
        }
        let region = plan.zone;
        let (h, v) = (detect::pitch(&pixels, region, true), detect::pitch(&pixels, region, false));
        match (h, v) {
            (Some(h), Some(v)) if h.size > 1.0 || v.size > 1.0 => {
                let plan = self.image.plan.as_mut().unwrap();
                plan.pixel = [h.size, v.size];
                // Start the zone on the grid and keep only whole game pixels:
                // a partial one at either end would be bar, not game.
                for (axis, pitch) in [(0, h), (1, v)] {
                    let offset = pitch.offset.round().min(plan.zone[axis + 2] as f32 - 1.0).max(0.0);
                    let count = ((plan.zone[axis + 2] as f32 - offset) / pitch.size).floor().max(1.0);
                    plan.zone[axis] += offset as u32;
                    plan.zone[axis + 2] = (count * pitch.size).round() as u32;
                }
                let [gw, gh] = plan.game();
                self.image.found = Some(match l {
                    crate::i18n::Lang::Fr => {
                        format!("Jeu en {gw} × {gh}, chaque pixel fait {:.2} × {:.2} pixels d'écran.", h.size, v.size)
                    }
                    crate::i18n::Lang::En => {
                        format!("Game at {gw} × {gh}, each pixel is {:.2} × {:.2} screen pixels.", h.size, v.size)
                    }
                });
                self.apply_plan();
            }
            _ => {
                self.image.found = Some(
                    l.t(
                        "Pas de grille nette (image lissée ou trop uniforme). Règle la taille à la main, la grille t'aide.",
                        "No clear grid (smoothed or too uniform picture). Set the size by hand, the grid helps.",
                    )
                    .to_string(),
                );
            }
        }
    }

    /// Step 1: the picture.
    fn section_capture(&mut self, ui: &mut egui::Ui) {
        let l = self.lang;
        ui.horizontal_wrapped(|ui| {
            if ui.button(l.t("📷 Capturer l'image", "📷 Capture the picture")).clicked() {
                self.request_capture();
            }
            let waiting =
                self.image.requested.is_some_and(|t| t.elapsed() < Duration::from_secs(3))
                    && self.image.capture.as_ref().is_none_or(|c| {
                        self.state.as_ref().and_then(|s| s.capture.as_ref()).is_none_or(|n| n.id == c.id)
                    });
            let stuck = self.image.requested.is_some_and(|t| t.elapsed() >= Duration::from_secs(3))
                && self.image.capture.is_none();
            if waiting {
                ui.spinner();
            } else if stuck {
                ui.colored_label(
                    egui::Color32::from_rgb(255, 170, 60),
                    l.t(
                        "Pas de capture : il faut un preset chargé et « Shader actif » coché.",
                        "No capture: a preset must be loaded and \"Shader on\" ticked.",
                    ),
                );
            }
            match &self.image.capture {
                Some(c) => ui.weak(match l {
                    crate::i18n::Lang::Fr => format!("image du jeu avant le shader, {}×{}", c.size[0], c.size[1]),
                    crate::i18n::Lang::En => format!("the game before the shader, {}×{}", c.size[0], c.size[1]),
                }),
                None => ui.weak(l.t(
                    "Capture l'image du jeu pour que tout se règle dessus.",
                    "Capture the game's picture: everything is set on it.",
                )),
            };
        });
    }

    /// Step 2: where the game is.
    fn section_zone(&mut self, ui: &mut egui::Ui) {
        let l = self.lang;
        ui.horizontal_wrapped(|ui| {
            ui.strong(l.t("Zone du jeu", "Game zone")).on_hover_text(l.t(
                "La partie de l'écran où le jeu dessine, sans les bandes noires. Le shader ne lit que celle-ci.",
                "The part of the screen the game draws in, without the black bars. The shader only reads this.",
            ));
            if ui
                .add_enabled(self.image.capture.is_some(), egui::Button::new(l.t("✨ Détecter", "✨ Detect")))
                .clicked()
            {
                self.detect_zone();
            }
            let editing = self.image.tool == Tool::Zone;
            if ui.selectable_label(editing, l.t("✋ Ajuster à la main", "✋ Adjust by hand")).clicked() {
                self.image.tool = if editing { Tool::View } else { Tool::Zone };
            }
            if ui.button(l.t("Tout l'écran", "Whole screen")).clicked() {
                let screen = self.screen();
                if let (Some(plan), Some([w, h])) = (self.image.plan.as_mut(), screen) {
                    plan.zone = [0, 0, w, h];
                }
                self.apply_plan();
            }
            if let Some(plan) = &self.image.plan {
                let [x, y, w, h] = plan.zone;
                ui.weak(format!("{w}×{h} @ {x},{y}"));
            }
        });
        if self.image.tool == Tool::Zone {
            if let Some(found) = &self.image.found {
                ui.weak(found.as_str());
            }
        }
    }

    /// Step 3: how big the game's pixels are.
    fn section_pixels(&mut self, ui: &mut egui::Ui) {
        let l = self.lang;
        let Some(mut plan) = self.image.plan else {
            ui.weak(l.t("Commence par la zone du jeu.", "Start with the game zone."));
            return;
        };
        let mut changed = false;
        ui.horizontal_wrapped(|ui| {
            ui.strong(l.t("Pixels du jeu", "Game pixels")).on_hover_text(l.t(
                "Combien de pixels d'écran couvre un pixel du jeu. Ils peuvent être plus hauts que larges, ou l'inverse.",
                "How many screen pixels one game pixel covers. They may be taller than wide, or the reverse.",
            ));
            if ui
                .add_enabled(self.image.capture.is_some(), egui::Button::new(l.t("✨ Détecter", "✨ Detect")))
                .clicked()
            {
                self.detect_pixels();
                plan = self.image.plan.unwrap_or(plan);
            }
            let grid = self.image.tool == Tool::Grid;
            if ui
                .selectable_label(grid, l.t("📐 Vérifier avec la grille", "📐 Check with the grid"))
                .on_hover_text(l.t(
                    "Les lignes doivent suivre les blocs du jeu. Fais glisser l'image pour décaler la grille.",
                    "The lines must follow the game's blocks. Drag the picture to shift the grid.",
                ))
                .clicked()
            {
                self.image.tool = if grid { Tool::View } else { Tool::Grid };
            }
        });
        ui.horizontal_wrapped(|ui| {
            ui.label(l.t("Taille d'un pixel", "Pixel size"));
            let before = plan.pixel;
            for (axis, label) in [(0usize, "↔"), (1usize, "↕")] {
                changed |= ui
                    .add(egui::DragValue::new(&mut plan.pixel[axis]).speed(0.01).range(0.5..=64.0).prefix(label))
                    .changed();
            }
            if self.image.lock && before != plan.pixel {
                // Keep the ratio they had, whichever one moved.
                if before[0] != plan.pixel[0] {
                    plan.pixel[1] = before[1] * plan.pixel[0] / before[0];
                } else {
                    plan.pixel[0] = before[0] * plan.pixel[1] / before[1];
                }
            }
            ui.checkbox(&mut self.image.lock, "🔒").on_hover_text(l.t("Garder le rapport", "Keep the ratio"));
            ui.separator();
            ui.label(l.t("Résolution du jeu", "Game resolution"));
            let mut game = plan.game();
            let mut edited = false;
            edited |= ui.add(egui::DragValue::new(&mut game[0]).range(1..=4096)).changed();
            ui.label("×");
            edited |= ui.add(egui::DragValue::new(&mut game[1]).range(1..=4096)).changed();
            for (w, h) in [(320, 200), (320, 240), (640, 400), (640, 480)] {
                if ui.small_button(format!("{w}×{h}")).clicked() {
                    game = [w, h];
                    edited = true;
                }
            }
            if edited {
                plan.pixel = [plan.zone[2] as f32 / game[0] as f32, plan.zone[3] as f32 / game[1] as f32];
                changed = true;
            }
        });
        if let Some(found) = &self.image.found {
            ui.weak(found.as_str());
        }
        if changed {
            self.image.plan = Some(plan);
            self.apply_plan();
        }
    }

    /// Step 4: shape and size on screen.
    fn section_shape(&mut self, ui: &mut egui::Ui) {
        let l = self.lang;
        let Some(mut plan) = self.image.plan else { return };
        let before = plan;
        ui.horizontal_wrapped(|ui| {
            ui.strong(l.t("Forme", "Shape"));
            ui.radio_value(&mut plan.shape, Shape::Crt, l.t("Écran 4:3 d'époque", "4:3 monitor of the time"))
                .on_hover_text(l.t(
                    "Comme sur un moniteur CRT : 320×200 en 4:3, pixels 1,2 fois plus hauts que larges.",
                    "As on a CRT monitor: 320×200 in 4:3, pixels 1.2 times taller than wide.",
                ));
            ui.radio_value(&mut plan.shape, Shape::Square, l.t("Pixels carrés", "Square pixels")).on_hover_text(l.t(
                "Chaque pixel (dupliqué) dessiné carré : doubler les lignes rend l'image deux fois plus haute.",
                "Every (duplicated) pixel drawn square: doubling the lines makes the picture twice as tall.",
            ));
            ui.radio_value(&mut plan.shape, Shape::AsShown, l.t("Comme à l'écran", "As shown now"));
        });
        ui.horizontal_wrapped(|ui| {
            ui.strong(l.t("Taille", "Size"));
            ui.radio_value(&mut plan.size, Size::Integer, l.t("Entière", "Whole multiple")).on_hover_text(l.t(
                "Un nombre entier de lignes d'écran par ligne du jeu : toutes les scanlines ont la même épaisseur.",
                "A whole number of screen lines per game line: every scanline has the same thickness.",
            ));
            ui.radio_value(&mut plan.size, Size::Fit, l.t("Plein écran", "Fill the screen"));
            ui.radio_value(&mut plan.size, Size::InPlace, l.t("À sa place", "Where it is"))
                .on_hover_text(l.t("Dans la zone actuelle du jeu.", "Inside the game's current zone."));
        });
        ui.horizontal_wrapped(|ui| {
            ui.strong(l.t("Dupliquer", "Duplicate")).on_hover_text(l.t(
                "Répète chaque pixel du jeu : ↕ 2 rend le jeu deux fois plus haut, ↔ 2 deux fois plus large. \
                 Le shader s'applique ensuite sur ce jeu dupliqué, ses scanlines gardent leur épaisseur.",
                "Repeats each game pixel: ↕ 2 makes the game twice as tall, ↔ 2 twice as wide. The shader then \
                 applies to that duplicated game, its scanlines keeping their thickness.",
            ));
            ui.add(egui::DragValue::new(&mut plan.dup[0]).range(1..=8).prefix("↔ "));
            ui.add(egui::DragValue::new(&mut plan.dup[1]).range(1..=8).prefix("↕ "));
            if let Some(screen) = self.screen() {
                let [gw, gh] = plan.game();
                let [iw, ih] = plan.input();
                let [_, _, dw, dh] = plan.display(screen);
                ui.weak(match l {
                    crate::i18n::Lang::Fr => {
                        format!(
                            "jeu {gw}×{gh} › shader {iw}×{ih} › affiché {dw}×{dh}, {:.2} lignes d'écran par ligne",
                            dh as f32 / ih as f32
                        )
                    }
                    crate::i18n::Lang::En => {
                        format!(
                            "game {gw}×{gh} › shader {iw}×{ih} › drawn {dw}×{dh}, {:.2} screen lines per line",
                            dh as f32 / ih as f32
                        )
                    }
                });
            }
        });
        if plan != before {
            self.image.plan = Some(plan);
            self.apply_plan();
        }
    }

    /// The final layout on a miniature screen: black screen, game drawn where
    /// the preset will draw it.
    fn preview(&self, ui: &mut egui::Ui, height: f32) {
        let (Some(plan), Some(screen), Some(c)) = (self.image.plan, self.screen(), self.image.capture.as_ref()) else {
            return;
        };
        let scale = height / screen[1] as f32;
        let (rect, _) = ui.allocate_exact_size(egui::vec2(screen[0] as f32 * scale, height), egui::Sense::hover());
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 2.0, egui::Color32::BLACK);
        let [dx, dy, dw, dh] = plan.display(screen).map(|v| v as f32 * scale);
        let target = egui::Rect::from_min_size(rect.min + egui::vec2(dx, dy), egui::vec2(dw, dh));
        let [zx, zy, zw, zh] = plan.zone.map(|v| v as f32);
        let (bw, bh) = (c.base[0] as f32, c.base[1] as f32);
        let uv = egui::Rect::from_min_max(egui::pos2(zx / bw, zy / bh), egui::pos2((zx + zw) / bw, (zy + zh) / bh));
        painter.image(c.texture.id(), target, uv, egui::Color32::WHITE);
        painter.rect_stroke(rect, 2.0, egui::Stroke::new(1.0, egui::Color32::GRAY), egui::StrokeKind::Inside);
    }

    /// The capture with the zone or the grid over it.
    fn canvas(&mut self, ui: &mut egui::Ui, max_height: f32) {
        let l = self.lang;
        let Some(c) = self.image.capture.as_ref() else { return };
        let (texture, base) = (c.texture.id(), c.base);
        ui.horizontal(|ui| {
            ui.weak(l.t("Zoom", "Zoom"));
            let mut fit = self.image.zoom == 0.0;
            if ui.selectable_label(fit, l.t("ajusté", "fit")).clicked() {
                fit = true;
                self.image.zoom = 0.0;
            }
            for z in [1.0, 2.0, 4.0, 8.0] {
                if ui.selectable_label(!fit && self.image.zoom == z, format!("×{z}")).clicked() {
                    self.image.zoom = z;
                }
            }
        });
        // Screen pixels to canvas points.
        let fit = (ui.available_width() / base[0] as f32).min(max_height / base[1] as f32);
        // ×N: N canvas points per screen pixel.
        let zoom = if self.image.zoom == 0.0 { fit } else { self.image.zoom };
        let canvas = egui::vec2(base[0] as f32 * zoom, base[1] as f32 * zoom);
        egui::ScrollArea::both().max_height(max_height).id_salt("capture").show(ui, |ui| {
            let (rect, response) = ui.allocate_exact_size(canvas, egui::Sense::drag());
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
                Tool::View => {}
                Tool::Zone => {
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
                        let mut x = zone.left();
                        while x <= zone.right() + 0.5 {
                            painter.line_segment([egui::pos2(x, zone.top()), egui::pos2(x, zone.bottom())], line);
                            x += step.x;
                        }
                        let mut y = zone.top();
                        while y <= zone.bottom() + 0.5 {
                            painter.line_segment([egui::pos2(zone.left(), y), egui::pos2(zone.right(), y)], line);
                            y += step.y;
                        }
                    } else {
                        painter.text(
                            zone.center(),
                            egui::Align2::CENTER_CENTER,
                            l.t("Zoome pour voir la grille", "Zoom in to see the grid"),
                            egui::FontId::proportional(16.0),
                            orange,
                        );
                    }
                    // Dragging shifts the whole zone, grid included, so the
                    // lines can be laid on the game's blocks.
                    if response.dragged() {
                        let d = response.drag_delta() / zoom;
                        self.image.drag = None;
                        let max_x = base[0].saturating_sub(plan.zone[2]) as f32;
                        let max_y = base[1].saturating_sub(plan.zone[3]) as f32;
                        let x = (plan.zone[0] as f32 + d.x).round().clamp(0.0, max_x);
                        let y = (plan.zone[1] as f32 + d.y).round().clamp(0.0, max_y);
                        plan.zone[0] = x as u32;
                        plan.zone[1] = y as u32;
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

    /// The Image tab: every section at once, then the raw settings.
    pub fn image_tab(&mut self, ui: &mut egui::Ui) {
        let l = self.lang;
        if self.image.capture.is_none() && self.image.requested.is_none() {
            self.request_capture();
        }
        egui::ScrollArea::vertical().id_salt("image-tab").show(ui, |ui| {
            self.section_capture(ui);
            ui.separator();
            self.section_zone(ui);
            ui.separator();
            self.section_pixels(ui);
            ui.separator();
            self.section_shape(ui);
            ui.horizontal(|ui| {
                ui.weak(l.t("Résultat", "Result"));
                self.preview(ui, 140.0);
            });
            ui.separator();
            self.canvas(ui, 420.0);
            ui.separator();
            egui::CollapsingHeader::new(l.t("Réglages avancés", "Advanced settings")).id_salt("advanced").show(
                ui,
                |ui| {
                    ui.weak(l.t(
                        "Les mêmes réglages un par un. Les sections du dessus les remplissent pour toi.",
                        "The same settings one by one. The sections above fill them in for you.",
                    ));
                    self.source_panel(ui);
                },
            );
        });
    }

    /// The "Set up this game" assistant.
    pub fn wizard_window(&mut self, ctx: &egui::Context) {
        let l = self.lang;
        let Some(step) = self.image.wizard else { return };
        if self.client.is_none() {
            self.image.wizard = None;
            return;
        }
        let titles = [
            l.t("1. Capture", "1. Capture"),
            l.t("2. Zone du jeu", "2. Game zone"),
            l.t("3. Pixels", "3. Pixels"),
            l.t("4. Forme et taille", "4. Shape and size"),
            l.t("5. Shader", "5. Shader"),
            l.t("6. Profil", "6. Profile"),
        ];
        let mut open = true;
        let mut next = step;
        egui::Window::new(l.t("Configurer ce jeu", "Set up this game"))
            .open(&mut open)
            .default_size([900.0, 700.0])
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    for (i, title) in titles.iter().enumerate() {
                        if ui.selectable_label(i == step, *title).clicked() {
                            next = i;
                        }
                    }
                });
                ui.separator();
                match step {
                    0 => {
                        ui.label(l.t(
                            "vkSlang prend une photo du jeu tel qu'il le dessine, avant le shader. Lance une scène \
                             bien remplie (pas un écran noir), puis capture.",
                            "vkSlang takes a picture of the game as it draws it, before the shader. Go to a busy \
                             scene (not a black screen), then capture.",
                        ));
                        self.section_capture(ui);
                        self.canvas(ui, 460.0);
                    }
                    1 => {
                        ui.label(l.t(
                            "Où le jeu se trouve à l'écran. « Détecter » enlève les bandes noires ; ajuste à la main \
                             si le jeu a lui-même des bords noirs.",
                            "Where the game is on the screen. \"Detect\" removes the black bars; adjust by hand if \
                             the game has black borders of its own.",
                        ));
                        self.section_zone(ui);
                        self.canvas(ui, 460.0);
                    }
                    2 => {
                        ui.label(l.t(
                            "La taille d'un pixel du jeu. « Détecter » la mesure sur la capture ; vérifie avec la \
                             grille au zoom ×4 : chaque case doit contenir un seul bloc de couleur.",
                            "The size of one game pixel. \"Detect\" measures it on the capture; check with the grid \
                             at ×4 zoom: every cell must hold a single block of colour.",
                        ));
                        self.section_pixels(ui);
                        self.canvas(ui, 420.0);
                    }
                    3 => {
                        ui.label(l.t(
                            "Comment le jeu doit apparaître. Le résultat s'applique tout de suite sur le jeu.",
                            "How the game should look. The result applies to the game right away.",
                        ));
                        self.section_shape(ui);
                        self.preview(ui, 300.0);
                    }
                    4 => self.wizard_shader(ui),
                    _ => self.wizard_profile(ui),
                }
                ui.separator();
                ui.horizontal(|ui| {
                    if ui.add_enabled(step > 0, egui::Button::new(l.t("◀ Précédent", "◀ Back"))).clicked() {
                        next = step - 1;
                    }
                    if step + 1 < STEPS && ui.button(l.t("Suivant ▶", "Next ▶")).clicked() {
                        next = step + 1;
                    }
                });
            });
        if !open {
            self.image.wizard = None;
            self.image.tool = Tool::View;
            return;
        }
        if next != step {
            self.enter_step(next);
        }
    }

    /// Opens the assistant on its first step.
    pub fn open_wizard(&mut self) {
        self.image.plan = None;
        self.image.found = None;
        self.image.capture = None;
        self.image.profile_name = self.state.as_ref().map(|s| s.process.clone()).unwrap_or_default();
        self.request_capture();
        self.image.wizard = Some(0);
        self.image.tool = Tool::View;
    }

    /// Moving to a step does its obvious first move for the user.
    fn enter_step(&mut self, step: usize) {
        self.image.wizard = Some(step);
        self.image.found = None;
        if matches!(step, 1 | 2) && self.image.capture.is_none() {
            self.image.found = Some(
                self.lang
                    .t(
                        "En attente de la capture : la détection se fera dès qu'elle arrive.",
                        "Waiting for the capture: detection runs as soon as it arrives.",
                    )
                    .to_string(),
            );
        }
        match step {
            1 => {
                self.image.tool = Tool::Zone;
                if self
                    .image
                    .plan
                    .is_none_or(|p| Some([0, 0, p.zone[2], p.zone[3]]) == self.screen().map(|[w, h]| [0, 0, w, h]))
                {
                    self.detect_zone();
                }
            }
            2 => {
                self.image.tool = Tool::Grid;
                self.image.zoom = 4.0;
                self.detect_pixels();
            }
            3 => {
                self.image.tool = Tool::View;
                if let Some(plan) = self.image.plan.as_mut() {
                    if plan.shape == Shape::AsShown && plan.size == Size::InPlace {
                        plan.shape = Shape::Crt;
                        plan.size = Size::Integer;
                    }
                }
                self.apply_plan();
            }
            _ => self.image.tool = Tool::View,
        }
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
        egui::ScrollArea::vertical().max_height(420.0).id_salt("wizard-presets").show(ui, |ui| {
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
                crate::i18n::Lang::Fr => format!("Charger automatiquement pour {}", state.process),
                crate::i18n::Lang::En => format!("Load automatically for {}", state.process),
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
                        crate::i18n::Lang::Fr => format!("profil « {} » enregistré", p.name),
                        crate::i18n::Lang::En => format!("profile \"{}\" saved", p.name),
                    });
                    self.image.wizard = None;
                    self.image.tool = Tool::View;
                }
                Err(e) => self.error(format!("{e}")),
            }
        }
    }
}
