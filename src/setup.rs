//! First-run download of the AI runtime (llama.cpp) and the model files.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};

use crate::paths;

const LLAMA_BUILD: &str = "b12064";
const MODEL_REPO: &str = "unsloth/Qwen3.5-4B-GGUF";
pub const MODEL_FILE: &str = "Qwen3.5-4B-Q4_K_M.gguf";
pub const MMPROJ_FILE: &str = "Qwen3.5-4B-mmproj-F16.gguf";

struct Asset {
    label: &'static str,
    url: String,
    dest: PathBuf,
    /// Zip archives are extracted into this directory after download.
    extract_to: Option<PathBuf>,
}

pub fn llama_server_exe() -> PathBuf {
    paths::runtime_dir().join("vulkan").join("llama-server.exe")
}

pub fn model_path() -> PathBuf {
    paths::models_dir().join(MODEL_FILE)
}

pub fn mmproj_path() -> PathBuf {
    paths::models_dir().join(MMPROJ_FILE)
}

fn assets() -> Vec<Asset> {
    vec![
        Asset {
            label: "AI runtime (llama.cpp)",
            url: format!(
                "https://github.com/ggml-org/llama.cpp/releases/download/{LLAMA_BUILD}/llama-{LLAMA_BUILD}-bin-win-vulkan-x64.zip"
            ),
            dest: paths::runtime_dir().join("llama-vulkan.zip"),
            extract_to: Some(paths::runtime_dir().join("vulkan")),
        },
        Asset {
            label: "Model AI (Qwen3.5-4B)",
            url: format!("https://huggingface.co/{MODEL_REPO}/resolve/main/{MODEL_FILE}"),
            dest: model_path(),
            extract_to: None,
        },
        Asset {
            label: "Bộ nhìn ảnh (vision)",
            url: format!("https://huggingface.co/{MODEL_REPO}/resolve/main/mmproj-F16.gguf"),
            dest: mmproj_path(),
            extract_to: None,
        },
    ]
}

pub fn is_ready() -> bool {
    llama_server_exe().exists() && model_path().exists() && mmproj_path().exists()
}

#[derive(Default, Clone)]
pub struct Progress {
    pub label: String,
    pub step: usize,
    pub total_steps: usize,
    pub downloaded: u64,
    pub size: u64,
    pub done: bool,
    pub error: Option<String>,
}

pub type SharedProgress = Arc<Mutex<Progress>>;

pub fn start(progress: SharedProgress) {
    std::thread::spawn(move || {
        let result = run(&progress);
        let mut p = progress.lock().unwrap();
        match result {
            Ok(()) => p.done = true,
            Err(e) => p.error = Some(format!("{e:#}")),
        }
    });
}

fn run(progress: &SharedProgress) -> Result<()> {
    let list = assets();
    let total = list.len();
    for (i, asset) in list.into_iter().enumerate() {
        {
            let mut p = progress.lock().unwrap();
            *p = Progress { label: asset.label.to_string(), step: i + 1, total_steps: total, ..Default::default() };
        }
        let already = match &asset.extract_to {
            Some(dir) => dir.join("llama-server.exe").exists(),
            None => asset.dest.exists(),
        };
        if already {
            continue;
        }
        download(&asset.url, &asset.dest, progress).with_context(|| format!("tải {} thất bại", asset.label))?;
        if let Some(dir) = &asset.extract_to {
            extract_zip(&asset.dest, dir)?;
            fs::remove_file(&asset.dest).ok();
        }
    }
    Ok(())
}

fn download(url: &str, dest: &Path, progress: &SharedProgress) -> Result<()> {
    fs::create_dir_all(dest.parent().unwrap())?;
    let part = dest.with_extension("part");
    let mut resp = ureq::get(url).call()?;
    let size = resp
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0);
    progress.lock().unwrap().size = size;

    let mut reader = resp.body_mut().as_reader();
    let mut file = File::create(&part)?;
    let mut buf = vec![0u8; 1 << 20];
    let mut downloaded = 0u64;
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n])?;
        downloaded += n as u64;
        progress.lock().unwrap().downloaded = downloaded;
    }
    file.flush()?;
    drop(file);
    if size > 0 && downloaded != size {
        bail!("tải thiếu dữ liệu ({downloaded}/{size} bytes)");
    }
    fs::rename(&part, dest)?;
    Ok(())
}

fn extract_zip(zip_path: &Path, dir: &Path) -> Result<()> {
    fs::create_dir_all(dir)?;
    let mut archive = zip::ZipArchive::new(File::open(zip_path)?)?;
    archive.extract(dir)?;
    Ok(())
}
