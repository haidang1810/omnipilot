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
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Rec {
    /// PNG file (inside the scenario folder) cropped around the element.
    pub template: String,
    /// Top-left corner of the template on the screen when it was recorded.
    pub x: u32,
    pub y: u32,
    /// How similar the screen must be to count as a match (0..1); None = default 0.88.
    #[serde(default)]
    pub threshold: Option<f32>,
}

impl Rec {
    pub fn threshold(&self) -> f32 {
        self.threshold.unwrap_or(crate::vision::MATCH_THRESHOLD)
    }
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
        /// Stop looping as soon as this holds (checked before every round).
        #[serde(default)]
        until: Option<Cond>,
    },
    /// "Nếu … thì … không thì …".
    If {
        cond: Cond,
        #[serde(default)]
        then_steps: Vec<Node>,
        #[serde(default)]
        else_steps: Vec<Node>,
    },
    /// End this run successfully (e.g. out of stamina).
    StopRun,
    /// Sets a variable: a number, plus/minus a number, or the number read in an area.
    SetVar { name: String, change: VarOp },
    /// Run another saved scenario's steps here ("kịch bản con").
    Call {
        scenario_id: String,
        /// Name at the time it was chosen, for display.
        #[serde(default)]
        name: String,
    },
    // ---- Steps set up by hand on the screenshot (no AI). ----
    /// Tap a fixed point, as fractions of the screen size.
    TapPoint { x: f32, y: f32 },
    /// Tap wherever the saved picture of a button is found (`all`: every copy of it).
    TapImage {
        #[serde(default)]
        all: bool,
        #[serde(default = "d_max_tap_all")]
        max: u32,
        rec: Rec,
    },
    /// Wait until the saved picture shows up.
    WaitImage {
        #[serde(default = "d_timeout")]
        timeout_sec: u32,
        rec: Rec,
    },
    /// Swipe between two points (fractions of the screen size).
    Swipe {
        from: [f32; 2],
        to: [f32; 2],
        #[serde(default = "d_swipe_ms")]
        ms: u32,
    },
}

fn d_swipe_ms() -> u32 {
    400
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Cmp {
    Lt,
    Le,
    Gt,
    Ge,
    Eq,
}

impl Cmp {
    pub const ALL: [Cmp; 5] = [Cmp::Lt, Cmp::Le, Cmp::Gt, Cmp::Ge, Cmp::Eq];

    pub fn symbol(self) -> &'static str {
        match self {
            Cmp::Lt => "<",
            Cmp::Le => "≤",
            Cmp::Gt => ">",
            Cmp::Ge => "≥",
            Cmp::Eq => "=",
        }
    }

    pub fn test(self, a: f64, b: f64) -> bool {
        match self {
            Cmp::Lt => a < b,
            Cmp::Le => a <= b,
            Cmp::Gt => a > b,
            Cmp::Ge => a >= b,
            Cmp::Eq => (a - b).abs() < 1e-9,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum VarOp {
    Set { value: f64 },
    Add { value: f64 },
    /// The first number read inside the area (fractions of the screen).
    Read { region: [f32; 4] },
}

/// How long a variable keeps its value.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Default)]
#[serde(rename_all = "snake_case")]
pub enum VarScope {
    /// Starts at 0 every run.
    #[default]
    Run,
    /// Kept between runs, back to 0 every day (e.g. battles fought today).
    Day,
    /// Kept until changed.
    Forever,
}

impl VarScope {
    pub const ALL: [VarScope; 3] = [VarScope::Run, VarScope::Day, VarScope::Forever];

    pub fn label(self) -> &'static str {
        match self {
            VarScope::Run => "về 0 mỗi lượt",
            VarScope::Day => "về 0 mỗi ngày",
            VarScope::Forever => "giữ mãi",
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct VarDef {
    pub name: String,
    #[serde(default)]
    pub scope: VarScope,
}

/// Something checked on the current screen.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Cond {
    /// The saved picture is visible (anywhere, or inside `region` if set).
    SeeImage {
        rec: Rec,
        #[serde(default)]
        region: Option<[f32; 4]>,
    },
    /// The first number read inside `region` (fractions of the screen) compares to `value`.
    /// "97/120" reads as 97.
    Number { region: [f32; 4], op: Cmp, value: f64 },
    /// A variable compares to `value`.
    Var { name: String, op: Cmp, value: f64 },
}

impl Cond {
    pub fn new_image() -> Self {
        Cond::SeeImage { rec: Rec::default(), region: None }
    }

    pub fn new_number() -> Self {
        Cond::Number { region: [0.0; 4], op: Cmp::Lt, value: 10.0 }
    }

    pub fn new_var() -> Self {
        Cond::Var { name: String::new(), op: Cmp::Ge, value: 10.0 }
    }

    pub fn needs_pick(&self) -> bool {
        match self {
            Cond::SeeImage { rec, .. } => rec.template.is_empty(),
            Cond::Number { region, .. } => region[2] <= region[0],
            Cond::Var { name, .. } => name.is_empty(),
        }
    }

    pub fn describe(&self) -> String {
        match self {
            _ if self.needs_pick() => "(chưa chọn trên màn hình)".into(),
            Cond::SeeImage { .. } => "thấy ảnh".into(),
            Cond::Number { op, value, .. } => format!("số trong vùng {} {value}", op.symbol()),
            Cond::Var { name, op, value } => format!("biến \"{name}\" {} {value}", op.symbol()),
        }
    }
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
    /// Set up by hand on the screenshot: `step` is fixed and `text` is only a note.
    #[serde(default)]
    pub manual: bool,
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
            manual: false,
        }
    }

    /// A step set up by hand (see `Step::needs_pick` for ones still missing their position).
    pub fn manual(step: Step) -> Self {
        Self { step: Some(step), manual: true, ..Self::new() }
    }

    pub fn new_loop() -> Self {
        Self { step: Some(Step::Loop { max: 50, steps: vec![Node::new()], until: None }), ..Self::new() }
    }

    pub fn new_if(cond: Cond) -> Self {
        Self { step: Some(Step::If { cond, then_steps: Vec::new(), else_steps: Vec::new() }), ..Self::new() }
    }

    /// Loops and ifs: rows that hold other rows.
    pub fn is_block(&self) -> bool {
        self.step.as_ref().is_some_and(Step::is_block)
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
            Step::Loop { max, until: None, .. } => format!("Lặp tối đa {max} lần"),
            Step::Loop { max, until: Some(c), .. } => format!("Lặp tối đa {max} lần, dừng khi {}", c.describe()),
            Step::If { cond, .. } => format!("Nếu {}", cond.describe()),
            Step::StopRun => "Dừng kịch bản (kết thúc lượt này)".into(),
            Step::SetVar { name, .. } if name.is_empty() => "Chọn biến".into(),
            Step::SetVar { name, change: VarOp::Set { value } } => format!("Đặt biến \"{name}\" = {value}"),
            Step::SetVar { name, change: VarOp::Add { value } } if *value < 0.0 => format!("Biến \"{name}\" trừ {}", -value),
            Step::SetVar { name, change: VarOp::Add { value } } => format!("Biến \"{name}\" cộng {value}"),
            Step::SetVar { name, change: VarOp::Read { region } } if region[2] <= region[0] => format!("Biến \"{name}\" = số đọc trong vùng (chưa chọn vùng)"),
            Step::SetVar { name, change: VarOp::Read { .. } } => format!("Biến \"{name}\" = số đọc trong vùng"),
            Step::Call { name, .. } => format!("Chạy kịch bản con \"{name}\""),
            _ if self.needs_pick() => "Chưa chọn vị trí trên màn hình".to_string(),
            Step::TapPoint { x, y } => format!("Bấm điểm ({:.0}%, {:.0}%)", x * 100.0, y * 100.0),
            Step::TapImage { all: false, .. } => "Bấm vào nút giống ảnh".to_string(),
            Step::TapImage { all: true, max, .. } => format!("Bấm hết các nút giống ảnh (tối đa {max})"),
            Step::WaitImage { timeout_sec, .. } => format!("Đợi đến khi thấy ảnh (tối đa {timeout_sec}s)"),
            Step::Swipe { from, to, .. } => format!(
                "Vuốt từ ({:.0}%, {:.0}%) đến ({:.0}%, {:.0}%)",
                from[0] * 100.0,
                from[1] * 100.0,
                to[0] * 100.0,
                to[1] * 100.0
            ),
        }
    }

    /// A hand-made step whose point / picture / swipe has not been chosen yet.
    pub fn needs_pick(&self) -> bool {
        match self {
            Step::TapPoint { x, .. } => *x < 0.0,
            Step::TapImage { rec, .. } | Step::WaitImage { rec, .. } => rec.template.is_empty(),
            Step::Swipe { from, to, .. } => from == to,
            Step::SetVar { name, change } => name.is_empty() || matches!(change, VarOp::Read { region } if region[2] <= region[0]),
            Step::Call { scenario_id, .. } => scenario_id.is_empty(),
            _ => false,
        }
    }

    /// The saved button picture of a hand-made step, if any.
    pub fn image(&self) -> Option<&Rec> {
        match self {
            Step::TapImage { rec, .. } | Step::WaitImage { rec, .. } if !rec.template.is_empty() => Some(rec),
            _ => None,
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
            Step::If { then_steps, else_steps, .. } => {
                clear_rec(then_steps);
                clear_rec(else_steps);
            }
            _ => {}
        }
    }

    /// Nested step lists (loop body, if/else branches), in path order.
    pub fn children_mut(&mut self) -> Vec<&mut Vec<Node>> {
        match self {
            Step::Loop { steps, .. } => vec![steps],
            Step::If { then_steps, else_steps, .. } => vec![then_steps, else_steps],
            _ => Vec::new(),
        }
    }

    pub fn children(&self) -> Vec<&Vec<Node>> {
        match self {
            Step::Loop { steps, .. } => vec![steps],
            Step::If { then_steps, else_steps, .. } => vec![then_steps, else_steps],
            _ => Vec::new(),
        }
    }

    /// Loops and ifs: rows that hold other rows.
    pub fn is_block(&self) -> bool {
        matches!(self, Step::Loop { .. } | Step::If { .. })
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

/// Calls `f` on every non-block node (and then on its "nếu không thấy" node), depth first.
pub fn for_each_leaf_mut(nodes: &mut [Node], f: &mut dyn FnMut(&mut Node)) {
    for n in nodes {
        match &mut n.step {
            Some(s) if s.is_block() => {
                for list in s.children_mut() {
                    for_each_leaf_mut(list, f);
                }
            }
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
            Some(s) if s.is_block() => s.children().into_iter().map(|c| dynamic_count(c)).sum(),
            _ => 0,
        })
        .sum()
}

/// Number of steps (loops and ifs count only what is inside them).
pub fn leaf_count(nodes: &[Node]) -> usize {
    nodes
        .iter()
        .map(|n| match &n.step {
            Some(s) if s.is_block() => s.children().into_iter().map(|c| leaf_count(c)).sum(),
            _ => 1,
        })
        .sum()
}

/// What to do when a popup rule's picture shows up.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PopupAction {
    /// Tap the matched picture itself (e.g. an "X" or "Đóng" button).
    TapImage,
    /// Tap a fixed point (fractions of the screen).
    TapPoint { x: f32, y: f32 },
    /// Android Back.
    Back,
}

/// "Thấy ảnh này ở đâu thì …" – checked before every step.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct PopupRule {
    pub rec: Rec,
    pub action: PopupAction,
    #[serde(default)]
    pub note: String,
}

/// How to get back on track when a run fails.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Recovery {
    /// Picture of the screen every run starts from (e.g. the game's main screen).
    #[serde(default)]
    pub home: Option<Rec>,
    /// Android package of the game, to reopen it when it crashed or was closed.
    #[serde(default)]
    pub package: String,
    /// How many times to press "back"/close before reopening the game.
    #[serde(default = "d_max_back")]
    pub max_back: u32,
    /// Runs retried after a successful recovery.
    #[serde(default = "d_retries")]
    pub retries: u32,
}

fn d_max_back() -> u32 {
    8
}
fn d_retries() -> u32 {
    2
}

impl Default for Recovery {
    fn default() -> Self {
        Self { home: None, package: String::new(), max_back: d_max_back(), retries: d_retries() }
    }
}

/// When a repeating scenario may run, and when it should stop.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Limits {
    /// "HH:MM" – only run between these times (both empty = any time). May wrap midnight.
    #[serde(default)]
    pub from: String,
    #[serde(default)]
    pub to: String,
    /// Stop after this many minutes (0 = no limit).
    #[serde(default)]
    pub max_minutes: u32,
    /// Stop after this many runs (0 = no limit).
    #[serde(default)]
    pub max_runs: u32,
}

/// One run, for the history list.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct HistoryEntry {
    pub started: String,
    pub ended: String,
    pub instance: String,
    pub ok: bool,
    pub message: String,
    /// Screenshot saved when the run failed (file in the history folder).
    #[serde(default)]
    pub screenshot: Option<String>,
}

#[derive(Serialize, Deserialize, Default)]
struct SavedVars {
    date: String,
    values: std::collections::HashMap<String, f64>,
}

const HISTORY_KEEP: usize = 100;
const HISTORY_SHOTS_KEEP: usize = 30;

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
    /// Emulator used by older versions (one per scenario); see `instances`.
    #[serde(default)]
    pub instance_index: u32,
    /// Emulators this scenario runs on, all at the same time.
    #[serde(default)]
    pub instances: Vec<u32>,
    #[serde(default)]
    pub popups: Vec<PopupRule>,
    #[serde(default)]
    pub recovery: Recovery,
    #[serde(default)]
    pub limits: Limits,
    /// Close the emulator when the scenario has nothing to do for a while
    /// (after a "Chạy 1 lần", after each daily run, or when the limits are reached).
    #[serde(default)]
    pub close_emulator_after: bool,
    #[serde(default)]
    pub variables: Vec<VarDef>,
    /// Start this scenario as soon as the app opens (e.g. after Windows starts).
    #[serde(default)]
    pub auto_start: bool,
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
            instances: Vec::new(),
            popups: Vec::new(),
            recovery: Recovery::default(),
            limits: Limits::default(),
            close_emulator_after: false,
            variables: Vec::new(),
            auto_start: false,
            schedule: Schedule::Repeat { pause_minutes: 0 },
            steps: vec![Node::new()],
            recorded: false,
            screen_size: None,
        }
    }

    pub fn dir(&self) -> PathBuf {
        paths::scenarios_dir().join(&self.id)
    }

    /// Every picture file the scenario uses (steps, popup rules, recovery).
    pub fn templates(&self) -> Vec<String> {
        let mut names = Vec::new();
        collect_templates(&self.steps, &mut names);
        names.extend(self.popups.iter().map(|p| p.rec.template.clone()));
        names.extend(self.recovery.home.iter().map(|r| r.template.clone()));
        names.retain(|n| !n.is_empty());
        names
    }

    /// The emulators to run on (older files only have `instance_index`).
    pub fn targets(&self) -> Vec<u32> {
        if self.instances.is_empty() { vec![self.instance_index] } else { self.instances.clone() }
    }

    /// Variables kept between runs ("về 0 mỗi ngày" / "giữ mãi"), per emulator.
    pub fn load_vars(&self, instance: u32) -> std::collections::HashMap<String, f64> {
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        let saved: SavedVars = fs::read_to_string(self.vars_file(instance))
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        self.variables
            .iter()
            .filter_map(|v| {
                let keep = match v.scope {
                    VarScope::Run => false,
                    VarScope::Day => saved.date == today,
                    VarScope::Forever => true,
                };
                keep.then(|| saved.values.get(&v.name).map(|n| (v.name.clone(), *n))).flatten()
            })
            .collect()
    }

    pub fn save_vars(&self, instance: u32, values: &std::collections::HashMap<String, f64>) {
        let keep: std::collections::HashMap<String, f64> = self
            .variables
            .iter()
            .filter(|v| v.scope != VarScope::Run)
            .filter_map(|v| values.get(&v.name).map(|n| (v.name.clone(), *n)))
            .collect();
        let saved = SavedVars { date: chrono::Local::now().format("%Y-%m-%d").to_string(), values: keep };
        if let Ok(json) = serde_json::to_string_pretty(&saved) {
            let _ = fs::create_dir_all(self.dir());
            let _ = fs::write(self.vars_file(instance), json);
        }
    }

    fn vars_file(&self, instance: u32) -> PathBuf {
        self.dir().join(format!("vars_{instance}.json"))
    }

    pub fn history_dir(&self) -> PathBuf {
        self.dir().join("history")
    }

    /// Adds a run to the history (newest last), keeping the last 100 runs and 30 screenshots.
    pub fn add_history(&self, entry: HistoryEntry) -> Result<()> {
        let dir = self.history_dir();
        fs::create_dir_all(&dir)?;
        let mut list = self.history();
        list.push(entry);
        let drop = list.len().saturating_sub(HISTORY_KEEP);
        list.drain(..drop);
        // Old failure screenshots go first.
        let shots: Vec<usize> = list.iter().enumerate().filter(|(_, e)| e.screenshot.is_some()).map(|(i, _)| i).collect();
        for &i in shots.iter().take(shots.len().saturating_sub(HISTORY_SHOTS_KEEP)) {
            if let Some(f) = list[i].screenshot.take() {
                let _ = fs::remove_file(dir.join(f));
            }
        }
        fs::write(dir.join("history.json"), serde_json::to_string_pretty(&list)?)?;
        Ok(())
    }

    pub fn history(&self) -> Vec<HistoryEntry> {
        fs::read_to_string(self.history_dir().join("history.json"))
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
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
        for name in self.templates() {
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

        let names = sc.templates();
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
            Some(s) if s.is_block() => {
                let cond = match s {
                    Step::If { cond, .. } => Some(cond),
                    Step::Loop { until, .. } => until.as_ref(),
                    _ => None,
                };
                if let Some(Cond::SeeImage { rec, .. }) = cond {
                    out.push(rec.template.clone());
                }
                for list in s.children() {
                    collect_templates(list, out);
                }
            }
            Some(s) => {
                if let Some(r) = s.image() {
                    out.push(r.template.clone());
                }
            }
            None => {}
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
