//! Configuration: `vkSlang.conf` (key = value) overridden by `VKSLANG_*` env vars.

use ash::vk;
use std::collections::HashMap;
use std::path::PathBuf;
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

#[derive(Debug, Clone)]
pub struct Config {
    pub preset: Option<PathBuf>,
    /// Logical (retro) resolution fed to the filter chain as `Original`.
    pub source_res: Option<vk::Extent2D>,
    pub source_filter: vk::Filter,
    pub source_rect: SourceRect,
    /// Executable names the layer is active for; empty = all.
    pub process: Vec<String>,
    /// Preset parameter overrides (`param.NAME = value`).
    pub params: Vec<(String, f32)>,
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

impl Config {
    fn load() -> Config {
        let mut kv = config_path()
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

        let source_filter = match kv.get("source_filter").map(|s| s.to_ascii_lowercase()).as_deref() {
            Some("linear") => vk::Filter::LINEAR,
            _ => vk::Filter::NEAREST,
        };

        let source_rect = kv
            .get("source_rect")
            .map(|s| {
                parse_rect(s).unwrap_or_else(|| {
                    crate::log_warn!("invalid source_rect '{s}', using full");
                    SourceRect::Full
                })
            })
            .unwrap_or(SourceRect::Full);

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
            source_res,
            source_filter,
            source_rect,
            process,
            params,
        }
    }

    /// Whether the layer should do anything in this process.
    pub fn active_for_process(&self) -> bool {
        if self.process.is_empty() {
            return true;
        }
        let exe = std::env::current_exe()
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            .unwrap_or_default();
        self.process.contains(&exe)
    }

    /// Area of a `extent`-sized swapchain image that holds the picture.
    pub fn source_rect_for(&self, extent: vk::Extent2D) -> vk::Rect2D {
        let full = vk::Rect2D { offset: vk::Offset2D::default(), extent };
        match self.source_rect {
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
        let cfg = Config {
            preset: None,
            source_res: None,
            source_filter: vk::Filter::NEAREST,
            source_rect: SourceRect::Aspect(4.0 / 3.0),
            process: vec![],
            params: vec![],
        };
        let r = cfg.source_rect_for(vk::Extent2D { width: 3840, height: 2160 });
        assert_eq!((r.offset.x, r.offset.y, r.extent.width, r.extent.height), (480, 0, 2880, 2160));
    }

    #[test]
    fn file() {
        let kv = parse_file("preset = /a/b.slangp # comment\n# x = y\nparam.GAMMA=2.4\n");
        assert_eq!(kv["preset"], "/a/b.slangp");
        assert_eq!(kv["param.GAMMA"], "2.4");
        assert!(!kv.contains_key("x"));
    }
}
