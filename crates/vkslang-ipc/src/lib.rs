//! Control protocol between the vkSlang layer (server, one per process) and
//! `vkslang-ui` (client).
//!
//! Transport: a Unix socket per process at
//! `$XDG_RUNTIME_DIR/vkslang/<pid>.sock`, one JSON object per line. Every
//! request is answered by exactly one [`Response`], and every successful
//! request returns the full, up-to-date [`State`].

use serde::{Deserialize, Serialize};
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const PROTOCOL_VERSION: u32 = 9;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    GetState,
    /// Change one preset parameter (applied on the next frame).
    SetParam { name: String, value: f32 },
    /// Drop every parameter override, back to the preset's values.
    ResetParams,
    /// Compile and switch to another preset (in the background).
    LoadPreset { path: String },
    /// Bypass the filter chain without unloading it.
    SetEnabled { enabled: bool },
    SetSource { source: SourceSettings },
    /// HDR uniforms (`BrightnessNits`, `ExpandGamut`) for HDR-aware presets.
    SetHdr { hdr: HdrSettings },
    /// Presentations per application frame (interlacing, BFI).
    SetSubframes { subframes: u32, black: bool },
    /// Grab the picture as the application drew it, before the preset, so the
    /// UI can measure its pixel size. Answered immediately; the capture shows
    /// up in `State::capture` once a frame has been presented.
    Capture { max_width: u32 },
}

/// A picture grabbed by the layer, written next to the control socket.
///
/// Raw file: magic `VKSC`, width, height (little endian u32), then RGBA8 rows.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Capture {
    pub path: String,
    /// Size of the captured image (downscaled to the requested width).
    pub size: [u32; 2],
    /// Size of the picture area it was taken from, in output pixels.
    pub picture: [u32; 2],
    /// Increments with every capture, so a client can tell them apart.
    pub id: u64,
}

pub const CAPTURE_MAGIC: [u8; 4] = *b"VKSC";

/// Color space of a swapchain or of a preset's final pass.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ColorSpace {
    #[default]
    Sdr,
    Hdr10,
    ScRgb,
    PqScRgb,
}

impl ColorSpace {
    pub fn is_hdr(self) -> bool {
        self != ColorSpace::Sdr
    }

    pub fn label(self) -> &'static str {
        match self {
            ColorSpace::Sdr => "SDR",
            ColorSpace::Hdr10 => "HDR10",
            ColorSpace::ScRgb => "scRGB",
            ColorSpace::PqScRgb => "PQ scRGB",
        }
    }
}

/// Caveat about running a preset of color space `preset` on a swapchain of
/// color space `output`, if any.
///
/// HDR10 vs scRGB is not a problem: HDR-aware presets (e.g. Sony Megatron v2)
/// adapt their encoding to `HDRMode`. What does not work yet is converting
/// between SDR and HDR, on the input or the output side.
pub fn color_space_mismatch(preset: ColorSpace, output: ColorSpace) -> Option<&'static str> {
    color_space_warning(preset, output, false)
}

/// Same, for an output the layer promoted to HDR: the application still
/// renders SDR, so an HDR preset is exactly what is wanted there.
pub fn color_space_warning(preset: ColorSpace, output: ColorSpace, promoted: bool) -> Option<&'static str> {
    if promoted {
        return (!preset.is_hdr())
            .then_some("the output was promoted to HDR but the preset writes SDR: pick an HDR preset (Sony Megatron) or set hdr_output = off");
    }
    match (preset.is_hdr(), output.is_hdr()) {
        (true, false) => Some("HDR preset on an SDR output: colors and brightness will be wrong"),
        (false, true) => Some(
            "SDR preset on an HDR output: the picture will look wrong (no SDR/HDR conversion yet). \
             For SDR games, apply vkSlang to the game and let gamescope --hdr-enabled --hdr-itm-enabled do the HDR",
        ),
        (true, true) => Some(
            "HDR output: the application's picture is already HDR encoded, while presets expect an SDR \
             picture as input, so colors may be off (input conversion not implemented yet)",
        ),
        (false, false) => None,
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq)]
pub struct HdrSettings {
    /// Paper white / SDR reference in nits (`BrightnessNits`).
    pub brightness_nits: f32,
    /// 0 Accurate, 1 Expanded, 2 Wide, 3 Super (`ExpandGamut`).
    pub expand_gamut: u32,
}

impl Default for HdrSettings {
    fn default() -> Self {
        HdrSettings { brightness_nits: 200.0, expand_gamut: 0 }
    }
}

pub const GAMUT_NAMES: [&str; 4] = ["Accurate", "Expanded", "Wide", "Super"];

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
pub struct Output {
    /// Swapchain size: what the application renders and what is presented.
    pub size: [u32; 2],
    /// Region of it holding the picture (differs with a 4:3 picture area).
    pub picture: [u32; 2],
    /// Size of the image fed to the preset as `Original`.
    pub input: [u32; 2],
    /// Vulkan format name, e.g. `A2B10G10R10_UNORM_PACK32`.
    pub format: String,
    pub color_space: ColorSpace,
    /// The layer turned the application's SDR swapchain into an HDR one, so
    /// the picture reaching the preset is SDR even though the output is HDR.
    pub promoted: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Filter {
    #[default]
    Nearest,
    Linear,
}

/// Size of the image handed to the filter chain as `Original`.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Default)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum SourceSize {
    /// The picture area itself, untouched.
    #[default]
    Native,
    /// The picture area divided by `by` (2 = half, 3 = a third...).
    Divide { by: f32 },
    /// A fixed resolution, whatever the output is.
    Fixed { size: [u32; 2] },
}

impl SourceSize {
    /// As written in vkSlang.conf (`native`, `/2`, `320x240`).
    pub fn to_config(self) -> String {
        match self {
            SourceSize::Native => "native".into(),
            SourceSize::Divide { by } => format!("/{by}"),
            SourceSize::Fixed { size: [w, h] } => format!("{w}x{h}"),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct SourceSettings {
    /// Logical source resolution.
    pub res: SourceSize,
    pub filter: Filter,
    /// Region read from the swapchain: `full`, `4:3`, or `X,Y,WxH`.
    pub rect: String,
    /// Region the preset draws into, same syntax. Different from `rect` it
    /// stretches the picture: a 640x360 source drawn into a 4:3 area gives
    /// the old "non-square pixels" look, scanlines stretched along with it.
    pub display: String,
}

impl Default for SourceSettings {
    fn default() -> Self {
        SourceSettings {
            res: SourceSize::default(),
            filter: Filter::Nearest,
            rect: "full".into(),
            display: "full".into(),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Param {
    pub name: String,
    pub description: String,
    /// Value from the preset (what "reset" goes back to).
    pub initial: f32,
    pub minimum: f32,
    pub maximum: f32,
    pub step: f32,
    pub value: f32,
}

impl Param {
    /// Presets use unadjustable parameters as section headers: either
    /// `min == max`, or a range no larger than one (tiny) step, as Sony
    /// Megatron does with `0.0 0.0 0.0001 0.0001`. On/off parameters
    /// (`0 1 1`) have a range of 1 and stay real sliders.
    pub fn is_header(&self) -> bool {
        let range = self.maximum - self.minimum;
        range <= 0.0 || (range <= self.step.abs() && range < 0.01)
    }

    /// Smallest change that counts as a user edit (ignores float noise from
    /// sliders snapping to `step`).
    pub fn epsilon(&self) -> f32 {
        self.step.abs().max(1e-6) * 1e-3
    }

    /// Whether the value differs from the preset's.
    pub fn is_modified(&self) -> bool {
        (self.value - self.initial).abs() > self.epsilon()
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
pub struct State {
    pub protocol: u32,
    pub pid: u32,
    pub process: String,
    pub enabled: bool,
    /// Preset currently running.
    pub preset: Option<String>,
    /// A preset is being compiled.
    pub loading: bool,
    /// Last load error, if any.
    pub error: Option<String>,
    pub source: SourceSettings,
    /// Swapchains being processed.
    pub outputs: Vec<Output>,
    /// Color space written by the running preset's final pass.
    pub preset_color_space: Option<ColorSpace>,
    pub hdr: HdrSettings,
    /// Frames per second the application draws.
    pub source_fps: f32,
    /// Presentations per second reaching the display (subframes included).
    pub present_fps: f32,
    /// Presentations per application frame (1 = untouched).
    pub subframes: u32,
    /// Extra subframes are black instead of running the preset.
    pub subframe_black: bool,
    /// Subframes the swapchain was created with room for; asking for more
    /// only takes effect after the application restarts.
    pub subframes_max: u32,
    /// Parameters in declaration order.
    pub params: Vec<Param>,
    /// Last picture grabbed by [`Request::Capture`].
    pub capture: Option<Capture>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
// The state is the whole point of a response; boxing it would only move the
// allocation around, and responses are built one at a time.
#[allow(clippy::large_enum_variant)]
pub enum Response {
    State(State),
    Error { message: String },
}

fn uid() -> u32 {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata("/proc/self").map(|m| m.uid()).unwrap_or(0)
}

/// Directory this process must create its socket in.
///
/// `$XDG_RUNTIME_DIR/vkslang` normally. Inside a Steam/Proton container
/// (pressure-vessel) the runtime directory is private to the container, so a
/// socket created there is invisible to `vkslang-ui` running on the host:
/// `$HOME/.local/state/vkslang` is used instead, since the home directory is
/// shared. `VKSLANG_SOCKET_DIR` overrides both.
pub fn socket_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("VKSLANG_SOCKET_DIR") {
        return PathBuf::from(dir);
    }
    let in_container = Path::new("/run/host/usr").is_dir();
    if let (true, Some(home)) = (in_container, std::env::var_os("HOME")) {
        return PathBuf::from(home).join(".local/state/vkslang");
    }
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) => PathBuf::from(dir).join("vkslang"),
        None => std::env::temp_dir().join(format!("vkslang-{}", uid())),
    }
}

/// Every directory sockets may live in, for a client looking for processes.
pub fn socket_dirs() -> Vec<PathBuf> {
    let mut dirs = vec![socket_dir()];
    for dir in [
        std::env::var_os("XDG_RUNTIME_DIR").map(|d| PathBuf::from(d).join("vkslang")),
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state/vkslang")),
    ]
    .into_iter()
    .flatten()
    {
        if !dirs.contains(&dir) {
            dirs.push(dir);
        }
    }
    dirs
}

pub fn socket_path(pid: u32) -> PathBuf {
    socket_dir().join(format!("{pid}.sock"))
}

/// Sockets currently present, as `(pid, path)`, across every candidate
/// directory. Some may be stale (process gone); [`Client::connect`] fails on
/// those and removes them.
pub fn list_sockets() -> Vec<(u32, PathBuf)> {
    let mut out: Vec<(u32, PathBuf)> = Vec::new();
    for dir in socket_dirs() {
        for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let path = entry.path();
            let Some(pid) = path.file_stem().and_then(|s| s.to_str()).and_then(|s| s.parse().ok()) else {
                continue;
            };
            if path.extension().is_some_and(|e| e == "sock") && !out.iter().any(|(p, _)| *p == pid) {
                out.push((pid, path));
            }
        }
    }
    out.sort();
    out
}

pub struct Client {
    writer: UnixStream,
    reader: BufReader<UnixStream>,
}

impl Client {
    pub fn connect(path: &Path) -> io::Result<Client> {
        let stream = match UnixStream::connect(path) {
            Ok(s) => s,
            Err(e) => {
                if e.kind() == io::ErrorKind::ConnectionRefused {
                    // Nobody listening: the process exited without cleanup.
                    let _ = std::fs::remove_file(path);
                }
                return Err(e);
            }
        };
        stream.set_read_timeout(Some(Duration::from_secs(2)))?;
        stream.set_write_timeout(Some(Duration::from_secs(2)))?;
        Ok(Client { reader: BufReader::new(stream.try_clone()?), writer: stream })
    }

    pub fn request(&mut self, request: &Request) -> io::Result<Response> {
        let mut line = serde_json::to_string(request)?;
        line.push('\n');
        self.writer.write_all(line.as_bytes())?;
        let mut answer = String::new();
        if self.reader.read_line(&mut answer)? == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        serde_json::from_str(&answer).map_err(|e| {
            // Most likely a layer speaking an older protocol: say so instead
            // of reporting a JSON error.
            #[derive(Deserialize)]
            struct Version {
                protocol: u32,
            }
            match serde_json::from_str::<Version>(&answer) {
                Ok(v) if v.protocol != PROTOCOL_VERSION => io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "layer speaks protocol v{}, this build expects v{PROTOCOL_VERSION} \
                         (restart the application with the updated layer)",
                        v.protocol
                    ),
                ),
                _ => io::Error::new(io::ErrorKind::InvalidData, e.to_string()),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_format() {
        let r = Request::SetParam { name: "GAMMA".into(), value: 2.4 };
        let s = serde_json::to_string(&r).unwrap();
        assert_eq!(s, r#"{"cmd":"set_param","name":"GAMMA","value":2.4}"#);
        assert_eq!(serde_json::from_str::<Request>(&s).unwrap(), r);
        assert_eq!(
            serde_json::from_str::<Request>(r#"{"cmd":"get_state"}"#).unwrap(),
            Request::GetState
        );
        assert_eq!(
            serde_json::to_string(&Request::SetHdr { hdr: HdrSettings::default() }).unwrap(),
            r#"{"cmd":"set_hdr","hdr":{"brightness_nits":200.0,"expand_gamut":0}}"#
        );
        let resp = Response::Error { message: "x".into() };
        let s = serde_json::to_string(&resp).unwrap();
        assert_eq!(s, r#"{"type":"error","message":"x"}"#);
    }

    #[test]
    fn headers() {
        let p = |initial, min, max, step| Param {
            name: "p".into(),
            description: "d".into(),
            initial,
            minimum: min,
            maximum: max,
            step,
            value: initial,
        };
        assert!(p(0.0, 0.0, 0.0, 0.0).is_header());
        // Sony Megatron section title
        assert!(p(0.0, 0.0, 0.0001, 0.0001).is_header());
        // On/off toggle and ordinary sliders stay adjustable
        assert!(!p(1.0, 0.0, 1.0, 1.0).is_header());
        assert!(!p(2.2, 1.0, 3.0, 0.05).is_header());
    }

    #[test]
    fn mismatch() {
        use ColorSpace::*;
        assert!(color_space_mismatch(Sdr, Sdr).is_none());
        assert!(color_space_mismatch(Hdr10, Sdr).is_some());
        assert!(color_space_mismatch(Sdr, Hdr10).is_some());
        // HDR10 vs scRGB is fine, only the input caveat remains.
        assert_eq!(color_space_mismatch(ScRgb, Hdr10), color_space_mismatch(Hdr10, Hdr10));
    }

    #[test]
    fn roundtrip_over_socket() {
        let dir = std::env::temp_dir().join(format!("vkslang-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("1.sock");
        let _ = std::fs::remove_file(&path);
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut writer = stream;
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            assert_eq!(serde_json::from_str::<Request>(&line).unwrap(), Request::GetState);
            let state = State { pid: 42, ..Default::default() };
            let mut out = serde_json::to_string(&Response::State(state)).unwrap();
            out.push('\n');
            writer.write_all(out.as_bytes()).unwrap();
        });
        let mut client = Client::connect(&path).unwrap();
        match client.request(&Request::GetState).unwrap() {
            Response::State(s) => assert_eq!(s.pid, 42),
            other => panic!("unexpected {other:?}"),
        }
        server.join().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
