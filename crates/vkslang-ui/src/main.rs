//! vkslang-ui — live control panel for the vkSlang Vulkan layer.
//!
//! Connects to `$XDG_RUNTIME_DIR/vkslang/<pid>.sock` of a running game (or
//! gamescope), and lets you switch presets, tweak parameters and the source
//! resolution while it runs, then save the result.

mod save;

use eframe::egui;
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use vkslang_ipc::{
    color_space_warning, Client, Filter, Request, Response, SourceSettings, SourceSize, State, GAMUT_NAMES,
};

const POLL: Duration = Duration::from_millis(400);
const SCAN: Duration = Duration::from_secs(2);

struct Target {
    pid: u32,
    path: PathBuf,
    label: String,
    /// The process is actually processing a swapchain. Gamescope, for
    /// instance, loads the layer but presents through Wayland, so there is
    /// nothing to control there.
    active: bool,
}

/// Editable copy of the source settings (not overwritten by polling while
/// the user edits it).
struct SourceEdit {
    mode: SourceSize,
    /// Kept while another mode is selected, so switching back restores them.
    width: u32,
    height: u32,
    divisor: f32,
    filter: Filter,
    rect: String,
}

impl SourceEdit {
    fn from(s: &SourceSettings) -> SourceEdit {
        let mut edit = SourceEdit {
            mode: s.res,
            width: 320,
            height: 240,
            divisor: 2.0,
            filter: s.filter,
            rect: s.rect.clone(),
        };
        match s.res {
            SourceSize::Fixed { size: [w, h] } => (edit.width, edit.height) = (w, h),
            SourceSize::Divide { by } => edit.divisor = by,
            SourceSize::Native => {}
        }
        edit
    }

    fn to_settings(&self) -> SourceSettings {
        SourceSettings { res: self.mode, filter: self.filter, rect: self.rect.trim().to_string() }
    }
}

struct App {
    targets: Vec<Target>,
    selected: Option<u32>,
    client: Option<Client>,
    state: Option<State>,
    message: Option<(String, bool)>,
    /// Processes that answered with something we cannot use (old protocol).
    incompatible: HashSet<u32>,
    /// Also list processes with no swapchain.
    show_all: bool,
    last_poll: Instant,
    last_scan: Instant,

    shader_root: String,
    presets: Vec<PathBuf>,
    tree: Tree,
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

/// Presets arranged as folders, the way they sit on disk.
#[derive(Default)]
struct Tree {
    /// Sub-folders, sorted by name.
    folders: BTreeMap<String, Tree>,
    /// `(file name, path relative to the root)`.
    presets: Vec<(String, PathBuf)>,
}

impl Tree {
    fn build(paths: &[PathBuf]) -> Tree {
        let mut root = Tree::default();
        for path in paths {
            let mut node = &mut root;
            let parts: Vec<_> = path.iter().collect();
            for dir in &parts[..parts.len().saturating_sub(1)] {
                node = node.folders.entry(dir.to_string_lossy().into_owned()).or_default();
            }
            if let Some(name) = parts.last() {
                node.presets.push((name.to_string_lossy().into_owned(), path.clone()));
            }
        }
        root.sort();
        root
    }

    fn sort(&mut self) {
        self.presets.sort();
        for folder in self.folders.values_mut() {
            folder.sort();
        }
    }

    /// Presets whose path contains every word, and the folders leading to
    /// them. `words` empty keeps everything.
    fn matches(&self, words: &[String]) -> bool {
        words.is_empty()
            || self.presets.iter().any(|(_, path)| {
                let s = path.to_string_lossy().to_lowercase();
                words.iter().all(|w| s.contains(w.as_str()))
            })
            || self.folders.values().any(|f| f.matches(words))
    }

    fn count(&self) -> usize {
        self.presets.len() + self.folders.values().map(Tree::count).sum::<usize>()
    }
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
            incompatible: HashSet::new(),
            show_all: false,
            last_poll: Instant::now() - POLL,
            last_scan: Instant::now() - SCAN,
            shader_root: default_shader_root(),
            presets: Vec::new(),
            tree: Tree::default(),
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
        let connected = self.selected;
        let active_now = self.state.as_ref().is_some_and(|s| !s.outputs.is_empty());
        self.targets = vkslang_ipc::list_sockets()
            .into_iter()
            .filter_map(|(pid, path)| {
                let Some(name) = process_name(pid) else {
                    // Process gone: clean the stale socket.
                    let _ = std::fs::remove_file(&path);
                    return None;
                };
                // Ask the others whether they have a swapchain; the connected
                // one is already known from its state.
                let active = if Some(pid) == connected {
                    active_now
                } else {
                    Client::connect(&path)
                        .ok()
                        .and_then(|mut c| c.request(&Request::GetState).ok())
                        .is_some_and(|r| matches!(r, Response::State(s) if !s.outputs.is_empty()))
                };
                Some(Target { pid, path, label: format!("{name} ({pid})"), active })
            })
            .collect();
        if self.selected.is_some_and(|pid| !self.targets.iter().any(|t| t.pid == pid)) {
            self.disconnect();
        }
        if self.selected.is_none() {
            // Skip processes running an incompatible layer or with nothing to
            // control.
            let candidates: Vec<u32> = self
                .targets
                .iter()
                .filter(|t| t.active && !self.incompatible.contains(&t.pid))
                .map(|t| t.pid)
                .collect();
            for pid in candidates {
                self.select(pid);
                if self.client.is_some() {
                    break;
                }
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
                if e.kind() == std::io::ErrorKind::InvalidData {
                    if let Some(pid) = self.selected {
                        self.incompatible.insert(pid);
                    }
                    self.error(format!("{e}"));
                } else {
                    self.error(format!("connection lost: {e}"));
                }
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
            self.tree = Tree::build(&self.presets);
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
            let hidden = self.targets.iter().filter(|t| !t.active).count();
            let show_all = self.show_all;
            egui::ComboBox::from_id_salt("target").selected_text(current).width(220.0).show_ui(ui, |ui| {
                for t in self.targets.iter().filter(|t| t.active || show_all) {
                    let label =
                        if t.active { t.label.clone() } else { format!("{} (no swapchain)", t.label) };
                    ui.selectable_value(&mut choice, Some(t.pid), label);
                }
            });
            if hidden > 0 {
                ui.checkbox(&mut self.show_all, format!("+{hidden} idle"))
                    .on_hover_text("Also list processes that load the layer but present no swapchain (gamescope…)");
            }
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
                let outputs: Vec<String> = state
                    .outputs
                    .iter()
                    .map(|o| format!("{}×{} {}", o.size[0], o.size[1], o.color_space.label()))
                    .collect();
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
        ui.weak(format!("{} presets", self.tree.count()));

        let running = self.state.as_ref().and_then(|s| s.preset.clone());
        let root = PathBuf::from(&self.shader_root);
        let mut clicked = None;
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            draw_tree(ui, &self.tree, &words, &root, running.as_deref(), &mut clicked);
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
            let native = matches!(edit.mode, SourceSize::Native);
            let divide = matches!(edit.mode, SourceSize::Divide { .. });
            if ui.selectable_label(native, "native").clicked() && !native {
                edit.mode = SourceSize::Native;
                changed = true;
            }
            if ui.selectable_label(divide, "divide").on_hover_text("Native size divided by N").clicked()
                && !divide
            {
                edit.mode = SourceSize::Divide { by: edit.divisor };
                changed = true;
            }
            if ui.selectable_label(!native && !divide, "fixed").clicked() && (native || divide) {
                edit.mode = SourceSize::Fixed { size: [edit.width, edit.height] };
                changed = true;
            }

            match edit.mode {
                SourceSize::Divide { .. } => {
                    ui.label("÷");
                    if ui
                        .add(egui::DragValue::new(&mut edit.divisor).speed(0.05).range(1.0..=16.0).max_decimals(2))
                        .changed()
                    {
                        edit.mode = SourceSize::Divide { by: edit.divisor };
                        changed = true;
                    }
                    for by in [2.0, 3.0, 4.0, 6.0] {
                        if ui.small_button(format!("÷{by:.0}")).clicked() {
                            edit.divisor = by;
                            edit.mode = SourceSize::Divide { by };
                            changed = true;
                        }
                    }
                }
                SourceSize::Fixed { .. } => {
                    let mut edited = ui.add(egui::DragValue::new(&mut edit.width).range(1..=7680)).changed();
                    ui.label("×");
                    edited |= ui.add(egui::DragValue::new(&mut edit.height).range(1..=4320)).changed();
                    for (w, h) in [(256, 224), (320, 240), (640, 480)] {
                        if ui.small_button(format!("{w}×{h}")).clicked() {
                            (edit.width, edit.height) = (w, h);
                            edited = true;
                        }
                    }
                    if edited {
                        edit.mode = SourceSize::Fixed { size: [edit.width, edit.height] };
                        changed = true;
                    }
                }
                SourceSize::Native => {}
            }
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

    fn hdr_panel(&mut self, ui: &mut egui::Ui) {
        let Some(state) = self.state.as_ref() else { return };
        let output = state.outputs.first().map(|o| o.color_space);
        let preset = state.preset_color_space;
        let mut hdr = state.hdr;
        let mut changed = false;

        ui.horizontal_wrapped(|ui| {
            ui.label("HDR");
            for o in &state.outputs {
                let promoted = if o.promoted { ", promoted by vkSlang" } else { "" };
                ui.weak(format!("output {} ({}{})", o.color_space.label(), o.format, promoted));
            }
            if let Some(p) = preset {
                ui.weak(format!("· preset {}", p.label()));
            }
        });
        let promoted = state.outputs.first().is_some_and(|o| o.promoted);
        if let (Some(p), Some(o)) = (preset, output) {
            if let Some(why) = color_space_warning(p, o, promoted) {
                ui.colored_label(egui::Color32::from_rgb(255, 170, 60), format!("⚠ {why}"));
            }
        }
        // Only meaningful for HDR-aware presets (HDRMode != 0).
        let hdr_active = preset.is_some_and(|p| p.is_hdr()) || output.is_some_and(|o| o.is_hdr());
        ui.add_enabled_ui(hdr_active, |ui| {
            ui.horizontal_wrapped(|ui| {
                changed |= ui
                    .add(
                        egui::Slider::new(&mut hdr.brightness_nits, 80.0..=2000.0)
                            .logarithmic(true)
                            .step_by(10.0)
                            .suffix(" nits")
                            .text("Paper white"),
                    )
                    .on_hover_text("BrightnessNits: SDR reference white")
                    .changed();
                ui.separator();
                ui.label("Gamut");
                for (i, name) in GAMUT_NAMES.iter().enumerate() {
                    changed |= ui.selectable_value(&mut hdr.expand_gamut, i as u32, *name).changed();
                }
            });
        });
        if changed {
            self.send(Request::SetHdr { hdr });
        }
    }

    fn presentation_panel(&mut self, ui: &mut egui::Ui) {
        let Some(state) = self.state.as_ref() else { return };
        let (mut subframes, mut black) = (state.subframes, state.subframe_black);
        let mut changed = false;

        ui.horizontal_wrapped(|ui| {
            ui.label("Presentations per frame");
            let (source, presented) = (state.source_fps, state.present_fps);
            changed |= ui
                .add(egui::Slider::new(&mut subframes, 1..=8).integer())
                .on_hover_text(
                    "Present each frame several times so interlacing presets alternate fields \
                     faster than the game draws. 3 suits a 60 Hz game on a 240 Hz display.",
                )
                .changed();
            ui.weak(format!("· {source:.0} fps source, {presented:.0} presented/s"));
            ui.add_enabled_ui(subframes > 1, |ui| {
                changed |= ui.selectable_value(&mut black, false, "preset").changed();
                changed |= ui
                    .selectable_value(&mut black, true, "black (BFI)")
                    .on_hover_text("Insert black frames instead of running the preset again")
                    .changed();
            });
        });
        if changed {
            self.send(Request::SetSubframes { subframes, black });
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
                            .max_decimals(4)
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

/// Draws one level of the tree; folders are collapsed unless a search is
/// running, in which case only matching branches are shown, opened.
fn draw_tree(
    ui: &mut egui::Ui,
    tree: &Tree,
    words: &[String],
    root: &Path,
    running: Option<&str>,
    clicked: &mut Option<PathBuf>,
) {
    let searching = !words.is_empty();
    for (name, folder) in &tree.folders {
        if !folder.matches(words) {
            continue;
        }
        egui::CollapsingHeader::new(name)
            .id_salt(name)
            .default_open(searching)
            .open(searching.then_some(true))
            .show(ui, |ui| draw_tree(ui, folder, words, root, running, clicked));
    }
    for (name, rel) in &tree.presets {
        if searching {
            let haystack = rel.to_string_lossy().to_lowercase();
            if !words.iter().all(|w| haystack.contains(w.as_str())) {
                continue;
            }
        }
        let abs = root.join(rel);
        let is_running = running == Some(abs.to_string_lossy().as_ref());
        if ui.selectable_label(is_running, name.as_str()).clicked() {
            *clicked = Some(abs);
        }
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
            self.hdr_panel(ui);
            ui.separator();
            self.presentation_panel(ui);
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
