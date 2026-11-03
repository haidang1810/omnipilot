//! Scenario model and persistence.
//!
//! A scenario is a list of `Node`s. Each node is one step the user typed
//! ("bấm xuất chiến góc phải dưới") plus the AI's interpretation of it, or a loop block.

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
        /// Text that must NOT be on or right above the element ("Hoàn toàn thu hồi").
        #[serde(default)]
        exclude: Vec<String>,
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
        exclude: Vec<String>,
        #[serde(default)]
        rec: Option<Rec>,
    },
    WaitUntil {
        #[serde(alias = "condition")]
        target: String,
        #[serde(default = "d_timeout")]
        timeout_sec: u32,
        #[serde(default)]
        exclude: Vec<String>,
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
        steps: Vec<Node>,
    },
}

/// One row of the scenario editor.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Node {
    /// What the user typed (empty for loop blocks).
    #[serde(default)]
    pub text: String,
    /// The AI's interpretation; None until it has been analysed.
    #[serde(default)]
    pub step: Option<Step>,
    /// "Nếu không thấy thì…": run this when the step finds nothing, then try again.
    #[serde(default)]
    pub otherwise: Option<Box<Node>>,
    /// How many times `otherwise` may run before giving up (e.g. pages to flip).
    #[serde(default = "d_otherwise_max")]
    pub otherwise_max: u32,
    /// Part of the screen to search, as fractions [x1, y1, x2, y2] of its size.
    #[serde(default)]
    pub region: Option<[f32; 4]>,
}

fn d_otherwise_max() -> u32 {
    10
}

impl Node {
    pub fn new() -> Self {
        Self::with_text("")
    }

    pub fn with_text(text: &str) -> Self {
        Self {
            text: text.into(),
            step: None,
            otherwise: None,
            otherwise_max: d_otherwise_max(),
            region: None,
        }
    }

    pub fn new_loop() -> Self {
        Self {
            step: Some(Step::Loop {
                max: 50,
                steps: vec![Node::new()],
            }),
            ..Self::new()
        }
    }

    pub fn is_loop(&self) -> bool {
        matches!(self.step, Some(Step::Loop { .. }))
    }
}

impl Step {
    /// Short Vietnamese summary of how the AI understood a step.
    pub fn describe(&self) -> String {
        match self {
            Step::Tap {
                target,
                hint,
                dynamic,
                exclude,
                ..
            } => {
                let mut s = format!("Bấm \"{target}\"");
                if !hint.is_empty() {
                    s += &format!(" ở {hint}");
                }
                if !exclude.is_empty() {
                    s += &format!(" · bỏ qua ô có chữ {}", quote_list(exclude));
                }
                if *dynamic {
                    s += " · AI tìm lại mỗi lần chạy";
                }
                s
            }
            Step::TapAll {
                target,
                max,
                exclude,
                ..
            } if !exclude.is_empty() => {
                format!(
                    "Bấm hết các \"{target}\" (tối đa {max}) · bỏ qua ô có chữ {}",
                    quote_list(exclude)
                )
            }
            Step::TapAll { target, max, .. } => format!("Bấm hết các \"{target}\" (tối đa {max})"),
            Step::WaitUntil {
                target,
                timeout_sec,
                ..
            } => format!("Đợi đến khi thấy \"{target}\" (tối đa {timeout_sec}s)"),
            Step::Wait { seconds } => format!("Chờ {seconds} giây"),
            Step::Back => "Nhấn Back".to_string(),
            Step::Loop { max, .. } => format!("Lặp tối đa {max} lần"),
        }
    }

    pub fn exclude(&self) -> &[String] {
        match self {
            Step::Tap { exclude, .. }
            | Step::TapAll { exclude, .. }
            | Step::WaitUntil { exclude, .. } => exclude,
            _ => &[],
        }
    }

    /// Drops what was learned.
    pub fn clear_rec(&mut self) {
        match self {
            Step::Tap { rec, .. } | Step::TapAll { rec, .. } | Step::WaitUntil { rec, .. } => {
                *rec = None
            }
            Step::Loop { steps, .. } => clear_rec(steps),
            _ => {}
        }
    }
}

fn quote_list(items: &[String]) -> String {
    items
        .iter()
        .map(|t| format!("\"{t}\""))
        .collect::<Vec<_>>()
        .join(", ")
}

pub fn clear_rec(nodes: &mut [Node]) {
    for_each_leaf_mut(nodes, &mut |n| {
        if let Some(s) = &mut n.step {
            s.clear_rec();
        }
    });
}

/// Calls `f` on every non-loop node (and then on its "nếu không thấy" node), depth first.
pub fn for_each_leaf_mut(nodes: &mut [Node], f: &mut dyn FnMut(&mut Node)) {
    for n in nodes {
        match &mut n.step {
            Some(Step::Loop { steps, .. }) => for_each_leaf_mut(steps, f),
            _ => {
                f(n);
                if let Some(o) = n.otherwise.as_deref_mut() {
                    f(o);
                }
            }
        }
    }
}

/// Text buttons first: an early "dấu X hoặc mũi tên" wording made the model pick
/// crossed-swords battle icons and map arrows instead of "Trở lại".
pub const CLOSE_TARGET: &str = "nút để quay lại / thoát khỏi màn hình này (thường ở góc trên: chữ \"Trở lại\", \"Quay lại\", \"Đóng\", \"Thoát\", nút mũi tên quay lại, hoặc nút X đóng cửa sổ)";
/// Wording used by earlier versions, still found in saved scenarios.
const OLD_CLOSE_TARGET: &str = "nút đóng hoặc quay lại (dấu X hoặc mũi tên quay lại)";

/// "đóng", "nút thoát", "quay lại"... – games draw these very differently,
/// so they get one generic description and an Android Back fallback.
pub fn is_close_target(target: &str) -> bool {
    let t = target.trim().to_lowercase();
    let t = t
        .strip_prefix("nút ")
        .or_else(|| t.strip_prefix("button "))
        .unwrap_or(&t)
        .trim();
    t == CLOSE_TARGET.to_lowercase().trim_start_matches("nút ")
        || t == OLD_CLOSE_TARGET.trim_start_matches("nút ")
        || [
            "đóng",
            "đóng lại",
            "thoát",
            "thoát ra",
            "quay lại",
            "trở lại",
            "x",
            "close",
            "back",
        ]
        .contains(&t)
}

/// Many games ignore Android Back, so "back" becomes a tap on the on-screen close
/// button (which still falls back to Android Back if none is visible).
pub fn normalize(step: Step) -> Step {
    match step {
        Step::Back => Step::Tap {
            target: CLOSE_TARGET.into(),
            hint: String::new(),
            dynamic: false,
            exclude: Vec::new(),
            rec: None,
        },
        Step::Tap {
            target,
            hint,
            dynamic,
            exclude,
            rec,
        } if is_close_target(&target) => Step::Tap {
            target: CLOSE_TARGET.into(),
            hint,
            dynamic,
            exclude,
            rec,
        },
        Step::Tap {
            target,
            hint,
            dynamic,
            exclude,
            rec,
        } => {
            // Which element passes the text filter changes from run to run.
            let dynamic = dynamic || !exclude.is_empty();
            Step::Tap {
                target: strip_verb(&target),
                hint,
                dynamic,
                exclude,
                rec,
            }
        }
        Step::TapAll {
            target,
            hint,
            max,
            exclude,
            rec,
        } => Step::TapAll {
            target: strip_verb(&target),
            hint,
            max,
            exclude,
            rec,
        },
        s => s,
    }
}

/// The model sometimes keeps the verb in the button name ("Vô túi đồ" → "túi đồ").
fn strip_verb(target: &str) -> String {
    const VERBS: [&str; 9] = [
        "bấm vào ",
        "nhấn vào ",
        "click vào ",
        "vô ",
        "vào ",
        "bấm ",
        "nhấn ",
        "click ",
        "mở ",
    ];
    let lower = target.trim().to_lowercase();
    for v in VERBS {
        if lower.starts_with(v) {
            // Skip by chars, not bytes: lowercasing may change UTF-8 lengths.
            let rest: String = target.trim().chars().skip(v.chars().count()).collect();
            if !rest.trim().is_empty() {
                return rest.trim().to_string();
            }
        }
    }
    target.trim().to_string()
}

/// Number of steps that still need the AI on every run.
pub fn dynamic_count(nodes: &[Node]) -> usize {
    nodes
        .iter()
        .map(|n| match &n.step {
            Some(Step::Tap { dynamic: true, .. }) => 1,
            Some(Step::Loop { steps, .. }) => dynamic_count(steps),
            _ => 0,
        })
        .sum()
}

/// Number of steps the user typed (loops not counted).
pub fn leaf_count(nodes: &[Node]) -> usize {
    nodes
        .iter()
        .map(|n| match &n.step {
            Some(Step::Loop { steps, .. }) => leaf_count(steps),
            _ => 1,
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
    /// Run once when Start is pressed, then stop.
    Once,
}

impl Schedule {
    pub fn describe(&self) -> String {
        match self {
            Schedule::Repeat { pause_minutes: 0 } => "Lặp liên tục".into(),
            Schedule::Repeat { pause_minutes } => format!("Lặp, nghỉ {pause_minutes} phút"),
            Schedule::Daily { times } => format!("Hằng ngày {}", times.join(", ")),
            Schedule::Once => "Chạy 1 lần".into(),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Scenario {
    pub id: String,
    pub name: String,
    pub instance_index: u32,
    pub schedule: Schedule,
    pub steps: Vec<Node>,
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
            instance_index: 0,
            schedule: Schedule::Repeat { pause_minutes: 0 },
            steps: vec![Node::new()],
            recorded: false,
            screen_size: None,
        }
    }

    pub fn dir(&self) -> PathBuf {
        paths::scenarios_dir().join(&self.id)
    }

    pub fn save(&self) -> Result<()> {
        fs::create_dir_all(self.dir())?;
        fs::write(
            self.dir().join("scenario.json"),
            serde_json::to_string_pretty(self)?,
        )?;
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
        let mut archive = zip::ZipArchive::new(fs::File::open(path)?)
            .context("file không phải kịch bản OmniPilot")?;
        let mut json = String::new();
        archive
            .by_name("scenario.json")
            .context("file thiếu scenario.json")?
            .read_to_string(&mut json)?;
        let mut sc: Scenario =
            serde_json::from_str(strip_bom(&json)).context("scenario.json không hợp lệ")?;
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

/// Files edited with some Windows tools start with a UTF-8 BOM, which serde rejects.
fn strip_bom(s: &str) -> &str {
    s.strip_prefix('\u{feff}').unwrap_or(s)
}

fn is_template_name(name: &str) -> bool {
    name.starts_with("tpl_")
        && name.ends_with(".png")
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
        && !name.contains("..")
}

fn collect_templates(nodes: &[Node], out: &mut Vec<String>) {
    for n in nodes {
        if let Some(o) = &n.otherwise {
            collect_templates(std::slice::from_ref(o.as_ref()), out);
        }
        match &n.step {
            Some(
                Step::Tap { rec: Some(r), .. }
                | Step::TapAll { rec: Some(r), .. }
                | Step::WaitUntil { rec: Some(r), .. },
            ) => out.push(r.template.clone()),
            Some(Step::Loop { steps, .. }) => collect_templates(steps, out),
            _ => {}
        }
    }
}

pub fn load_all() -> Vec<Scenario> {
    let Ok(entries) = fs::read_dir(paths::scenarios_dir()) else {
        return Vec::new();
    };
    let mut list: Vec<Scenario> = entries
        .flatten()
        .filter_map(|e| fs::read_to_string(e.path().join("scenario.json")).ok())
        .filter_map(|s| serde_json::from_str(strip_bom(&s)).ok())
        .collect();
    list.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    list
}
