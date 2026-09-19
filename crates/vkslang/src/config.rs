//! Configuration: `vkSlang.conf` (key = value) overridden by `VKSLANG_*` env vars.

use ash::vk;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SourceRect {
    /// The whole swapchain image.
    Full,
    /// Centered region of the given aspect ratio (width / height).
    Aspect(f32),
    /// Explicit rectangle in swapchain pixels.
    Explicit(vk::Rect2D),
}

/// How the swapchain image is turned into the chain's `Original` input.
#[derive(Debug, Clone, PartialEq)]
pub struct Source {
    /// Logical (retro) resolution fed to the filter chain as `Original`.
    pub res: Option<vk::Extent2D>,
    pub filter: vk::Filter,
    pub rect: SourceRect,
    /// `rect` as written by the user (`4:3` stays `4:3`).
    pub rect_spec: String,
}

impl Default for Source {
    fn default() -> Self {
        Source { res: None, filter: vk::Filter::NEAREST, rect: SourceRect::Full, rect_spec: "full".into() }
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub preset: Option<PathBuf>,
    pub source: Source,
    /// Executable names the layer is active for; empty = all.
    pub process: Vec<String>,
    /// Preset parameter overrides (`param.NAME = value`).
    pub params: Vec<(String, f32)>,
    /// Control socket for vkslang-ui (`VKSLANG_IPC=0` disables it).
    pub ipc: bool,
    /// HDR uniforms for HDR-aware presets.
    pub hdr: vkslang_ipc::HdrSettings,
}

pub fn get() -> &'static Config {
    static CONFIG: OnceLock<Config> = OnceLock::new();
    CONFIG.get_or_init(Config::load)
}

fn config_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("VKSLANG_CONFIG") {
        return Some(PathBuf::from(p));
    }
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("vkSlang").join("vkSlang.conf"))
}

fn parse_file(text: &str) -> HashMap<String, String> {
    text.lines()
        .map(|l| l.split('#').next().unwrap_or("").trim())
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .filter(|(k, _)| !k.is_empty())
        .collect()
}

/// `320x240`
pub fn parse_extent(s: &str) -> Option<vk::Extent2D> {
    let (w, h) = s.trim().split_once(['x', 'X'])?;
    let (width, height) = (w.trim().parse().ok()?, h.trim().parse().ok()?);
    (width > 0 && height > 0).then_some(vk::Extent2D { width, height })
}

/// `full`, `4:3`, `1.333`, or `X,Y,WxH`
pub fn parse_rect(s: &str) -> Option<SourceRect> {
    let s = s.trim();
    if s.is_empty() || s.eq_ignore_ascii_case("full") {
        return Some(SourceRect::Full);
    }
    let parts: Vec<&str> = s.split(',').collect();
    if let [x, y, wh] = parts[..] {
        let extent = parse_extent(wh)?;
        return Some(SourceRect::Explicit(vk::Rect2D {
            offset: vk::Offset2D { x: x.trim().parse().ok()?, y: y.trim().parse().ok()? },
            extent,
        }));
    }
    let ratio = match s.split_once(':') {
        Some((w, h)) => w.trim().parse::<f32>().ok()? / h.trim().parse::<f32>().ok()?,
        None => s.parse().ok()?,
    };
    (ratio.is_finite() && ratio > 0.0).then_some(SourceRect::Aspect(ratio))
}

/// Clamps HDR settings to the ranges the shaders accept.
pub fn sanitize_hdr(hdr: vkslang_ipc::HdrSettings) -> vkslang_ipc::HdrSettings {
    vkslang_ipc::HdrSettings {
        brightness_nits: if hdr.brightness_nits.is_finite() { hdr.brightness_nits.clamp(0.0, 10000.0) } else { 200.0 },
        expand_gamut: hdr.expand_gamut.min(3),
    }
}

fn parse_hdr(kv: &HashMap<String, String>) -> vkslang_ipc::HdrSettings {
    let default = vkslang_ipc::HdrSettings::default();
    sanitize_hdr(vkslang_ipc::HdrSettings {
        brightness_nits: kv
            .get("brightness_nits")
            .and_then(|v| v.parse().ok())
            .unwrap_or(default.brightness_nits),
        expand_gamut: kv.get("expand_gamut").and_then(|v| v.parse().ok()).unwrap_or(default.expand_gamut),
    })
}

impl Config {
    fn load() -> Config {
        let mut kv = config_path()
            .map(|p| resolve_path(&p))
            .and_then(|p| std::fs::read_to_string(&p).ok().map(|t| (p, t)))
            .map(|(p, t)| {
                crate::log_debug!("using config {}", p.display());
                parse_file(&t)
            })
            .unwrap_or_default();

        // Environment wins over the file: VKSLANG_SOURCE_RES -> source_res
        for (k, v) in std::env::vars() {
            if let Some(key) = k.strip_prefix("VKSLANG_") {
                if !matches!(key, "CONFIG" | "LOG") {
                    // VKSLANG_SOURCE_RES -> source_res
                    kv.insert(key.to_ascii_lowercase(), v);
                }
            }
        }

        let source_res = kv
            .get("source_res")
            .filter(|s| !s.is_empty() && !s.eq_ignore_ascii_case("native"))
            .and_then(|s| {
                let e = parse_extent(s);
                if e.is_none() {
                    crate::log_warn!("invalid source_res '{s}', expected WxH");
                }
                e
            });

        let filter = match kv.get("source_filter").map(|s| s.to_ascii_lowercase()).as_deref() {
            Some("linear") => vk::Filter::LINEAR,
            _ => vk::Filter::NEAREST,
        };

        let (rect, rect_spec) = match kv.get("source_rect") {
            Some(spec) => match parse_rect(spec) {
                Some(rect) => (rect, spec.clone()),
                None => {
                    crate::log_warn!("invalid source_rect '{spec}', using full");
                    (SourceRect::Full, "full".into())
                }
            },
            None => (SourceRect::Full, "full".into()),
        };

        let process = kv
            .get("process")
            .map(|s| s.split(',').map(|p| p.trim().to_string()).filter(|p| !p.is_empty()).collect())
            .unwrap_or_default();

        let params = kv
            .iter()
            .filter_map(|(k, v)| Some((k.strip_prefix("param.")?.to_string(), v.parse().ok()?)))
            .collect();

        Config {
            preset: kv.get("preset").filter(|p| !p.is_empty()).map(PathBuf::from),
            source: Source { res: source_res, filter, rect, rect_spec },
            process,
            params,
            ipc: kv.get("ipc").map_or(true, |v| v != "0" && !v.eq_ignore_ascii_case("false")),
            hdr: parse_hdr(&kv),
        }
    }

    /// Whether the layer should do anything in this process.
    pub fn active_for_process(&self) -> bool {
        if self.process.is_empty() {
            return true;
        }
        self.process.contains(&exe_name())
    }
}

/// Resolves a configured path, falling back to the same path under
/// `/run/host` for Steam/Proton: inside pressure-vessel the container has its
/// own `/usr`, and the host filesystem is mounted at `/run/host`.
pub fn resolve_path(path: &Path) -> PathBuf {
    if path.exists() {
        return path.to_path_buf();
    }
    if let Ok(rest) = path.strip_prefix("/") {
        let host = Path::new("/run/host").join(rest);
        if host.exists() {
            crate::log_info!("{} not found, using {}", path.display(), host.display());
            return host;
        }
    }
    path.to_path_buf()
}

pub fn exe_name() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_default()
}

impl Source {
    pub fn from_ipc(s: &vkslang_ipc::SourceSettings) -> Result<Source, String> {
        let rect = parse_rect(&s.rect).ok_or_else(|| format!("invalid source rect '{}'", s.rect))?;
        let res = match s.res {
            Some([0, _]) | Some([_, 0]) => return Err("source resolution must be non-zero".into()),
            Some([width, height]) => Some(vk::Extent2D { width, height }),
            None => None,
        };
        let filter = match s.filter {
            vkslang_ipc::Filter::Nearest => vk::Filter::NEAREST,
            vkslang_ipc::Filter::Linear => vk::Filter::LINEAR,
        };
        Ok(Source { res, filter, rect, rect_spec: s.rect.trim().to_string() })
    }

    pub fn to_ipc(&self) -> vkslang_ipc::SourceSettings {
        vkslang_ipc::SourceSettings {
            res: self.res.map(|e| [e.width, e.height]),
            filter: if self.filter == vk::Filter::LINEAR {
                vkslang_ipc::Filter::Linear
            } else {
                vkslang_ipc::Filter::Nearest
            },
            rect: self.rect_spec.clone(),
        }
    }

    /// Area of a `extent`-sized swapchain image that holds the picture.
    pub fn picture_rect(&self, extent: vk::Extent2D) -> vk::Rect2D {
        let full = vk::Rect2D { offset: vk::Offset2D::default(), extent };
        match self.rect {
            SourceRect::Full => full,
            SourceRect::Aspect(ratio) => {
                let (w, h) = (extent.width as f32, extent.height as f32);
                let (rw, rh) = if w / h > ratio { (h * ratio, h) } else { (w, w / ratio) };
                let (rw, rh) = (rw.round().max(1.0) as u32, rh.round().max(1.0) as u32);
                vk::Rect2D {
                    offset: vk::Offset2D {
                        x: ((extent.width - rw) / 2) as i32,
                        y: ((extent.height - rh) / 2) as i32,
                    },
                    extent: vk::Extent2D { width: rw, height: rh },
                }
            }
            SourceRect::Explicit(r) => {
                // Clamp into the image so the blit stays valid after a resize.
                let x = (r.offset.x.max(0) as u32).min(extent.width - 1);
                let y = (r.offset.y.max(0) as u32).min(extent.height - 1);
                vk::Rect2D {
                    offset: vk::Offset2D { x: x as i32, y: y as i32 },
                    extent: vk::Extent2D {
                        width: r.extent.width.min(extent.width - x),
                        height: r.extent.height.min(extent.height - y),
                    },
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extent() {
        assert_eq!(parse_extent("320x240"), Some(vk::Extent2D { width: 320, height: 240 }));
        assert_eq!(parse_extent(" 640 X 480 "), Some(vk::Extent2D { width: 640, height: 480 }));
        assert_eq!(parse_extent("0x240"), None);
        assert_eq!(parse_extent("native"), None);
    }

    #[test]
    fn rect() {
        assert_eq!(parse_rect("full"), Some(SourceRect::Full));
        assert_eq!(parse_rect("4:3"), Some(SourceRect::Aspect(4.0 / 3.0)));
        let src = Source { rect: SourceRect::Aspect(4.0 / 3.0), ..Default::default() };
        let r = src.picture_rect(vk::Extent2D { width: 3840, height: 2160 });
        assert_eq!((r.offset.x, r.offset.y, r.extent.width, r.extent.height), (480, 0, 2880, 2160));
    }

    #[test]
    fn hdr() {
        let kv = parse_file("brightness_nits = 400\nexpand_gamut = 9\n");
        let hdr = parse_hdr(&kv);
        assert_eq!((hdr.brightness_nits, hdr.expand_gamut), (400.0, 3));
        let hdr = parse_hdr(&HashMap::new());
        assert_eq!((hdr.brightness_nits, hdr.expand_gamut), (200.0, 0));
    }

    #[test]
    fn file() {
        let kv = parse_file("preset = /a/b.slangp # comment\n# x = y\nparam.GAMMA=2.4\n");
        assert_eq!(kv["preset"], "/a/b.slangp");
        assert_eq!(kv["param.GAMMA"], "2.4");
        assert!(!kv.contains_key("x"));
    }
}
