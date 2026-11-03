use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use eframe::egui::{self, Color32, Margin, RichText, TextureHandle, vec2};
use egui_phosphor::regular as ic;
use image::RgbaImage;
use serde::{Deserialize, Serialize};

use crate::ai::AiServer;
use crate::device::{self, Instance};
use crate::executor::{self, Event, SharedAi};
use crate::jobs::{self, Job};
use crate::script::{self, Cond, Node, Scenario, Schedule, Step};
use crate::theme::{self, ACCENT, BORDER, CARD_HI, GREEN, MUTED, PANEL, RED, TEXT, YELLOW};
use crate::{paths, setup};

#[derive(Serialize, Deserialize)]
struct Settings {
    ld_dir: Option<PathBuf>,
    /// Closing the window hides it in the tray (runs go on).
    #[serde(default = "yes")]
    close_to_tray: bool,
    /// Ctrl+F12 stops everything, from any program.
    #[serde(default = "yes")]
    hotkey: bool,
    /// Emulators running a scenario at once; 0 = automatic (about 4GB of RAM each).
    #[serde(default)]
    max_parallel: usize,
}

fn yes() -> bool {
    true
}

impl Default for Settings {
    fn default() -> Self {
        Self { ld_dir: None, close_to_tray: true, hotkey: true, max_parallel: 0 }
    }
}

/// How many emulators may run at once: the user's choice, or ~4GB of RAM each.
fn effective_parallel(s: &Settings) -> usize {
    if s.max_parallel > 0 { s.max_parallel } else { ((device::total_ram_gb() / 4) as usize).max(1) }
}

impl Settings {
    fn load() -> Self {
        std::fs::read_to_string(paths::settings_file())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }
    fn save(&self) {
        let _ = std::fs::create_dir_all(paths::data_dir());
        let _ = std::fs::write(
            paths::settings_file(),
            serde_json::to_string_pretty(self).unwrap_or_default(),
        );
    }
}

/// What the user is choosing on the screenshot.
#[derive(Clone, Copy, PartialEq, Debug)]
enum PickKind {
    /// Search area for a step (drag a box).
    Region,
    /// A point to tap (click).
    Point,
    /// A button picture (drag a box around it).
    Image,
    /// A swipe (drag from start to end).
    Swipe,
}

impl PickKind {
    fn instruction(self) -> &'static str {
        match self {
            PickKind::Region => "Kéo chuột khoanh vùng AI được tìm cho bước này",
            PickKind::Point => "Bấm vào điểm trên ảnh mà app sẽ bấm",
            PickKind::Image => "Kéo chuột khoanh quanh nút (app nhớ hình nút này và tìm nó mỗi lần chạy)",
            PickKind::Swipe => "Kéo chuột từ điểm bắt đầu đến điểm kết thúc của thao tác vuốt",
        }
    }
}

/// A finished pick, in fractions of the screen size.
enum Picked {
    Rect([f32; 4]),
    Point([f32; 2]),
    Line([f32; 2], [f32; 2]),
}

/// Shows the latest screenshot with a marker where the last tap happened,
/// and lets the user pick points, boxes and swipes on it.
#[derive(Default)]
struct ScreenView {
    tex: Option<TextureHandle>,
    /// The screenshot itself, to cut button pictures from.
    img: Option<RgbaImage>,
    size: [u32; 2],
    tap: Option<(u32, u32)>,
    shown_gen: u64,
    /// Start and current pointer position while dragging.
    drag: Option<(egui::Pos2, egui::Pos2)>,
    /// Boxes drawn on top (fractions of the screen), e.g. "thử khớp" results.
    marks: Vec<([f32; 4], Color32, String)>,
}

impl ScreenView {
    fn set(&mut self, ctx: &egui::Context, img: &RgbaImage, tap: Option<(u32, u32)>) {
        let color = egui::ColorImage::from_rgba_unmultiplied([img.width() as usize, img.height() as usize], img.as_raw());
        match &mut self.tex {
            Some(t) => t.set(color, egui::TextureOptions::LINEAR),
            None => self.tex = Some(ctx.load_texture("screen", color, egui::TextureOptions::LINEAR)),
        }
        self.img = Some(img.clone());
        self.size = [img.width(), img.height()];
        self.tap = tap;
        self.marks.clear();
    }

    /// Draws the screen with the last tap and `region` on top. With `pick`, the user
    /// clicks or drags on it; the result is returned when done.
    fn ui(&mut self, ui: &mut egui::Ui, region: Option<[f32; 4]>, pick: Option<PickKind>) -> Option<Picked> {
        let w = ui.available_width();
        let Some(tex) = &self.tex else {
            let (rect, _) = ui.allocate_exact_size(vec2(w, w * 9.0 / 16.0), egui::Sense::hover());
            ui.painter().rect_filled(rect, 10, theme::INPUT);
            let text = if pick.is_some() { "Đang chụp màn hình giả lập…" } else { "Chưa có ảnh màn hình" };
            ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, format!("{}  {text}", ic::DEVICE_MOBILE_SPEAKER), egui::FontId::proportional(15.0), MUTED);
            return None;
        };
        let size = vec2(w, w * self.size[1] as f32 / self.size[0] as f32);
        let sense = if pick.is_some() { egui::Sense::click_and_drag() } else { egui::Sense::hover() };
        let resp = ui.add(egui::Image::new((tex.id(), size)).corner_radius(10).sense(sense));
        let rect = resp.rect;
        let to_screen = |f: [f32; 4]| egui::Rect::from_two_pos(rect.lerp_inside(vec2(f[0], f[1])), rect.lerp_inside(vec2(f[2], f[3])));
        let to_frac = |p: egui::Pos2| {
            let v = (p - rect.min) / rect.size();
            [v.x.clamp(0.0, 1.0), v.y.clamp(0.0, 1.0)]
        };
        let painter = ui.painter().clone();

        if let Some(r) = region {
            let r = to_screen(r);
            painter.rect_filled(r, 4, ACCENT.gamma_multiply(0.12));
            painter.rect_stroke(r, 4, egui::Stroke::new(2.0_f32, ACCENT), egui::StrokeKind::Inside);
            painter.text(r.left_top() + vec2(6.0, 4.0), egui::Align2::LEFT_TOP, "Vùng tìm", egui::FontId::new(13.0, theme::bold()), ACCENT);
        }
        if let Some((x, y)) = self.tap {
            let p = rect.min + vec2(x as f32 / self.size[0] as f32 * size.x, y as f32 / self.size[1] as f32 * size.y);
            painter.circle_stroke(p, 13.0, egui::Stroke::new(3.0_f32, RED));
            painter.circle_filled(p, 3.5, RED);
        }
        for (f, color, label) in &self.marks {
            let r = to_screen(*f);
            painter.rect_stroke(r, 3, egui::Stroke::new(2.0_f32, *color), egui::StrokeKind::Outside);
            painter.text(r.left_bottom() + vec2(0.0, 2.0), egui::Align2::LEFT_TOP, label, egui::FontId::new(12.0, theme::bold()), *color);
        }
        let Some(kind) = pick else {
            self.drag = None;
            return None;
        };

        let resp = resp.on_hover_cursor(egui::CursorIcon::Crosshair);
        if kind == PickKind::Point {
            if let Some(p) = resp.hover_pos() {
                painter.circle_stroke(p, 12.0, egui::Stroke::new(2.0_f32, YELLOW));
            }
            if resp.clicked() {
                return resp.interact_pointer_pos().map(|p| Picked::Point(to_frac(p)));
            }
            return None;
        }

        if resp.drag_started() {
            if let Some(p) = resp.interact_pointer_pos() {
                self.drag = Some((p, p));
            }
        }
        if let (Some((start, _)), Some(p)) = (self.drag, resp.interact_pointer_pos()) {
            self.drag = Some((start, p));
        }
        if let Some((a, b)) = self.drag {
            let stroke = egui::Stroke::new(2.5_f32, YELLOW);
            if kind == PickKind::Swipe {
                painter.circle_filled(a, 5.0, YELLOW);
                painter.arrow(a, b - a, stroke);
            } else {
                let r = egui::Rect::from_two_pos(a, b);
                painter.rect_filled(r, 4, YELLOW.gamma_multiply(0.15));
                painter.rect_stroke(r, 4, stroke, egui::StrokeKind::Inside);
            }
        }
        if resp.drag_stopped() {
            if let Some((a, b)) = self.drag.take() {
                let (fa, fb) = (to_frac(a), to_frac(b));
                // Ignore clicks / tiny drags.
                let (dx, dy) = ((fb[0] - fa[0]).abs(), (fb[1] - fa[1]).abs());
                return match kind {
                    PickKind::Swipe if dx.max(dy) > 0.03 => Some(Picked::Line(fa, fb)),
                    PickKind::Region | PickKind::Image if dx > 0.01 && dy > 0.01 => {
                        Some(Picked::Rect([fa[0].min(fb[0]), fa[1].min(fb[1]), fa[0].max(fb[0]), fa[1].max(fb[1])]))
                    }
                    _ => None,
                };
            }
        }
        None
    }
}

/// Small textures of saved button pictures, shown next to hand-made steps.
#[derive(Default)]
struct ThumbCache {
    map: std::collections::HashMap<PathBuf, Option<TextureHandle>>,
}

impl ThumbCache {
    fn get(&mut self, ctx: &egui::Context, path: PathBuf) -> Option<TextureHandle> {
        self.map
            .entry(path.clone())
            .or_insert_with(|| {
                let img = image::open(&path).ok()?.to_rgba8();
                let color = egui::ColorImage::from_rgba_unmultiplied([img.width() as usize, img.height() as usize], img.as_raw());
                Some(ctx.load_texture(path.to_string_lossy(), color, egui::TextureOptions::LINEAR))
            })
            .clone()
    }
}

enum TaskMsg {
    Event(Event),
    Compiled(Result<Vec<Node>, String>),
    /// Learned steps and the screen resolution they were recorded at.
    TrialDone(Result<(Vec<Node>, Option<[u32; 2]>), String>),
    /// "Thử bước này" finished.
    StepDone(Result<(), String>),
}

/// The step (and which part of it) whose point / picture / area is being picked.
#[derive(Clone, PartialEq, Debug)]
struct PickTarget {
    /// Step path (see `node_at`); unused for popup rules and the start screen.
    path: Vec<usize>,
    slot: Slot,
    kind: PickKind,
}

/// Which part of a step (or of the scenario) a pick is for.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Slot {
    /// The step itself (its point / picture / swipe / search area).
    Step,
    /// The step's "nếu không thấy thì" action.
    Otherwise,
    /// The condition of an "if" block or a loop's "dừng khi".
    Cond,
    /// The picture of popup rule #i.
    PopupImage(usize),
    /// The point tapped by popup rule #i.
    PopupPoint(usize),
    /// The start screen used by recovery.
    Home,
    /// The area read by an "Đặt biến = số trong vùng" step.
    VarRegion,
}

/// One saved picture checked against the current screenshot.
struct CheckResult {
    label: String,
    template: String,
    threshold: f32,
    /// Best similarity found and where (fractions of the screen).
    score: f32,
    at: Option<[f32; 4]>,
}

/// Every saved picture of a scenario, named like the editor shows it ("Bước 3", "Popup 1"…).
fn template_refs(sc: &Scenario) -> Vec<(String, script::Rec)> {
    fn walk(nodes: &[Node], n: &mut usize, out: &mut Vec<(String, script::Rec)>) {
        for node in nodes {
            match &node.step {
                Some(s) if s.is_block() => {
                    let cond = match s {
                        Step::If { cond, .. } => Some(("khối Nếu", cond)),
                        Step::Loop { until: Some(c), .. } => Some(("dừng vòng lặp", c)),
                        _ => None,
                    };
                    if let Some((what, Cond::SeeImage { rec, .. })) = cond {
                        if !rec.template.is_empty() {
                            out.push((format!("Điều kiện {what} (sau bước {n})"), rec.clone()));
                        }
                    }
                    for list in s.children() {
                        walk(list, n, out);
                    }
                }
                step => {
                    *n += 1;
                    let rec = match step {
                        Some(s) => s.image().cloned().or(match s {
                            Step::Tap { rec, .. } | Step::TapAll { rec, .. } | Step::WaitUntil { rec, .. } => rec.clone(),
                            _ => None,
                        }),
                        None => None,
                    };
                    if let Some(r) = rec {
                        out.push((format!("Bước {n}"), r));
                    }
                    if let Some(r) = node.otherwise.as_ref().and_then(|o| o.step.as_ref()).and_then(|s| s.image()) {
                        out.push((format!("Bước {n} · nếu không thấy"), r.clone()));
                    }
                }
            }
        }
    }
    let mut out = Vec::new();
    walk(&sc.steps, &mut 0, &mut out);
    for (i, p) in sc.popups.iter().enumerate() {
        if !p.rec.template.is_empty() {
            let name = if p.note.trim().is_empty() { String::new() } else { format!(" ({})", p.note.trim()) };
            out.push((format!("Popup {}{name}", i + 1), p.rec.clone()));
        }
    }
    if let Some(h) = &sc.recovery.home {
        out.push(("Màn hình gốc".into(), h.clone()));
    }
    out
}

struct Task {
    rx: Receiver<TaskMsg>,
    stop: Arc<AtomicBool>,
    label: &'static str,
    /// "Xem AI hiểu thế nào": ends with its Compiled message (a trial sends one midway).
    analyse_only: bool,
}

struct Editor {
    sc: Scenario,
    is_new: bool,
    times_text: String,
    pause_text: String,
    /// The AI stays loaded while the editor is open (analyse + trial + retries)
    /// and is killed when the editor closes.
    ai: SharedAi,
    task: Option<Task>,
    log: Vec<String>,
    screen: ScreenView,
    /// Some(..) after a trial run finished.
    trial: Option<Result<(), String>>,
    error: Option<String>,
    /// What is being picked on the screenshot, if anything.
    picking: Option<PickTarget>,
    /// A screenshot being taken (for picking).
    shot_rx: Option<Receiver<Result<RgbaImage, String>>>,
    /// "Dừng trước bước này" for runs started from the editor.
    breakpoint: Option<Vec<usize>>,
    /// "Kiểm tra ảnh mẫu" results, and the check running in the background.
    checks: Vec<CheckResult>,
    checks_rx: Option<Receiver<Vec<CheckResult>>>,
    /// Games installed on the emulator (for "mở lại game").
    packages: Vec<String>,
    packages_rx: Option<Receiver<Result<Vec<String>, String>>>,
    thumbs: ThumbCache,
}

impl Editor {
    fn new(sc: Scenario, is_new: bool) -> Self {
        let times_text = match &sc.schedule {
            Schedule::Daily { times } => times.join(", "),
            _ => "08:00, 20:00".into(),
        };
        let pause_text = match &sc.schedule {
            Schedule::Repeat { pause_minutes } => pause_minutes.to_string(),
            _ => "0".into(),
        };
        Self {
            sc,
            is_new,
            times_text,
            pause_text,
            ai: Arc::new(Mutex::new(None)),
            task: None,
            log: Vec::new(),
            screen: ScreenView::default(),
            trial: None,
            error: None,
            picking: None,
            shot_rx: None,
            breakpoint: None,
            checks: Vec::new(),
            checks_rx: None,
            packages: Vec::new(),
            packages_rx: None,
            thumbs: ThumbCache::default(),
        }
    }

    fn close(&mut self) {
        if let Some(t) = &self.task {
            t.stop.store(true, Ordering::Relaxed);
        }
        // Free the AI's RAM without blocking the UI on a busy lock.
        let ai = self.ai.clone();
        std::thread::spawn(move || *ai.lock().unwrap() = None);
    }

    fn start_analyse(&mut self) {
        self.error = None;
        let (tx, rx) = channel();
        let ai = self.ai.clone();
        let mut nodes = self.sc.steps.clone();
        std::thread::spawn(move || {
            let log_tx = tx.clone();
            let log = move |s: String| {
                let _ = log_tx.send(TaskMsg::Event(Event::Log(s)));
            };
            let r = compile_nodes(&ai, &mut nodes, &log)
                .map(|_| nodes)
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(TaskMsg::Compiled(r));
        });
        self.task = Some(Task {
            rx,
            stop: Arc::new(AtomicBool::new(false)),
            label: "AI đang đọc các bước...",
            analyse_only: true,
        });
    }

    fn start_trial(&mut self, ld: PathBuf, inst: Instance) {
        self.error = None;
        self.trial = None;
        let (tx, rx) = channel();
        let stop = Arc::new(AtomicBool::new(false));
        let (ai, dir, stop2) = (self.ai.clone(), self.sc.dir(), stop.clone());
        let popups = self.sc.popups.clone();
        let mut nodes = self.sc.steps.clone();
        let breakpoint = self.breakpoint.clone();
        std::thread::spawn(move || {
            let r = (|| -> anyhow::Result<(Vec<Node>, Option<[u32; 2]>)> {
                let log_tx = tx.clone();
                let log = move |s: String| {
                    let _ = log_tx.send(TaskMsg::Event(Event::Log(s)));
                };
                compile_nodes(&ai, &mut nodes, &log)?;
                let _ = tx.send(TaskMsg::Compiled(Ok(nodes.clone())));
                script::clear_rec(&mut nodes);
                let (etx, erx) = channel::<Event>();
                let fwd = tx.clone();
                std::thread::spawn(move || {
                    while let Ok(e) = erx.recv() {
                        let _ = fwd.send(TaskMsg::Event(e));
                    }
                });
                let mut runner = executor::new_runner(&ld, inst, dir.clone(), ai, true, stop2, etx);
                runner.popups = popups;
                runner.popup_dir = dir;
                runner.stop_before = breakpoint;
                runner.ensure_device()?;
                runner.run_all(&mut nodes)?;
                Ok((nodes, runner.screen_size))
            })();
            let _ = tx.send(TaskMsg::TrialDone(r.map_err(|e| format!("{e:#}"))));
        });
        self.task = Some(Task {
            rx,
            stop,
            label: "Đang chạy thử...",
            analyse_only: false,
        });
    }

    /// "Thử bước này": runs one step on the emulator right away. Nothing is saved.
    fn start_test_step(&mut self, ld: PathBuf, inst: Instance, node: Node) {
        self.error = None;
        let (tx, rx) = channel();
        let stop = Arc::new(AtomicBool::new(false));
        let (ai, dir, stop2) = (self.ai.clone(), self.sc.dir(), stop.clone());
        let popups = self.sc.popups.clone();
        std::thread::spawn(move || {
            let r = (|| -> anyhow::Result<()> {
                let log_tx = tx.clone();
                let log = move |s: String| {
                    let _ = log_tx.send(TaskMsg::Event(Event::Log(s)));
                };
                let mut nodes = vec![node];
                compile_nodes(&ai, &mut nodes, &log)?;
                let (etx, erx) = channel::<Event>();
                let fwd = tx.clone();
                std::thread::spawn(move || {
                    while let Ok(e) = erx.recv() {
                        let _ = fwd.send(TaskMsg::Event(e));
                    }
                });
                let mut runner = executor::new_runner(&ld, inst, dir.clone(), ai, true, stop2, etx);
                runner.popups = popups;
                runner.popup_dir = dir;
                runner.ensure_device()?;
                runner.run_all(&mut nodes)
            })();
            let _ = tx.send(TaskMsg::StepDone(r.map_err(|e| format!("{e:#}"))));
        });
        self.task = Some(Task { rx, stop, label: "Đang thử bước...", analyse_only: false });
    }

    /// Applies a finished pick to its step. Button pictures are cut from the screenshot
    /// and saved in the scenario folder.
    /// Cuts a part of the current screenshot and saves it in the scenario folder.
    fn save_crop(&self, r: [f32; 4]) -> Result<script::Rec, String> {
        let img = self.screen.img.as_ref().ok_or("Chưa có ảnh màn hình")?;
        let (w, h) = (img.width() as f32, img.height() as f32);
        let (x, y) = ((r[0] * w) as u32, (r[1] * h) as u32);
        let (cw, ch) = ((((r[2] - r[0]) * w) as u32).max(8), (((r[3] - r[1]) * h) as u32).max(8));
        let (cw, ch) = (cw.min(img.width() - x), ch.min(img.height() - y));
        let dir = self.sc.dir();
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let name = format!("tpl_{}.png", uuid::Uuid::new_v4().simple());
        image::imageops::crop_imm(img, x, y, cw, ch).to_image().save(dir.join(&name)).map_err(|e| e.to_string())?;
        Ok(script::Rec { template: name, x, y, threshold: None })
    }

    /// Applies a finished pick to what it was for.
    fn apply_pick(&mut self, target: &PickTarget, picked: Picked) -> Result<(), String> {
        let mismatch = || "Kiểu chọn không khớp".to_string();
        match (target.slot, picked) {
            (Slot::PopupImage(i), Picked::Rect(r)) => {
                let rec = self.save_crop(r)?;
                self.sc.popups.get_mut(i).ok_or("Quy tắc này không còn nữa")?.rec = rec;
            }
            (Slot::PopupPoint(i), Picked::Point([x, y])) => {
                self.sc.popups.get_mut(i).ok_or("Quy tắc này không còn nữa")?.action = script::PopupAction::TapPoint { x, y };
            }
            (Slot::Home, Picked::Rect(r)) => self.sc.recovery.home = Some(self.save_crop(r)?),
            (Slot::VarRegion, Picked::Rect(r)) => {
                let node = node_at(&mut self.sc.steps, &target.path).ok_or("Bước này không còn nữa")?;
                match &mut node.step {
                    Some(Step::SetVar { change: script::VarOp::Read { region }, .. }) => *region = r,
                    _ => return Err(mismatch()),
                }
            }
            (Slot::Cond, picked) => {
                let rec = match (&picked, target.kind) {
                    (Picked::Rect(r), PickKind::Image) => Some(self.save_crop(*r)?),
                    _ => None,
                };
                let node = node_at(&mut self.sc.steps, &target.path).ok_or("Bước này không còn nữa")?;
                let cond = cond_mut(node).ok_or_else(mismatch)?;
                match (cond, picked, target.kind) {
                    (Cond::SeeImage { rec: old, .. }, _, PickKind::Image) => *old = rec.ok_or_else(mismatch)?,
                    (Cond::SeeImage { region, .. }, Picked::Rect(r), PickKind::Region) => *region = Some(r),
                    (Cond::Number { region, .. }, Picked::Rect(r), PickKind::Region) => *region = r,
                    _ => return Err(mismatch()),
                }
            }
            (Slot::Step | Slot::Otherwise, picked) => {
                let rec = match (&picked, target.kind) {
                    (Picked::Rect(r), PickKind::Image) => Some(self.save_crop(*r)?),
                    _ => None,
                };
                let node = target_node(&mut self.sc.steps, target).ok_or("Bước này không còn nữa")?;
                match (target.kind, picked) {
                    (PickKind::Region, Picked::Rect(r)) => node.region = Some(r),
                    (PickKind::Point, Picked::Point([x, y])) => node.step = Some(Step::TapPoint { x, y }),
                    (PickKind::Swipe, Picked::Line(from, to)) => {
                        let ms = match node.step {
                            Some(Step::Swipe { ms, .. }) => ms,
                            _ => 400,
                        };
                        node.step = Some(Step::Swipe { from, to, ms });
                    }
                    (PickKind::Image, _) => {
                        let rec = rec.ok_or_else(mismatch)?;
                        match &mut node.step {
                            Some(Step::TapImage { rec: old, .. } | Step::WaitImage { rec: old, .. }) => *old = rec,
                            _ => node.step = Some(Step::TapImage { all: false, max: 30, rec }),
                        }
                    }
                    _ => return Err(mismatch()),
                }
            }
            _ => return Err(mismatch()),
        }
        Ok(())
    }

    /// Grabs the emulator's current screen in the background (for picking search areas).
    fn capture(&mut self, ld: PathBuf, inst: Instance) {
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let dev = device::Device::new(&ld, &inst);
            dev.connect();
            let _ = tx.send(
                dev.screenshot()
                    .map(|f| f.img)
                    .map_err(|e| format!("Không chụp được màn hình: {e:#}")),
            );
        });
        self.shot_rx = Some(rx);
    }

    /// "Thử khớp": finds one picture on the current screenshot and boxes it.
    fn test_match(&mut self, rec: &script::Rec) {
        let Some(screen) = self.screen.img.clone() else {
            self.error = Some("Chụp màn hình giả lập trước đã".into());
            return;
        };
        let Ok(tpl) = image::open(self.sc.dir().join(&rec.template)) else { return };
        let tpl = tpl.to_rgba8();
        let (w, h) = (screen.width() as f32, screen.height() as f32);
        match crate::vision::best(&screen, &tpl) {
            Some((x, y, score)) => {
                let ok = score >= rec.threshold();
                let f = [x as f32 / w, y as f32 / h, (x + tpl.width()) as f32 / w, (y + tpl.height()) as f32 / h];
                self.screen.marks = vec![(f, if ok { GREEN } else { RED }, format!("{:.0}%", score * 100.0))];
                jobs::push_log(&mut self.log, format!(
                    "Thử khớp: giống nhất {:.0}% (cần ≥ {:.0}%) → {}",
                    score * 100.0,
                    rec.threshold() * 100.0,
                    if ok { "khớp" } else { "KHÔNG khớp" }
                ));
            }
            None => jobs::push_log(&mut self.log, "Thử khớp: ảnh mẫu lớn hơn màn hình hoặc không có chi tiết".into()),
        }
    }

    /// "Kiểm tra ảnh mẫu": every saved picture against the current screenshot, in the background.
    fn check_templates(&mut self) {
        let Some(screen) = self.screen.img.clone() else {
            self.error = Some("Chụp màn hình giả lập trước đã".into());
            return;
        };
        let refs = template_refs(&self.sc);
        let dir = self.sc.dir();
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let (w, h) = (screen.width() as f32, screen.height() as f32);
            let results = refs
                .into_iter()
                .map(|(label, rec)| {
                    let found = image::open(dir.join(&rec.template)).ok().and_then(|t| {
                        let t = t.to_rgba8();
                        crate::vision::best(&screen, &t).map(|(x, y, sc)| (sc, [x as f32 / w, y as f32 / h, (x + t.width()) as f32 / w, (y + t.height()) as f32 / h]))
                    });
                    CheckResult {
                        label,
                        template: rec.template.clone(),
                        threshold: rec.threshold(),
                        score: found.map(|f| f.0).unwrap_or(0.0),
                        at: found.map(|f| f.1),
                    }
                })
                .collect();
            let _ = tx.send(results);
        });
        self.checks_rx = Some(rx);
    }

    /// "Chạy từ đây": runs the scenario from a step (up to the breakpoint, if any). Nothing is saved.
    fn start_run_from(&mut self, ld: PathBuf, inst: Instance, from: Vec<usize>) {
        self.error = None;
        let (tx, rx) = channel();
        let stop = Arc::new(AtomicBool::new(false));
        let (ai, dir, stop2) = (self.ai.clone(), self.sc.dir(), stop.clone());
        let popups = self.sc.popups.clone();
        let mut nodes = self.sc.steps.clone();
        let breakpoint = self.breakpoint.clone();
        std::thread::spawn(move || {
            let r = (|| -> anyhow::Result<()> {
                let log_tx = tx.clone();
                let log = move |s: String| {
                    let _ = log_tx.send(TaskMsg::Event(Event::Log(s)));
                };
                compile_nodes(&ai, &mut nodes, &log)?;
                let (etx, erx) = channel::<Event>();
                let fwd = tx.clone();
                std::thread::spawn(move || {
                    while let Ok(e) = erx.recv() {
                        let _ = fwd.send(TaskMsg::Event(e));
                    }
                });
                let mut runner = executor::new_runner(&ld, inst, dir.clone(), ai, true, stop2, etx);
                runner.popups = popups;
                runner.popup_dir = dir;
                runner.start_at = Some(from);
                runner.stop_before = breakpoint;
                runner.ensure_device()?;
                runner.run_all(&mut nodes)
            })();
            let _ = tx.send(TaskMsg::StepDone(r.map_err(|e| format!("{e:#}"))));
        });
        self.task = Some(Task { rx, stop, label: "Đang chạy từ bước đã chọn...", analyse_only: false });
    }

    /// Lists the games installed on the emulator, in the background.
    fn load_packages(&mut self, ld: PathBuf, inst: Instance) {
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let dev = device::Device::new(&ld, &inst);
            dev.connect();
            let current = dev.foreground_package();
            let r = dev.packages().map(|mut list| {
                // The game on screen right now goes first: it is most likely the one.
                if let Some(c) = current.filter(|c| list.contains(c)) {
                    list.retain(|p| *p != c);
                    list.insert(0, c);
                }
                list
            });
            let _ = tx.send(r.map_err(|e| format!("Không lấy được danh sách game: {e:#}")));
        });
        self.packages_rx = Some(rx);
    }

    fn poll(&mut self, ctx: &egui::Context) {
        if let Some(rx) = &self.checks_rx {
            if let Ok(results) = rx.try_recv() {
                self.screen.marks = results
                    .iter()
                    .filter_map(|c| {
                        let color = if c.score >= c.threshold { GREEN } else { RED };
                        c.at.map(|f| (f, color, format!("{} {:.0}%", c.label, c.score * 100.0)))
                    })
                    .collect();
                self.checks = results;
                self.checks_rx = None;
            }
        }
        if let Some(rx) = &self.packages_rx {
            if let Ok(r) = rx.try_recv() {
                match r {
                    Ok(list) => self.packages = list,
                    Err(e) => self.error = Some(e),
                }
                self.packages_rx = None;
            }
        }
        if let Some(rx) = &self.shot_rx {
            if let Ok(r) = rx.try_recv() {
                match r {
                    Ok(img) => self.screen.set(ctx, &img, None),
                    Err(e) => self.error = Some(e),
                }
                self.shot_rx = None;
            }
        }
        let Some(task) = &self.task else { return };
        let analyse_only = task.analyse_only;
        let mut finished = false;
        while let Ok(msg) = task.rx.try_recv() {
            match msg {
                TaskMsg::Event(Event::Log(l)) => jobs::push_log(&mut self.log, l),
                TaskMsg::Event(Event::Screen { img, tap }) => self.screen.set(ctx, &img, tap),
                TaskMsg::Compiled(Ok(nodes)) => {
                    self.sc.steps = nodes;
                    finished |= analyse_only;
                }
                TaskMsg::Compiled(Err(e)) => {
                    finished = true;
                    self.error = Some(e);
                }
                TaskMsg::StepDone(r) => {
                    finished = true;
                    match r {
                        Ok(()) => jobs::push_log(&mut self.log, "Thử bước xong.".into()),
                        Err(e) => jobs::push_log(&mut self.log, format!("Thử bước lỗi: {e}")),
                    }
                }
                TaskMsg::TrialDone(r) => {
                    finished = true;
                    match r {
                        Ok((nodes, size)) => {
                            jobs::push_log(
                                &mut self.log,
                                "Chạy thử xong. Kiểm tra lại game xem đã đúng chưa.".into(),
                            );
                            self.sc.steps = nodes;
                            self.sc.screen_size = size;
                            self.trial = Some(Ok(()));
                        }
                        Err(e) => {
                            jobs::push_log(&mut self.log, format!("Chạy thử lỗi: {e}"));
                            self.trial = Some(Err(e));
                        }
                    }
                }
            }
        }
        if finished {
            self.task = None;
        }
    }
}

fn ensure_ai(ai: &SharedAi, log: &dyn Fn(String)) -> anyhow::Result<()> {
    let mut g = ai.lock().unwrap();
    if g.is_none() {
        log("Đang bật AI (lần đầu có thể mất 10-60 giây)...".into());
        *g = Some(AiServer::start()?);
        log("AI đã sẵn sàng.".into());
    }
    Ok(())
}

/// Removes empty rows and empty loops (hand-made steps are never empty).
fn prune_empty(nodes: &mut Vec<Node>) {
    nodes.retain_mut(|n| {
        if n.otherwise.as_ref().is_some_and(|o| !o.manual && o.text.trim().is_empty()) {
            n.otherwise = None;
        }
        match &mut n.step {
            Some(Step::Loop { steps, .. }) => {
                prune_empty(steps);
                !steps.is_empty()
            }
            _ => n.manual || !n.text.trim().is_empty(),
        }
    });
}

/// Asks the AI to interpret every step that has not been interpreted yet.
fn compile_nodes(ai: &SharedAi, nodes: &mut Vec<Node>, log: &dyn Fn(String)) -> anyhow::Result<()> {
    prune_empty(nodes);
    if nodes.is_empty() {
        anyhow::bail!("Chưa có bước nào. Bấm \"Thêm bước\" để nhập.");
    }
    let mut pending = 0;
    script::for_each_leaf_mut(nodes, &mut |n| pending += n.step.is_none() as usize);
    if pending == 0 {
        return Ok(());
    }
    ensure_ai(ai, log)?;
    let guard = ai.lock().unwrap();
    let server = guard.as_ref().unwrap();
    let mut err = None;
    script::for_each_leaf_mut(nodes, &mut |n| {
        if n.step.is_some() || err.is_some() {
            return;
        }
        match server.compile_node(n) {
            Ok(()) => {
                log(format!(
                    "AI hiểu \"{}\" → {}",
                    n.text,
                    n.step.as_ref().map(Step::describe).unwrap_or_default()
                ));
                if let Some(o) = &n.otherwise {
                    log(format!("  nếu không thấy thì: \"{}\"", o.text));
                }
            }
            Err(e) => err = Some(e),
        }
    });
    match err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

enum Page {
    Setup,
    Main,
}

#[derive(Clone, PartialEq)]
enum DetailTab {
    /// Live log/screen of the job on this emulator (None = the first one).
    Live(Option<u32>),
    History,
}

pub struct App {
    page: Page,
    setup: setup::SharedProgress,
    settings: Settings,
    ld_text: String,
    instances: Vec<Instance>,
    inst_error: Option<String>,
    scenarios: Vec<Scenario>,
    jobs: Vec<Job>,
    selected: Option<String>,
    job_screen: ScreenView,
    /// Detail panel: an emulator's live run, or the history list.
    detail_tab: DetailTab,
    /// History loaded for the selected scenario (refreshed when a run ends).
    history: Vec<script::HistoryEntry>,
    history_for: Option<String>,
    editor: Option<Editor>,
    confirm_delete: Option<String>,
    message: Option<(String, Color32)>,
    _desktop: crate::desktop::Desktop,
    show_settings: bool,
    show_emulators: bool,
    /// Whether "Khởi động cùng Windows" is on (read from the registry).
    autostart: bool,
    /// Set once the close-to-tray hint has been shown.
    hinted_tray: bool,
}

enum RowAction {
    Start(String),
    Stop(String),
    Edit(String),
    Delete(String),
    Select(String),
    Save(String),
    Export(String),
}

/// Warning shown when a scenario's templates were recorded at another resolution.
fn size_warning(sc: &Scenario, inst: &Instance) -> Option<String> {
    let rec = sc.screen_size?;
    (inst.size != [0, 0] && rec != inst.size).then(|| {
        format!(
            "Kịch bản được tạo ở {}x{} nhưng \"{}\" là {}x{}. Nên chỉnh giả lập cùng độ phân giải; \
             nếu không, lần chạy đầu AI sẽ phải tìm lại các nút (chậm hơn).",
            rec[0], rec[1], inst.name, inst.size[0], inst.size[1]
        )
    })
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        theme::apply(&cc.egui_ctx);
        crate::ai::kill_stale();
        let setup = Arc::new(Mutex::new(setup::Progress::default()));
        let page = if setup::is_ready() {
            Page::Main
        } else {
            setup::start(setup.clone());
            Page::Setup
        };
        let mut settings = Settings::load();
        if settings
            .ld_dir
            .as_ref()
            .is_none_or(|d| !device::is_ldplayer_dir(d))
        {
            settings.ld_dir = device::find_ldplayer();
            settings.save();
        }
        let ld_text = settings
            .ld_dir
            .as_ref()
            .map(|d| d.display().to_string())
            .unwrap_or_default();
        let hotkey = settings.hotkey;
        let mut app = Self {
            page,
            setup,
            settings,
            ld_text,
            instances: Vec::new(),
            inst_error: None,
            scenarios: script::load_all(),
            jobs: Vec::new(),
            selected: None,
            job_screen: ScreenView::default(),
            detail_tab: DetailTab::Live(None),
            history: Vec::new(),
            history_for: None,
            editor: None,
            confirm_delete: None,
            message: None,
            _desktop: crate::desktop::setup(&cc.egui_ctx, hotkey),
            show_settings: false,
            show_emulators: false,
            autostart: crate::desktop::autostart_enabled(),
            hinted_tray: false,
        };
        jobs::gate::set_max_parallel(effective_parallel(&app.settings));
        app.refresh_instances();
        // Scenarios marked "tự chạy khi mở app" (e.g. after Windows starts).
        if matches!(app.page, Page::Main) {
            let ids: Vec<String> = app.scenarios.iter().filter(|s| s.auto_start).map(|s| s.id.clone()).collect();
            for id in ids {
                app.apply(RowAction::Start(id));
            }
        }
        // `--select <id> [history]`: opens the detail panel of a scenario (for screenshots).
        let args: Vec<String> = std::env::args().collect();
        if let Some(i) = args.iter().position(|a| a == "--select") {
            app.selected = args.get(i + 1).cloned();
            if args.get(i + 2).map(String::as_str) == Some("history") {
                app.detail_tab = DetailTab::History;
            }
        }
        if std::env::args().any(|a| a == "--demo") {
            let mut ed = Editor::new(demo_scenario(&app.instances), true);
            // Show the search-area picker on the live screen.
            if let (Some(ld), Some(inst)) = (
                &app.settings.ld_dir,
                app.instances.iter().find(|i| i.running),
            ) {
                ed.capture(ld.clone(), inst.clone());
                ed.picking = Some(PickTarget { path: vec![1, 0, 0], slot: Slot::Step, kind: PickKind::Image });
            }
            app.editor = Some(ed);
        }
        app
    }

    fn info(&mut self, text: impl Into<String>) {
        self.message = Some((text.into(), GREEN));
    }

    fn warn(&mut self, text: impl Into<String>) {
        self.message = Some((text.into(), YELLOW));
    }

    fn refresh_instances(&mut self) {
        self.inst_error = None;
        match &self.settings.ld_dir {
            Some(dir) => match device::list_instances(dir) {
                Ok(list) => self.instances = list,
                Err(e) => self.inst_error = Some(format!("{e:#}")),
            },
            None => {
                self.inst_error = Some(
                    "Không tìm thấy LDPlayer. Nhập thư mục cài LDPlayer (chứa ldconsole.exe)."
                        .into(),
                )
            }
        }
    }

    fn setup_ui(&mut self, ctx: &egui::Context) {
        let p = self.setup.lock().unwrap().clone();
        if p.done {
            self.page = Page::Main;
            return;
        }
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.add_space((ui.available_height() / 2.0 - 140.0).max(20.0));
            ui.vertical_centered(|ui| {
                ui.set_max_width(520.0);
                theme::card(ui, |ui| {
                    ui.vertical_centered(|ui| {
                        ui.label(RichText::new(ic::ROBOT).size(42.0).color(ACCENT));
                        ui.heading("Chuẩn bị lần đầu");
                        ui.label(theme::muted(
                            "Đang tải AI chạy trên máy (~3,3GB). Chỉ tải một lần duy nhất.",
                        ));
                        ui.add_space(14.0);
                        if let Some(err) = &p.error {
                            ui.colored_label(RED, format!("Lỗi: {err}"));
                            ui.add_space(6.0);
                            if ui
                                .add(theme::button(
                                    format!("{} Thử lại", ic::ARROWS_CLOCKWISE),
                                    ACCENT,
                                ))
                                .clicked()
                            {
                                *self.setup.lock().unwrap() = setup::Progress::default();
                                setup::start(self.setup.clone());
                            }
                            return;
                        }
                        ui.label(format!(
                            "Bước {}/{} · {}",
                            p.step,
                            p.total_steps.max(1),
                            p.label
                        ));
                        let frac = if p.size > 0 {
                            p.downloaded as f32 / p.size as f32
                        } else {
                            0.0
                        };
                        ui.add(
                            egui::ProgressBar::new(frac)
                                .desired_width(440.0)
                                .fill(ACCENT)
                                .corner_radius(6)
                                .text(format!(
                                    "{:.0} / {:.0} MB",
                                    p.downloaded as f64 / 1e6,
                                    p.size as f64 / 1e6
                                )),
                        );
                    });
                });
            });
        });
    }

    fn header(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("top")
            .frame(
                egui::Frame::new()
                    .fill(PANEL)
                    .inner_margin(Margin::symmetric(20, 14)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(ic::GAME_CONTROLLER).size(26.0).color(ACCENT));
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 0.0;
                        ui.label(RichText::new("OmniPilot").family(theme::bold()).size(20.0));
                        ui.label(
                            RichText::new("Auto mọi game trên LDPlayer")
                                .size(12.5)
                                .color(MUTED),
                        );
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .add(theme::button(format!("{} Tạo kịch bản", ic::PLUS), ACCENT))
                            .clicked()
                        {
                            let mut sc = Scenario::new();
                            if let Some(i) = self
                                .instances
                                .iter()
                                .find(|i| i.running)
                                .or(self.instances.first())
                            {
                                sc.instance_index = i.index;
                                sc.instances = vec![i.index];
                            }
                            self.editor = Some(Editor::new(sc, true));
                        }
                        if ui.button(ic::GEAR).on_hover_text("Cài đặt").clicked() {
                            self.show_settings = !self.show_settings;
                        }
                        if ui.button(format!("{} Giả lập", ic::DEVICE_MOBILE)).on_hover_text("Mở/tắt, sắp xếp cửa sổ giả lập").clicked() {
                            self.show_emulators = !self.show_emulators;
                            self.refresh_instances();
                        }
                        if self.jobs.iter().any(|j| !j.is_finished())
                            && ui.add(theme::button(format!("{} Dừng tất cả", ic::STOP), RED)).on_hover_text(format!("Phím tắt: {}", crate::desktop::HOTKEY_LABEL)).clicked()
                        {
                            jobs::stop_all();
                        }
                        if ui
                            .button(format!("{} Nhập", ic::DOWNLOAD_SIMPLE))
                            .on_hover_text("Mở file .omnipilot người khác chia sẻ")
                            .clicked()
                        {
                            self.import_dialog();
                        }
                        if ui
                            .button(ic::ARROWS_CLOCKWISE)
                            .on_hover_text("Làm mới danh sách giả lập")
                            .clicked()
                        {
                            self.refresh_instances();
                        }
                    });
                });
            });
    }

    fn banners(&mut self, ui: &mut egui::Ui) {
        if let Some(err) = self.inst_error.clone() {
            theme::card_frame(RED).show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new(format!("{} {err}", ic::WARNING)).color(RED));
                    ui.add(
                        egui::TextEdit::singleline(&mut self.ld_text)
                            .hint_text("D:\\LDPlayer\\LDPlayer9")
                            .margin(theme::INPUT_MARGIN)
                            .desired_width(260.0),
                    );
                    if ui.button("Lưu").clicked() {
                        let dir = PathBuf::from(self.ld_text.trim());
                        if device::is_ldplayer_dir(&dir) {
                            self.settings.ld_dir = Some(dir);
                            self.settings.save();
                            self.refresh_instances();
                        } else {
                            self.inst_error =
                                Some("Thư mục này không có ldconsole.exe và adb.exe".into());
                        }
                    }
                });
            });
            ui.add_space(8.0);
        }
        if let Some((msg, color)) = self.message.clone() {
            theme::card_frame(color.gamma_multiply(0.6)).show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(ic::INFO).color(color));
                    ui.add(egui::Label::new(RichText::new(msg).color(TEXT)).wrap());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button(ic::X).clicked() {
                            self.message = None;
                        }
                    });
                });
            });
            ui.add_space(8.0);
        }
    }

    fn main_ui(&mut self, ctx: &egui::Context) {
        for job in &mut self.jobs {
            if let Some(updated) = job.poll() {
                if let Some(s) = self.scenarios.iter_mut().find(|s| s.id == updated.id) {
                    *s = updated;
                }
            }
        }
        self.header(ctx);

        if self.selected.is_some() {
            egui::TopBottomPanel::bottom("detail")
                .resizable(true)
                .default_height(320.0)
                .frame(
                    egui::Frame::new()
                        .fill(PANEL)
                        .inner_margin(Margin::same(16)),
                )
                .show(ctx, |ui| self.detail_ui(ui, ctx));
        }

        let mut actions = Vec::new();
        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(theme::BG)
                    .inner_margin(Margin::same(20)),
            )
            .show(ctx, |ui| {
                self.banners(ui);
                if self.scenarios.is_empty() {
                    ui.add_space(60.0);
                    ui.vertical_centered(|ui| {
                        ui.label(RichText::new(ic::LIST_CHECKS).size(48.0).color(MUTED));
                        ui.label(theme::title("Chưa có kịch bản nào"));
                        ui.label(theme::muted(
                            "Bấm \"Tạo kịch bản\" rồi nhập từng bước bằng tiếng Việt.",
                        ));
                    });
                    return;
                }
                ui.label(theme::title(format!("Kịch bản ({})", self.scenarios.len())));
                ui.add_space(4.0);
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        let instances = &self.instances;
                        for sc in &mut self.scenarios {
                            let running: Vec<&Job> = self.jobs.iter().filter(|j| j.scenario_id == sc.id && !j.is_finished()).collect();
                            let selected = self.selected.as_deref() == Some(sc.id.as_str());
                            scenario_card(ui, sc, &running, instances, selected, &mut actions);
                            ui.add_space(8.0);
                        }
                    });
            });

        for a in actions {
            self.apply(a);
        }
        self.delete_dialog(ctx);
    }

    fn apply(&mut self, action: RowAction) {
        match action {
            RowAction::Select(id) => {
                self.selected = if self.selected.as_deref() == Some(id.as_str()) {
                    None
                } else {
                    Some(id)
                };
                self.job_screen = ScreenView::default();
            }
            RowAction::Save(id) => {
                if let Some(sc) = self.scenarios.iter().find(|s| s.id == id) {
                    let _ = sc.save();
                }
            }
            RowAction::Start(id) => {
                let Some(sc) = self.scenarios.iter().find(|s| s.id == id).cloned() else {
                    return;
                };
                let Some(ld_dir) = self.settings.ld_dir.clone() else {
                    self.warn("Chưa cấu hình LDPlayer");
                    return;
                };
                let mut notes = Vec::new();
                if !sc.recorded {
                    notes.push("Kịch bản chưa chạy thử nên lần chạy này sẽ dùng AI (chậm hơn).".to_string());
                }
                let mut started = 0;
                self.jobs.retain(|j| !j.is_finished());
                for index in sc.targets() {
                    let Some(inst) = self.instances.iter().find(|i| i.index == index).cloned() else {
                        notes.push(format!("Không còn giả lập #{index}."));
                        continue;
                    };
                    if self.jobs.iter().any(|j| j.instance_index == inst.index && j.scenario_id == sc.id) {
                        continue; // already running there
                    }
                    if let Some(w) = size_warning(&sc, &inst) {
                        notes.push(w);
                    }
                    self.jobs.push(jobs::start(sc.clone(), ld_dir.clone(), inst));
                    started += 1;
                }
                if started == 0 {
                    notes.push("Chưa chạy được trên giả lập nào. Chọn giả lập cho kịch bản trước.".into());
                }
                if !notes.is_empty() {
                    self.warn(notes.join(" "));
                }
                self.selected = Some(id);
                self.detail_tab = DetailTab::Live(None);
                self.job_screen = ScreenView::default();
            }
            RowAction::Stop(id) => {
                for j in self.jobs.iter().filter(|j| j.scenario_id == id) {
                    j.stop.store(true, Ordering::Relaxed);
                }
            }
            RowAction::Edit(id) => {
                if let Some(sc) = self.scenarios.iter().find(|s| s.id == id).cloned() {
                    self.editor = Some(Editor::new(sc, false));
                }
            }
            RowAction::Delete(id) => self.confirm_delete = Some(id),
            RowAction::Export(id) => {
                let Some(sc) = self.scenarios.iter().find(|s| s.id == id) else {
                    return;
                };
                let file_name = format!("{}.omnipilot", sanitize_file_name(&sc.name));
                let Some(path) = rfd::FileDialog::new()
                    .add_filter("Kịch bản OmniPilot", &["omnipilot"])
                    .set_file_name(file_name)
                    .save_file()
                else {
                    return;
                };
                match sc.export(&path) {
                    Ok(()) => self.info(format!("Đã xuất kịch bản ra {}", path.display())),
                    Err(e) => self.warn(format!("Xuất thất bại: {e:#}")),
                }
            }
        }
    }

    fn import_dialog(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter("Kịch bản OmniPilot", &["omnipilot"])
            .pick_file()
        else {
            return;
        };
        match Scenario::import(&path) {
            Ok(mut sc) => {
                // Point it at a local emulator; the sharer's instance index means nothing here.
                if let Some(i) = self
                    .instances
                    .iter()
                    .find(|i| i.running)
                    .or(self.instances.first())
                {
                    sc.instance_index = i.index;
                    sc.instances = vec![i.index];
                    let _ = sc.save();
                }
                match self
                    .instances
                    .iter()
                    .find(|i| i.index == sc.instance_index)
                    .and_then(|i| size_warning(&sc, i))
                {
                    Some(w) => self.warn(format!("Đã nhập \"{}\". {w}", sc.name)),
                    None => self.info(format!(
                        "Đã nhập \"{}\". Chọn giả lập rồi bấm Chạy.",
                        sc.name
                    )),
                }
                self.selected = Some(sc.id.clone());
                self.scenarios.push(sc);
                self.scenarios
                    .sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
            }
            Err(e) => self.warn(format!("Nhập thất bại: {e:#}")),
        }
    }

    fn delete_dialog(&mut self, ctx: &egui::Context) {
        let Some(id) = self.confirm_delete.clone() else {
            return;
        };
        let name = self
            .scenarios
            .iter()
            .find(|s| s.id == id)
            .map(|s| s.name.clone())
            .unwrap_or_default();
        egui::Window::new("Xóa kịch bản")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label(format!("Xóa kịch bản \"{name}\"? Không thể hoàn tác."));
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui
                        .add(theme::button(format!("{} Xóa", ic::TRASH), RED))
                        .clicked()
                    {
                        if let Some(sc) = self.scenarios.iter().find(|s| s.id == id) {
                            sc.delete();
                        }
                        self.scenarios.retain(|s| s.id != id);
                        if self.selected.as_deref() == Some(id.as_str()) {
                            self.selected = None;
                        }
                        self.confirm_delete = None;
                    }
                    if ui.button("Hủy").clicked() {
                        self.confirm_delete = None;
                    }
                });
            });
    }

    fn detail_ui(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let Some(id) = self.selected.clone() else {
            return;
        };
        let Some(sc) = self.scenarios.iter().find(|s| s.id == id).cloned() else {
            return;
        };
        // Newest job per emulator for this scenario.
        let mut tabs: Vec<(u32, String, bool)> = Vec::new();
        for j in self.jobs.iter().rev().filter(|j| j.scenario_id == id) {
            if !tabs.iter().any(|t| t.0 == j.instance_index) {
                tabs.push((j.instance_index, j.instance_name.clone(), !j.is_finished()));
            }
        }
        tabs.reverse();
        if self.history_for.as_deref() != Some(id.as_str()) || ui.ctx().input(|i| i.time) % 5.0 < 0.3 {
            self.history = sc.history();
            self.history_for = Some(id.clone());
        }

        ui.horizontal(|ui| {
            ui.label(theme::title(format!("{} {}", ic::TERMINAL_WINDOW, sc.name)));
            ui.add_space(8.0);
            for (index, name, running) in &tabs {
                let selected = match self.detail_tab {
                    DetailTab::Live(Some(i)) => i == *index,
                    DetailTab::Live(None) => tabs.first().map(|t| t.0) == Some(*index),
                    DetailTab::History => false,
                };
                let dot = if *running { "● " } else { "" };
                if ui.selectable_label(selected, format!("{dot}{name}")).clicked() {
                    self.detail_tab = DetailTab::Live(Some(*index));
                    self.job_screen = ScreenView::default();
                }
            }
            if ui.selectable_label(self.detail_tab == DetailTab::History, format!("{} Lịch sử ({})", ic::CLOCK_COUNTER_CLOCKWISE, self.history.len())).clicked() {
                self.detail_tab = DetailTab::History;
                self.job_screen = ScreenView::default();
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button(ic::X).on_hover_text("Đóng").clicked() {
                    self.selected = None;
                }
            });
        });

        if self.detail_tab == DetailTab::History {
            let mut show: Option<PathBuf> = None;
            ui.columns(2, |cols| {
                egui::ScrollArea::vertical().id_salt("history").auto_shrink([false, false]).max_height(280.0).show(&mut cols[0], |ui| {
                    if self.history.is_empty() {
                        ui.label(theme::muted("Chưa có lượt chạy nào."));
                    }
                    for e in self.history.iter().rev() {
                        let (icon, color) = if e.ok { (ic::CHECK_CIRCLE, GREEN) } else { (ic::X_CIRCLE, RED) };
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(icon).color(color));
                            ui.label(RichText::new(format!("{} → {}", e.started, e.ended)).monospace().size(12.5));
                            ui.label(theme::muted(&e.instance));
                            if let Some(file) = &e.screenshot {
                                if ui.small_button(format!("{} Ảnh lúc lỗi", ic::IMAGE)).clicked() {
                                    show = Some(sc.history_dir().join(file));
                                }
                            }
                        });
                        if !e.ok {
                            ui.add(egui::Label::new(RichText::new(&e.message).size(12.5).color(RED)).wrap());
                        }
                        ui.add_space(2.0);
                    }
                });
                self.job_screen.ui(&mut cols[1], None, None);
            });
            if let Some(path) = show {
                if let Ok(img) = image::open(&path) {
                    self.job_screen.set(ctx, &img.to_rgba8(), None);
                }
            }
            return;
        }

        let wanted = match self.detail_tab {
            DetailTab::Live(Some(i)) => Some(i),
            _ => tabs.first().map(|t| t.0),
        };
        let Some(job) = self.jobs.iter().rev().find(|j| j.scenario_id == id && Some(j.instance_index) == wanted) else {
            ui.label(theme::muted("Kịch bản này chưa chạy lần nào kể từ khi mở app. Bấm Chạy để bắt đầu, hoặc xem tab Lịch sử."));
            return;
        };
        if job.screen_gen != self.job_screen.shown_gen {
            if let Some((img, tap)) = &job.screen {
                self.job_screen.set(ctx, img, *tap);
            }
            self.job_screen.shown_gen = job.screen_gen;
        }
        ui.columns(2, |cols| {
            cols[0].label(theme::muted(format!("Số lượt đã xong: {}", job.runs)));
            log_ui(&mut cols[0], &job.log);
            self.job_screen.ui(&mut cols[1], None, None);
        });
    }

    fn editor_ui(&mut self, ctx: &egui::Context) {
        let mut close: Option<bool> = None; // Some(saved)
        let instances = self.instances.clone();
        let ld_dir = self.settings.ld_dir.clone();
        let editing = self.editor.as_ref().map(|e| e.sc.id.clone()).unwrap_or_default();
        let others: Vec<(String, String)> = self.scenarios.iter().filter(|s| s.id != editing).map(|s| (s.id.clone(), s.name.clone())).collect();
        let ed = self.editor.as_mut().unwrap();
        ed.poll(ctx);
        let busy = ed.task.is_some();

        egui::TopBottomPanel::top("ed_top")
            .frame(
                egui::Frame::new()
                    .fill(PANEL)
                    .inner_margin(Margin::symmetric(20, 14)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(
                            !busy,
                            egui::Button::new(format!("{} Quay lại", ic::ARROW_LEFT)),
                        )
                        .clicked()
                    {
                        close = Some(false);
                    }
                    ui.add_space(8.0);
                    ui.label(
                        RichText::new(if ed.is_new {
                            "Tạo kịch bản"
                        } else {
                            "Sửa kịch bản"
                        })
                        .family(theme::bold())
                        .size(20.0),
                    );
                });
            });

        // Action bar.
        egui::TopBottomPanel::bottom("ed_actions")
            .frame(
                egui::Frame::new()
                    .fill(PANEL)
                    .inner_margin(Margin::symmetric(20, 12)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    let inst = instances
                        .iter()
                        .find(|i| i.index == ed.sc.instance_index)
                        .cloned();
                    let has_steps = script::leaf_count(&ed.sc.steps) > 0
                        && ed
                            .sc
                            .steps
                            .iter()
                            .any(|n| n.is_block() || n.manual || !n.text.trim().is_empty());
                    if let Some(t) = &ed.task {
                        if ui
                            .add(theme::button(format!("{} Dừng", ic::STOP), RED))
                            .clicked()
                        {
                            t.stop.store(true, Ordering::Relaxed);
                        }
                        ui.spinner();
                        ui.label(t.label);
                    } else {
                        let can_trial = has_steps && ld_dir.is_some() && inst.is_some();
                        let btn = ui.add_enabled(
                            can_trial,
                            theme::button(format!("{} Chạy thử", ic::PLAY), ACCENT),
                        );
                        let btn = if inst.is_none() {
                            btn.on_disabled_hover_text("Chọn giả lập trước")
                        } else {
                            btn
                        };
                        if btn.clicked() {
                            ed.start_trial(ld_dir.clone().unwrap(), inst.clone().unwrap());
                        }
                        if ui
                            .add_enabled(
                                has_steps,
                                egui::Button::new(format!("{} Xem AI hiểu thế nào", ic::BRAIN)),
                            )
                            .on_hover_text("AI đọc các bước nhưng chưa bấm gì vào game")
                            .clicked()
                        {
                            ed.start_analyse();
                        }
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let ok = matches!(ed.trial, Some(Ok(())));
                        let named = !ed.sc.name.trim().is_empty();
                        let can_save = ok && named && !busy;
                        let fill = if can_save {
                            Color32::from_rgb(0x16, 0xa3, 0x6f)
                        } else {
                            CARD_HI
                        };
                        let save = ui.add_enabled(
                            can_save,
                            theme::button(format!("{} Lưu kịch bản", ic::CHECK_CIRCLE), fill),
                        );
                        let save = if !ok {
                            save.on_disabled_hover_text("Chạy thử thành công rồi mới lưu được")
                        } else if !named {
                            save.on_disabled_hover_text("Nhập tên kịch bản trước")
                        } else {
                            save
                        };
                        if save.clicked() {
                            close = Some(true);
                        }
                    });
                });
            });

        egui::CentralPanel::default().frame(egui::Frame::new().fill(theme::BG).inner_margin(Margin::same(20))).show(ctx, |ui| {
            ui.columns(2, |cols| {
                egui::ScrollArea::vertical().id_salt("form").auto_shrink([false, false]).show(&mut cols[0], |ui| {
                    ui.add_enabled_ui(!busy, |ui| {
                        theme::card(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.label(theme::title(format!("{} Thông tin", ic::SLIDERS_HORIZONTAL)));
                            ui.add_space(4.0);
                            ui.label(theme::muted("Tên kịch bản"));
                            ui.add(egui::TextEdit::singleline(&mut ed.sc.name).hint_text("vd: Đánh map cốt truyện").margin(theme::INPUT_MARGIN).desired_width(f32::INFINITY));
                            ui.add_space(4.0);
                            ui.label(theme::muted("Giả lập"));
                            ui.horizontal_wrapped(|ui| {
                                targets_menu(ui, "ed_targets", &mut ed.sc, &instances);
                                if ed.sc.targets().len() > 1 {
                                    let first = instances.iter().find(|i| i.index == ed.sc.instance_index).map(|i| i.name.clone()).unwrap_or_default();
                                    ui.label(theme::muted(format!("Chụp màn hình và chạy thử dùng \"{first}\"")));
                                }
                            });
                            ui.add_space(4.0);
                            ui.label(theme::muted("Kiểu chạy"));
                            schedule_ui(ui, ed);
                            ui.add_space(4.0);
                            limits_ui(ui, ed);
                        });
                        ui.add_space(12.0);
                        theme::card(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.label(theme::title(format!("{} Các bước", ic::LIST_NUMBERS)));
                            ui.label(theme::muted("Mỗi ô là một bước, viết như nói chuyện. vd: \"bấm xuất chiến góc phải dưới\", \"chọn ải mới nhất\", \"đợi đánh xong\", \"chờ 3 giây\"."));
                            ui.add_space(6.0);
                            let dir = ed.sc.dir();
                            let var_names: Vec<String> = ed.sc.variables.iter().map(|v| v.name.clone()).filter(|n| !n.trim().is_empty()).collect();
                            let mut st = StepsUi {
                                counter: 0,
                                changed: false,
                                picking: ed.picking.as_ref(),
                                pick_request: None,
                                test_request: None,
                                thumbs: &mut ed.thumbs,
                                dir,
                                others: &others,
                                var_names: &var_names,
                                match_request: None,
                                run_from: None,
                                breakpoint: ed.breakpoint.as_ref(),
                                toggle_breakpoint: None,
                            };
                            nodes_ui(ui, &mut ed.sc.steps, &mut Vec::new(), &mut st);
                            ui.add_space(4.0);
                            ui.horizontal(|ui| {
                                if let Some(new) = add_step_menu(ui, format!("{} Thêm bước", ic::PLUS)) {
                                    push_new(&mut ed.sc.steps, new, &[], &mut st);
                                }
                                if ui.button(format!("{} Thêm vòng lặp", ic::REPEAT)).on_hover_text("Lặp lại một nhóm bước, vd: đánh hết các map").clicked() {
                                    ed.sc.steps.push(Node::new_loop());
                                    st.changed = true;
                                }
                            });
                            let (changed, pick_request, test_request) = (st.changed, st.pick_request, st.test_request);
                            let (match_request, run_from, toggle_bp) = (st.match_request, st.run_from, st.toggle_breakpoint);
                            if let Some(bp) = toggle_bp {
                                ed.breakpoint = if ed.breakpoint.as_ref() == Some(&bp) { None } else { Some(bp) };
                            }
                            if let Some(rec) = match_request {
                                ed.test_match(&rec);
                            }
                            if changed {
                                ed.trial = None;
                            }
                            let inst = instances.iter().find(|i| i.index == ed.sc.instance_index).cloned();
                            if let Some(t) = pick_request {
                                // Asking for the same pick again cancels it.
                                ed.picking = if ed.picking.as_ref() == Some(&t) { None } else { Some(t) };
                                if ed.picking.is_some() && ed.screen.tex.is_none() {
                                    if let (Some(ld), Some(inst)) = (&ld_dir, &inst) {
                                        ed.capture(ld.clone(), inst.clone());
                                    }
                                }
                            }
                            if let Some(node) = test_request {
                                match (&ld_dir, inst.clone()) {
                                    (Some(ld), Some(inst)) => ed.start_test_step(ld.clone(), inst, node),
                                    _ => ed.error = Some("Chọn giả lập trước khi thử bước".into()),
                                }
                            }
                            if let Some(from) = run_from {
                                match (&ld_dir, inst) {
                                    (Some(ld), Some(inst)) => ed.start_run_from(ld.clone(), inst, from),
                                    _ => ed.error = Some("Chọn giả lập trước khi chạy".into()),
                                }
                            }
                            let n = script::dynamic_count(&ed.sc.steps);
                            if n > 0 {
                                ui.add_space(4.0);
                                ui.label(RichText::new(format!("{} {n} bước có mục tiêu thay đổi mỗi lần (vd \"ải mới nhất\") nên lần chạy nào AI cũng bật lên cho các bước này.", ic::LIGHTNING)).color(YELLOW).size(13.0));
                            }
                        });
                        ui.add_space(12.0);
                        variables_card(ui, ed);
                        ui.add_space(12.0);
                        let pick = safety_card(ui, ed, &instances, &ld_dir);
                        if let Some(t) = pick {
                            ed.picking = if ed.picking.as_ref() == Some(&t) { None } else { Some(t) };
                            if ed.picking.is_some() && ed.screen.tex.is_none() {
                                if let (Some(ld), Some(inst)) = (&ld_dir, instances.iter().find(|i| i.index == ed.sc.instance_index)) {
                                    ed.capture(ld.clone(), inst.clone());
                                }
                            }
                        }
                    });

                    if let Some(e) = &ed.error {
                        ui.add_space(12.0);
                        theme::card_frame(RED).show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.label(RichText::new(format!("{} {e}", ic::WARNING)).color(RED));
                        });
                    }
                    if let Some(result) = &ed.trial {
                        ui.add_space(12.0);
                        match result {
                            Ok(()) => {
                                theme::card_frame(GREEN).show(ui, |ui| {
                                    ui.set_width(ui.available_width());
                                    ui.label(RichText::new(format!("{} Chạy thử xong", ic::CHECK_CIRCLE)).color(GREEN).family(theme::bold()));
                                    ui.label("Nếu game đã chạy đúng như bạn muốn, bấm \"Lưu kịch bản\". Chưa đúng thì sửa bước rồi chạy thử lại.");
                                });
                            }
                            Err(e) => {
                                theme::card_frame(RED).show(ui, |ui| {
                                    ui.set_width(ui.available_width());
                                    ui.label(RichText::new(format!("{} Chạy thử chưa thành công", ic::X_CIRCLE)).color(RED).family(theme::bold()));
                                    ui.label(e.as_str());
                                    ui.label(theme::muted("Sửa lại bước bị lỗi (viết đúng chữ trên nút) rồi chạy thử lại."));
                                });
                            }
                        }
                    }
                    ui.add_space(12.0);
                    ui.label(theme::muted(format!(
                        "{} Lần chạy thử đầu tiên sẽ chậm vì AI phải nhìn từng màn hình. Sau khi lưu, app dùng ảnh mẫu các nút nên những lần sau chạy nhanh và không cần AI.",
                        ic::INFO
                    )));
                });

                let ui = &mut cols[1];
                theme::card(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        ui.label(theme::title(format!("{} Màn hình giả lập", ic::MONITOR)));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let inst = instances.iter().find(|i| i.index == ed.sc.instance_index);
                            let can = !busy && ed.shot_rx.is_none() && ld_dir.is_some() && inst.is_some();
                            if ui.add_enabled(can, egui::Button::new(format!("{} Chụp màn hình", ic::CAMERA))).on_hover_text("Lấy màn hình hiện tại của giả lập").clicked() {
                                ed.capture(ld_dir.clone().unwrap(), inst.cloned().unwrap());
                            }
                            if ed.shot_rx.is_some() || ed.checks_rx.is_some() {
                                ui.spinner();
                            }

                        });
                    });
                    ui.horizontal(|ui| {
                        let can_check = ed.screen.img.is_some() && ed.checks_rx.is_none();
                        let tip = "Tìm từng ảnh nút của kịch bản trên ảnh màn hình hiện tại: biết ngay ảnh nào còn khớp sau khi game cập nhật";
                        let btn = ui.add_enabled(can_check, egui::Button::new(format!("{} Kiểm tra ảnh mẫu", ic::CHECKS)).small());
                        if btn.on_hover_text(tip).on_disabled_hover_text("Chụp màn hình giả lập trước").clicked() {
                            ed.check_templates();
                        }
                    });
                    ui.add_space(4.0);
                    let picking = ed.picking.clone();
                    let region = picking.as_ref().filter(|p| p.kind == PickKind::Region).and_then(|p| node_at(&mut ed.sc.steps, &p.path)).and_then(|n| n.region);
                    if let Some(p) = &picking {
                        theme::card_frame(YELLOW.gamma_multiply(0.6)).inner_margin(Margin::same(10)).show(ui, |ui| {
                            ui.horizontal(|ui| {
                                let text = format!("{} {}", ic::CURSOR_CLICK, p.kind.instruction());
                                let text_width = ui.available_width() - 60.0; // room for "Hủy"
                                ui.scope(|ui| {
                                    ui.set_max_width(text_width);
                                    ui.add(egui::Label::new(RichText::new(text).color(YELLOW)).wrap());
                                });
                                if ui.small_button("Hủy").clicked() {
                                    ed.picking = None;
                                }
                            });
                        });
                        ui.add_space(6.0);
                    }
                    if !ed.checks.is_empty() {
                        checks_ui(ui, ed);
                    }
                    if let Some(picked) = ed.screen.ui(ui, region, picking.as_ref().map(|p| p.kind)) {
                        if let Some(t) = &picking {
                            match ed.apply_pick(t, picked) {
                                Ok(()) => ed.trial = None,
                                Err(e) => ed.error = Some(e),
                            }
                        }
                        ed.picking = None;
                    }
                });
                ui.add_space(12.0);
                theme::card(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.label(theme::title(format!("{} Nhật ký", ic::TERMINAL_WINDOW)));
                    ui.add_space(4.0);
                    log_ui(ui, &ed.log);
                });
            });
        });

        if let Some(saved) = close {
            let mut ed = self.editor.take().unwrap();
            ed.close();
            if saved {
                ed.sc.recorded = true;
                match ed.sc.save() {
                    Ok(()) => {
                        let sc = ed.sc.clone();
                        match self.scenarios.iter_mut().find(|s| s.id == sc.id) {
                            Some(s) => *s = sc,
                            None => self.scenarios.push(sc),
                        }
                        self.scenarios
                            .sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
                        self.info(format!(
                            "Đã lưu \"{}\". Chọn giả lập rồi bấm Chạy.",
                            ed.sc.name
                        ));
                    }
                    Err(e) => self.warn(format!("Lưu thất bại: {e:#}")),
                }
            } else if ed.is_new {
                ed.sc.delete(); // drop templates from an abandoned trial
            }
        }
    }
}

/// Results of "Kiểm tra ảnh mẫu".
fn checks_ui(ui: &mut egui::Ui, ed: &mut Editor) {
    let dir = ed.sc.dir();
    let ok = ed.checks.iter().filter(|c| c.score >= c.threshold).count();
    ui.horizontal(|ui| {
        ui.label(RichText::new(format!("Kiểm tra ảnh mẫu: {ok}/{} khớp trên màn hình này", ed.checks.len())).family(theme::bold()));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.small_button(ic::X).clicked() {
                ed.checks.clear();
                ed.screen.marks.clear();
            }
        });
    });
    ui.label(theme::muted("Ảnh không khớp có thể chỉ là chưa ở đúng màn hình đó. Nếu đang ở đúng màn hình mà vẫn đỏ: khoanh lại ảnh hoặc giảm độ giống."));
    egui::ScrollArea::vertical().id_salt("checks").max_height(170.0).show(ui, |ui| {
        for c in &ed.checks {
            ui.horizontal(|ui| {
                let good = c.score >= c.threshold;
                ui.label(RichText::new(if good { ic::CHECK_CIRCLE } else { ic::X_CIRCLE }).color(if good { GREEN } else { RED }));
                if let Some(tex) = ed.thumbs.get(ui.ctx(), dir.join(&c.template)) {
                    let [w, h] = tex.size().map(|v| v as f32);
                    let scale = (24.0 / h).min(90.0 / w).min(1.0);
                    ui.add(egui::Image::new((tex.id(), vec2(w * scale, h * scale))));
                }
                ui.label(&c.label);
                ui.label(theme::muted(format!("giống {:.0}% (cần {:.0}%)", c.score * 100.0, c.threshold * 100.0)));
            });
        }
    });
    ui.add_space(6.0);
}

/// "Biến": numbers kept while a scenario runs (counters, values read on screen).
fn variables_card(ui: &mut egui::Ui, ed: &mut Editor) {
    theme::card(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.label(theme::title(format!("{} Biến", ic::BRACKETS_CURLY)));
        ui.label(theme::muted("Lưu số để dùng lại: đếm số trận đã đánh, lưu số thể lực đọc được… Dùng ở bước \"Đặt biến\" và điều kiện \"Nếu biến …\"."));
        let saved = ed.sc.load_vars(ed.sc.instance_index);
        let mut remove = None;
        for (i, v) in ed.sc.variables.iter_mut().enumerate() {
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut v.name).hint_text("tên biến, vd số trận").desired_width(150.0).margin(Margin::symmetric(8, 4)));
                egui::ComboBox::from_id_salt(("var-scope", i)).selected_text(v.scope.label()).width(130.0).show_ui(ui, |ui| {
                    for sc in script::VarScope::ALL {
                        ui.selectable_value(&mut v.scope, sc, sc.label());
                    }
                });
                if let Some(n) = saved.get(&v.name) {
                    ui.label(theme::muted(format!("đang = {n}")));
                }
                if ui.small_button(ic::TRASH).clicked() {
                    remove = Some(i);
                }
            });
        }
        if let Some(i) = remove {
            ed.sc.variables.remove(i);
        }
        if ui.button(format!("{} Thêm biến", ic::PLUS)).clicked() {
            let n = ed.sc.variables.len() + 1;
            ed.sc.variables.push(script::VarDef { name: format!("biến {n}"), scope: script::VarScope::Run });
        }
    });
}

/// Time window, run limits and emulator options.
fn limits_ui(ui: &mut egui::Ui, ed: &mut Editor) {
    let l = &mut ed.sc.limits;
    if matches!(ed.sc.schedule, Schedule::Repeat { .. }) {
        ui.horizontal_wrapped(|ui| {
            ui.label(theme::muted("Chỉ chạy từ"));
            ui.add(egui::TextEdit::singleline(&mut l.from).hint_text("08:00").desired_width(56.0).margin(theme::INPUT_MARGIN));
            ui.label(theme::muted("đến"));
            ui.add(egui::TextEdit::singleline(&mut l.to).hint_text("23:00").desired_width(56.0).margin(theme::INPUT_MARGIN));
            ui.label(theme::muted("(để trống = cả ngày)"));
        });
        let bad = |t: &str| !t.trim().is_empty() && chrono::NaiveTime::parse_from_str(t.trim(), "%H:%M").is_err();
        if bad(&l.from) || bad(&l.to) {
            ui.colored_label(RED, "Giờ phải dạng HH:MM, vd 08:00");
        }
        ui.horizontal_wrapped(|ui| {
            ui.label(theme::muted("Dừng sau"));
            ui.add(egui::DragValue::new(&mut l.max_minutes).range(0..=10000));
            ui.label(theme::muted("phút hoặc"));
            ui.add(egui::DragValue::new(&mut l.max_runs).range(0..=100000));
            ui.label(theme::muted("lượt (0 = không giới hạn)"));
        });
    }
    ui.checkbox(&mut ed.sc.close_emulator_after, "Tắt giả lập khi xong (sau mỗi lượt theo giờ, hoặc khi chạy đủ)");
    ui.checkbox(&mut ed.sc.auto_start, "Tự chạy khi mở app (dùng cùng \"Khởi động cùng Windows\" trong Cài đặt)");
}

/// "Popup & tự phục hồi": rules checked before every step, and how to recover from errors.
/// Returns a pick request (popup picture/point, start screen).
fn safety_card(ui: &mut egui::Ui, ed: &mut Editor, instances: &[Instance], ld_dir: &Option<PathBuf>) -> Option<PickTarget> {
    let mut pick = None;
    let dir = ed.sc.dir();
    let picking = ed.picking.clone();
    let is_picking = |slot: Slot| picking.as_ref().is_some_and(|p| p.slot == slot);
    let request = |slot: Slot, kind: PickKind| Some(PickTarget { path: Vec::new(), slot, kind });
    let mut changed = false;
    let mut load_games: Option<Instance> = None;
    let mut test_match: Option<script::Rec> = None;
    theme::card(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.label(theme::title(format!("{} Popup & tự phục hồi", ic::SHIELD_CHECK)));
        ui.label(theme::muted("Giúp kịch bản chạy lâu không kẹt: tự đóng popup bất ngờ, và tự quay về màn hình gốc hoặc mở lại game khi lỗi."));
        ui.add_space(6.0);

        ui.label(RichText::new("Popup: thấy ảnh này ở đâu thì…").family(theme::bold()));
        let mut remove = None;
        for (i, rule) in ed.sc.popups.iter_mut().enumerate() {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(format!("{}.", i + 1)).color(MUTED));
                if !rule.rec.template.is_empty() {
                    if let Some(tex) = ed.thumbs.get(ui.ctx(), dir.join(&rule.rec.template)) {
                        let [w, h] = tex.size().map(|v| v as f32);
                        let scale = (34.0 / h).min(140.0 / w).min(1.5);
                        ui.add(egui::Image::new((tex.id(), vec2(w * scale, h * scale))).corner_radius(4));
                    }
                }
                if pick_button(ui, is_picking(Slot::PopupImage(i)), rule.rec.template.is_empty(), "ảnh popup") {
                    pick = request(Slot::PopupImage(i), PickKind::Image);
                }
                if !rule.rec.template.is_empty() {
                    let mut pct = (rule.rec.threshold() * 100.0).round();
                    if ui.add(egui::DragValue::new(&mut pct).range(50.0..=99.0).suffix("%")).on_hover_text("Độ giống cần có").changed() {
                        rule.rec.threshold = Some(pct / 100.0);
                        changed = true;
                    }
                    if ui.add(egui::Button::new(RichText::new(ic::CROSSHAIR).size(13.0)).small()).on_hover_text("Thử khớp trên ảnh màn hình").clicked() {
                        test_match = Some(rule.rec.clone());
                    }
                }
                ui.label(theme::muted("thì"));
                let current = match rule.action {
                    script::PopupAction::TapImage => "bấm vào chính ảnh đó",
                    script::PopupAction::TapPoint { .. } => "bấm vào một điểm",
                    script::PopupAction::Back => "nhấn Back",
                };
                egui::ComboBox::from_id_salt(("popup-action", i)).selected_text(current).width(170.0).show_ui(ui, |ui| {
                    if ui.selectable_label(rule.action == script::PopupAction::TapImage, "bấm vào chính ảnh đó").clicked() {
                        rule.action = script::PopupAction::TapImage;
                        changed = true;
                    }
                    if ui.selectable_label(matches!(rule.action, script::PopupAction::TapPoint { .. }), "bấm vào một điểm").clicked() {
                        pick = request(Slot::PopupPoint(i), PickKind::Point);
                    }
                    if ui.selectable_label(rule.action == script::PopupAction::Back, "nhấn Back").clicked() {
                        rule.action = script::PopupAction::Back;
                        changed = true;
                    }
                });
                if let script::PopupAction::TapPoint { x, y } = rule.action {
                    ui.label(theme::muted(format!("({:.0}%, {:.0}%)", x * 100.0, y * 100.0)));
                }
                ui.add(egui::TextEdit::singleline(&mut rule.note).hint_text("tên (vd bảng sự kiện)").desired_width(130.0));
                if ui.small_button(ic::TRASH).on_hover_text("Xóa quy tắc").clicked() {
                    remove = Some(i);
                }
            });
        }
        if let Some(i) = remove {
            ed.sc.popups.remove(i);
            changed = true;
        }
        if ui.button(format!("{} Thêm quy tắc popup", ic::PLUS)).on_hover_text("Khoanh nút đóng của popup (vd dấu X), app sẽ tự bấm mỗi khi nó hiện ra").clicked() {
            ed.sc.popups.push(script::PopupRule { rec: empty_rec(), action: script::PopupAction::TapImage, note: String::new() });
            pick = request(Slot::PopupImage(ed.sc.popups.len() - 1), PickKind::Image);
            changed = true;
        }

        ui.add_space(10.0);
        ui.label(RichText::new("Khi lỗi: tự phục hồi rồi chạy lại").family(theme::bold()));
        let r = &mut ed.sc.recovery;
        ui.horizontal_wrapped(|ui| {
            ui.label(theme::muted("Màn hình gốc"));
            if let Some(home) = &r.home {
                if let Some(tex) = ed.thumbs.get(ui.ctx(), dir.join(&home.template)) {
                    let [w, h] = tex.size().map(|v| v as f32);
                    let scale = (34.0 / h).min(140.0 / w).min(1.5);
                    ui.add(egui::Image::new((tex.id(), vec2(w * scale, h * scale))).corner_radius(4));
                }
            }
            if pick_button(ui, is_picking(Slot::Home), r.home.is_none(), "một phần của màn hình gốc") {
                pick = request(Slot::Home, PickKind::Image);
            }
            if r.home.is_some() && ui.small_button(ic::X).clicked() {
                r.home = None;
                changed = true;
            }
        });
        ui.label(RichText::new("Khoanh một chi tiết chỉ có ở màn hình bắt đầu kịch bản (vd nút \"Xuất chiến\" ở màn hình chính).").size(12.5).color(MUTED));
        ui.horizontal_wrapped(|ui| {
            ui.label(theme::muted("Game để mở lại"));
            let shown = if r.package.is_empty() { "(chưa chọn)".to_string() } else { r.package.clone() };
            egui::ComboBox::from_id_salt("recovery-package").selected_text(shown).width(260.0).show_ui(ui, |ui| {
                if ui.selectable_label(r.package.is_empty(), "(không mở lại game)").clicked() {
                    r.package.clear();
                    changed = true;
                }
                for p in &ed.packages {
                    if ui.selectable_label(r.package == *p, p).clicked() {
                        r.package = p.clone();
                        changed = true;
                    }
                }
            });
            let inst = instances.iter().find(|i| i.index == ed.sc.instance_index);
            let can = ed.packages_rx.is_none() && ld_dir.is_some() && inst.is_some();
            if ui.add_enabled(can, egui::Button::new(format!("{} Lấy danh sách game", ic::ARROWS_CLOCKWISE)).small()).on_hover_text("Mở game trên giả lập trước, game đó sẽ đứng đầu danh sách").clicked() {
                load_games = inst.cloned();
            }
            if ed.packages_rx.is_some() {
                ui.spinner();
            }
        });
        ui.horizontal_wrapped(|ui| {
            ui.label(theme::muted("Nhấn Back tối đa"));
            changed |= ui.add(egui::DragValue::new(&mut r.max_back).range(0..=30)).changed();
            ui.label(theme::muted("lần để về màn hình gốc, không được thì khởi động lại game; chạy lại tối đa"));
            changed |= ui.add(egui::DragValue::new(&mut r.retries).range(0..=10)).changed();
            ui.label(theme::muted("lần"));
        });
    });
    if let (Some(inst), Some(ld)) = (load_games, ld_dir) {
        ed.load_packages(ld.clone(), inst);
    }
    if let Some(rec) = test_match {
        ed.test_match(&rec);
    }
    if changed {
        ed.trial = None;
    }
    pick
}

/// `omnipilot --demo`: opens the editor with an example, to show what a scenario looks like.
fn demo_scenario(instances: &[Instance]) -> Scenario {
    let mut sc = Scenario::new();
    sc.name = "Đánh map cốt truyện".into();
    sc.schedule = Schedule::Once;
    if let Some(i) = instances.iter().find(|i| i.running) {
        sc.instance_index = i.index;
    }
    let node = Node::with_text;
    let mut challenge = Node::manual(Step::TapImage { all: false, max: 30, rec: empty_rec() });
    challenge.text = "nút Khiêu chiến".into();
    challenge.otherwise = Some(Box::new(Node::manual(Step::Swipe { from: [0.6, 0.8], to: [0.6, 0.3], ms: 400 })));
    let mut lp = Node::new_loop();
    if let Some(Step::Loop { steps, .. }) = &mut lp.step {
        *steps = vec![challenge, node("đợi đánh xong"), Node::manual(Step::Wait { seconds: 2.0 })];
    }
    sc.steps = vec![Node::manual(Step::TapPoint { x: 0.42, y: 0.12 }), lp, node("thoát ra")];
    sc.popups = vec![script::PopupRule { rec: empty_rec(), action: script::PopupAction::Back, note: "bảng sự kiện".into() }];
    sc.recovery.package = "com.kaiqigu.cjyxyn.kvsdk.offline".into();
    sc
}

fn schedule_ui(ui: &mut egui::Ui, ed: &mut Editor) {
    let repeat = matches!(ed.sc.schedule, Schedule::Repeat { .. });
    let daily = matches!(ed.sc.schedule, Schedule::Daily { .. });
    let once = ed.sc.schedule == Schedule::Once;
    ui.horizontal(|ui| {
        if ui
            .selectable_label(repeat, format!("{} Lặp lại liên tục", ic::REPEAT))
            .clicked()
        {
            ed.sc.schedule = Schedule::Repeat {
                pause_minutes: ed.pause_text.trim().parse().unwrap_or(0),
            };
        }
        if ui
            .selectable_label(daily, format!("{} Chạy theo giờ", ic::CLOCK))
            .clicked()
        {
            ed.sc.schedule = Schedule::Daily {
                times: jobs::parse_times(&ed.times_text),
            };
        }
        if ui
            .selectable_label(once, format!("{} Chạy 1 lần", ic::PLAY_CIRCLE))
            .clicked()
        {
            ed.sc.schedule = Schedule::Once;
        }
    });
    match &mut ed.sc.schedule {
        Schedule::Repeat { pause_minutes } => {
            ui.horizontal(|ui| {
                ui.label(theme::muted("Nghỉ giữa các lượt"));
                if ui
                    .add(
                        egui::TextEdit::singleline(&mut ed.pause_text)
                            .margin(theme::INPUT_MARGIN)
                            .desired_width(48.0),
                    )
                    .changed()
                {
                    *pause_minutes = ed.pause_text.trim().parse().unwrap_or(0);
                }
                ui.label(theme::muted("phút (0 = chạy liên tục)"));
            });
        }
        Schedule::Daily { times } => {
            ui.horizontal(|ui| {
                ui.label(theme::muted("Giờ chạy"));
                if ui
                    .add(
                        egui::TextEdit::singleline(&mut ed.times_text)
                            .hint_text("08:00, 20:30")
                            .margin(theme::INPUT_MARGIN)
                            .desired_width(180.0),
                    )
                    .changed()
                {
                    *times = jobs::parse_times(&ed.times_text);
                }
            });
            if times.is_empty() {
                ui.colored_label(RED, "Chưa có giờ hợp lệ, vd: 08:00, 20:30");
            }
        }
        Schedule::Once => {
            ui.label(theme::muted(
                "Bấm Chạy là chạy hết các bước đúng một lần rồi tự dừng.",
            ));
        }
    }
}

enum NodeAction {
    Up(usize),
    Down(usize),
    Remove(usize),
}

/// State shared while drawing the (nested) step list.
struct StepsUi<'a> {
    /// Running step number shown in the badges.
    counter: usize,
    changed: bool,
    /// What is being picked on the screenshot right now.
    picking: Option<&'a PickTarget>,
    /// Set when the user asks to pick something for a step.
    pick_request: Option<PickTarget>,
    /// Set when the user clicks "Thử bước này".
    test_request: Option<Node>,
    thumbs: &'a mut ThumbCache,
    /// Scenario folder, where button pictures are stored.
    dir: PathBuf,
    /// Other scenarios (id, name), for "Chạy kịch bản con".
    others: &'a [(String, String)],
    /// Variable names of the scenario.
    var_names: &'a [String],
    /// "Thử khớp": the picture to look for on the current screenshot.
    match_request: Option<script::Rec>,
    /// "Chạy từ đây".
    run_from: Option<Vec<usize>>,
    /// "Dừng trước bước này" (shown on that step; clicking toggles it).
    breakpoint: Option<&'a Vec<usize>>,
    toggle_breakpoint: Option<Vec<usize>>,
}

impl StepsUi<'_> {
    fn is_picking(&self, path: &[usize], slot: Slot, kind: PickKind) -> bool {
        self.picking.is_some_and(|p| p.path == path && p.slot == slot && p.kind == kind)
    }

    fn request(&mut self, path: &[usize], slot: Slot, kind: PickKind) {
        self.pick_request = Some(PickTarget { path: path.to_vec(), slot, kind });
    }

    /// Small picture of a saved button / screen part.
    fn thumb(&mut self, ui: &mut egui::Ui, rec: &script::Rec) {
        if let Some(tex) = self.thumbs.get(ui.ctx(), self.dir.join(&rec.template)) {
            let [w, h] = tex.size().map(|v| v as f32);
            let scale = (34.0 / h).min(160.0 / w).min(1.5);
            ui.add(egui::Image::new((tex.id(), vec2(w * scale, h * scale))).corner_radius(4)).on_hover_text("Ảnh đã khoanh");
        }
    }
}

/// Finds a node by its path: an index in each list, and between nested lists the
/// number of the child list (0 = loop body / "thì", 1 = "không thì").
fn node_at<'a>(nodes: &'a mut [Node], path: &[usize]) -> Option<&'a mut Node> {
    let (first, rest) = path.split_first()?;
    let node = nodes.get_mut(*first)?;
    if rest.is_empty() {
        return Some(node);
    }
    let (branch, rest) = rest.split_first()?;
    let list = node.step.as_mut()?.children_mut().into_iter().nth(*branch)?;
    node_at(list, rest)
}

/// The node a pick is for: the step itself or its "nếu không thấy thì" action.
fn target_node<'a>(nodes: &'a mut [Node], t: &PickTarget) -> Option<&'a mut Node> {
    let node = node_at(nodes, &t.path)?;
    if t.slot == Slot::Otherwise { node.otherwise.as_deref_mut() } else { Some(node) }
}

/// The condition of an "if" block or a loop's "dừng khi".
fn cond_mut(node: &mut Node) -> Option<&mut Cond> {
    match node.step.as_mut()? {
        Step::If { cond, .. } => Some(cond),
        Step::Loop { until, .. } => until.as_mut(),
        _ => None,
    }
}

/// A growing, height-draggable text area for one step.
fn step_text_box(ui: &mut egui::Ui, id: impl std::hash::Hash, text: &mut String, hint: &str) -> bool {
    let w = ui.available_width();
    let mut changed = false;
    egui::Resize::default()
        .id_salt(id)
        .resizable([false, true])
        .with_stroke(false)
        .min_width(w)
        .max_width(w)
        .default_size(vec2(w, 40.0))
        .min_height(40.0)
        .max_height(300.0)
        .show(ui, |ui| {
            let edit = egui::TextEdit::multiline(text).hint_text(hint).margin(theme::INPUT_MARGIN).desired_rows(1).desired_width(w);
            changed = ui.add_sized(ui.available_size(), edit).changed();
        });
    changed
}

/// One line telling how the AI understood a step.
fn interpretation(ui: &mut egui::Ui, node: &Node, indent: f32) {
    let (text, color) = match &node.step {
        Some(s @ Step::Tap { dynamic: true, .. }) => (format!("{} {}", ic::LIGHTNING, s.describe()), YELLOW),
        Some(s) => (format!("{} {}", ic::ARROW_BEND_DOWN_RIGHT, s.describe()), GREEN),
        None if node.text.trim().is_empty() => return,
        None => ("AI chưa đọc bước này".to_string(), MUTED),
    };
    ui.horizontal(|ui| {
        ui.add_space(indent);
        ui.add(egui::Label::new(RichText::new(text).size(12.5).color(color)).wrap());
    });
}

fn empty_rec() -> script::Rec {
    script::Rec::default()
}

/// What has to be picked on the screen right after adding a step.
type NewStep = (Node, Option<(Slot, PickKind)>);

/// The kinds of step the user can add, in menu order (None = separator with a heading).
fn new_step_kinds() -> Vec<Option<(String, &'static str, fn() -> NewStep)>> {
    let image = Some((Slot::Step, PickKind::Image));
    let _ = image;
    vec![
        Some((format!("{} Viết bằng chữ (AI hiểu)", ic::PENCIL_SIMPLE), "Mô tả bước bằng tiếng Việt, AI tự tìm nút", || (Node::new(), None))),
        None,
        Some((format!("{} Bấm vào nút (khoanh ảnh nút)", ic::IMAGE_SQUARE), "Khoanh quanh nút trên ảnh màn hình; mỗi lần chạy app tìm nút giống vậy rồi bấm", || {
            (Node::manual(Step::TapImage { all: false, max: 30, rec: empty_rec() }), Some((Slot::Step, PickKind::Image)))
        })),
        Some((format!("{} Bấm hết các nút giống nhau", ic::COPY), "vd bấm hết các nút \"Nhận\" trên màn hình", || {
            (Node::manual(Step::TapImage { all: true, max: 30, rec: empty_rec() }), Some((Slot::Step, PickKind::Image)))
        })),
        Some((format!("{} Bấm vào một điểm cố định", ic::HAND_TAP), "Bấm đúng một vị trí trên màn hình", || {
            (Node::manual(Step::TapPoint { x: -1.0, y: -1.0 }), Some((Slot::Step, PickKind::Point)))
        })),
        Some((format!("{} Đợi đến khi thấy ảnh", ic::HOURGLASS), "Khoanh một phần màn hình; app đợi đến khi nó xuất hiện (vd chữ \"Chiến thắng\")", || {
            (Node::manual(Step::WaitImage { timeout_sec: 300, rec: empty_rec() }), Some((Slot::Step, PickKind::Image)))
        })),
        Some((format!("{} Vuốt", ic::HAND_SWIPE_RIGHT), "Kéo từ điểm này đến điểm kia (vd cuộn danh sách)", || {
            (Node::manual(Step::Swipe { from: [0.0, 0.0], to: [0.0, 0.0], ms: 400 }), Some((Slot::Step, PickKind::Swipe)))
        })),
        Some((format!("{} Chờ vài giây", ic::TIMER), "Nghỉ một lúc trước bước tiếp theo", || (Node::manual(Step::Wait { seconds: 2.0 }), None))),
        Some((format!("{} Nhấn nút Back của Android", ic::ARROW_U_UP_LEFT), "Phím quay lại của điện thoại (một số game bỏ qua phím này)", || {
            (Node::manual(Step::Back), None)
        })),
        None,
        Some((format!("{} Nếu thấy ảnh … thì … không thì …", ic::GIT_BRANCH), "Rẽ nhánh theo việc có thấy một hình trên màn hình hay không", || {
            (Node::new_if(Cond::new_image()), Some((Slot::Cond, PickKind::Image)))
        })),
        Some((format!("{} Nếu số trong vùng … thì …", ic::HASH), "Đọc số trong một vùng (vd thể lực 97/120 đọc ra 97) rồi so sánh", || {
            (Node::new_if(Cond::new_number()), Some((Slot::Cond, PickKind::Region)))
        })),
        Some((format!("{} Nếu biến … thì …", ic::BRACKETS_CURLY), "Rẽ nhánh theo giá trị một biến (vd số trận đã đánh hôm nay)", || {
            (Node::new_if(Cond::new_var()), None)
        })),
        Some((format!("{} Đặt biến (gán, cộng, đọc số)", ic::CALCULATOR), "vd cộng 1 vào \"số trận\" sau mỗi trận, hoặc lưu số thể lực đọc được", || {
            (Node::manual(Step::SetVar { name: String::new(), change: script::VarOp::Add { value: 1.0 } }), None)
        })),
        Some((format!("{} Dừng kịch bản", ic::STOP_CIRCLE), "Kết thúc lượt chạy này (vd khi hết thể lực)", || (Node::manual(Step::StopRun), None))),
        Some((format!("{} Chạy kịch bản con", ic::FLOW_ARROW), "Chạy các bước của một kịch bản khác ở đây (vd \"về màn hình chính\")", || {
            (Node::manual(Step::Call { scenario_id: String::new(), name: String::new() }), None)
        })),
    ]
}

/// "Thêm bước" menu.
fn add_step_menu(ui: &mut egui::Ui, label: String) -> Option<NewStep> {
    let mut out = None;
    let headings = ["Làm bằng tay trên màn hình (không cần AI)", "Điều kiện, kịch bản con"];
    ui.menu_button(label, |ui| {
        let mut heading = 0;
        for item in new_step_kinds() {
            match item {
                None => {
                    ui.separator();
                    ui.label(RichText::new(headings[heading.min(1)]).size(12.5).color(MUTED));
                    heading += 1;
                }
                Some((text, tip, make)) => {
                    if ui.button(text).on_hover_text(tip).clicked() {
                        out = Some(make());
                        ui.close();
                    }
                }
            }
        }
    });
    out
}

fn step_icon(step: &Step) -> &'static str {
    match step {
        Step::TapPoint { .. } => ic::HAND_TAP,
        Step::TapImage { all: true, .. } => ic::COPY,
        Step::TapImage { .. } => ic::IMAGE_SQUARE,
        Step::WaitImage { .. } => ic::HOURGLASS,
        Step::Swipe { .. } => ic::HAND_SWIPE_RIGHT,
        Step::Wait { .. } => ic::TIMER,
        Step::Back => ic::ARROW_U_UP_LEFT,
        Step::StopRun => ic::STOP_CIRCLE,
        Step::SetVar { .. } => ic::CALCULATOR,
        Step::Call { .. } => ic::FLOW_ARROW,
        _ => ic::PENCIL_SIMPLE,
    }
}

fn pick_kind_of(step: &Step) -> Option<PickKind> {
    match step {
        Step::TapPoint { .. } => Some(PickKind::Point),
        Step::TapImage { .. } | Step::WaitImage { .. } => Some(PickKind::Image),
        Step::Swipe { .. } => Some(PickKind::Swipe),
        _ => None,
    }
}

fn pick_button(ui: &mut egui::Ui, picking: bool, unset: bool, what: &str) -> bool {
    let label = match (picking, unset) {
        (true, _) => "Đang chọn trên màn hình…".to_string(),
        (false, true) => format!("Chọn {what}"),
        (false, false) => format!("Chọn lại {what}"),
    };
    ui.add(egui::Button::new(RichText::new(format!("{} {label}", ic::CURSOR_CLICK)).size(13.0)).small().selected(picking)).clicked()
}

/// "Độ giống ≥ N%" slider and "Thử khớp" button for a saved picture.
fn threshold_ui(ui: &mut egui::Ui, rec: &mut script::Rec, st: &mut StepsUi) {
    if rec.template.is_empty() {
        return;
    }
    let mut pct = (rec.threshold() * 100.0).round();
    ui.label(RichText::new("độ giống ≥").size(13.0).color(MUTED));
    if ui.add(egui::DragValue::new(&mut pct).range(50.0..=99.0).suffix("%")).on_hover_text("Thấp hơn: dễ khớp hơn (nút có hiệu ứng). Cao hơn: chặt hơn (tránh khớp nhầm nút giống nhau). Mặc định 88%").changed() {
        rec.threshold = Some(pct / 100.0);
        st.changed = true;
    }
    if ui.add(egui::Button::new(RichText::new(format!("{} Thử khớp", ic::CROSSHAIR)).size(13.0)).small()).on_hover_text("Tìm ảnh này trên ảnh màn hình hiện tại").clicked() {
        st.match_request = Some(rec.clone());
    }
}

/// Picks a variable by name.
fn var_combo(ui: &mut egui::Ui, id: impl std::hash::Hash, name: &mut String, st: &mut StepsUi) {
    let shown = if name.is_empty() { "(chọn biến)".to_string() } else { name.clone() };
    egui::ComboBox::from_id_salt(id).selected_text(shown).width(130.0).show_ui(ui, |ui| {
        if st.var_names.is_empty() {
            ui.label(theme::muted("Thêm biến ở thẻ \"Biến\" bên dưới"));
        }
        for v in st.var_names {
            if ui.selectable_label(name == v, v).clicked() {
                *name = v.clone();
                st.changed = true;
            }
        }
    });
}

/// Editor for a condition ("thấy ảnh" / "số trong vùng …").
fn cond_ui(ui: &mut egui::Ui, cond: &mut Cond, st: &mut StepsUi, path: &[usize]) {
    ui.horizontal_wrapped(|ui| match cond {
        Cond::SeeImage { rec, region } => {
            ui.label(RichText::new("thấy ảnh").color(TEXT));
            if !rec.template.is_empty() {
                let rec = rec.clone();
                st.thumb(ui, &rec);
            }
            let picking = st.is_picking(path, Slot::Cond, PickKind::Image);
            if pick_button(ui, picking, rec.template.is_empty(), "ảnh") {
                st.request(path, Slot::Cond, PickKind::Image);
            }
            threshold_ui(ui, rec, st);
            let picking = st.is_picking(path, Slot::Cond, PickKind::Region);
            let label = if region.is_some() { format!("{} Vùng tìm ✓", ic::SELECTION) } else { format!("{} Vùng tìm", ic::SELECTION) };
            if ui.add(egui::Button::new(RichText::new(label).size(13.0)).small().selected(picking || region.is_some())).clicked() {
                st.request(path, Slot::Cond, PickKind::Region);
            }
            if region.is_some() && ui.small_button(ic::X).on_hover_text("Bỏ vùng tìm").clicked() {
                *region = None;
                st.changed = true;
            }
        }
        Cond::Number { region, op, value } => {
            ui.label(RichText::new("số trong vùng").color(TEXT));
            let unset = region[2] <= region[0];
            let picking = st.is_picking(path, Slot::Cond, PickKind::Region);
            if pick_button(ui, picking, unset, "vùng") {
                st.request(path, Slot::Cond, PickKind::Region);
            }
            egui::ComboBox::from_id_salt(("cmp", path.to_vec())).selected_text(op.symbol()).width(46.0).show_ui(ui, |ui| {
                for c in script::Cmp::ALL {
                    if ui.selectable_value(op, c, c.symbol()).changed() {
                        st.changed = true;
                    }
                }
            });
            if ui.add(egui::DragValue::new(value).speed(1.0)).changed() {
                st.changed = true;
            }
        }
        Cond::Var { name, op, value } => {
            ui.label(RichText::new("biến").color(TEXT));
            var_combo(ui, ("cond-var", path.to_vec()), name, st);
            egui::ComboBox::from_id_salt(("cmp-var", path.to_vec())).selected_text(op.symbol()).width(46.0).show_ui(ui, |ui| {
                for c in script::Cmp::ALL {
                    if ui.selectable_value(op, c, c.symbol()).changed() {
                        st.changed = true;
                    }
                }
            });
            if ui.add(egui::DragValue::new(value).speed(1.0)).changed() {
                st.changed = true;
            }
        }
    });
}

/// Description, button picture and settings of a hand-made step.
fn manual_body(ui: &mut egui::Ui, node: &mut Node, st: &mut StepsUi, path: &[usize], otherwise: bool) {
    let slot = if otherwise { Slot::Otherwise } else { Slot::Step };
    let Some(step) = node.step.as_mut() else { return };
    let unset = step.needs_pick();
    ui.horizontal_wrapped(|ui| {
        let color = if unset { YELLOW } else { TEXT };
        let text = match &*step {
            Step::Call { scenario_id, .. } if scenario_id.is_empty() => "Chọn kịch bản con".to_string(),
            s => s.describe(),
        };
        ui.label(RichText::new(format!("{} {text}", step_icon(step))).color(color).family(theme::bold()).size(14.0));
        if let Some(rec) = step.image() {
            let rec = rec.clone();
            st.thumb(ui, &rec);
        }
    });
    ui.horizontal_wrapped(|ui| {
        if let Some(kind) = pick_kind_of(step) {
            if pick_button(ui, st.is_picking(path, slot, kind), unset, "trên màn hình") {
                st.request(path, slot, kind);
            }
        }
        if let Step::TapImage { rec, .. } | Step::WaitImage { rec, .. } = step {
            threshold_ui(ui, rec, st);
        }
        let muted = |t: &str| RichText::new(t).size(13.0).color(MUTED);
        let changed = match step {
            Step::SetVar { name, change } => {
                var_combo(ui, ("setvar", path.to_vec(), otherwise), name, st);
                let kind = match change {
                    script::VarOp::Set { .. } => "=",
                    script::VarOp::Add { .. } => "cộng thêm",
                    script::VarOp::Read { .. } => "= số đọc trong vùng",
                };
                let mut c = false;
                egui::ComboBox::from_id_salt(("varop", path.to_vec(), otherwise)).selected_text(kind).width(150.0).show_ui(ui, |ui| {
                    if ui.selectable_label(matches!(change, script::VarOp::Set { .. }), "=").clicked() {
                        *change = script::VarOp::Set { value: 0.0 };
                        c = true;
                    }
                    if ui.selectable_label(matches!(change, script::VarOp::Add { .. }), "cộng thêm (số âm để trừ)").clicked() {
                        *change = script::VarOp::Add { value: 1.0 };
                        c = true;
                    }
                    if ui.selectable_label(matches!(change, script::VarOp::Read { .. }), "= số đọc trong vùng").clicked() {
                        *change = script::VarOp::Read { region: [0.0; 4] };
                        st.request(path, Slot::VarRegion, PickKind::Region);
                        c = true;
                    }
                });
                match change {
                    script::VarOp::Set { value } | script::VarOp::Add { value } => c |= ui.add(egui::DragValue::new(value).speed(1.0)).changed(),
                    script::VarOp::Read { region } => {
                        if pick_button(ui, st.is_picking(path, Slot::VarRegion, PickKind::Region), region[2] <= region[0], "vùng") {
                            st.request(path, Slot::VarRegion, PickKind::Region);
                        }
                    }
                }
                c
            }
            Step::Wait { seconds } => {
                ui.label(muted("chờ"));
                let c = ui.add(egui::DragValue::new(seconds).range(0.5..=3600.0).speed(0.5)).changed();
                ui.label(muted("giây"));
                c
            }
            Step::TapImage { all: true, max, .. } => {
                ui.label(muted("tối đa"));
                let c = ui.add(egui::DragValue::new(max).range(1..=200)).changed();
                ui.label(muted("lần"));
                c
            }
            Step::WaitImage { timeout_sec, .. } => {
                ui.label(muted("đợi tối đa"));
                let c = ui.add(egui::DragValue::new(timeout_sec).range(1..=3600)).changed();
                ui.label(muted("giây"));
                c
            }
            Step::Swipe { ms, .. } => {
                ui.label(muted("trong"));
                let c = ui.add(egui::DragValue::new(ms).range(50..=5000).speed(10)).changed();
                ui.label(muted("ms"));
                c
            }
            Step::Call { scenario_id, name } => {
                let mut c = false;
                let shown = if name.is_empty() { "(chọn kịch bản)".to_string() } else { name.clone() };
                egui::ComboBox::from_id_salt(("call", path.to_vec(), otherwise)).selected_text(shown).width(240.0).show_ui(ui, |ui| {
                    if st.others.is_empty() {
                        ui.label(theme::muted("Chưa có kịch bản nào khác"));
                    }
                    for (id, n) in st.others {
                        if ui.selectable_label(scenario_id == id, n).clicked() {
                            *scenario_id = id.clone();
                            *name = n.clone();
                            c = true;
                        }
                    }
                });
                c
            }
            _ => false,
        };
        st.changed |= changed;
    });
    if !otherwise {
        ui.add(egui::TextEdit::singleline(&mut node.text).hint_text("ghi chú (không bắt buộc)").margin(Margin::symmetric(8, 4)).desired_width(f32::INFINITY));
    }
}

/// Adds a node to `list` and, if needed, asks to pick something for it right away.
fn push_new(list: &mut Vec<Node>, (node, pick): NewStep, list_path: &[usize], st: &mut StepsUi) {
    list.push(node);
    if let Some((slot, kind)) = pick {
        let mut p = list_path.to_vec();
        p.push(list.len() - 1);
        st.request(&p, slot, kind);
    }
    st.changed = true;
}

/// A loop / if block with its nested lists.
fn block_ui(ui: &mut egui::Ui, node: &mut Node, path: &mut Vec<usize>, st: &mut StepsUi, row_buttons: &mut dyn FnMut(&mut egui::Ui)) {
    let frame = egui::Frame::new().fill(CARD_HI).stroke(egui::Stroke::new(1.0_f32, ACCENT.gamma_multiply(0.5))).corner_radius(10).inner_margin(Margin::same(12));
    frame.show(ui, |ui| match node.step.as_mut() {
        Some(Step::Loop { max, steps, until }) => {
            ui.horizontal(|ui| {
                ui.label(RichText::new(format!("{} Lặp lại tối đa", ic::REPEAT)).color(ACCENT).family(theme::bold()));
                st.changed |= ui.add(egui::DragValue::new(max).range(1..=999)).changed();
                ui.label(RichText::new("lần").color(ACCENT).family(theme::bold()));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| row_buttons(ui));
            });
            ui.label(RichText::new("Tự dừng khi một bước bên trong không tìm thấy nút (vd hết map).").size(12.5).color(MUTED));
            ui.horizontal_wrapped(|ui| {
                match until {
                    Some(c) => {
                        ui.label(RichText::new(format!("{} Dừng sớm khi", ic::FLAG_CHECKERED)).color(ACCENT));
                        cond_ui(ui, c, st, path);
                        if ui.small_button(ic::X).on_hover_text("Bỏ điều kiện dừng").clicked() {
                            *until = None;
                            st.changed = true;
                        }
                    }
                    None => {
                        ui.menu_button(RichText::new(format!("{} Dừng sớm khi…", ic::FLAG_CHECKERED)).size(13.0), |ui| {
                            if ui.button(format!("{} thấy một ảnh", ic::IMAGE_SQUARE)).clicked() {
                                *until = Some(Cond::new_image());
                                st.request(path, Slot::Cond, PickKind::Image);
                                st.changed = true;
                                ui.close();
                            }
                            if ui.button(format!("{} số trong vùng …", ic::HASH)).clicked() {
                                *until = Some(Cond::new_number());
                                st.request(path, Slot::Cond, PickKind::Region);
                                st.changed = true;
                                ui.close();
                            }
                            if ui.button(format!("{} biến …", ic::BRACKETS_CURLY)).clicked() {
                                *until = Some(Cond::new_var());
                                st.changed = true;
                                ui.close();
                            }
                        });
                    }
                }
            });
            ui.add_space(4.0);
            path.push(0);
            nodes_ui(ui, steps, path, st);
            if let Some(new) = add_step_menu(ui, format!("{} Thêm bước vào vòng lặp", ic::PLUS)) {
                let list_path = path.clone();
                push_new(steps, new, &list_path, st);
            }
            path.pop();
        }
        Some(Step::If { cond, then_steps, else_steps }) => {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(format!("{} Nếu", ic::GIT_BRANCH)).color(ACCENT).family(theme::bold()));
                cond_ui(ui, cond, st, path);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| row_buttons(ui));
            });
            for (branch, (title, list)) in [("thì", then_steps), ("không thì", else_steps)].into_iter().enumerate() {
                ui.add_space(4.0);
                ui.label(RichText::new(title).color(ACCENT).family(theme::bold()).size(13.5));
                path.push(branch);
                if list.is_empty() {
                    ui.label(RichText::new("(không làm gì)").size(12.5).color(MUTED));
                }
                nodes_ui(ui, list, path, st);
                if let Some(new) = add_step_menu(ui, format!("{} Thêm bước", ic::PLUS)) {
                    let list_path = path.clone();
                    push_new(list, new, &list_path, st);
                }
                path.pop();
            }
        }
        _ => {}
    });
}

/// Editable list of steps; loops and ifs render as nested blocks.
fn nodes_ui(ui: &mut egui::Ui, nodes: &mut Vec<Node>, path: &mut Vec<usize>, st: &mut StepsUi) {
    let mut action = None;
    let len = nodes.len();
    let in_block = !path.is_empty();
    for (i, node) in nodes.iter_mut().enumerate() {
        path.push(i);
        let mut row_buttons = |ui: &mut egui::Ui| {
            if ui.small_button(ic::TRASH).on_hover_text("Xóa").clicked() {
                action = Some(NodeAction::Remove(i));
            }
            if ui.add_enabled(i + 1 < len, egui::Button::new(ic::ARROW_DOWN).small()).on_hover_text("Chuyển xuống").clicked() {
                action = Some(NodeAction::Down(i));
            }
            if ui.add_enabled(i > 0, egui::Button::new(ic::ARROW_UP).small()).on_hover_text("Chuyển lên").clicked() {
                action = Some(NodeAction::Up(i));
            }
        };

        if node.is_block() {
            block_ui(ui, node, path, st, &mut row_buttons);
            ui.add_space(4.0);
            path.pop();
            continue;
        }

        st.counter += 1;
        let number = st.counter;
        ui.horizontal_top(|ui| {
            let (rect, _) = ui.allocate_exact_size(vec2(26.0, 38.0), egui::Sense::hover());
            let done = node.step.as_ref().is_some_and(|s| !s.needs_pick());
            let fill = if st.breakpoint == Some(&*path) { RED } else if done { ACCENT.gamma_multiply(0.35) } else { BORDER };
            ui.painter().circle_filled(rect.center(), 12.0, fill);
            ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, number.to_string(), egui::FontId::new(13.0, theme::bold()), TEXT);
            // Buttons first (right to left) so the content takes exactly what is left.
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                row_buttons(ui);
                ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
                    if node.manual {
                        manual_body(ui, node, st, path, false);
                    } else {
                        let hint = if in_block { "vd: chọn ải mới nhất" } else { "vd: bấm xuất chiến góc phải dưới" };
                        if step_text_box(ui, ("step-box", number), &mut node.text, hint) {
                            node.step = None;
                            st.changed = true;
                        }
                    }
                });
            });
        });
        if !node.manual {
            interpretation(ui, node, 34.0);
        }

        // Extras: test, search area, "nếu không thấy thì…".
        let searches = !node.manual || matches!(node.step, Some(Step::TapImage { .. } | Step::WaitImage { .. }));
        let can_fallback = !node.manual || matches!(node.step, Some(Step::TapImage { all: false, .. }));
        let testable = !matches!(node.step, Some(Step::StopRun));
        ui.horizontal_wrapped(|ui| {
            ui.add_space(30.0);
            let small = |t: String| egui::Button::new(RichText::new(t).size(13.0)).small();
            if testable && ui.add(small(format!("{} Thử bước này", ic::PLAY))).on_hover_text("Chạy riêng bước này trên giả lập ngay bây giờ").clicked() {
                st.test_request = Some(node.clone());
            }
            if ui.add(small(format!("{} Chạy từ đây", ic::SKIP_FORWARD))).on_hover_text("Chạy kịch bản bắt đầu từ bước này (bỏ qua các bước trước)").clicked() {
                st.run_from = Some(path.clone());
            }
            let is_bp = st.breakpoint == Some(&*path);
            let bp = small(format!("{} Dừng trước bước này", ic::PAUSE_CIRCLE)).selected(is_bp);
            if ui.add(bp).on_hover_text("Chạy thử sẽ dừng lại ngay trước bước này (bấm lần nữa để bỏ)").clicked() {
                st.toggle_breakpoint = Some(path.clone());
            }
            if searches {
                let picking_this = st.is_picking(path, Slot::Step, PickKind::Region);
                let label = match (picking_this, node.region.is_some()) {
                    (true, _) => format!("{} Đang chọn vùng…", ic::SELECTION),
                    (false, true) => format!("{} Vùng tìm ✓", ic::SELECTION),
                    (false, false) => format!("{} Vùng tìm", ic::SELECTION),
                };
                let tip = "Kéo chuột trên ảnh màn hình để chỉ tìm trong vùng đó (tránh nhầm với chỗ khác)";
                if ui.add(small(label).selected(picking_this || node.region.is_some())).on_hover_text(tip).clicked() {
                    st.request(path, Slot::Step, PickKind::Region);
                }
                if node.region.is_some() && ui.small_button(ic::X).on_hover_text("Bỏ vùng tìm (tìm cả màn hình)").clicked() {
                    node.region = None;
                    st.changed = true;
                }
            }
            if can_fallback && node.otherwise.is_none() {
                let tip = "Làm một việc khác khi không tìm thấy (vd bấm mũi tên sang trang) rồi tìm lại";
                if ui.add(small(format!("{} Nếu không thấy thì…", ic::ARROW_ELBOW_DOWN_RIGHT))).on_hover_text(tip).clicked() {
                    node.otherwise = Some(Box::new(Node::new()));
                    st.changed = true;
                }
            }
        });

        let mut remove_otherwise = false;
        let mut replace_otherwise: Option<NewStep> = None;
        if let Some(other) = node.otherwise.as_deref_mut() {
            ui.horizontal_top(|ui| {
                ui.add_space(30.0);
                let width = ui.available_width() - 20.0; // minus the frame's inner margin
                egui::Frame::new().fill(theme::INPUT.gamma_multiply(0.6)).corner_radius(8).inner_margin(Margin::same(10)).show(ui, |ui| {
                    // The parent row is horizontal; lay the block out top to bottom.
                    ui.vertical(|ui| {
                        ui.set_width(width);
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(format!("{} Nếu không thấy thì", ic::ARROW_ELBOW_DOWN_RIGHT)).color(ACCENT).family(theme::bold()).size(13.5));
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if ui.small_button(ic::X).on_hover_text("Bỏ").clicked() {
                                    remove_otherwise = true;
                                }
                                replace_otherwise = add_step_menu(ui, format!("{} Đổi cách", ic::SWAP));
                            });
                        });
                        if other.manual {
                            manual_body(ui, other, st, path, true);
                        } else {
                            let hint = "vd: bấm mũi tên màu xanh góc phải để sang trang";
                            if step_text_box(ui, ("otherwise-box", number), &mut other.text, hint) {
                                other.step = None;
                                st.changed = true;
                            }
                            interpretation(ui, other, 0.0);
                        }
                        ui.horizontal(|ui| {
                            ui.label(RichText::new("rồi tìm lại, tối đa").size(13.0).color(MUTED));
                            st.changed |= ui.add(egui::DragValue::new(&mut node.otherwise_max).range(1..=99)).changed();
                            ui.label(RichText::new("lần").size(13.0).color(MUTED));
                        });
                    });
                });
            });
        }
        if remove_otherwise {
            node.otherwise = None;
            st.changed = true;
        }
        if let Some((new, pick)) = replace_otherwise {
            if new.is_block() {
                // Blocks can't be a fallback; keep the existing one.
            } else {
                node.otherwise = Some(Box::new(new));
                if let Some((_, kind)) = pick {
                    st.request(path, Slot::Otherwise, kind);
                }
                st.changed = true;
            }
        }
        ui.add_space(8.0);
        path.pop();
    }
    match action {
        Some(NodeAction::Up(i)) => nodes.swap(i, i - 1),
        Some(NodeAction::Down(i)) => nodes.swap(i, i + 1),
        Some(NodeAction::Remove(i)) => {
            nodes.remove(i);
        }
        None => return,
    }
    st.changed = true;
}

/// Name of the chosen emulators, e.g. "LDPlayer, Acc 2" or "3 giả lập".
fn targets_label(sc: &Scenario, instances: &[Instance]) -> String {
    let names: Vec<String> = sc
        .targets()
        .iter()
        .map(|i| instances.iter().find(|x| x.index == *i).map(|x| x.name.clone()).unwrap_or_else(|| format!("#{i}")))
        .collect();
    match names.len() {
        0 => "(chọn giả lập)".into(),
        1 | 2 => names.join(", "),
        n => format!("{n} giả lập"),
    }
}

/// Checkbox list of emulators; returns true when the choice changed.
fn targets_menu(ui: &mut egui::Ui, id: &str, sc: &mut Scenario, instances: &[Instance]) -> bool {
    let mut changed = false;
    let label = format!("{}  {}  {}", ic::DEVICE_MOBILE, targets_label(sc, instances), ic::CARET_DOWN);
    ui.push_id(id, |ui| {
        ui.menu_button(label, |ui| {
            ui.label(theme::muted("Chạy cùng lúc trên các giả lập:"));
            let mut chosen = sc.targets();
            for inst in instances {
                let mut on = chosen.contains(&inst.index);
                let text = format!("{}{}", inst.name, if inst.running { "  ● đang mở" } else { "" });
                if ui.checkbox(&mut on, text).changed() {
                    if on {
                        chosen.push(inst.index);
                    } else {
                        chosen.retain(|i| *i != inst.index);
                    }
                    chosen.sort();
                    sc.instances = chosen.clone();
                    if let Some(first) = chosen.first() {
                        sc.instance_index = *first;
                    }
                    changed = true;
                }
            }
        });
    });
    changed
}

fn scenario_card(ui: &mut egui::Ui, sc: &mut Scenario, jobs: &[&Job], instances: &[Instance], selected: bool, actions: &mut Vec<RowAction>) {
    let running = !jobs.is_empty();
    let border = if selected { ACCENT } else { BORDER };
    let resp = theme::card_frame(border).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.label(RichText::new(&sc.name).family(theme::bold()).size(17.0));
                ui.horizontal(|ui| {
                    let icon = match sc.schedule {
                        Schedule::Daily { .. } => ic::CLOCK,
                        Schedule::Once => ic::PLAY_CIRCLE,
                        Schedule::Repeat { .. } => ic::REPEAT,
                    };
                    theme::chip(ui, format!("{icon} {}", sc.schedule.describe()), ACCENT);
                    theme::chip(ui, format!("{} bước", script::leaf_count(&sc.steps)), MUTED);
                    if !sc.recorded {
                        theme::chip(ui, "Chưa chạy thử", YELLOW);
                    }
                    let n = script::dynamic_count(&sc.steps);
                    if n > 0 {
                        theme::chip(ui, format!("{} AI {n} bước", ic::LIGHTNING), YELLOW);
                    }
                    if !sc.popups.is_empty() {
                        theme::chip(ui, format!("{} {} popup", ic::SHIELD_CHECK, sc.popups.len()), MUTED);
                    }
                    if sc.recovery.home.is_some() || !sc.recovery.package.is_empty() {
                        theme::chip(ui, format!("{} tự phục hồi", ic::FIRST_AID), MUTED);
                    }
                });
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if running {
                    if ui.add(theme::button(format!("{} Dừng", ic::STOP), RED)).clicked() {
                        actions.push(RowAction::Stop(sc.id.clone()));
                    }
                } else {
                    ui.menu_button(ic::DOTS_THREE_VERTICAL, |ui| {
                        if ui.button(format!("{} Sửa", ic::PENCIL_SIMPLE)).clicked() {
                            actions.push(RowAction::Edit(sc.id.clone()));
                            ui.close();
                        }
                        if ui.add_enabled(sc.recorded, egui::Button::new(format!("{} Xuất file chia sẻ", ic::UPLOAD_SIMPLE))).clicked() {
                            actions.push(RowAction::Export(sc.id.clone()));
                            ui.close();
                        }
                        if ui.button(RichText::new(format!("{} Xóa", ic::TRASH)).color(RED)).clicked() {
                            actions.push(RowAction::Delete(sc.id.clone()));
                            ui.close();
                        }
                    });
                    if ui.add(theme::button(format!("{} Chạy", ic::PLAY), ACCENT)).clicked() {
                        actions.push(RowAction::Start(sc.id.clone()));
                    }
                }
                ui.add_space(8.0);
                ui.add_enabled_ui(!running, |ui| {
                    if targets_menu(ui, &sc.id.clone(), sc, instances) {
                        actions.push(RowAction::Save(sc.id.clone()));
                    }
                });
                ui.add_space(8.0);
                match jobs {
                    [] => {
                        ui.label(theme::muted("Đang tắt"));
                        theme::status_dot(ui, MUTED);
                    }
                    [j] => {
                        ui.label(RichText::new(&j.status).color(GREEN));
                        theme::status_dot(ui, GREEN);
                    }
                    many => {
                        ui.label(RichText::new(format!("Đang chạy trên {} giả lập", many.len())).color(GREEN));
                        theme::status_dot(ui, GREEN);
                    }
                }
            });
        });
    });
    if resp.response.interact(egui::Sense::click()).clicked() {
        actions.push(RowAction::Select(sc.id.clone()));
    }
}

fn sanitize_file_name(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| if r#"\/:*?"<>|"#.contains(c) { '_' } else { c })
        .collect();
    if s.trim().is_empty() {
        "kich-ban".into()
    } else {
        s.trim().to_string()
    }
}

fn log_ui(ui: &mut egui::Ui, log: &[String]) {
    egui::Frame::new()
        .fill(theme::INPUT)
        .corner_radius(8)
        .inner_margin(Margin::same(10))
        .show(ui, |ui| {
            egui::ScrollArea::vertical()
                .id_salt("log")
                .stick_to_bottom(true)
                .auto_shrink([false, false])
                .max_height(260.0)
                .show(ui, |ui| {
                    if log.is_empty() {
                        ui.label(theme::muted("(chưa có gì)"));
                    }
                    for line in log {
                        ui.label(RichText::new(line).monospace().size(12.5).color(TEXT));
                    }
                });
        });
}

impl App {
    fn settings_window(&mut self, ctx: &egui::Context) {
        let mut open = self.show_settings;
        let mut changed = false;
        egui::Window::new(format!("{} Cài đặt", ic::GEAR)).open(&mut open).collapsible(false).resizable(false).default_width(460.0).show(ctx, |ui| {
            let mut auto = self.autostart;
            if ui.checkbox(&mut auto, "Khởi động cùng Windows (mở ẩn dưới khay hệ thống)").changed() {
                match crate::desktop::set_autostart(auto) {
                    Ok(()) => self.autostart = auto,
                    Err(e) => self.message = Some((format!("Không đổi được: {e:#}"), YELLOW)),
                }
            }
            ui.label(theme::muted("Kịch bản nào bật \"Tự chạy khi mở app\" sẽ tự bắt đầu khi máy khởi động."));
            ui.add_space(6.0);
            changed |= ui.checkbox(&mut self.settings.close_to_tray, "Bấm ✕ thì ẩn xuống khay hệ thống (kịch bản vẫn chạy)").changed();
            changed |= ui.checkbox(&mut self.settings.hotkey, format!("Phím {} dừng tất cả, bấm ở đâu cũng được", crate::desktop::HOTKEY_LABEL)).changed();
            ui.label(theme::muted("Đổi phím tắt có tác dụng sau khi mở lại app."));
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.label("Tối đa giả lập chạy cùng lúc");
                changed |= ui.add(egui::DragValue::new(&mut self.settings.max_parallel).range(0..=32)).changed();
            });
            ui.label(theme::muted(format!(
                "0 = tự động: máy có {}GB RAM nên chạy tối đa {} giả lập (khoảng 4GB mỗi cái). Giả lập vượt quá sẽ chờ đến lượt.",
                device::total_ram_gb(),
                ((device::total_ram_gb() / 4) as usize).max(1)
            )));
            ui.add_space(6.0);
            ui.label(theme::muted("Máy sẽ không tự ngủ khi đang có kịch bản chạy."));
        });
        self.show_settings = open;
        if changed {
            self.settings.save();
            jobs::gate::set_max_parallel(effective_parallel(&self.settings));
        }
    }

    fn emulators_window(&mut self, ctx: &egui::Context) {
        let mut open = self.show_emulators;
        let mut refresh = false;
        egui::Window::new(format!("{} Giả lập", ic::DEVICE_MOBILE)).open(&mut open).collapsible(false).default_width(520.0).show(ctx, |ui| {
            let Some(ld) = self.settings.ld_dir.clone() else {
                ui.label("Chưa tìm thấy LDPlayer.");
                return;
            };
            egui::Grid::new("emulators").num_columns(4).spacing([14.0, 8.0]).striped(true).show(ui, |ui| {
                for inst in &self.instances {
                    let (dot, color) = if inst.running { ("●", GREEN) } else { ("○", MUTED) };
                    ui.label(RichText::new(dot).color(color));
                    ui.label(&inst.name);
                    ui.label(theme::muted(format!("{}x{}", inst.size[0], inst.size[1])));
                    let busy = self.jobs.iter().any(|j| j.instance_index == inst.index && !j.is_finished());
                    if inst.running {
                        let btn = ui.add_enabled(!busy, egui::Button::new(format!("{} Tắt", ic::POWER)));
                        if btn.on_disabled_hover_text("Đang có kịch bản chạy trên giả lập này").clicked() {
                            let _ = device::quit_instance(&ld, inst.index);
                            refresh = true;
                        }
                    } else if ui.button(format!("{} Mở", ic::PLAY)).clicked() {
                        let _ = device::launch_instance(&ld, inst.index);
                        refresh = true;
                    }
                    ui.end_row();
                }
            });
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui.button(format!("{} Sắp xếp cửa sổ", ic::SQUARES_FOUR)).on_hover_text("Xếp các cửa sổ giả lập đang mở cạnh nhau").clicked() {
                    let _ = device::sort_windows(&ld);
                }
                if ui.button(format!("{} Làm mới", ic::ARROWS_CLOCKWISE)).clicked() {
                    refresh = true;
                }
            });
            ui.label(theme::muted(format!(
                "Chạy cùng lúc tối đa {} giả lập (đổi trong Cài đặt). Mỗi kịch bản có thể tự tắt giả lập khi xong.",
                effective_parallel(&self.settings)
            )));
        });
        self.show_emulators = open;
        if refresh {
            // ldconsole needs a moment before the new state shows up.
            std::thread::sleep(Duration::from_millis(600));
            self.refresh_instances();
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Closing the window keeps the app (and the runs) going in the tray.
        if ctx.input(|i| i.viewport().close_requested())
            && self.settings.close_to_tray
            && !crate::desktop::QUITTING.load(Ordering::Relaxed)
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            crate::desktop::hide_window(ctx);
            if !self.hinted_tray {
                self.hinted_tray = true;
                self.info("OmniPilot vẫn chạy dưới khay hệ thống. Chuột phải vào icon để mở lại hoặc thoát hẳn.");
            }
        }
        match self.page {
            Page::Setup => self.setup_ui(ctx),
            Page::Main if self.editor.is_some() => self.editor_ui(ctx),
            Page::Main => self.main_ui(ctx),
        }
        if self.show_settings {
            self.settings_window(ctx);
        }
        if self.show_emulators {
            self.emulators_window(ctx);
        }
        // Workers report through channels; keep polling while anything is active.
        ctx.request_repaint_after(Duration::from_millis(250));
    }
}
