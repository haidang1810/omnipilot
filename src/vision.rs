//! Cheap, AI-free image checks used when replaying a recorded scenario.

use image::imageops::{self, FilterType};
use image::{GrayImage, RgbaImage};

/// Matching runs at half resolution to stay fast on weak CPUs.
const SCALE: u32 = 2;
/// Zero-mean normalized cross-correlation needed to accept a match.
pub const MATCH_THRESHOLD: f32 = 0.88;

fn half_gray(img: &RgbaImage) -> GrayImage {
    let g = imageops::grayscale(img);
    imageops::resize(&g, (g.width() / SCALE).max(1), (g.height() / SCALE).max(1), FilterType::Triangle)
}

/// Integral images of values and squared values, (w+1)*(h+1).
struct Integral {
    w: usize,
    sum: Vec<f64>,
    sq: Vec<f64>,
}

impl Integral {
    fn new(img: &GrayImage) -> Self {
        let (w, h) = (img.width() as usize, img.height() as usize);
        let stride = w + 1;
        let mut sum = vec![0.0; stride * (h + 1)];
        let mut sq = vec![0.0; stride * (h + 1)];
        let raw = img.as_raw();
        for y in 0..h {
            let (mut rs, mut rq) = (0.0, 0.0);
            for x in 0..w {
                let v = raw[y * w + x] as f64;
                rs += v;
                rq += v * v;
                sum[(y + 1) * stride + x + 1] = sum[y * stride + x + 1] + rs;
                sq[(y + 1) * stride + x + 1] = sq[y * stride + x + 1] + rq;
            }
        }
        Self { w, sum, sq }
    }

    fn rect(&self, t: &[f64], x: usize, y: usize, w: usize, h: usize) -> f64 {
        let s = self.w + 1;
        t[(y + h) * s + x + w] - t[y * s + x + w] - t[(y + h) * s + x] + t[y * s + x]
    }
}

/// Finds `tpl` on `screen`. Searches around `near` (top-left, full-res pixels) first,
/// then the whole screen. Returns the full-res top-left corner and the score.
pub fn find(screen: &RgbaImage, tpl: &RgbaImage, near: Option<(u32, u32)>) -> Option<(u32, u32, f32)> {
    let s = half_gray(screen);
    let t = half_gray(tpl);
    let (sw, sh, tw, th) = (s.width() as usize, s.height() as usize, t.width() as usize, t.height() as usize);
    if tw < 4 || th < 4 || tw > sw || th > sh {
        return None;
    }

    let n = (tw * th) as f64;
    let traw: Vec<f64> = t.as_raw().iter().map(|&v| v as f64).collect();
    let tmean = traw.iter().sum::<f64>() / n;
    let tz: Vec<f64> = traw.iter().map(|v| v - tmean).collect();
    let tnorm = tz.iter().map(|v| v * v).sum::<f64>().sqrt();
    if tnorm < 1e-3 {
        return None; // flat template, nothing to match on
    }
    let integ = Integral::new(&s);
    let sraw = s.as_raw();

    let score_at = |x: usize, y: usize| -> f32 {
        let sum = integ.rect(&integ.sum, x, y, tw, th);
        let sq = integ.rect(&integ.sq, x, y, tw, th);
        let var = sq - sum * sum / n;
        if var <= 1e-6 {
            return 0.0;
        }
        let mut cross = 0.0;
        for j in 0..th {
            let row = &sraw[(y + j) * sw + x..(y + j) * sw + x + tw];
            let trow = &tz[j * tw..(j + 1) * tw];
            for i in 0..tw {
                cross += trow[i] * row[i] as f64;
            }
        }
        (cross / (var.sqrt() * tnorm)) as f32
    };

    let search = |x0: usize, y0: usize, x1: usize, y1: usize| -> (usize, usize, f32) {
        let mut best = (0, 0, f32::MIN);
        for y in y0..=y1 {
            for x in x0..=x1 {
                let sc = score_at(x, y);
                if sc > best.2 {
                    best = (x, y, sc);
                }
            }
        }
        best
    };

    let (max_x, max_y) = (sw - tw, sh - th);
    if let Some((nx, ny)) = near {
        let r = 60 / SCALE as usize;
        let (cx, cy) = ((nx / SCALE) as usize, (ny / SCALE) as usize);
        let best = search(cx.saturating_sub(r).min(max_x), cy.saturating_sub(r).min(max_y), (cx + r).min(max_x), (cy + r).min(max_y));
        if best.2 >= MATCH_THRESHOLD {
            return Some((best.0 as u32 * SCALE, best.1 as u32 * SCALE, best.2));
        }
    }
    let best = search(0, 0, max_x, max_y);
    (best.2 >= MATCH_THRESHOLD).then(|| (best.0 as u32 * SCALE, best.1 as u32 * SCALE, best.2))
}

/// Whether the screen really changed (new screen or popup), ignoring idle animations.
/// Measured on a real game: animated main screen ≈ 0.01 of pixels change strongly,
/// opening another screen ≈ 0.65.
pub fn changed(a: &RgbaImage, b: &RgbaImage) -> bool {
    let small = |img: &RgbaImage| imageops::resize(&imageops::grayscale(img), 64, 36, FilterType::Triangle);
    let (a, b) = (small(a), small(b));
    let strong = a.as_raw().iter().zip(b.as_raw()).filter(|(x, y)| x.abs_diff(**y) > 40).count();
    strong as f32 / (64.0 * 36.0) > 0.05
}
