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
use crate::script::{self, Node, Scenario, Schedule, Step};
use crate::theme::{self, ACCENT, BORDER, CARD_HI, GREEN, MUTED, PANEL, RED, TEXT, YELLOW};
use crate::{paths, setup};

#[derive(Serialize, Deserialize, Default)]
struct Settings {
    ld_dir: Option<PathBuf>,
}

impl Settings {
    fn load() -> Self {
        std::fs::read_to_string(paths::settings_file()).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
    }
    fn save(&self) {
        let _ = std::fs::create_dir_all(paths::data_dir());
        let _ = std::fs::write(paths::settings_file(), serde_json::to_string_pretty(self).unwrap_or_default());
    }
}

/// Shows the latest screenshot with a marker where the last tap happened.
#[derive(Default)]
struct ScreenView {
    tex: Option<TextureHandle>,
    size: [u32; 2],
    tap: Option<(u32, u32)>,
    shown_gen: u64,
}

impl ScreenView {
    fn set(&mut self, ctx: &egui::Context, img: &RgbaImage, tap: Option<(u32, u32)>) {
        let color = egui::ColorImage::from_rgba_unmultiplied([img.width() as usize, img.height() as usize], img.as_raw());
        match &mut self.tex {
            Some(t) => t.set(color, egui::TextureOptions::LINEAR),
            None => self.tex = Some(ctx.load_texture("screen", color, egui::TextureOptions::LINEAR)),
        }
        self.size = [img.width(), img.height()];
        self.tap = tap;
    }

    fn ui(&self, ui: &mut egui::Ui) {
        let w = ui.available_width();
        let Some(tex) = &self.tex else {
            let (rect, _) = ui.allocate_exact_size(vec2(w, w * 9.0 / 16.0), egui::Sense::hover());
            ui.painter().rect_filled(rect, 10, theme::INPUT);
            ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, format!("{}  Chưa có ảnh màn hình", ic::DEVICE_MOBILE_SPEAKER), egui::FontId::proportional(15.0), MUTED);
            return;
        };
        let size = vec2(w, w * self.size[1] as f32 / self.size[0] as f32);
        let resp = ui.add(egui::Image::new((tex.id(), size)).corner_radius(10));
        if let Some((x, y)) = self.tap {
            let p = resp.rect.min + vec2(x as f32 / self.size[0] as f32 * size.x, y as f32 / self.size[1] as f32 * size.y);
            ui.painter().circle_stroke(p, 13.0, egui::Stroke::new(3.0_f32, RED));
            ui.painter().circle_filled(p, 3.5, RED);
        }
    }
}

enum TaskMsg {
    Event(Event),
    Compiled(Result<Vec<Node>, String>),
    /// Learned steps and the screen resolution they were recorded at.
    TrialDone(Result<(Vec<Node>, Option<[u32; 2]>), String>),
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
            let r = compile_nodes(&ai, &mut nodes, &log).map(|_| nodes).map_err(|e| format!("{e:#}"));
            let _ = tx.send(TaskMsg::Compiled(r));
        });
        self.task = Some(Task { rx, stop: Arc::new(AtomicBool::new(false)), label: "AI đang đọc các bước...", analyse_only: true });
    }

    fn start_trial(&mut self, ld: PathBuf, inst: Instance) {
        self.error = None;
        self.trial = None;
        let (tx, rx) = channel();
        let stop = Arc::new(AtomicBool::new(false));
        let (ai, dir, stop2) = (self.ai.clone(), self.sc.dir(), stop.clone());
        let mut nodes = self.sc.steps.clone();
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
                let mut runner = executor::new_runner(&ld, inst, dir, ai, true, stop2, etx);
                runner.ensure_device()?;
                runner.run(&mut nodes)?;
                Ok((nodes, runner.screen_size))
            })();
            let _ = tx.send(TaskMsg::TrialDone(r.map_err(|e| format!("{e:#}"))));
        });
        self.task = Some(Task { rx, stop, label: "Đang chạy thử...", analyse_only: false });
    }

    fn poll(&mut self, ctx: &egui::Context) {
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
                TaskMsg::TrialDone(r) => {
                    finished = true;
                    match r {
                        Ok((nodes, size)) => {
                            jobs::push_log(&mut self.log, "Chạy thử xong. Kiểm tra lại game xem đã đúng chưa.".into());
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

/// Removes empty rows and empty loops.
fn prune_empty(nodes: &mut Vec<Node>) {
    nodes.retain_mut(|n| match &mut n.step {
        Some(Step::Loop { steps, .. }) => {
            prune_empty(steps);
            !steps.is_empty()
        }
        _ => !n.text.trim().is_empty(),
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
        match server.compile_step(&n.text) {
            Ok(s) => {
                log(format!("AI hiểu \"{}\" → {}", n.text, s.describe()));
                n.step = Some(s);
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
    editor: Option<Editor>,
    confirm_delete: Option<String>,
    message: Option<(String, Color32)>,
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
        if settings.ld_dir.as_ref().is_none_or(|d| !device::is_ldplayer_dir(d)) {
            settings.ld_dir = device::find_ldplayer();
            settings.save();
        }
        let ld_text = settings.ld_dir.as_ref().map(|d| d.display().to_string()).unwrap_or_default();
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
            editor: None,
            confirm_delete: None,
            message: None,
        };
        app.refresh_instances();
        if std::env::args().any(|a| a == "--demo") {
            app.editor = Some(Editor::new(demo_scenario(&app.instances), true));
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
            None => self.inst_error = Some("Không tìm thấy LDPlayer. Nhập thư mục cài LDPlayer (chứa ldconsole.exe).".into()),
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
                        ui.label(theme::muted("Đang tải AI chạy trên máy (~3,3GB). Chỉ tải một lần duy nhất."));
                        ui.add_space(14.0);
                        if let Some(err) = &p.error {
                            ui.colored_label(RED, format!("Lỗi: {err}"));
                            ui.add_space(6.0);
                            if ui.add(theme::button(format!("{} Thử lại", ic::ARROWS_CLOCKWISE), ACCENT)).clicked() {
                                *self.setup.lock().unwrap() = setup::Progress::default();
                                setup::start(self.setup.clone());
                            }
                            return;
                        }
                        ui.label(format!("Bước {}/{} · {}", p.step, p.total_steps.max(1), p.label));
                        let frac = if p.size > 0 { p.downloaded as f32 / p.size as f32 } else { 0.0 };
                        ui.add(
                            egui::ProgressBar::new(frac)
                                .desired_width(440.0)
                                .fill(ACCENT)
                                .corner_radius(6)
                                .text(format!("{:.0} / {:.0} MB", p.downloaded as f64 / 1e6, p.size as f64 / 1e6)),
                        );
                    });
                });
            });
        });
    }

    fn header(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("top").frame(egui::Frame::new().fill(PANEL).inner_margin(Margin::symmetric(20, 14))).show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new(ic::GAME_CONTROLLER).size(26.0).color(ACCENT));
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    ui.label(RichText::new("OmniPilot").family(theme::bold()).size(20.0));
                    ui.label(RichText::new("Auto mọi game trên LDPlayer").size(12.5).color(MUTED));
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.add(theme::button(format!("{} Tạo kịch bản", ic::PLUS), ACCENT)).clicked() {
                        let mut sc = Scenario::new();
                        if let Some(i) = self.instances.iter().find(|i| i.running).or(self.instances.first()) {
                            sc.instance_index = i.index;
                        }
                        self.editor = Some(Editor::new(sc, true));
                    }
                    if ui.button(format!("{} Nhập", ic::DOWNLOAD_SIMPLE)).on_hover_text("Mở file .omnipilot người khác chia sẻ").clicked() {
                        self.import_dialog();
                    }
                    if ui.button(ic::ARROWS_CLOCKWISE).on_hover_text("Làm mới danh sách giả lập").clicked() {
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
                    ui.add(egui::TextEdit::singleline(&mut self.ld_text).hint_text("D:\\LDPlayer\\LDPlayer9").margin(theme::INPUT_MARGIN).desired_width(260.0));
                    if ui.button("Lưu").clicked() {
                        let dir = PathBuf::from(self.ld_text.trim());
                        if device::is_ldplayer_dir(&dir) {
                            self.settings.ld_dir = Some(dir);
                            self.settings.save();
                            self.refresh_instances();
                        } else {
                            self.inst_error = Some("Thư mục này không có ldconsole.exe và adb.exe".into());
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
                .frame(egui::Frame::new().fill(PANEL).inner_margin(Margin::same(16)))
                .show(ctx, |ui| self.detail_ui(ui, ctx));
        }

        let mut actions = Vec::new();
        egui::CentralPanel::default().frame(egui::Frame::new().fill(theme::BG).inner_margin(Margin::same(20))).show(ctx, |ui| {
            self.banners(ui);
            if self.scenarios.is_empty() {
                ui.add_space(60.0);
                ui.vertical_centered(|ui| {
                    ui.label(RichText::new(ic::LIST_CHECKS).size(48.0).color(MUTED));
                    ui.label(theme::title("Chưa có kịch bản nào"));
                    ui.label(theme::muted("Bấm \"Tạo kịch bản\" rồi nhập từng bước bằng tiếng Việt."));
                });
                return;
            }
            ui.label(theme::title(format!("Kịch bản ({})", self.scenarios.len())));
            ui.add_space(4.0);
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                let instances = &self.instances;
                for sc in &mut self.scenarios {
                    let job = self.jobs.iter().find(|j| j.scenario_id == sc.id && !j.is_finished());
                    let selected = self.selected.as_deref() == Some(sc.id.as_str());
                    scenario_card(ui, sc, job, instances, selected, &mut actions);
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
                self.selected = if self.selected.as_deref() == Some(id.as_str()) { None } else { Some(id) };
                self.job_screen = ScreenView::default();
            }
            RowAction::Save(id) => {
                if let Some(sc) = self.scenarios.iter().find(|s| s.id == id) {
                    let _ = sc.save();
                }
            }
            RowAction::Start(id) => {
                let Some(sc) = self.scenarios.iter().find(|s| s.id == id).cloned() else { return };
                let Some(ld_dir) = self.settings.ld_dir.clone() else {
                    self.warn("Chưa cấu hình LDPlayer");
                    return;
                };
                let Some(inst) = self.instances.iter().find(|i| i.index == sc.instance_index).cloned() else {
                    self.warn("Chọn giả lập cho kịch bản trước");
                    return;
                };
                if self.jobs.iter().any(|j| j.instance_index == inst.index && !j.is_finished()) {
                    self.warn(format!("Giả lập \"{}\" đang chạy kịch bản khác", inst.name));
                    return;
                }
                if !sc.recorded {
                    self.warn("Kịch bản chưa chạy thử nên lần chạy này sẽ dùng AI (chậm hơn).");
                } else if let Some(w) = size_warning(&sc, &inst) {
                    self.warn(w);
                }
                self.jobs.retain(|j| !j.is_finished());
                self.jobs.push(jobs::start(sc, ld_dir, inst));
                self.selected = Some(id);
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
                let Some(sc) = self.scenarios.iter().find(|s| s.id == id) else { return };
                let file_name = format!("{}.omnipilot", sanitize_file_name(&sc.name));
                let Some(path) = rfd::FileDialog::new().add_filter("Kịch bản OmniPilot", &["omnipilot"]).set_file_name(file_name).save_file() else {
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
        let Some(path) = rfd::FileDialog::new().add_filter("Kịch bản OmniPilot", &["omnipilot"]).pick_file() else { return };
        match Scenario::import(&path) {
            Ok(mut sc) => {
                // Point it at a local emulator; the sharer's instance index means nothing here.
                if let Some(i) = self.instances.iter().find(|i| i.running).or(self.instances.first()) {
                    sc.instance_index = i.index;
                    let _ = sc.save();
                }
                match self.instances.iter().find(|i| i.index == sc.instance_index).and_then(|i| size_warning(&sc, i)) {
                    Some(w) => self.warn(format!("Đã nhập \"{}\". {w}", sc.name)),
                    None => self.info(format!("Đã nhập \"{}\". Chọn giả lập rồi bấm Chạy.", sc.name)),
                }
                self.selected = Some(sc.id.clone());
                self.scenarios.push(sc);
                self.scenarios.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
            }
            Err(e) => self.warn(format!("Nhập thất bại: {e:#}")),
        }
    }

    fn delete_dialog(&mut self, ctx: &egui::Context) {
        let Some(id) = self.confirm_delete.clone() else { return };
        let name = self.scenarios.iter().find(|s| s.id == id).map(|s| s.name.clone()).unwrap_or_default();
        egui::Window::new("Xóa kịch bản").collapsible(false).resizable(false).anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0]).show(ctx, |ui| {
            ui.label(format!("Xóa kịch bản \"{name}\"? Không thể hoàn tác."));
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui.add(theme::button(format!("{} Xóa", ic::TRASH), RED)).clicked() {
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
        let Some(id) = self.selected.clone() else { return };
        let name = self.scenarios.iter().find(|s| s.id == id).map(|s| s.name.clone()).unwrap_or_default();
        ui.horizontal(|ui| {
            ui.label(theme::title(format!("{} {name}", ic::TERMINAL_WINDOW)));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button(ic::X).on_hover_text("Đóng").clicked() {
                    self.selected = None;
                }
            });
        });
        let Some(job) = self.jobs.iter().rev().find(|j| j.scenario_id == id) else {
            ui.label(theme::muted("Kịch bản này chưa chạy lần nào kể từ khi mở app. Bấm Chạy để bắt đầu."));
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
            self.job_screen.ui(&mut cols[1]);
        });
    }

    fn editor_ui(&mut self, ctx: &egui::Context) {
        let mut close: Option<bool> = None; // Some(saved)
        let instances = self.instances.clone();
        let ld_dir = self.settings.ld_dir.clone();
        let ed = self.editor.as_mut().unwrap();
        ed.poll(ctx);
        let busy = ed.task.is_some();

        egui::TopBottomPanel::top("ed_top").frame(egui::Frame::new().fill(PANEL).inner_margin(Margin::symmetric(20, 14))).show(ctx, |ui| {
            ui.horizontal(|ui| {
                if ui.add_enabled(!busy, egui::Button::new(format!("{} Quay lại", ic::ARROW_LEFT))).clicked() {
                    close = Some(false);
                }
                ui.add_space(8.0);
                ui.label(RichText::new(if ed.is_new { "Tạo kịch bản" } else { "Sửa kịch bản" }).family(theme::bold()).size(20.0));
            });
        });

        // Action bar.
        egui::TopBottomPanel::bottom("ed_actions").frame(egui::Frame::new().fill(PANEL).inner_margin(Margin::symmetric(20, 12))).show(ctx, |ui| {
            ui.horizontal(|ui| {
                let inst = instances.iter().find(|i| i.index == ed.sc.instance_index).cloned();
                let has_steps = script::leaf_count(&ed.sc.steps) > 0 && ed.sc.steps.iter().any(|n| n.is_loop() || !n.text.trim().is_empty());
                if let Some(t) = &ed.task {
                    if ui.add(theme::button(format!("{} Dừng", ic::STOP), RED)).clicked() {
                        t.stop.store(true, Ordering::Relaxed);
                    }
                    ui.spinner();
                    ui.label(t.label);
                } else {
                    let can_trial = has_steps && ld_dir.is_some() && inst.is_some();
                    let btn = ui.add_enabled(can_trial, theme::button(format!("{} Chạy thử", ic::PLAY), ACCENT));
                    let btn = if inst.is_none() { btn.on_disabled_hover_text("Chọn giả lập trước") } else { btn };
                    if btn.clicked() {
                        ed.start_trial(ld_dir.clone().unwrap(), inst.clone().unwrap());
                    }
                    if ui.add_enabled(has_steps, egui::Button::new(format!("{} Xem AI hiểu thế nào", ic::BRAIN))).on_hover_text("AI đọc các bước nhưng chưa bấm gì vào game").clicked() {
                        ed.start_analyse();
                    }
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let ok = matches!(ed.trial, Some(Ok(())));
                    let named = !ed.sc.name.trim().is_empty();
                    let can_save = ok && named && !busy;
                    let fill = if can_save { Color32::from_rgb(0x16, 0xa3, 0x6f) } else { CARD_HI };
                    let save = ui.add_enabled(can_save, theme::button(format!("{} Lưu kịch bản", ic::CHECK_CIRCLE), fill));
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
                            let text = instances.iter().find(|i| i.index == ed.sc.instance_index).map(|i| i.name.clone()).unwrap_or("(chọn giả lập)".into());
                            egui::ComboBox::from_id_salt("ed_inst").selected_text(format!("{}  {text}", ic::DEVICE_MOBILE)).width(280.0).show_ui(ui, |ui| {
                                for inst in &instances {
                                    let label = format!("{}{}", inst.name, if inst.running { "  ● đang mở" } else { "" });
                                    ui.selectable_value(&mut ed.sc.instance_index, inst.index, label);
                                }
                            });
                            ui.add_space(4.0);
                            ui.label(theme::muted("Kiểu chạy"));
                            schedule_ui(ui, ed);
                        });
                        ui.add_space(12.0);
                        theme::card(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.label(theme::title(format!("{} Các bước", ic::LIST_NUMBERS)));
                            ui.label(theme::muted("Mỗi ô là một bước, viết như nói chuyện. vd: \"bấm xuất chiến góc phải dưới\", \"chọn ải mới nhất\", \"đợi đánh xong\", \"chờ 3 giây\"."));
                            ui.add_space(6.0);
                            let mut changed = false;
                            let mut counter = 0;
                            nodes_ui(ui, &mut ed.sc.steps, false, &mut counter, &mut changed);
                            ui.add_space(4.0);
                            ui.horizontal(|ui| {
                                if ui.button(format!("{} Thêm bước", ic::PLUS)).clicked() {
                                    ed.sc.steps.push(Node::new());
                                    changed = true;
                                }
                                if ui.button(format!("{} Thêm vòng lặp", ic::REPEAT)).on_hover_text("Lặp lại một nhóm bước, vd: đánh hết các map").clicked() {
                                    ed.sc.steps.push(Node::new_loop());
                                    changed = true;
                                }
                            });
                            if changed {
                                ed.trial = None;
                            }
                            let n = script::dynamic_count(&ed.sc.steps);
                            if n > 0 {
                                ui.add_space(4.0);
                                ui.label(RichText::new(format!("{} {n} bước có mục tiêu thay đổi mỗi lần (vd \"ải mới nhất\") nên lần chạy nào AI cũng bật lên cho các bước này.", ic::LIGHTNING)).color(YELLOW).size(13.0));
                            }
                        });
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
                    ui.label(theme::title(format!("{} Màn hình giả lập", ic::MONITOR)));
                    ui.add_space(4.0);
                    ed.screen.ui(ui);
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
                        self.scenarios.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
                        self.info(format!("Đã lưu \"{}\". Chọn giả lập rồi bấm Chạy.", ed.sc.name));
                    }
                    Err(e) => self.warn(format!("Lưu thất bại: {e:#}")),
                }
            } else if ed.is_new {
                ed.sc.delete(); // drop templates from an abandoned trial
            }
        }
    }
}

/// `omnipilot --demo`: opens the editor with an example, to show what a scenario looks like.
fn demo_scenario(instances: &[Instance]) -> Scenario {
    let mut sc = Scenario::new();
    sc.name = "Đánh map cốt truyện".into();
    if let Some(i) = instances.iter().find(|i| i.running) {
        sc.instance_index = i.index;
    }
    let node = |t: &str| Node { text: t.into(), step: None };
    let mut lp = Node::new_loop();
    if let Some(Step::Loop { steps, .. }) = &mut lp.step {
        *steps = vec![node("chọn ải mới nhất"), node("bấm tấn công"), node("đợi đánh xong"), node("bấm tiếp tục")];
    }
    sc.steps = vec![
        node("bấm xuất chiến góc phải dưới, nếu có bảng thông báo sự kiện hiện ra thì đóng nó trước rồi mới bấm xuất chiến"),
        lp,
        node("thoát ra"),
    ];
    sc
}

fn schedule_ui(ui: &mut egui::Ui, ed: &mut Editor) {
    let daily = matches!(ed.sc.schedule, Schedule::Daily { .. });
    ui.horizontal(|ui| {
        if ui.selectable_label(!daily, format!("{} Lặp lại liên tục", ic::REPEAT)).clicked() {
            ed.sc.schedule = Schedule::Repeat { pause_minutes: ed.pause_text.trim().parse().unwrap_or(0) };
        }
        if ui.selectable_label(daily, format!("{} Chạy theo giờ", ic::CLOCK)).clicked() {
            ed.sc.schedule = Schedule::Daily { times: jobs::parse_times(&ed.times_text) };
        }
    });
    match &mut ed.sc.schedule {
        Schedule::Repeat { pause_minutes } => {
            ui.horizontal(|ui| {
                ui.label(theme::muted("Nghỉ giữa các lượt"));
                if ui.add(egui::TextEdit::singleline(&mut ed.pause_text).margin(theme::INPUT_MARGIN).desired_width(48.0)).changed() {
                    *pause_minutes = ed.pause_text.trim().parse().unwrap_or(0);
                }
                ui.label(theme::muted("phút (0 = chạy liên tục)"));
            });
        }
        Schedule::Daily { times } => {
            ui.horizontal(|ui| {
                ui.label(theme::muted("Giờ chạy"));
                if ui.add(egui::TextEdit::singleline(&mut ed.times_text).hint_text("08:00, 20:30").margin(theme::INPUT_MARGIN).desired_width(180.0)).changed() {
                    *times = jobs::parse_times(&ed.times_text);
                }
            });
            if times.is_empty() {
                ui.colored_label(RED, "Chưa có giờ hợp lệ, vd: 08:00, 20:30");
            }
        }
    }
}

enum NodeAction {
    Up(usize),
    Down(usize),
    Remove(usize),
}

/// Editable list of steps; loops render as nested blocks.
fn nodes_ui(ui: &mut egui::Ui, nodes: &mut Vec<Node>, in_loop: bool, counter: &mut usize, changed: &mut bool) {
    let mut action = None;
    let len = nodes.len();
    for (i, node) in nodes.iter_mut().enumerate() {
        let row_buttons = |ui: &mut egui::Ui, action: &mut Option<NodeAction>| {
            if ui.small_button(ic::TRASH).on_hover_text("Xóa").clicked() {
                *action = Some(NodeAction::Remove(i));
            }
            if ui.add_enabled(i + 1 < len, egui::Button::new(ic::ARROW_DOWN).small()).on_hover_text("Chuyển xuống").clicked() {
                *action = Some(NodeAction::Down(i));
            }
            if ui.add_enabled(i > 0, egui::Button::new(ic::ARROW_UP).small()).on_hover_text("Chuyển lên").clicked() {
                *action = Some(NodeAction::Up(i));
            }
        };

        if let Some(Step::Loop { max, steps }) = &mut node.step {
            egui::Frame::new()
                .fill(CARD_HI)
                .stroke(egui::Stroke::new(1.0_f32, ACCENT.gamma_multiply(0.5)))
                .corner_radius(10)
                .inner_margin(Margin::same(12))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(format!("{} Lặp lại tối đa", ic::REPEAT)).color(ACCENT).family(theme::bold()));
                        if ui.add(egui::DragValue::new(max).range(1..=999)).changed() {
                            *changed = true;
                        }
                        ui.label(RichText::new("lần").color(ACCENT).family(theme::bold()));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| row_buttons(ui, &mut action));
                    });
                    ui.label(RichText::new("Tự dừng khi một bước bên trong không tìm thấy nút (vd hết map).").size(12.5).color(MUTED));
                    ui.add_space(4.0);
                    nodes_ui(ui, steps, true, counter, changed);
                    if ui.button(format!("{} Thêm bước vào vòng lặp", ic::PLUS)).clicked() {
                        steps.push(Node::new());
                        *changed = true;
                    }
                });
            ui.add_space(4.0);
            continue;
        }

        *counter += 1;
        ui.horizontal_top(|ui| {
            let (rect, _) = ui.allocate_exact_size(vec2(26.0, 38.0), egui::Sense::hover());
            ui.painter().circle_filled(rect.center(), 12.0, if node.step.is_some() { ACCENT.gamma_multiply(0.35) } else { BORDER });
            ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, counter.to_string(), egui::FontId::new(13.0, theme::bold()), TEXT);
            // Buttons first (right to left) so the text box takes exactly what is left.
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                row_buttons(ui, &mut action);
                let w = ui.available_width();
                let hint = if in_loop { "vd: chọn ải mới nhất" } else { "vd: bấm xuất chiến góc phải dưới" };
                // Text area: wraps and grows with the text, and its height can be dragged
                // from the bottom-right corner.
                egui::Resize::default()
                    .id_salt(("step-box", *counter))
                    .resizable([false, true])
                    .with_stroke(false)
                    .min_width(w)
                    .max_width(w)
                    .default_size(vec2(w, 40.0))
                    .min_height(40.0)
                    .max_height(300.0)
                    .show(ui, |ui| {
                        let edit = egui::TextEdit::multiline(&mut node.text).hint_text(hint).margin(theme::INPUT_MARGIN).desired_rows(1).desired_width(w);
                        if ui.add_sized(ui.available_size(), edit).changed() {
                            node.step = None;
                            *changed = true;
                        }
                    });
            });
        });
        let (text, color) = match &node.step {
            Some(s @ Step::Tap { dynamic: true, .. }) => (format!("{} {}", ic::LIGHTNING, s.describe()), YELLOW),
            Some(s) => (format!("{} {}", ic::ARROW_BEND_DOWN_RIGHT, s.describe()), GREEN),
            None if node.text.trim().is_empty() => (String::new(), MUTED),
            None => ("AI chưa đọc bước này".to_string(), MUTED),
        };
        if !text.is_empty() {
            ui.horizontal(|ui| {
                ui.add_space(34.0);
                ui.label(RichText::new(text).size(12.5).color(color));
            });
        }
    }
    match action {
        Some(NodeAction::Up(i)) => nodes.swap(i, i - 1),
        Some(NodeAction::Down(i)) => nodes.swap(i, i + 1),
        Some(NodeAction::Remove(i)) => {
            nodes.remove(i);
        }
        None => return,
    }
    *changed = true;
}

fn scenario_card(ui: &mut egui::Ui, sc: &mut Scenario, job: Option<&Job>, instances: &[Instance], selected: bool, actions: &mut Vec<RowAction>) {
    let running = job.is_some();
    let border = if selected { ACCENT } else { BORDER };
    let resp = theme::card_frame(border).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.label(RichText::new(&sc.name).family(theme::bold()).size(17.0));
                ui.horizontal(|ui| {
                    let icon = if matches!(sc.schedule, Schedule::Daily { .. }) { ic::CLOCK } else { ic::REPEAT };
                    theme::chip(ui, format!("{icon} {}", sc.schedule.describe()), ACCENT);
                    theme::chip(ui, format!("{} bước", script::leaf_count(&sc.steps)), MUTED);
                    if !sc.recorded {
                        theme::chip(ui, "Chưa chạy thử", YELLOW);
                    }
                    let n = script::dynamic_count(&sc.steps);
                    if n > 0 {
                        theme::chip(ui, format!("{} AI {n} bước", ic::LIGHTNING), YELLOW);
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
                    let current = instances.iter().find(|i| i.index == sc.instance_index);
                    let text = current.map(|i| i.name.clone()).unwrap_or_else(|| format!("#{}", sc.instance_index));
                    let before = sc.instance_index;
                    egui::ComboBox::from_id_salt(("inst", sc.id.as_str())).selected_text(format!("{}  {text}", ic::DEVICE_MOBILE)).width(220.0).show_ui(ui, |ui| {
                        for inst in instances {
                            let label = format!("{}{}", inst.name, if inst.running { "  ● đang mở" } else { "" });
                            ui.selectable_value(&mut sc.instance_index, inst.index, label);
                        }
                    });
                    if sc.instance_index != before {
                        actions.push(RowAction::Save(sc.id.clone()));
                    }
                });
                ui.add_space(8.0);
                match job {
                    Some(j) => {
                        ui.label(RichText::new(&j.status).color(GREEN));
                        theme::status_dot(ui, GREEN);
                    }
                    None => {
                        ui.label(theme::muted("Đang tắt"));
                        theme::status_dot(ui, MUTED);
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
    let s: String = name.chars().map(|c| if r#"\/:*?"<>|"#.contains(c) { '_' } else { c }).collect();
    if s.trim().is_empty() { "kich-ban".into() } else { s.trim().to_string() }
}

fn log_ui(ui: &mut egui::Ui, log: &[String]) {
    egui::Frame::new().fill(theme::INPUT).corner_radius(8).inner_margin(Margin::same(10)).show(ui, |ui| {
        egui::ScrollArea::vertical().id_salt("log").stick_to_bottom(true).auto_shrink([false, false]).max_height(260.0).show(ui, |ui| {
            if log.is_empty() {
                ui.label(theme::muted("(chưa có gì)"));
            }
            for line in log {
                ui.label(RichText::new(line).monospace().size(12.5).color(TEXT));
            }
        });
    });
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        match self.page {
            Page::Setup => self.setup_ui(ctx),
            Page::Main if self.editor.is_some() => self.editor_ui(ctx),
            Page::Main => self.main_ui(ctx),
        }
        // Workers report through channels; keep polling while anything is active.
        ctx.request_repaint_after(Duration::from_millis(250));
    }
}
