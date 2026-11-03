//! Fast, AI-free reading of short text (numbers like "97/120", "Lv.252", "16:08") with the
//! PaddleOCR PP-OCRv5 latin recognition model (~8MB ONNX). One text line takes ~10ms.
//! Its alphabet has no Vietnamese tone marks: numbers and plain Latin text only.

use std::sync::{Mutex, OnceLock};

use anyhow::{Context, Result};
use image::RgbaImage;
use ort::session::Session;
use ort::value::Tensor;

use crate::setup;

struct Ocr {
    session: Session,
    /// Class i (1-based; 0 is the CTC blank) is dict[i - 1]; one past the end is a space.
    dict: Vec<String>,
    // Keeps ONNX Runtime alive as long as the session.
    _env: ort::environment::Environment,
}

static OCR: OnceLock<Mutex<Option<Ocr>>> = OnceLock::new();

fn load() -> Result<Ocr> {
    let env = ort::environment::init().build().context("không khởi tạo được ONNX Runtime")?;
    let session = Session::builder(&env)?
        .commit_from_file(setup::ocr_model_path())
        .context("không mở được model OCR")?;
    let dict = std::fs::read_to_string(setup::ocr_dict_path())?.lines().map(str::to_string).collect();
    Ok(Ocr { session, dict, _env: env })
}

/// Reads one line of text in `img` (a small crop around it).
pub fn read_line(img: &RgbaImage) -> Result<String> {
    let cell = OCR.get_or_init(|| Mutex::new(None));
    let mut guard = cell.lock().unwrap();
    if guard.is_none() {
        *guard = Some(load()?);
    }
    let ocr = guard.as_mut().unwrap();

    // The model takes 48px-high lines, normalized to [-1, 1], channels in BGR order.
    let h = 48u32;
    let w = ((img.width() as f32 * h as f32 / img.height().max(1) as f32).ceil() as u32).clamp(16, 2000);
    let resized = image::imageops::resize(img, w, h, image::imageops::FilterType::Triangle);
    let plane = (h * w) as usize;
    let mut data = vec![0f32; 3 * plane];
    for (x, y, p) in resized.enumerate_pixels() {
        let i = (y * w + x) as usize;
        for c in 0..3 {
            data[c * plane + i] = p[2 - c] as f32 / 127.5 - 1.0;
        }
    }
    let input = Tensor::from_array(([1usize, 3, h as usize, w as usize], data.into_boxed_slice()))?;
    let outputs = ocr.session.run(ort::inputs![input])?;
    let (shape, probs) = outputs[0].try_extract_tensor::<f32>()?;
    let (steps, classes) = (shape[1] as usize, shape[2] as usize);

    // Greedy CTC decoding: best class per step, collapse repeats, drop blanks.
    let mut text = String::new();
    let mut last = 0usize;
    for s in 0..steps {
        let row = &probs[s * classes..(s + 1) * classes];
        let best = row.iter().enumerate().fold((0, f32::MIN), |a, (i, &p)| if p > a.1 { (i, p) } else { a }).0;
        if best != 0 && best != last {
            text.push_str(ocr.dict.get(best - 1).map(String::as_str).unwrap_or(" "));
        }
        last = best;
    }
    Ok(text.trim().to_string())
}
