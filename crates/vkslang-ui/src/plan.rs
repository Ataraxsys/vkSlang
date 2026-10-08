//! The picture as the user thinks about it, turned into layer settings.
//!
//! Four questions instead of a dozen settings: where is the game on the
//! screen, how big is one of its pixels, what shape should it have, and how
//! big should it be drawn. The answers give the picture area, the source
//! resolution, the duplication and the display area, consistently.

use vkslang_ipc::{Filter, SourceSettings, SourceSize};

/// Shape the game is drawn with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    /// Every game pixel square once duplicated: doubling the lines makes
    /// the picture twice as tall.
    Square,
    /// A 4:3 monitor of the time, whatever the resolution: 320x200 comes out
    /// with pixels 1.2 times taller than wide, as on a VGA screen.
    Crt,
    /// As the game shows on screen now.
    AsShown,
}

/// How big it is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Size {
    /// As large as the screen allows.
    Fit,
    /// A whole number of screen lines per game line, so every scanline gets
    /// the same thickness.
    Integer,
    /// Where the game is now: the bars around it stay as they are.
    InPlace,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Plan {
    /// Where the game is on the screen: `[x, y, width, height]`.
    pub zone: [u32; 4],
    /// Screen pixels per game pixel, per axis.
    pub pixel: [f32; 2],
    /// Each game pixel repeated this many times, per axis.
    pub dup: [u32; 2],
    pub shape: Shape,
    pub size: Size,
}

impl Plan {
    /// The game's own resolution.
    pub fn game(&self) -> [u32; 2] {
        [
            ((self.zone[2] as f32 / self.pixel[0].max(0.01)).round() as u32).max(1),
            ((self.zone[3] as f32 / self.pixel[1].max(0.01)).round() as u32).max(1),
        ]
    }

    /// What the preset receives: the game's pixels, duplicated.
    pub fn input(&self) -> [u32; 2] {
        let [w, h] = self.game();
        [w * self.dup[0].max(1), h * self.dup[1].max(1)]
    }

    /// Width over height of the drawn picture.
    ///
    /// Duplication always reshapes the game itself: ↕ 2 makes it twice as
    /// tall whatever the shape, and the shader then works on that taller
    /// game, its scanlines keeping their thickness.
    pub fn aspect(&self) -> f32 {
        let [w, h] = self.game();
        let base = match self.shape {
            Shape::Square => w as f32 / h as f32,
            Shape::Crt => 4.0 / 3.0,
            Shape::AsShown => self.zone[2] as f32 / self.zone[3].max(1) as f32,
        };
        base * self.dup[0].max(1) as f32 / self.dup[1].max(1) as f32
    }

    /// Where the preset draws, `[x, y, width, height]` on a `screen`.
    pub fn display(&self, screen: [u32; 2]) -> [u32; 4] {
        let aspect = self.aspect();
        let [sw, sh] = screen.map(|v| v as f32);
        let (w, h) = match self.size {
            Size::Fit => fit(sw, sh, aspect),
            Size::InPlace => fit(self.zone[2] as f32, self.zone[3] as f32, aspect),
            Size::Integer => {
                // Whole screen pixels per game pixel on both axes, so every
                // pixel (and scanline) is the same size; the shape is then
                // as close to the asked one as whole numbers allow.
                let [cols, lines] = self.input().map(|v| v as f32);
                let width_per_height = aspect * lines / cols;
                let mut sy = (sh / lines).floor().max(1.0);
                loop {
                    let sx = (sy * width_per_height).round().max(1.0);
                    if cols * sx <= sw || sy <= 1.0 {
                        break (cols * sx, lines * sy);
                    }
                    sy -= 1.0;
                }
            }
        };
        let (w, h) = (w.round().min(sw) as u32, h.round().min(sh) as u32);
        let (cx, cy) = match self.size {
            Size::InPlace => {
                (self.zone[0] as f32 + self.zone[2] as f32 / 2.0, self.zone[1] as f32 + self.zone[3] as f32 / 2.0)
            }
            _ => (sw / 2.0, sh / 2.0),
        };
        let x = (cx - w as f32 / 2.0).round().clamp(0.0, sw - w as f32) as u32;
        let y = (cy - h as f32 / 2.0).round().clamp(0.0, sh - h as f32) as u32;
        [x, y, w.max(1), h.max(1)]
    }

    /// Layer settings for this plan, keeping the filter of `current`.
    pub fn settings(&self, screen: [u32; 2], current: &SourceSettings) -> SourceSettings {
        let rect = |[x, y, w, h]: [u32; 4]| format!("{x},{y},{w}x{h}");
        SourceSettings {
            res: SourceSize::Fixed { size: self.game() },
            duplicate: self.dup.map(|d| d.clamp(1, 8)),
            filter: if current.filter == Filter::Linear { Filter::Linear } else { Filter::Nearest },
            rect: rect(self.zone),
            display: rect(self.display(screen)),
            display_scale: [1.0, 1.0],
            source_scale: [1.0, 1.0],
        }
    }
}

/// The largest `aspect` rectangle inside `w` by `h`.
fn fit(w: f32, h: f32, aspect: f32) -> (f32, f32) {
    if w / h > aspect {
        (h * aspect, h)
    } else {
        (w, w / aspect)
    }
}

/// Parses `X,Y,WxH` as written in the settings.
pub fn parse_area(s: &str) -> Option<[u32; 4]> {
    let mut parts = s.split(',');
    let x = parts.next()?.trim().parse().ok()?;
    let y = parts.next()?.trim().parse().ok()?;
    let (w, h) = parts.next()?.split_once(['x', 'X'])?;
    if parts.next().is_some() {
        return None;
    }
    Some([x, y, w.trim().parse().ok()?, h.trim().parse().ok()?])
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCREEN: [u32; 2] = [3840, 2160];

    /// A DOS 320x200 game, integer-scaled ×10 and centred by gamescope.
    fn dos() -> Plan {
        Plan { zone: [320, 80, 3200, 2000], pixel: [10.0, 10.0], dup: [1, 1], shape: Shape::Crt, size: Size::Fit }
    }

    #[test]
    fn a_dos_game_on_a_crt() {
        let plan = dos();
        assert_eq!(plan.game(), [320, 200]);
        // 4:3 on the full height, pixels 1.2 times taller than wide.
        assert_eq!(plan.display(SCREEN), [480, 0, 2880, 2160]);
        let s = plan.settings(SCREEN, &SourceSettings::default());
        assert_eq!(s.res, SourceSize::Fixed { size: [320, 200] });
        assert_eq!(s.rect, "320,80,3200x2000");
        assert_eq!(s.display, "480,0,2880x2160");
    }

    #[test]
    fn doubled_lines_make_the_picture_taller() {
        // Half-height pixels: 640x200 drawn square once lines are doubled.
        let plan =
            Plan { zone: [0, 480, 3840, 1200], pixel: [6.0, 6.0], dup: [1, 2], shape: Shape::Square, size: Size::Fit };
        assert_eq!(plan.game(), [640, 200]);
        assert_eq!(plan.input(), [640, 400]);
        assert_eq!(plan.display(SCREEN), [192, 0, 3456, 2160]);
        let single = Plan { dup: [1, 1], ..plan };
        assert!(single.display(SCREEN)[3] < plan.display(SCREEN)[3], "doubling changes the shape");
    }

    #[test]
    fn duplicating_reshapes_the_game_in_every_shape() {
        for shape in [Shape::Crt, Shape::Square, Shape::AsShown] {
            let plan = Plan { shape, size: Size::Fit, ..dos() };
            let doubled = Plan { dup: [1, 2], ..plan };
            assert!(
                (doubled.aspect() * 2.0 - plan.aspect()).abs() < 1e-4,
                "{shape:?}: twice as tall, not just more lines for the shader"
            );
        }
    }

    #[test]
    fn integer_size_gives_even_scanlines() {
        let plan = Plan { size: Size::Integer, ..dos() };
        let [_, _, w, h] = plan.display(SCREEN);
        // 8×10 screen pixels per game pixel: whole on both axes, as close
        // to 4:3 as whole numbers allow (2560×2000).
        assert_eq!((w, h), (2560, 2000));
        assert_eq!((w % 320, h % 200), (0, 0));
        // Square pixels stay square.
        let square = Plan { shape: Shape::Square, ..plan };
        assert_eq!(square.display(SCREEN)[2..], [3200, 2000]);
    }

    #[test]
    fn in_place_stays_on_the_game() {
        let plan = Plan { shape: Shape::AsShown, size: Size::InPlace, ..dos() };
        assert_eq!(plan.display(SCREEN), plan.zone);
    }

    #[test]
    fn areas_round_trip() {
        assert_eq!(parse_area("320,80,3200x2000"), Some([320, 80, 3200, 2000]));
        assert_eq!(parse_area("4:3"), None);
        assert_eq!(parse_area("full"), None);
    }
}
