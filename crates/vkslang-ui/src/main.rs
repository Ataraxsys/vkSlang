//! vkslang-ui — live control panel for the vkSlang Vulkan layer.
//!
//! Connects to `$XDG_RUNTIME_DIR/vkslang/<pid>.sock` of a running game (or
//! gamescope), and lets you switch presets, tweak parameters and the source
//! resolution while it runs, then save the result.

mod detect;
mod i18n;
mod image;
mod plan;
mod profile;
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

/// How many screen pixels one game pixel covers: the divisor that brings the
/// screen back to the game's own grid. Integer scaling keeps whole screen
/// pixels per game pixel; otherwise the picture is fitted to the screen.
fn game_scale(screen: [u32; 2], game: [u32; 2], integer: bool) -> f32 {
    let [gw, gh] = [game[0].max(1), game[1].max(1)];
    if integer {
        (screen[0] / gw).min(screen[1] / gh).max(1) as f32
    } else {
        (screen[0] as f32 / gw as f32).min(screen[1] as f32 / gh as f32).max(1.0)
    }
}

/// Where the game sits on the screen, `[x, y, width, height]`: scaled by
/// [`game_scale`] and centred, as gamescope places it.
fn game_rect(screen: [u32; 2], game: [u32; 2], integer: bool) -> [u32; 4] {
    let by = game_scale(screen, game, integer);
    let w = ((game[0].max(1) as f32 * by).round() as u32).min(screen[0]);
    let h = ((game[1].max(1) as f32 * by).round() as u32).min(screen[1]);
    [(screen[0] - w) / 2, (screen[1] - h) / 2, w, h]
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    Shader,
    Image,
    Display,
    Profiles,
}

#[derive(Clone, PartialEq)]
struct ChainEntry {
    path: PathBuf,
    enabled: bool,
}

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
    /// Each source pixel repeated this many times per axis.
    duplicate: [u32; 2],
    /// Kept while another mode is selected, so switching back restores them.
    width: u32,
    height: u32,
    divisor: f32,
    filter: Filter,
    rect: String,
    display: String,
    display_scale: [f32; 2],
    /// Move both axes together, keeping the ratio they had when locked.
    scale_locked: bool,
    /// That ratio, vertical over horizontal.
    scale_ratio: f32,
    /// Same, for the picture inside the drawn area.
    source_scale: [f32; 2],
    source_locked: bool,
    source_ratio: f32,
    /// Divisor calculator: screen size (`None` follows the output), the
    /// game's resolution, and whether the game is integer-scaled.
    calc_screen: Option<[u32; 2]>,
    calc_game: [u32; 2],
    calc_integer: bool,
}

impl SourceEdit {
    fn from(s: &SourceSettings) -> SourceEdit {
        let mut edit = SourceEdit {
            mode: s.res,
            duplicate: s.duplicate,
            width: 320,
            height: 240,
            divisor: 2.0,
            filter: s.filter,
            rect: s.rect.clone(),
            display: s.display.clone(),
            display_scale: s.display_scale,
            source_scale: s.source_scale,
            source_locked: true,
            source_ratio: if s.source_scale[0] > 0.0 { s.source_scale[1] / s.source_scale[0] } else { 1.0 },
            scale_locked: true,
            scale_ratio: if s.display_scale[0] > 0.0 { s.display_scale[1] / s.display_scale[0] } else { 1.0 },
            calc_screen: None,
            calc_game: [640, 480],
            calc_integer: true,
        };
        match s.res {
            SourceSize::Fixed { size: [w, h] } => (edit.width, edit.height) = (w, h),
            SourceSize::Divide { by } => edit.divisor = by,
            SourceSize::Native => {}
        }
        edit
    }

    fn to_settings(&self) -> SourceSettings {
        SourceSettings {
            res: self.mode,
            duplicate: self.duplicate,
            filter: self.filter,
            rect: self.rect.trim().to_string(),
            display: self.display.trim().to_string(),
            display_scale: self.display_scale,
            source_scale: self.source_scale,
        }
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
    /// Presets ticked in the browser, in the order they will run. A
    /// disabled entry stays in the list but is left out of the chain, so it
    /// can be switched off without losing its place or anyone's settings.
    chain: Vec<ChainEntry>,

    param_filter: String,
    /// Language of the panel, remembered between runs.
    lang: i18n::Lang,
    tab: Tab,
    /// Capture, picture plan and assistant.
    image: image::ImageState,
    source: Option<SourceEdit>,
    save_path: String,
    /// Named profiles, and the parameters waiting for a chain to finish
    /// compiling before they can be applied.
    profiles: Vec<profile::Profile>,
    profile_name: String,
    pending_params: Option<std::collections::BTreeMap<String, f32>>,
    /// Profile whose 🗙 was clicked, waiting for a second click.
    confirm_delete: Option<String>,
    /// Default profile of a process as read from vkSlang.conf, kept rather
    /// than reading the file on every repaint. `None` until read.
    default_profile: Option<(String, Option<String>)>,
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
            chain: Vec::new(),
            param_filter: String::new(),
            lang: i18n::Prefs::load().lang,
            tab: Tab::Image,
            image: image::ImageState::default(),
            source: None,
            save_path: String::new(),
            profiles: profile::read_dir(&profile::dir()).0,
            profile_name: String::new(),
            pending_params: None,
            confirm_delete: None,
            default_profile: None,
        }
    }

    /// Reloads the profiles, reporting any file that could not be read
    /// rather than leaving the user wondering where a profile went.
    fn reload_profiles(&mut self) {
        let (profiles, failures) = profile::read_dir(&profile::dir());
        self.profiles = profiles;
        // vkSlang.conf may have been edited by hand too.
        self.default_profile = None;
        if !failures.is_empty() {
            self.error(format!(
                "{} {}",
                self.lang.t("profil(s) illisible(s) :", "unreadable profile(s):"),
                failures.join("; ")
            ));
        }
    }

    fn error(&mut self, msg: impl Into<String>) {
        self.message = Some((msg.into(), true));
    }

    fn info(&mut self, msg: impl Into<String>) {
        self.message = Some((msg.into(), false));
    }

    fn scan_targets(&mut self) {
        vkslang_ipc::remove_stale_files();
        let connected = self.selected;
        let active_now = self.state.as_ref().is_some_and(|s| !s.outputs.is_empty());
        self.targets = vkslang_ipc::list_sockets()
            .into_iter()
            .filter_map(|(pid, path)| {
                // Process gone between the sweep and now: skip it, the next
                // sweep removes its files.
                let name = process_name(pid)?;
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
            Err(e) => self.error(format!(
                "{} {}: {e}",
                self.lang.t("connexion impossible à", "cannot connect to"),
                path.display()
            )),
        }
    }

    fn send(&mut self, request: Request) {
        let Some(client) = self.client.as_mut() else { return };
        match client.request(&request) {
            Ok(Response::State(state)) => {
                if self.source.is_none() {
                    self.source = Some(SourceEdit::from(&state.source));
                }
                if self.save_path.is_empty() || self.state.as_ref().map(|s| &s.presets) != Some(&state.presets) {
                    self.save_path =
                        save::default_preset_path(state.presets.first().map(String::as_str)).display().to_string();
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
                    self.error(format!("{} {e}", self.lang.t("connexion perdue :", "connection lost:")));
                }
                self.disconnect();
                self.last_scan = Instant::now() - SCAN;
            }
        }
    }

    /// Applies a saved profile: the chain first, its parameters once it has
    /// finished compiling.
    ///
    /// The layer keeps parameter tweaks across chain changes, so they are
    /// reset first: a profile holds only the parameters it changed, and the
    /// previous look's tweaks would otherwise leak into it.
    fn apply_profile(&mut self, p: &profile::Profile) {
        if self.client.is_none() {
            self.error(self.lang.t("aucun jeu connecté", "no game connected"));
            return;
        }
        self.chain = p.presets.iter().map(|s| ChainEntry { path: PathBuf::from(s), enabled: true }).collect();
        self.send(Request::ResetParams);
        self.send(Request::SetEnabled { enabled: true });
        self.send(Request::LoadPresets { paths: p.presets.clone() });
        self.send(Request::SetSource { source: p.source.clone() });
        self.send(Request::SetHdr { hdr: p.hdr });
        self.send(Request::SetSubframes { subframes: p.subframes, black: p.subframe_black });
        self.source = Some(SourceEdit::from(&p.source));
        self.pending_params = Some(p.params.clone());
        self.info(format!("{} « {} »", self.lang.t("profil appliqué :", "profile applied:"), p.name));
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
        // The chain has finished compiling: the profile's parameters can go.
        if self.state.as_ref().is_some_and(|s| !s.loading && !s.params.is_empty()) {
            if let Some(params) = self.pending_params.take() {
                for (name, value) in params {
                    self.send(Request::SetParam { name, value });
                }
            }
        }
        if self.scanned_root.as_deref() != Some(self.shader_root.as_str()) {
            self.presets = scan_presets(Path::new(&self.shader_root));
            self.tree = Tree::build(&self.presets);
            self.scanned_root = Some(self.shader_root.clone());
        }
    }

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        let l = self.lang;
        ui.horizontal(|ui| {
            ui.strong("vkSlang");
            ui.separator();
            let current = self
                .selected
                .and_then(|pid| self.targets.iter().find(|t| t.pid == pid))
                .map_or(l.t("aucun jeu", "no game").to_string(), |t| t.label.clone());
            let mut choice = self.selected;
            let hidden = self.targets.iter().filter(|t| !t.active).count();
            let show_all = self.show_all;
            egui::ComboBox::from_id_salt("target").selected_text(current).width(220.0).show_ui(ui, |ui| {
                for t in self.targets.iter().filter(|t| t.active || show_all) {
                    let label = if t.active {
                        t.label.clone()
                    } else {
                        format!("{} ({})", t.label, l.t("rien à l'écran", "nothing on screen"))
                    };
                    ui.selectable_value(&mut choice, Some(t.pid), label);
                }
            });
            if hidden > 0 {
                ui.checkbox(&mut self.show_all, format!("+{hidden} {}", l.t("inactifs", "idle"))).on_hover_text(l.t(
                    "Aussi les programmes qui chargent vkSlang sans rien afficher (gamescope sans --backend sdl…)",
                    "Also programs that load vkSlang but show nothing (gamescope without --backend sdl…)",
                ));
            }
            if choice != self.selected {
                if let Some(pid) = choice {
                    self.select(pid);
                }
            }
            if ui.button("⟳").on_hover_text(l.t("Chercher les jeux", "Look for games")).clicked() {
                self.scan_targets();
            }
            if let Some(mut enabled) = self.state.as_ref().map(|s| s.enabled) {
                if ui
                    .checkbox(&mut enabled, l.t("Shader actif", "Shader on"))
                    .on_hover_text(l.t("Comparer avec et sans", "Compare with and without"))
                    .changed()
                {
                    self.send(Request::SetEnabled { enabled });
                }
            }
            if self.state.as_ref().is_some_and(|s| s.loading) {
                ui.spinner();
                ui.label(l.t("compilation…", "compiling…"));
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let mut lang = self.lang;
                ui.selectable_value(&mut lang, i18n::Lang::En, "EN");
                ui.selectable_value(&mut lang, i18n::Lang::Fr, "FR");
                if lang != self.lang {
                    self.lang = lang;
                    i18n::Prefs { lang }.save();
                }
                ui.separator();
                if ui
                    .add_enabled(
                        self.client.is_some(),
                        egui::Button::new(
                            egui::RichText::new(l.t("🎯 Configurer ce jeu", "🎯 Set up this game")).strong(),
                        ),
                    )
                    .on_hover_text(l.t(
                        "Pas à pas : zone du jeu, pixels, forme, shader, profil",
                        "Step by step: game zone, pixels, shape, shader, profile",
                    ))
                    .clicked()
                {
                    self.open_wizard();
                }
            });
        });
        if let Some(error) = self.state.as_ref().and_then(|s| s.error.clone()) {
            ui.colored_label(
                egui::Color32::LIGHT_RED,
                format!("{} {error}", l.t("Erreur du preset :", "Preset error:")),
            );
        }
        if let Some((msg, is_error)) = self.message.clone() {
            let color = if is_error { egui::Color32::LIGHT_RED } else { egui::Color32::LIGHT_GREEN };
            let dismiss = ui
                .horizontal(|ui| {
                    ui.colored_label(color, msg.as_str());
                    ui.small_button("🗙").clicked()
                })
                .inner;
            if dismiss {
                self.message = None;
            }
        }
    }

    fn preset_browser(&mut self, ui: &mut egui::Ui) {
        let l = self.lang;
        ui.heading(l.t("Presets", "Presets"));
        ui.horizontal(|ui| {
            ui.label(l.t("Dossier", "Folder"));
            ui.add(egui::TextEdit::singleline(&mut self.shader_root).desired_width(f32::INFINITY));
        });
        ui.add(
            egui::TextEdit::singleline(&mut self.preset_filter)
                .hint_text(l.t("Rechercher (ex. crt royale)", "Search (e.g. crt royale)"))
                .desired_width(f32::INFINITY),
        );
        let words: Vec<String> = self.preset_filter.to_lowercase().split_whitespace().map(String::from).collect();
        ui.weak(format!("{} presets", self.tree.count()));

        let running: Vec<String> = self.state.as_ref().map(|s| s.presets.clone()).unwrap_or_default();
        let root = PathBuf::from(&self.shader_root);
        let mut clicked = None;
        let mut chain = std::mem::take(&mut self.chain);
        let mut apply_chain = false;

        if !chain.is_empty() {
            ui.separator();
            ui.horizontal(|ui| {
                ui.strong(format!("{} ({})", l.t("Chaîne", "Chain"), chain.len()));
                apply_chain = ui.button(l.t("Appliquer", "Apply")).clicked();
                if ui.button(l.t("Vider", "Clear")).clicked() {
                    chain.clear();
                }
            });
            let mut swap = None;
            let mut remove = None;
            let mut toggled = false;
            for (i, entry) in chain.iter_mut().enumerate() {
                ui.horizontal(|ui| {
                    let name = entry.path.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
                    ui.weak(format!("{}.", i + 1));
                    // Switching one off leaves the others, and their
                    // parameters, exactly as they are.
                    toggled |= ui
                        .checkbox(&mut entry.enabled, "")
                        .on_hover_text(l.t("Activer celui-ci dans la chaîne", "Run this one in the chain"))
                        .changed();
                    if ui.small_button("↑").clicked() && i > 0 {
                        swap = Some((i - 1, i));
                    }
                    if ui.small_button("↓").clicked() {
                        swap = Some((i, i + 1));
                    }
                    if ui.small_button("🗙").clicked() {
                        remove = Some(i);
                    }
                    let label = if entry.enabled {
                        egui::RichText::new(name)
                    } else {
                        egui::RichText::new(name).weak().strikethrough()
                    };
                    ui.label(label).on_hover_text(entry.path.display().to_string());
                });
            }
            // A change to the chain applies straight away: waiting for
            // Apply after unticking would be surprising.
            apply_chain |= toggled || swap.is_some() || remove.is_some();
            if let Some((a, b)) = swap.filter(|(_, b)| *b < chain.len()) {
                chain.swap(a, b);
            }
            if let Some(i) = remove {
                chain.remove(i);
            }
            ui.separator();
        }

        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            draw_tree(ui, &self.tree, &words, &root, &running, &mut clicked, &mut chain, l);
        });
        self.chain = chain;

        if self.client.is_none() && (clicked.is_some() || apply_chain) {
            self.error(l.t("aucun jeu connecté", "no game connected"));
        } else if let Some(path) = clicked {
            // A plain click runs that preset on its own.
            self.chain = vec![ChainEntry { path: path.clone(), enabled: true }];
            self.send(Request::LoadPresets { paths: vec![path.display().to_string()] });
        } else if apply_chain {
            let paths: Vec<String> =
                self.chain.iter().filter(|e| e.enabled).map(|e| e.path.display().to_string()).collect();
            if paths.is_empty() {
                // Nothing left to run: bypass rather than fail.
                self.send(Request::SetEnabled { enabled: false });
            } else {
                self.send(Request::SetEnabled { enabled: true });
                self.send(Request::LoadPresets { paths });
            }
        }
    }

    fn source_panel(&mut self, ui: &mut egui::Ui) {
        let l = self.lang;
        let mut changed = false;
        if let Some(o) = self.state.as_ref().and_then(|s| s.outputs.first()).cloned() {
            let ([w, h], [pw, ph], [iw, ih]) = (o.size, o.picture, o.input);
            let picture =
                if [pw, ph] == [w, h] { String::new() } else { format!("{} {pw}×{ph}, ", l.t("image", "picture")) };
            ui.weak(format!(
                "{} {w}×{h}, {picture}{} {iw}×{ih}",
                l.t("écran", "screen"),
                l.t("entrée du shader", "shader input")
            ));
        }
        let output_size = self.state.as_ref().and_then(|s| s.outputs.first()).map(|o| o.size);
        let Some(edit) = self.source.as_mut() else { return };
        ui.horizontal_wrapped(|ui| {
            ui.label(l.t("Résolution source", "Source resolution"));
            let native = matches!(edit.mode, SourceSize::Native);
            let divide = matches!(edit.mode, SourceSize::Divide { .. });
            if ui.selectable_label(native, l.t("native", "native")).clicked() && !native {
                edit.mode = SourceSize::Native;
                changed = true;
            }
            if ui
                .selectable_label(divide, l.t("divisée", "divide"))
                .on_hover_text(l.t("Taille native divisée par N", "Native size divided by N"))
                .clicked()
                && !divide
            {
                edit.mode = SourceSize::Divide { by: edit.divisor };
                changed = true;
            }
            if ui.selectable_label(!native && !divide, l.t("fixe", "fixed")).clicked() && (native || divide) {
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
        if matches!(edit.mode, SourceSize::Divide { .. }) {
            ui.horizontal_wrapped(|ui| {
                ui.label(l.t("Trouver le diviseur :", "Find the divisor:")).on_hover_text(l.t(
                    "Pixels d'écran par pixel du jeu, d'après les résolutions de l'écran et du jeu. \
                     En échelle entière, chaque pixel du jeu couvre un nombre entier de pixels d'écran.",
                    "Screen pixels per game pixel, from the screen and game resolutions. \
                     With integer scaling each game pixel covers a whole number of screen pixels.",
                ));
                let mut screen = edit.calc_screen.or(output_size).unwrap_or([3840, 2160]);
                ui.label(l.t("écran", "screen"));
                let mut edited = ui.add(egui::DragValue::new(&mut screen[0]).range(1..=7680)).changed();
                ui.label("×");
                edited |= ui.add(egui::DragValue::new(&mut screen[1]).range(1..=4320)).changed();
                if edited {
                    edit.calc_screen = Some(screen);
                }
                if edit.calc_screen.is_some()
                    && output_size.is_some()
                    && ui
                        .small_button("↺")
                        .on_hover_text(l.t("Revenir à la taille de sortie", "Back to the output size"))
                        .clicked()
                {
                    edit.calc_screen = None;
                }
                ui.label(l.t("jeu", "game"));
                ui.add(egui::DragValue::new(&mut edit.calc_game[0]).range(1..=7680));
                ui.label("×");
                ui.add(egui::DragValue::new(&mut edit.calc_game[1]).range(1..=4320));
                ui.checkbox(&mut edit.calc_integer, l.t("échelle entière", "integer scale"));
                let by = game_scale(screen, edit.calc_game, edit.calc_integer);
                let [x, y, w, h] = game_rect(screen, edit.calc_game, edit.calc_integer);
                if ui
                    .button(format!("{} ÷{}", l.t("utiliser", "use"), (by * 100.0).round() / 100.0))
                    .on_hover_text(format!("{} {w}×{h}", l.t("Le jeu est affiché en", "The game is shown at")))
                    .clicked()
                {
                    edit.divisor = by;
                    edit.mode = SourceSize::Divide { by };
                    changed = true;
                }
                if ui
                    .button(l.t("cadrer le jeu", "frame the game"))
                    .on_hover_text(format!(
                        "{w}×{h} @ {x},{y}, ÷{} — {}",
                        (by * 100.0).round() / 100.0,
                        l.t(
                            "Lire et dessiner seulement là où est le jeu : le preset voit exactement ses pixels.",
                            "Read and draw only where the game is: the preset sees exactly its pixels.",
                        )
                    ))
                    .clicked()
                {
                    let area = format!("{x},{y},{w}x{h}");
                    edit.rect = area.clone();
                    edit.display = area;
                    edit.divisor = by;
                    edit.mode = SourceSize::Divide { by };
                    edit.source_scale = [1.0, 1.0];
                    edit.source_ratio = 1.0;
                    edit.display_scale = [1.0, 1.0];
                    edit.scale_ratio = 1.0;
                    changed = true;
                }
            });
        }
        ui.horizontal_wrapped(|ui| {
            ui.label(l.t("Échelle de l'image", "Picture scale")).on_hover_text(l.t(
                "Taille de l'image dans la zone : une région plus petite est lue, l'image grandit \
                 et le preset garde les mêmes scanlines et le même masque (×2 en vertical, par exemple)",
                "Size of the picture inside that area: a smaller region is read, so the picture grows \
                 while the preset keeps the same scanlines and mask (×2 vertically, for instance)",
            ));
            let mut moved: Option<usize> = None;
            for (axis, label) in [(0usize, "↔"), (1usize, "↕")] {
                if ui
                    .add(
                        egui::Slider::new(&mut edit.source_scale[axis], 0.25..=4.0)
                            .step_by(0.01)
                            .fixed_decimals(2)
                            .text(label),
                    )
                    .changed()
                {
                    moved = Some(axis);
                }
            }
            if let Some(axis) = moved {
                if edit.source_locked {
                    let ratio = edit.source_ratio.max(0.01);
                    let other = if axis == 0 { edit.source_scale[0] * ratio } else { edit.source_scale[1] / ratio };
                    edit.source_scale[1 - axis] = other.clamp(0.25, 4.0);
                }
                changed = true;
            }
            if ui
                .checkbox(&mut edit.source_locked, "lock")
                .on_hover_text(
                    l.t("Garder le rapport actuel entre les deux axes", "Keep the current ratio between the two axes"),
                )
                .changed()
                && edit.source_locked
            {
                edit.source_ratio =
                    if edit.source_scale[0] > 0.0 { edit.source_scale[1] / edit.source_scale[0] } else { 1.0 };
            }
            if ui.small_button("1:1").clicked() {
                edit.source_scale = [1.0, 1.0];
                edit.source_ratio = 1.0;
                changed = true;
            }
        });
        ui.horizontal_wrapped(|ui| {
            ui.label(l.t("Dupliquer les pixels", "Duplicate pixels"))
                .on_hover_text(l.t("Répète chaque pixel source, pour les modes aux pixels non carrés (DOS 320×200 : ↕ 2 donne 400 vraies lignes au preset)", "Repeat each source pixel, for modes whose pixels are not square (DOS 320×200: ↕ 2 gives the preset 400 real lines)"));
            for (axis, label) in [(0usize, "↔"), (1usize, "↕")] {
                changed |= ui
                    .add(egui::DragValue::new(&mut edit.duplicate[axis]).range(1..=8).prefix(label))
                    .changed();
            }
            ui.separator();
            ui.label(l.t("Filtre", "Filter"));
            changed |= ui.selectable_value(&mut edit.filter, Filter::Nearest, l.t("net", "nearest")).changed();
            changed |= ui.selectable_value(&mut edit.filter, Filter::Linear, l.t("lissé", "linear")).changed();
            ui.separator();
            ui.label(l.t("Zone lue", "Picture area"));
            let r = ui.add(egui::TextEdit::singleline(&mut edit.rect).desired_width(110.0));
            changed |= r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            for preset in ["full", "4:3", "16:9"] {
                if ui.small_button(preset).clicked() {
                    edit.rect = preset.into();
                    changed = true;
                }
            }
        });
        ui.horizontal_wrapped(|ui| {
            ui.label(l.t("Zone dessinée", "Display area"))
                .on_hover_text(l.t("Où le preset dessine. Différente de la zone lue, elle étire l'image : une source 640×360 dessinée en 4:3 donne des pixels non carrés, scanlines étirées avec.", "Where the preset draws. Different from the picture area it stretches the image: a 640×360 source drawn into a 4:3 area gives non-square pixels, scanlines stretched with it."));
            let r = ui.add(egui::TextEdit::singleline(&mut edit.display).desired_width(110.0));
            changed |= r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            for preset in ["full", "4:3", "16:9", "5:4"] {
                if ui.small_button(preset).clicked() {
                    edit.display = preset.into();
                    changed = true;
                }
            }
        });
        // A row of its own: egui cannot wrap a slider, and sharing the row
        // pushed the second one past the window, widening the whole panel.
        ui.horizontal_wrapped(|ui| {
            // Two different things, kept apart on purpose: one moves the
            // drawn area (preset and picture together), the other moves the
            // picture inside it (the preset keeps its geometry).
            ui.label(l.t("Échelle de la zone", "Area scale")).on_hover_text(l.t(
                "Taille de la zone où dessine le preset : le preset et l'image grandissent ensemble",
                "Size of the area the preset draws into: it carries the preset and the picture together",
            ));
            let mut moved: Option<usize> = None;
            for (axis, label) in [(0usize, "↔"), (1usize, "↕")] {
                if ui
                    .add(
                        egui::Slider::new(&mut edit.display_scale[axis], 0.25..=2.0)
                            .step_by(0.01)
                            .fixed_decimals(2)
                            .text(label),
                    )
                    .changed()
                {
                    moved = Some(axis);
                }
            }
            if let Some(axis) = moved {
                if edit.scale_locked {
                    // Follow the ratio rather than forcing both to be equal.
                    let ratio = edit.scale_ratio.max(0.01);
                    let other = if axis == 0 { edit.display_scale[0] * ratio } else { edit.display_scale[1] / ratio };
                    edit.display_scale[1 - axis] = other.clamp(0.25, 2.0);
                }
                changed = true;
            }
            if ui
                .checkbox(&mut edit.scale_locked, "lock")
                .on_hover_text(
                    l.t("Garder le rapport actuel entre les deux axes", "Keep the current ratio between the two axes"),
                )
                .changed()
                && edit.scale_locked
            {
                edit.scale_ratio =
                    if edit.display_scale[0] > 0.0 { edit.display_scale[1] / edit.display_scale[0] } else { 1.0 };
            }
            if ui
                .small_button("1:1")
                .on_hover_text(l.t("Revenir à la pleine taille, carrée", "Back to full size, square"))
                .clicked()
            {
                edit.display_scale = [1.0, 1.0];
                edit.scale_ratio = 1.0;
                changed = true;
            }
        });
        if changed {
            let source = edit.to_settings();
            self.send(Request::SetSource { source });
        }
    }

    fn hdr_panel(&mut self, ui: &mut egui::Ui) {
        let l = self.lang;
        let Some(state) = self.state.as_ref() else { return };
        let output = state.outputs.first().map(|o| o.color_space);
        let preset = state.preset_color_space;
        let mut hdr = state.hdr;
        let mut changed = false;

        ui.horizontal_wrapped(|ui| {
            ui.label("HDR");
            for o in &state.outputs {
                let promoted = if o.promoted { l.t(", promue par vkSlang", ", promoted by vkSlang") } else { "" };
                ui.weak(format!("{} {} ({}{})", l.t("sortie", "output"), o.color_space.label(), o.format, promoted));
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
                            .text(l.t("Blanc papier", "Paper white")),
                    )
                    .on_hover_text(
                        l.t("BrightnessNits : blanc de référence SDR", "BrightnessNits: SDR reference white"),
                    )
                    .changed();
                ui.separator();
                ui.label(l.t("Gamut", "Gamut"));
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
        let l = self.lang;
        let Some(state) = self.state.as_ref() else { return };
        let (mut subframes, mut black) = (state.subframes, state.subframe_black);
        let mut changed = false;

        ui.horizontal_wrapped(|ui| {
            ui.label(l.t("Présentations par image", "Presentations per frame"));
            let (source, presented) = (state.source_fps, state.present_fps);
            changed |= ui
                .add(egui::Slider::new(&mut subframes, 1..=8).integer())
                .on_hover_text(l.t(
                    "Présente chaque image plusieurs fois pour que les presets entrelacés alternent les trames \
                     plus vite que le jeu. 3 convient à un jeu 60 Hz sur un écran 240 Hz.",
                    "Present each frame several times so interlacing presets alternate fields \
                     faster than the game draws. 3 suits a 60 Hz game on a 240 Hz display.",
                ))
                .changed();
            ui.weak(format!("· {source:.0} fps source, {presented:.0} presented/s"));
            ui.add_enabled_ui(subframes > 1, |ui| {
                changed |= ui.selectable_value(&mut black, false, l.t("preset", "preset")).changed();
                changed |= ui
                    .selectable_value(&mut black, true, l.t("noir (BFI)", "black (BFI)"))
                    .on_hover_text(l.t(
                        "Insère des images noires au lieu de relancer le preset",
                        "Insert black frames instead of running the preset again",
                    ))
                    .changed();
            });
        });
        if changed {
            self.send(Request::SetSubframes { subframes, black });
        }
    }

    fn params_panel(&mut self, ui: &mut egui::Ui) {
        let l = self.lang;
        let Some(state) = self.state.as_mut() else { return };
        ui.horizontal(|ui| {
            ui.heading(l.t("Paramètres", "Parameters"));
            ui.add(
                egui::TextEdit::singleline(&mut self.param_filter)
                    .hint_text(l.t("Filtrer", "Filter"))
                    .desired_width(160.0),
            );
        });
        let filter = self.param_filter.to_lowercase();
        let mut requests = Vec::new();
        if ui.button(l.t("Tout réinitialiser", "Reset all")).clicked() {
            requests.push(Request::ResetParams);
        }
        // A vertical-only scroll area grows to its widest row, and a long
        // parameter description used to widen the whole panel past the
        // window: the width is capped and descriptions are truncated.
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .max_width(ui.available_width())
            .max_height(ui.available_height().max(120.0))
            .show(ui, |ui| {
                if state.params.is_empty() {
                    ui.weak(l.t("Ce preset n'a pas de paramètres.", "This preset has no parameters."));
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
                            .max_decimals(4);
                        let slider = ui.add(slider).on_hover_text(p.name.as_str());
                        if slider.changed() {
                            if (p.value - before).abs() > p.epsilon() {
                                requests.push(Request::SetParam { name: p.name.clone(), value: p.value });
                            } else {
                                // Only step snapping, not a user edit.
                                p.value = before;
                            }
                        }
                        if p.is_modified()
                            && ui.small_button("↺").on_hover_text(l.t("Valeur du preset", "Preset value")).clicked()
                        {
                            p.value = p.initial;
                            requests.push(Request::SetParam { name: p.name.clone(), value: p.initial });
                        }
                        let description = p.description.trim();
                        ui.add(egui::Label::new(description).truncate())
                            .on_hover_text(format!("{description}\n{}", p.name));
                    });
                }
            });
        for r in requests {
            self.send(r);
        }
    }

    fn profile_panel(&mut self, ui: &mut egui::Ui) {
        let l = self.lang;
        let Some(state) = self.state.clone() else { return };
        ui.horizontal_wrapped(|ui| {
            ui.label(l.t("Profil", "Profile"));
            ui.add(
                egui::TextEdit::singleline(&mut self.profile_name).hint_text(l.t("nom", "name")).desired_width(160.0),
            );
            let named = !self.profile_name.trim().is_empty();
            if ui
                .add_enabled(named, egui::Button::new(l.t("Enregistrer", "Save")))
                .on_hover_text(l.t(
                    "Presets, paramètres, résolution, zones, HDR et sous-images",
                    "Presets, parameters, resolution, areas, HDR and subframes",
                ))
                .clicked()
            {
                let p = profile::Profile::from_state(&self.profile_name, &state);
                match profile::save(&p) {
                    Ok(path) => {
                        self.info(format!("{} {}", l.t("enregistré :", "saved"), path.display()));
                        self.reload_profiles();
                    }
                    Err(e) => self.error(format!("{} {e}", l.t("échec de l'enregistrement :", "save failed:"))),
                }
            }
            if ui.button("⟳").on_hover_text(l.t("Relire les profils", "Rescan profiles")).clicked() {
                self.reload_profiles();
            }
        });

        let profiles = self.profiles.clone();
        let default_profile = match &self.default_profile {
            Some((process, name)) if *process == state.process => name.clone(),
            _ => {
                let name = save::default_profile_for(&state.process);
                self.default_profile = Some((state.process.clone(), name.clone()));
                name
            }
        };
        let confirm_delete = self.confirm_delete.clone();
        let mut apply = None;
        let mut delete = None;
        let mut ask_delete = None;
        let mut keep = false;
        let mut set_default: Option<Option<profile::Profile>> = None;
        ui.horizontal_wrapped(|ui| {
            if profiles.is_empty() {
                ui.weak(l.t("Aucun profil pour l'instant.", "No profile saved yet."));
            }
            for p in &profiles {
                let running = state.presets == p.presets;
                if ui
                    .selectable_label(running, &p.name)
                    .on_hover_text(format!("{} preset(s), {} parameter(s)", p.presets.len(), p.params.len()))
                    .clicked()
                {
                    apply = Some(p.clone());
                }
                let is_default = default_profile == Some(p.name.clone());
                if ui
                    .small_button(if is_default { "★" } else { "☆" })
                    .on_hover_text(format!(
                        "{} {} › {}",
                        l.t("Charger automatiquement", "Load automatically"),
                        p.name,
                        state.process
                    ))
                    .clicked()
                {
                    set_default = Some(if is_default { None } else { Some(p.clone()) });
                }
                if confirm_delete.as_deref() == Some(p.name.as_str()) {
                    if ui
                        .small_button(l.t("supprimer", "delete"))
                        .on_hover_text(format!("{} {}", l.t("Supprimer définitivement", "Delete for good"), p.name))
                        .clicked()
                    {
                        delete = Some(p.clone());
                    }
                    keep |= ui.small_button(l.t("garder", "keep")).clicked();
                } else if ui
                    .small_button("🗙")
                    .on_hover_text(format!("{} {}", l.t("Supprimer", "Delete"), p.name))
                    .clicked()
                {
                    ask_delete = Some(p.name.clone());
                }
                ui.separator();
            }
        });
        if let Some(name) = ask_delete {
            self.confirm_delete = Some(name);
        }
        if keep {
            self.confirm_delete = None;
        }
        if let Some(p) = apply {
            self.profile_name = p.name.clone();
            self.apply_profile(&p);
        }
        if let Some(choice) = set_default {
            let name = choice.as_ref().map(|p| p.name.clone());
            self.default_profile = None;
            match save::set_default_profile(&state.process, name.as_deref()) {
                Ok(()) => self.info(match &name {
                    Some(n) => {
                        format!("{n} › {} ({})", state.process, l.t("chargement automatique", "loads automatically"))
                    }
                    None => format!(
                        "{}: {}",
                        state.process,
                        l.t("plus de profil automatique", "no automatic profile any more")
                    ),
                }),
                Err(e) => self.error(format!("vkSlang.conf: {e}")),
            }
        }
        if let Some(p) = delete {
            self.confirm_delete = None;
            match profile::delete(&p) {
                Ok(()) => {
                    self.info(format!("{} {}", l.t("supprimé :", "deleted"), p.name));
                    self.reload_profiles();
                }
                Err(e) => self.error(format!("{} {e}", l.t("échec de la suppression :", "delete failed:"))),
            }
        }
    }

    fn save_panel(&mut self, ui: &mut egui::Ui) {
        let l = self.lang;
        let Some(state) = self.state.clone() else { return };
        ui.separator();
        ui.horizontal(|ui| {
            ui.label(l.t("Preset", "Preset"));
            ui.add(
                egui::TextEdit::singleline(&mut self.save_path)
                    .desired_width((ui.available_width() - 260.0).max(120.0)),
            );
            let single = state.presets.len() == 1;
            if ui
                .add_enabled(single, egui::Button::new(l.t("Exporter en .slangp", "Save .slangp")))
                .on_hover_text(if single {
                    l.t(
                        "#reference + paramètres modifiés (compatible RetroArch)",
                        "#reference + changed parameters (RetroArch compatible)",
                    )
                } else {
                    l.t(
                        "Un .slangp référence un seul preset ; enregistre un profil pour garder une chaîne",
                        "A .slangp references a single preset; save a profile to keep a chain",
                    )
                })
                .clicked()
            {
                let path = PathBuf::from(&self.save_path);
                match save::save_slangp(&state, &path) {
                    Ok(()) => self.info(format!("{} {}", l.t("enregistré :", "saved"), path.display())),
                    Err(e) => self.error(format!("{} {e}", l.t("échec de l'enregistrement :", "save failed:"))),
                }
            }
            let global = format!(
                "{} {}",
                l.t(
                    "S'applique à TOUS les jeux où tourne vkSlang (gamescope compris). Pour un seul jeu, \
                     enregistre plutôt un profil et coche ☆. Fichier :",
                    "Applies to EVERY game vkSlang runs in (gamescope included). For one game, save a \
                     profile and star it instead. File:",
                ),
                save::config_path().display()
            );
            if ui.button(l.t("Enregistrer pour tous les jeux", "Save for all games")).on_hover_text(global).clicked() {
                match save::save_config(&state) {
                    Ok(path) => self.info(format!("{} {}", l.t("mis à jour :", "updated"), path.display())),
                    Err(e) => self.error(format!("{} {e}", l.t("échec de l'enregistrement :", "save failed:"))),
                }
            }
        });
    }
}

/// Draws one level of the tree; folders are collapsed unless a search is
/// running, in which case only matching branches are shown, opened.
#[allow(clippy::too_many_arguments)]
fn draw_tree(
    ui: &mut egui::Ui,
    tree: &Tree,
    words: &[String],
    root: &Path,
    running: &[String],
    clicked: &mut Option<PathBuf>,
    chain: &mut Vec<ChainEntry>,
    l: i18n::Lang,
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
            .show(ui, |ui| draw_tree(ui, folder, words, root, running, clicked, chain, l));
    }
    for (name, rel) in &tree.presets {
        if searching {
            let haystack = rel.to_string_lossy().to_lowercase();
            if !words.iter().all(|w| haystack.contains(w.as_str())) {
                continue;
            }
        }
        let abs = root.join(rel);
        let is_running = running.iter().any(|p| p == abs.to_string_lossy().as_ref());
        ui.horizontal(|ui| {
            // Ticking several presets chains them, in ticking order.
            let mut ticked = chain.iter().any(|e| e.path == abs);
            if ui.checkbox(&mut ticked, "").on_hover_text(l.t("Ajouter à la chaîne", "Add to the chain")).changed() {
                if ticked {
                    chain.push(ChainEntry { path: abs.clone(), enabled: true });
                } else {
                    chain.retain(|e| e.path != abs);
                }
            }
            if ui.selectable_label(is_running, name.as_str()).clicked() {
                *clicked = Some(abs.clone());
            }
        });
    }
}

impl App {
    /// Shown until a game is connected.
    fn welcome(&mut self, ui: &mut egui::Ui) {
        let l = self.lang;
        ui.add_space(24.0);
        ui.heading(l.t("Aucun jeu connecté", "No game connected"));
        ui.add_space(8.0);
        ui.label(l.t(
            "Lance un jeu avec vkSlang, il apparaîtra ici tout seul :",
            "Start a game with vkSlang, it shows up here by itself:",
        ));
        ui.code("ENABLE_VKSLANG=1 VKSLANG_PRESET=/…/crt-easymode.slangp %command%");
        ui.add_space(8.0);
        ui.label(l.t(
            "Avec gamescope (jeux 32 bits, OpenGL, SDL…), le shader s'applique à sa sortie :",
            "With gamescope (32-bit, OpenGL, SDL games…), the shader applies to its output:",
        ));
        ui.code("ENABLE_VKSLANG=1 VKSLANG_PROCESS=gamescope gamescope --backend sdl -f -- %command%");
        ui.add_space(8.0);
        ui.weak(l.t(
            "Il faut un preset au lancement : dans la commande, dans vkSlang.conf, ou par un profil ★.",
            "A preset is needed at startup: in the command, in vkSlang.conf, or through a ★ profile.",
        ));
        ui.weak(format!("{}: {}", l.t("Sockets", "Sockets"), vkslang_ipc::socket_dir().display()));
    }

    /// One line on what reaches the screen.
    fn status_line(&self, ui: &mut egui::Ui) {
        let l = self.lang;
        let Some(state) = &self.state else { return };
        ui.horizontal_wrapped(|ui| {
            let running = state
                .presets
                .iter()
                .map(|p| Path::new(p).file_stem().map_or(p.clone(), |s| s.to_string_lossy().into_owned()))
                .collect::<Vec<_>>()
                .join(" + ");
            ui.strong(if running.is_empty() { l.t("(aucun preset)", "(no preset)").to_string() } else { running });
            if let Some(o) = state.outputs.first() {
                let [w, h] = o.size;
                let [iw, ih] = o.input;
                ui.weak(match l {
                    i18n::Lang::Fr => format!("· écran {w}×{h} · le shader reçoit {iw}×{ih}"),
                    i18n::Lang::En => format!("· screen {w}×{h} · the shader gets {iw}×{ih}"),
                });
            }
            ui.weak(match l {
                i18n::Lang::Fr => format!("· {:.0} i/s", state.source_fps),
                i18n::Lang::En => format!("· {:.0} fps", state.source_fps),
            });
        });
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.tick();
        let ctx = ui.ctx().clone();
        self.poll_capture(&ctx);
        let l = self.lang;
        egui::Panel::top("top").show(ui, |ui| self.top_bar(ui));
        egui::CentralPanel::default().show(ui, |ui| {
            if self.client.is_none() {
                self.welcome(ui);
                return;
            }
            self.status_line(ui);
            ui.horizontal(|ui| {
                for (tab, label) in [
                    (Tab::Image, l.t("🖼  Image", "🖼  Picture")),
                    (Tab::Shader, "✨  Shader"),
                    (Tab::Display, l.t("🖥  Affichage", "🖥  Display")),
                    (Tab::Profiles, l.t("💾  Profils", "💾  Profiles")),
                ] {
                    if ui.selectable_label(self.tab == tab, egui::RichText::new(label).size(18.0)).clicked() {
                        self.tab = tab;
                    }
                }
            });
            ui.separator();
            match self.tab {
                Tab::Image => self.image_tab(ui),
                Tab::Shader => {
                    egui::Panel::left("presets")
                        .resizable(true)
                        .default_size(340.0)
                        .show(ui, |ui| self.preset_browser(ui));
                    self.params_panel(ui);
                }
                Tab::Display => {
                    self.hdr_panel(ui);
                    ui.separator();
                    self.presentation_panel(ui);
                }
                Tab::Profiles => {
                    self.profile_panel(ui);
                    self.save_panel(ui);
                }
            }
        });
        self.wizard_window(&ctx);
        ctx.request_repaint_after(POLL);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn game_scale_matches_gamescope() {
        // 640×480 on a 4K screen: ×4 when integer-scaled (2560×1920), fitted
        // to the height otherwise (2880×2160).
        assert_eq!(game_scale([3840, 2160], [640, 480], true), 4.0);
        assert_eq!(game_scale([3840, 2160], [640, 480], false), 4.5);
        assert_eq!(game_scale([2560, 1440], [320, 240], true), 6.0);
        // Never below 1, even for a game larger than the screen.
        assert_eq!(game_scale([1280, 720], [1920, 1080], true), 1.0);
        assert_eq!(game_scale([1280, 720], [1920, 1080], false), 1.0);
    }

    #[test]
    fn game_rect_is_where_gamescope_draws() {
        // 640×480 ×4 on a 4K screen: 2560×1920, centred.
        assert_eq!(game_rect([3840, 2160], [640, 480], true), [640, 120, 2560, 1920]);
        // Fitted: the full height, pillarboxed.
        assert_eq!(game_rect([3840, 2160], [640, 480], false), [480, 0, 2880, 2160]);
        // Larger than the screen: clamped, never outside.
        assert_eq!(game_rect([1280, 720], [1920, 1080], true), [0, 0, 1280, 720]);
    }
}
