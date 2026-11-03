//! Scenario model and persistence.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::paths;

/// What the trial run learned about a step, so later runs need no AI.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Rec {
    /// PNG file (inside the scenario folder) cropped around the element.
    pub template: String,
    /// Top-left corner of the template on the screen when it was recorded.
    pub x: u32,
    pub y: u32,
}

fn d_max_tap_all() -> u32 {
    30
}
fn d_timeout() -> u32 {
    300
}
fn d_wait() -> f32 {
    2.0
}
fn d_loop_max() -> u32 {
    50
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Step {
    Tap {
        target: String,
        #[serde(default)]
        hint: String,
        /// The target changes every run ("map chưa đi"), so it always needs the AI.
        #[serde(default)]
        dynamic: bool,
        #[serde(default)]
        rec: Option<Rec>,
    },
    TapAll {
        target: String,
        #[serde(default)]
        hint: String,
        #[serde(default = "d_max_tap_all")]
        max: u32,
        #[serde(default)]
        rec: Option<Rec>,
    },
    WaitUntil {
        #[serde(alias = "condition")]
        target: String,
        #[serde(default = "d_timeout")]
        timeout_sec: u32,
        #[serde(default)]
        rec: Option<Rec>,
    },
    Wait {
        #[serde(default = "d_wait")]
        seconds: f32,
    },
    Back,
    Loop {
        #[serde(default = "d_loop_max")]
        max: u32,
        steps: Vec<Step>,
    },
}

impl Step {
    pub fn describe(&self) -> String {
        match self {
            Step::Tap { target, hint, dynamic, .. } => {
                let mut s = format!("Bấm: {target}");
                if !hint.is_empty() {
                    s += &format!(" ({hint})");
                }
                if *dynamic {
                    s += "  [AI mỗi lần]";
                }
                s
            }
            Step::TapAll { target, .. } => format!("Bấm hết: {target}"),
            Step::WaitUntil { target, timeout_sec, .. } => format!("Đợi đến khi thấy: {target} (tối đa {timeout_sec}s)"),
            Step::Wait { seconds } => format!("Chờ {seconds}s"),
            Step::Back => "Nhấn Back".to_string(),
            Step::Loop { max, .. } => format!("Lặp (tối đa {max} lần):"),
        }
    }

    pub fn target_mut(&mut self) -> Option<&mut String> {
        match self {
            Step::Tap { target, .. } | Step::TapAll { target, .. } | Step::WaitUntil { target, .. } => Some(target),
            _ => None,
        }
    }

    /// Drops what was learned, e.g. after the user edits the target text.
    pub fn clear_rec(&mut self) {
        match self {
            Step::Tap { rec, .. } | Step::TapAll { rec, .. } | Step::WaitUntil { rec, .. } => *rec = None,
            Step::Loop { steps, .. } => steps.iter_mut().for_each(Step::clear_rec),
            _ => {}
        }
    }
}

pub const CLOSE_TARGET: &str = "nút đóng hoặc quay lại (dấu X hoặc mũi tên quay lại)";

/// "đóng", "nút thoát", "quay lại"... – games draw these very differently,
/// so they get one generic description and an Android Back fallback.
pub fn is_close_target(target: &str) -> bool {
    let t = target.trim().to_lowercase();
    let t = t.strip_prefix("nút ").or_else(|| t.strip_prefix("button ")).unwrap_or(&t).trim();
    t == CLOSE_TARGET.to_lowercase().trim_start_matches("nút ")
        || ["đóng", "đóng lại", "thoát", "quay lại", "trở lại", "x", "close", "back"].contains(&t)
}

/// Replaces close-like targets with the generic description, recursively.
/// Many games ignore Android Back, so "back" also becomes a tap on the on-screen
/// close button (which still falls back to Android Back if none is visible).
pub fn normalize(steps: &mut [Step]) {
    for s in steps {
        match s {
            Step::Back => *s = Step::Tap { target: CLOSE_TARGET.into(), hint: String::new(), dynamic: false, rec: None },
            Step::Tap { target, .. } if is_close_target(target) => *target = CLOSE_TARGET.into(),
            Step::Loop { steps, .. } => normalize(steps),
            _ => {}
        }
    }
}

/// Number of steps that still need the AI on every run.
pub fn dynamic_count(steps: &[Step]) -> usize {
    steps
        .iter()
        .map(|s| match s {
            Step::Tap { dynamic: true, .. } => 1,
            Step::Loop { steps, .. } => dynamic_count(steps),
            _ => 0,
        })
        .sum()
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Schedule {
    /// Run again and again, pausing between runs (0 = back-to-back).
    Repeat { pause_minutes: u32 },
    /// Run at fixed times every day ("HH:MM").
    Daily { times: Vec<String> },
}

impl Schedule {
    pub fn describe(&self) -> String {
        match self {
            Schedule::Repeat { pause_minutes: 0 } => "Lặp liên tục".into(),
            Schedule::Repeat { pause_minutes } => format!("Lặp, nghỉ {pause_minutes} phút"),
            Schedule::Daily { times } => format!("Hằng ngày {}", times.join(", ")),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Scenario {
    pub id: String,
    pub name: String,
    pub description: String,
    pub instance_index: u32,
    pub schedule: Schedule,
    pub steps: Vec<Step>,
    /// True once a trial run was confirmed by the user.
    pub recorded: bool,
    /// Emulator resolution the templates were recorded at.
    #[serde(default)]
    pub screen_size: Option<[u32; 2]>,
}

impl Scenario {
    pub fn new() -> Self {
        Self {
            id: uuid::Uuid::new_v4().simple().to_string(),
            name: String::new(),
            description: String::new(),
            instance_index: 0,
            schedule: Schedule::Repeat { pause_minutes: 0 },
            steps: Vec::new(),
            recorded: false,
            screen_size: None,
        }
    }

    pub fn dir(&self) -> PathBuf {
        paths::scenarios_dir().join(&self.id)
    }

    pub fn save(&self) -> Result<()> {
        fs::create_dir_all(self.dir())?;
        fs::write(self.dir().join("scenario.json"), serde_json::to_string_pretty(self)?)?;
        Ok(())
    }

    pub fn delete(&self) {
        let _ = fs::remove_dir_all(self.dir());
    }

    /// Writes a shareable `.omnipilot` file: a zip with scenario.json and the templates.
    pub fn export(&self, path: &Path) -> Result<()> {
        let mut zip = zip::ZipWriter::new(fs::File::create(path)?);
        let opts = zip::write::SimpleFileOptions::default();
        zip.start_file("scenario.json", opts)?;
        zip.write_all(serde_json::to_string_pretty(self)?.as_bytes())?;
        let mut names = Vec::new();
        collect_templates(&self.steps, &mut names);
        for name in names {
            if let Ok(bytes) = fs::read(self.dir().join(&name)) {
                zip.start_file(name, opts)?;
                zip.write_all(&bytes)?;
            }
        }
        zip.finish()?;
        Ok(())
    }

    /// Reads a `.omnipilot` file into a new scenario (fresh id, so imports never clash).
    pub fn import(path: &Path) -> Result<Scenario> {
        let mut archive = zip::ZipArchive::new(fs::File::open(path)?).context("file không phải kịch bản OmniPilot")?;
        let mut json = String::new();
        archive.by_name("scenario.json").context("file thiếu scenario.json")?.read_to_string(&mut json)?;
        let mut sc: Scenario = serde_json::from_str(&json).context("scenario.json không hợp lệ")?;
        sc.id = uuid::Uuid::new_v4().simple().to_string();

        let mut names = Vec::new();
        collect_templates(&sc.steps, &mut names);
        if names.iter().any(|n| !is_template_name(n)) {
            bail!("file kịch bản chứa tên ảnh không hợp lệ");
        }
        fs::create_dir_all(sc.dir())?;
        for name in names {
            // Only the plain template files the steps reference are extracted (no paths).
            if let Ok(mut f) = archive.by_name(&name) {
                let mut bytes = Vec::new();
                f.read_to_end(&mut bytes)?;
                fs::write(sc.dir().join(&name), bytes)?;
            }
        }
        sc.save()?;
        Ok(sc)
    }
}

fn is_template_name(name: &str) -> bool {
    name.starts_with("tpl_") && name.ends_with(".png") && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.') && !name.contains("..")
}

fn collect_templates(steps: &[Step], out: &mut Vec<String>) {
    for s in steps {
        match s {
            Step::Tap { rec: Some(r), .. } | Step::TapAll { rec: Some(r), .. } | Step::WaitUntil { rec: Some(r), .. } => out.push(r.template.clone()),
            Step::Loop { steps, .. } => collect_templates(steps, out),
            _ => {}
        }
    }
}

pub fn load_all() -> Vec<Scenario> {
    let Ok(entries) = fs::read_dir(paths::scenarios_dir()) else { return Vec::new() };
    let mut list: Vec<Scenario> = entries
        .flatten()
        .filter_map(|e| fs::read_to_string(e.path().join("scenario.json")).ok())
        .filter_map(|s| serde_json::from_str(&s).ok())
        .collect();
    list.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    list
}
