//! vkslang-ui — live control panel for the vkSlang Vulkan layer.
//!
//! Connects to `$XDG_RUNTIME_DIR/vkslang/<pid>.sock` of a running game (or
//! gamescope), and lets you switch presets, tweak parameters and the source
//! resolution while it runs, then save the result.

mod save;

use eframe::egui;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use vkslang_ipc::{Client, Filter, Request, Response, SourceSettings, State};

const POLL: Duration = Duration::from_millis(400);
const SCAN: Duration = Duration::from_secs(2);

struct Target {
    pid: u32,
    path: PathBuf,
    label: String,
}

/// Editable copy of the source settings (not overwritten by polling while
/// the user edits it).
struct SourceEdit {
    native: bool,
    width: u32,
    height: u32,
    filter: Filter,
    rect: String,
}

impl SourceEdit {
    fn from(s: &SourceSettings) -> SourceEdit {
        let [width, height] = s.res.unwrap_or([320, 240]);
        SourceEdit { native: s.res.is_none(), width, height, filter: s.filter, rect: s.rect.clone() }
    }

    fn to_settings(&self) -> SourceSettings {
        SourceSettings {
            res: (!self.native).then_some([self.width, self.height]),
            filter: self.filter,
            rect: self.rect.trim().to_string(),
        }
    }
}

struct App {
    targets: Vec<Target>,
    selected: Option<u32>,
    client: Option<Client>,
    state: Option<State>,
    message: Option<(String, bool)>,
    last_poll: Instant,
    last_scan: Instant,

    shader_root: String,
    presets: Vec<PathBuf>,
    scanned_root: Option<String>,
    preset_filter: String,

    param_filter: String,
    source: Option<SourceEdit>,
    save_path: String,
}

fn default_shader_root() -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    [
        format!("{home}/.config/retroarch/shaders/shaders_slang"),
        format!("{home}/.local/share/libretro/shaders/shaders_slang"),
        "/usr/share/libretro/shaders/shaders_slang".to_string(),
    ]
    .into_iter()
    .find(|p| Path::new(p).is_dir())
    .unwrap_or_else(|| "/usr/share/libretro/shaders/shaders_slang".into())
}

fn scan_presets(root: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, root: &Path, out: &mut Vec<PathBuf>, depth: u32) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() && depth < 8 {
                walk(&path, root, out, depth + 1);
            } else if path.extension().is_some_and(|e| e == "slangp") {
                if let Ok(rel) = path.strip_prefix(root) {
                    out.push(rel.to_path_buf());
                }
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out, 0);
    out.sort();
    out
}

fn process_name(pid: u32) -> Option<String> {
    std::fs::read_to_string(format!("/proc/{pid}/comm")).ok().map(|s| s.trim().to_string())
}

impl App {
    fn new() -> App {
        App {
            targets: Vec::new(),
            selected: None,
            client: None,
            state: None,
            message: None,
            last_poll: Instant::now() - POLL,
            last_scan: Instant::now() - SCAN,
            shader_root: default_shader_root(),
            presets: Vec::new(),
            scanned_root: None,
            preset_filter: String::new(),
            param_filter: String::new(),
            source: None,
            save_path: String::new(),
        }
    }

    fn error(&mut self, msg: impl Into<String>) {
        self.message = Some((msg.into(), true));
    }

    fn info(&mut self, msg: impl Into<String>) {
        self.message = Some((msg.into(), false));
    }

    fn scan_targets(&mut self) {
        self.targets = vkslang_ipc::list_sockets()
            .into_iter()
            .filter_map(|(pid, path)| {
                let Some(name) = process_name(pid) else {
                    // Process gone: clean the stale socket.
                    let _ = std::fs::remove_file(&path);
                    return None;
                };
                Some(Target { pid, path, label: format!("{name} ({pid})") })
            })
            .collect();
        if self.selected.is_some_and(|pid| !self.targets.iter().any(|t| t.pid == pid)) {
            self.disconnect();
        }
        if self.selected.is_none() {
            if let Some(pid) = self.targets.first().map(|t| t.pid) {
                self.select(pid);
            }
        }
    }

    fn disconnect(&mut self) {
        self.selected = None;
        self.client = None;
        self.state = None;
        self.source = None;
    }

    fn select(&mut self, pid: u32) {
        self.disconnect();
        let Some(path) = self.targets.iter().find(|t| t.pid == pid).map(|t| t.path.clone()) else { return };
        match Client::connect(&path) {
            Ok(client) => {
                self.selected = Some(pid);
                self.client = Some(client);
                self.send(Request::GetState);
            }
            Err(e) => self.error(format!("cannot connect to {}: {e}", path.display())),
        }
    }

    fn send(&mut self, request: Request) {
        let Some(client) = self.client.as_mut() else { return };
        match client.request(&request) {
            Ok(Response::State(state)) => {
                if self.source.is_none() {
                    self.source = Some(SourceEdit::from(&state.source));
                }
                if self.save_path.is_empty() || self.state.as_ref().map(|s| &s.preset) != Some(&state.preset) {
                    self.save_path = save::default_preset_path(state.preset.as_deref()).display().to_string();
                }
                self.state = Some(state);
            }
            Ok(Response::Error { message }) => self.error(message),
            Err(e) => {
                self.error(format!("connection lost: {e}"));
                self.disconnect();
                self.last_scan = Instant::now() - SCAN;
            }
        }
    }

    fn tick(&mut self) {
        if self.last_scan.elapsed() >= SCAN {
            self.last_scan = Instant::now();
            self.scan_targets();
        }
        if self.client.is_some() && self.last_poll.elapsed() >= POLL {
            self.last_poll = Instant::now();
            self.send(Request::GetState);
        }
        if self.scanned_root.as_deref() != Some(self.shader_root.as_str()) {
            self.presets = scan_presets(Path::new(&self.shader_root));
            self.scanned_root = Some(self.shader_root.clone());
        }
    }

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.strong("vkSlang");
            ui.separator();
            let current = self
                .selected
                .and_then(|pid| self.targets.iter().find(|t| t.pid == pid))
                .map_or("no process".to_string(), |t| t.label.clone());
            let mut choice = self.selected;
            egui::ComboBox::from_id_salt("target").selected_text(current).width(220.0).show_ui(ui, |ui| {
                for t in &self.targets {
                    ui.selectable_value(&mut choice, Some(t.pid), t.label.as_str());
                }
            });
            if choice != self.selected {
                if let Some(pid) = choice {
                    self.select(pid);
                }
            }
            if ui.button("⟳").on_hover_text("Rescan processes").clicked() {
                self.scan_targets();
            }

            if let Some(mut enabled) = self.state.as_ref().map(|s| s.enabled) {
                if ui.checkbox(&mut enabled, "Shader enabled").changed() {
                    self.send(Request::SetEnabled { enabled });
                }
            }
            if let Some(state) = &self.state {
                if state.loading {
                    ui.spinner();
                    ui.label("compiling…");
                }
                let outputs: Vec<String> = state.outputs.iter().map(|[w, h]| format!("{w}×{h}")).collect();
                if !outputs.is_empty() {
                    ui.weak(format!("output {}", outputs.join(", ")));
                }
            }
        });
        if let Some(error) = self.state.as_ref().and_then(|s| s.error.clone()) {
            ui.colored_label(egui::Color32::LIGHT_RED, format!("Preset error: {error}"));
        }
        if let Some((msg, is_error)) = self.message.clone() {
            let color = if is_error { egui::Color32::LIGHT_RED } else { egui::Color32::LIGHT_GREEN };
            let dismiss = ui
                .horizontal(|ui| {
                    ui.colored_label(color, msg.as_str());
                    ui.small_button("✕").clicked()
                })
                .inner;
            if dismiss {
                self.message = None;
            }
        }
    }

    fn preset_browser(&mut self, ui: &mut egui::Ui) {
        ui.heading("Presets");
        ui.horizontal(|ui| {
            ui.label("Folder");
            ui.add(egui::TextEdit::singleline(&mut self.shader_root).desired_width(f32::INFINITY));
        });
        ui.add(
            egui::TextEdit::singleline(&mut self.preset_filter)
                .hint_text("Search (e.g. crt royale)")
                .desired_width(f32::INFINITY),
        );
        let words: Vec<String> = self.preset_filter.to_lowercase().split_whitespace().map(String::from).collect();
        let shown: Vec<&PathBuf> = self
            .presets
            .iter()
            .filter(|p| {
                let s = p.to_string_lossy().to_lowercase();
                words.iter().all(|w| s.contains(w.as_str()))
            })
            .collect();
        ui.weak(format!("{} / {} presets", shown.len(), self.presets.len()));
        let running = self.state.as_ref().and_then(|s| s.preset.clone());
        let root = PathBuf::from(&self.shader_root);
        let mut clicked = None;
        let row_height = ui.text_style_height(&egui::TextStyle::Body);
        egui::ScrollArea::vertical().auto_shrink([false, false]).show_rows(ui, row_height, shown.len(), |ui, range| {
            for rel in &shown[range] {
                let abs = root.join(rel);
                let is_running = running.as_deref() == Some(abs.to_string_lossy().as_ref());
                if ui.selectable_label(is_running, rel.to_string_lossy().into_owned()).clicked() {
                    clicked = Some(abs);
                }
            }
        });
        if let Some(path) = clicked {
            if self.client.is_some() {
                self.send(Request::LoadPreset { path: path.display().to_string() });
            } else {
                self.error("no process connected");
            }
        }
    }

    fn source_panel(&mut self, ui: &mut egui::Ui) {
        let Some(edit) = self.source.as_mut() else { return };
        let mut changed = false;
        ui.horizontal_wrapped(|ui| {
            ui.label("Source resolution");
            changed |= ui.checkbox(&mut edit.native, "native").changed();
            ui.add_enabled_ui(!edit.native, |ui| {
                changed |= ui.add(egui::DragValue::new(&mut edit.width).range(1..=7680)).changed();
                ui.label("×");
                changed |= ui.add(egui::DragValue::new(&mut edit.height).range(1..=4320)).changed();
                for (w, h) in [(256, 224), (320, 240), (640, 480)] {
                    if ui.small_button(format!("{w}×{h}")).clicked() {
                        (edit.width, edit.height) = (w, h);
                        changed = true;
                    }
                }
            });
        });
        ui.horizontal_wrapped(|ui| {
            ui.label("Filter");
            changed |= ui.selectable_value(&mut edit.filter, Filter::Nearest, "nearest").changed();
            changed |= ui.selectable_value(&mut edit.filter, Filter::Linear, "linear").changed();
            ui.separator();
            ui.label("Picture area");
            let r = ui.add(egui::TextEdit::singleline(&mut edit.rect).desired_width(110.0));
            changed |= r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            for preset in ["full", "4:3", "16:9"] {
                if ui.small_button(preset).clicked() {
                    edit.rect = preset.into();
                    changed = true;
                }
            }
        });
        if changed {
            let source = edit.to_settings();
            self.send(Request::SetSource { source });
        }
    }

    fn params_panel(&mut self, ui: &mut egui::Ui) {
        let Some(state) = self.state.as_mut() else { return };
        ui.horizontal(|ui| {
            ui.heading("Parameters");
            ui.add(egui::TextEdit::singleline(&mut self.param_filter).hint_text("Filter").desired_width(160.0));
        });
        let filter = self.param_filter.to_lowercase();
        let mut requests = Vec::new();
        if ui.button("Reset all").clicked() {
            requests.push(Request::ResetParams);
        }
        egui::ScrollArea::vertical().auto_shrink([false, false]).max_height((ui.available_height() - 90.0).max(120.0)).show(
            ui,
            |ui| {
                if state.params.is_empty() {
                    ui.weak("This preset has no parameters.");
                }
                for p in &mut state.params {
                    let matches = filter.is_empty()
                        || p.name.to_lowercase().contains(&filter)
                        || p.description.to_lowercase().contains(&filter);
                    if !matches {
                        continue;
                    }
                    if p.is_header() {
                        if !p.description.trim().is_empty() {
                            ui.add_space(4.0);
                            ui.strong(p.description.trim());
                        }
                        continue;
                    }
                    ui.horizontal(|ui| {
                        let before = p.value;
                        let slider = egui::Slider::new(&mut p.value, p.minimum..=p.maximum)
                            .step_by(p.step.max(0.0001) as f64)
                            .text(p.description.trim());
                        let slider = ui.add(slider).on_hover_text(p.name.as_str());
                        if slider.changed() {
                            if (p.value - before).abs() > p.epsilon() {
                                requests.push(Request::SetParam { name: p.name.clone(), value: p.value });
                            } else {
                                // Only step snapping, not a user edit.
                                p.value = before;
                            }
                        }
                        if p.is_modified() && ui.small_button("↺").on_hover_text("Preset value").clicked() {
                            p.value = p.initial;
                            requests.push(Request::SetParam { name: p.name.clone(), value: p.initial });
                        }
                    });
                }
            },
        );
        for r in requests {
            self.send(r);
        }
    }

    fn save_panel(&mut self, ui: &mut egui::Ui) {
        let Some(state) = self.state.clone() else { return };
        ui.separator();
        ui.horizontal(|ui| {
            ui.label("Preset");
            ui.add(egui::TextEdit::singleline(&mut self.save_path).desired_width((ui.available_width() - 260.0).max(120.0)));
            if ui.button("Save .slangp").on_hover_text("#reference + changed parameters (RetroArch compatible)").clicked() {
                let path = PathBuf::from(&self.save_path);
                match save::save_slangp(&state, &path) {
                    Ok(()) => self.info(format!("saved {}", path.display())),
                    Err(e) => self.error(format!("save failed: {e}")),
                }
            }
            if ui.button("Save as default").on_hover_text(save::config_path().display().to_string()).clicked() {
                match save::save_config(&state) {
                    Ok(path) => self.info(format!("updated {}", path.display())),
                    Err(e) => self.error(format!("save failed: {e}")),
                }
            }
        });
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.tick();
        egui::Panel::top("top").show(ui, |ui| self.top_bar(ui));
        egui::Panel::left("presets").resizable(true).default_size(340.0).show(ui, |ui| self.preset_browser(ui));
        egui::CentralPanel::default().show(ui, |ui| {
            if self.client.is_none() {
                ui.heading("No process connected");
                ui.label("Start a Vulkan game with ENABLE_VKSLANG=1 and a preset; it will show up here.");
                ui.weak(format!("Sockets: {}", vkslang_ipc::socket_dir().display()));
                return;
            }
            if let Some(preset) = self.state.as_ref().and_then(|s| s.preset.clone()) {
                ui.label(egui::RichText::new(preset).monospace());
            }
            self.source_panel(ui);
            ui.separator();
            self.params_panel(ui);
            self.save_panel(ui);
        });
        ui.ctx().request_repaint_after(POLL);
    }
}

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("vkSlang")
            .with_inner_size([1000.0, 720.0])
            .with_min_inner_size([640.0, 400.0]),
        ..Default::default()
    };
    eframe::run_native("vkSlang", options, Box::new(|_cc| Ok(Box::new(App::new()))))
}
