//! Finding the game's picture and pixel grid on a capture, so the user does
//! not have to: where the game sits on the screen (inside black bars), and
//! how many screen pixels each game pixel covers, per axis.
//!
//! Works on the raw RGBA8 capture, at full resolution: a downscaled capture
//! blends neighbouring pixels and hides the grid.

/// A capture: RGBA8 rows.
pub struct Pixels<'a> {
    pub data: &'a [u8],
    pub width: u32,
    pub height: u32,
}

impl Pixels<'_> {
    fn at(&self, x: u32, y: u32) -> [u8; 3] {
        let i = ((y * self.width + x) * 4) as usize;
        [self.data[i], self.data[i + 1], self.data[i + 2]]
    }
}

/// Brighter than this on any channel counts as picture, not bar.
const BLACK: u8 = 12;

/// The rectangle `[x, y, width, height]` outside of which every row and
/// column is black: the game inside gamescope's or the emulator's bars.
pub fn content_bounds(p: &Pixels) -> Option<[u32; 4]> {
    let lit = |x, y| p.at(x, y).iter().any(|&c| c > BLACK);
    let row = |y| (0..p.width).any(|x| lit(x, y));
    let col = |x, top, bottom| (top..bottom).any(|y| lit(x, y));
    let top = (0..p.height).find(|&y| row(y))?;
    let bottom = (0..p.height).rev().find(|&y| row(y))? + 1;
    let left = (0..p.width).find(|&x| col(x, top, bottom))?;
    let right = (0..p.width).rev().find(|&x| col(x, top, bottom))? + 1;
    Some([left, top, right - left, bottom - top])
}

/// Size of a game pixel along one axis, and where the grid starts.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pitch {
    pub size: f32,
    /// Offset of the first boundary from the start of the region, in
    /// `0..size`.
    pub offset: f32,
    /// Share of colour changes falling on the grid, 0 to 1.
    pub score: f32,
}

/// Positions where the colour changes, along `x` (`horizontal`) or `y`,
/// inside `region`, relative to its start. Sampled, so a 4K capture stays
/// quick.
fn edges(p: &Pixels, region: [u32; 4], horizontal: bool) -> Vec<u32> {
    let [rx, ry, rw, rh] = region;
    let (len, lines) = if horizontal { (rw, rh) } else { (rh, rw) };
    let step = (lines / 240).max(1);
    let mut out = Vec::new();
    for line in (0..lines).step_by(step as usize) {
        let pick = |i: u32| {
            if horizontal {
                p.at(rx + i, ry + line)
            } else {
                p.at(rx + line, ry + i)
            }
        };
        let mut prev = pick(0);
        for i in 1..len {
            let cur = pick(i);
            if cur != prev {
                out.push(i);
            }
            prev = cur;
        }
    }
    out
}

/// How well boundaries at `offset + k * size` explain the edges.
fn fit(edges: &[u32], size: f32) -> (f32, f32) {
    // Phases binned in tenths of a pixel. An edge counts when it falls
    // within half a pixel of the grid (a fractional scale rounds each
    // boundary to the nearest pixel), but never within reach of the next
    // boundary, or a grid of 2 would explain every 1-pixel change.
    let bins = (size * 10.0).ceil() as usize;
    let window = ((size * 2.0).floor() as usize).clamp(1, 5);
    let mut hist = vec![0u32; bins.max(1)];
    for &e in edges {
        let phase = (e as f32).rem_euclid(size);
        hist[((phase * 10.0) as usize).min(bins - 1)] += 1;
    }
    let mut best = (0u32, 0usize);
    for start in 0..bins {
        let count: u32 = (0..=2 * window).map(|d| hist[(start + bins - window + d) % bins]).sum();
        if count > best.0 {
            best = (count, start);
        }
    }
    (best.0 as f32 / edges.len().max(1) as f32, best.1 as f32 / 10.0)
}

/// The largest grid that explains nearly every colour change along one axis.
/// The largest, because a grid of half the size explains them too.
pub fn pitch(p: &Pixels, region: [u32; 4], horizontal: bool) -> Option<Pitch> {
    let all = edges(p, region, horizontal);
    if all.len() < 16 {
        return None;
    }
    // Plenty for a reliable vote, few enough to stay instant.
    let stride = (all.len() / 40_000).max(1);
    let edges: Vec<u32> = all.into_iter().step_by(stride).collect();
    let len = if horizontal { region[2] } else { region[3] } as f32;
    let mut size = (len / 16.0).min(64.0);
    while size >= 1.0 {
        let (score, offset) = fit(&edges, size);
        if score >= 0.97 {
            return Some(Pitch { size, offset, score });
        }
        // Integer sizes first (integer scaling), fractions in between.
        size -= 0.05;
        size = (size * 20.0).round() / 20.0;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `game` sized picture of pseudo-random colours, each pixel drawn
    /// `cell` screen pixels wide and tall, at `at` in a black `screen`.
    fn screen(screen: [u32; 2], game: [u32; 2], cell: [u32; 2], at: [u32; 2]) -> Vec<u8> {
        let mut data = vec![0u8; (screen[0] * screen[1] * 4) as usize];
        for gy in 0..game[1] {
            for gx in 0..game[0] {
                let seed = gx.wrapping_mul(73_856_093) ^ gy.wrapping_mul(19_349_663);
                let colour = [(seed % 200) as u8 + 40, (seed / 7 % 200) as u8 + 40, (seed / 13 % 200) as u8 + 40];
                for dy in 0..cell[1] {
                    for dx in 0..cell[0] {
                        let (x, y) = (at[0] + gx * cell[0] + dx, at[1] + gy * cell[1] + dy);
                        let i = ((y * screen[0] + x) * 4) as usize;
                        data[i..i + 3].copy_from_slice(&colour);
                        data[i + 3] = 255;
                    }
                }
            }
        }
        data
    }

    #[test]
    fn finds_the_game_inside_black_bars() {
        let data = screen([800, 500], [80, 60], [6, 6], [160, 70]);
        let p = Pixels { data: &data, width: 800, height: 500 };
        assert_eq!(content_bounds(&p), Some([160, 70, 480, 360]));
    }

    #[test]
    fn finds_square_pixels() {
        let data = screen([800, 500], [80, 60], [6, 6], [160, 70]);
        let p = Pixels { data: &data, width: 800, height: 500 };
        let area = content_bounds(&p).unwrap();
        assert_eq!(pitch(&p, area, true).map(|p| p.size), Some(6.0));
        assert_eq!(pitch(&p, area, false).map(|p| p.size), Some(6.0));
    }

    #[test]
    fn finds_half_height_pixels() {
        // A 640x200 mode shown with square pixels: lines half as tall as
        // pixels are wide.
        let data = screen([800, 500], [100, 120], [6, 3], [100, 70]);
        let p = Pixels { data: &data, width: 800, height: 500 };
        let area = content_bounds(&p).unwrap();
        assert_eq!(area, [100, 70, 600, 360]);
        assert_eq!(pitch(&p, area, true).map(|p| p.size), Some(6.0));
        assert_eq!(pitch(&p, area, false).map(|p| p.size), Some(3.0));
    }

    #[test]
    fn finds_a_fractional_scale() {
        // 640 pixels fitted into 2880: 4.5 screen pixels each, nearest.
        let (w, h) = (2880u32, 90u32);
        let mut data = vec![0u8; (w * h * 4) as usize];
        for y in 0..h {
            for x in 0..w {
                let gx = (x as f32 / 4.5) as u32;
                let seed = gx.wrapping_mul(73_856_093) ^ (y / 3).wrapping_mul(19_349_663);
                let i = ((y * w + x) * 4) as usize;
                data[i..i + 4].copy_from_slice(&[(seed % 200) as u8 + 40, (seed / 7 % 200) as u8 + 40, 90, 255]);
            }
        }
        let p = Pixels { data: &data, width: w, height: h };
        assert_eq!(pitch(&p, [0, 0, w, h], true).map(|p| p.size), Some(4.5));
        assert_eq!(pitch(&p, [0, 0, w, h], false).map(|p| p.size), Some(3.0));
    }

    #[test]
    fn a_blurry_picture_has_no_grid() {
        // Every pixel different: nothing larger than one pixel explains it,
        // and one pixel means no scaling was found.
        let data = screen([300, 200], [300, 200], [1, 1], [0, 0]);
        let p = Pixels { data: &data, width: 300, height: 200 };
        let found = pitch(&p, [0, 0, 300, 200], true).map(|p| p.size);
        assert!(found.is_none_or(|s| s <= 1.0), "{found:?}");
    }
}

/// Runs the detection on a real capture: `VKSLANG_TEST_CAPTURE=<file>
/// cargo test -p vkslang-ui real_capture -- --ignored --nocapture`.
#[cfg(test)]
#[test]
#[ignore]
fn real_capture() {
    let path = std::env::var("VKSLANG_TEST_CAPTURE").expect("VKSLANG_TEST_CAPTURE");
    let data = std::fs::read(path).unwrap();
    let width = u32::from_le_bytes(data[4..8].try_into().unwrap());
    let height = u32::from_le_bytes(data[8..12].try_into().unwrap());
    let p = Pixels { data: &data[12..], width, height };
    let zone = content_bounds(&p);
    println!("capture {width}x{height}, zone {zone:?}");
    let zone = zone.unwrap();
    println!("horizontal {:?}", pitch(&p, zone, true));
    println!("vertical {:?}", pitch(&p, zone, false));
}
