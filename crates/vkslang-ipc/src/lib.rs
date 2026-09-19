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

pub const PROTOCOL_VERSION: u32 = 2;

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
}

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

/// Why a preset and an output do not fit together, if they don't.
pub fn color_space_mismatch(preset: ColorSpace, output: ColorSpace) -> Option<&'static str> {
    match (preset.is_hdr(), output.is_hdr()) {
        (true, false) => Some("HDR preset on an SDR output: colors and brightness will be wrong"),
        (false, true) => Some(
            "SDR preset on an HDR output: the image will look wrong (no inverse tonemapping yet); \
             use an HDR preset such as hdr/crt-sony-megatron-v2-default.slangp",
        ),
        _ if preset != output && !(preset == ColorSpace::ScRgb && output == ColorSpace::PqScRgb) => {
            Some("the preset's HDR format differs from the output's (HDR10 vs scRGB)")
        }
        _ => None,
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
    pub size: [u32; 2],
    /// Vulkan format name, e.g. `A2B10G10R10_UNORM_PACK32`.
    pub format: String,
    pub color_space: ColorSpace,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Filter {
    #[default]
    Nearest,
    Linear,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct SourceSettings {
    /// Logical source resolution; `None` = swapchain (picture) size.
    pub res: Option<[u32; 2]>,
    pub filter: Filter,
    /// `full`, `4:3`, or `X,Y,WxH`.
    pub rect: String,
}

impl Default for SourceSettings {
    fn default() -> Self {
        SourceSettings { res: None, filter: Filter::Nearest, rect: "full".into() }
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
    /// RetroArch presets use `min == max` parameters as section headers.
    pub fn is_header(&self) -> bool {
        self.minimum == self.maximum
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
    /// Parameters in declaration order.
    pub params: Vec<Param>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    State(State),
    Error { message: String },
}

fn uid() -> u32 {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata("/proc/self").map(|m| m.uid()).unwrap_or(0)
}

/// Directory holding one socket per process running the layer.
pub fn socket_dir() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) => PathBuf::from(dir).join("vkslang"),
        None => std::env::temp_dir().join(format!("vkslang-{}", uid())),
    }
}

pub fn socket_path(pid: u32) -> PathBuf {
    socket_dir().join(format!("{pid}.sock"))
}

/// Sockets currently present, as `(pid, path)`. Some may be stale (process
/// gone); [`Client::connect`] fails on those and removes them.
pub fn list_sockets() -> Vec<(u32, PathBuf)> {
    let mut out: Vec<_> = std::fs::read_dir(socket_dir())
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            let pid = path.file_stem()?.to_str()?.parse().ok()?;
            (path.extension()? == "sock").then_some((pid, path))
        })
        .collect();
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
        Ok(serde_json::from_str(&answer)?)
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
    fn mismatch() {
        use ColorSpace::*;
        assert!(color_space_mismatch(Sdr, Sdr).is_none());
        assert!(color_space_mismatch(Hdr10, Hdr10).is_none());
        assert!(color_space_mismatch(ScRgb, PqScRgb).is_none());
        assert!(color_space_mismatch(Hdr10, Sdr).is_some());
        assert!(color_space_mismatch(Sdr, Hdr10).is_some());
        assert!(color_space_mismatch(Hdr10, ScRgb).is_some());
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
