use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use eframe::egui::{self, Color32, RichText, TextureHandle};
use image::RgbaImage;
use serde::{Deserialize, Serialize};

use crate::ai::AiServer;
use crate::device::{self, Instance};
use crate::executor::{self, Event, SharedAi};
use crate::jobs::{self, Job};
use crate::script::{self, Scenario, Schedule, Step};
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
        let Some(tex) = &self.tex else {
            ui.label(RichText::new("(chưa có ảnh màn hình)").weak());
            return;
        };
        let w = ui.available_width().min(640.0);
        let size = egui::vec2(w, w * self.size[1] as f32 / self.size[0] as f32);
        let resp = ui.add(egui::Image::new((tex.id(), size)));
        if let Some((x, y)) = self.tap {
            let p = resp.rect.min + egui::vec2(x as f32 / self.size[0] as f32 * size.x, y as f32 / self.size[1] as f32 * size.y);
            ui.painter().circle_stroke(p, 12.0, egui::Stroke::new(3.0_f32, Color32::RED));
            ui.painter().circle_filled(p, 3.0, Color32::RED);
        }
    }
}

enum TaskMsg {
    Event(Event),
    Compiled(Result<Vec<Step>, String>),
    /// Learned steps and the screen resolution they were recorded at.
    TrialDone(Result<(Vec<Step>, Option<[u32; 2]>), String>),
}

struct Task {
    rx: Receiver<TaskMsg>,
    stop: Arc<AtomicBool>,
    label: &'static str,
}

struct Editor {
    sc: Scenario,
    is_new: bool,
    times_text: String,
    pause_text: String,
    /// The AI stays loaded while the editor is open (compile + trial + retries)
    /// and is killed when the editor closes.
    ai: SharedAi,
    task: Option<Task>,
    log: Vec<String>,
    screen: ScreenView,
    /// Some(..) after a trial run finished.
    trial: Option<Result<Vec<Step>, String>>,
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

fn compile_with(ai: &SharedAi, description: &str, log: &dyn Fn(String)) -> anyhow::Result<Vec<Step>> {
    ensure_ai(ai, log)?;
    log("AI đang phân tích mô tả...".into());
    let steps = ai.lock().unwrap().as_ref().unwrap().compile(description)?;
    if steps.is_empty() {
        anyhow::bail!("AI không tạo được bước nào từ mô tả");
    }
    Ok(steps)
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
    message: Option<String>,
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
        install_fonts(&cc.egui_ctx);
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
        app
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
            ui.add_space(80.0);
            ui.vertical_centered(|ui| {
                ui.heading("OmniPilot – chuẩn bị lần đầu");
                ui.add_space(10.0);
                ui.label("Đang tải AI chạy trên máy (~3,3GB). Chỉ tải một lần duy nhất.");
                ui.add_space(20.0);
                if let Some(err) = &p.error {
                    ui.colored_label(Color32::LIGHT_RED, format!("Lỗi: {err}"));
                    if ui.button("Thử lại").clicked() {
                        *self.setup.lock().unwrap() = setup::Progress::default();
                        setup::start(self.setup.clone());
                    }
                    return;
                }
                ui.label(format!("Bước {}/{}: {}", p.step, p.total_steps.max(1), p.label));
                let frac = if p.size > 0 { p.downloaded as f32 / p.size as f32 } else { 0.0 };
                ui.add(
                    egui::ProgressBar::new(frac)
                        .desired_width(420.0)
                        .text(format!("{:.0} / {:.0} MB", p.downloaded as f64 / 1e6, p.size as f64 / 1e6)),
                );
            });
        });
    }

    fn main_ui(&mut self, ctx: &egui::Context) {
        for job in &mut self.jobs {
            if let Some(updated) = job.poll() {
                if let Some(s) = self.scenarios.iter_mut().find(|s| s.id == updated.id) {
                    *s = updated;
                }
            }
        }

        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("OmniPilot");
                ui.label(RichText::new("auto mọi game trên LDPlayer").weak());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("⟳ Làm mới giả lập").clicked() {
                        self.refresh_instances();
                    }
                    if ui.button("📥 Nhập kịch bản").on_hover_text("Mở file .omnipilot người khác chia sẻ").clicked() {
                        self.import_dialog();
                    }
                    if ui.add(egui::Button::new("➕ Tạo kịch bản")).clicked() {
                        let mut sc = Scenario::new();
                        if let Some(i) = self.instances.iter().find(|i| i.running).or(self.instances.first()) {
                            sc.instance_index = i.index;
                        }
                        self.editor = Some(Editor::new(sc, true));
                    }
                });
            });
            if let Some(err) = self.inst_error.clone() {
                ui.horizontal(|ui| {
                    ui.colored_label(Color32::LIGHT_RED, err);
                    ui.text_edit_singleline(&mut self.ld_text);
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
            }
            if let Some(msg) = self.message.clone() {
                ui.horizontal(|ui| {
                    ui.colored_label(Color32::YELLOW, msg);
                    if ui.small_button("✖").clicked() {
                        self.message = None;
                    }
                });
            }
        });

        let mut actions = Vec::new();
        egui::TopBottomPanel::bottom("detail").resizable(true).default_height(300.0).show(ctx, |ui| {
            self.detail_ui(ui, ctx);
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            if self.scenarios.is_empty() {
                ui.add_space(40.0);
                ui.vertical_centered(|ui| ui.label("Chưa có kịch bản nào. Bấm \"➕ Tạo kịch bản\" để bắt đầu."));
                return;
            }
            egui::ScrollArea::vertical().show(ui, |ui| {
                egui::Grid::new("scenarios").striped(true).num_columns(5).spacing([16.0, 8.0]).show(ui, |ui| {
                    ui.strong("Kịch bản");
                    ui.strong("Lịch");
                    ui.strong("Giả lập");
                    ui.strong("Trạng thái");
                    ui.strong("");
                    ui.end_row();
                    let instances = &self.instances;
                    for sc in &mut self.scenarios {
                        let job = self.jobs.iter().find(|j| j.scenario_id == sc.id && !j.is_finished());
                        let running = job.is_some();
                        let selected = self.selected.as_deref() == Some(sc.id.as_str());
                        let name = if sc.recorded { sc.name.clone() } else { format!("{} (chưa chạy thử)", sc.name) };
                        if ui.selectable_label(selected, name).clicked() {
                            actions.push(RowAction::Select(sc.id.clone()));
                        }
                        ui.label(sc.schedule.describe());
                        ui.add_enabled_ui(!running, |ui| {
                            let current = instances.iter().find(|i| i.index == sc.instance_index);
                            let text = current.map(|i| i.name.clone()).unwrap_or_else(|| format!("#{}", sc.instance_index));
                            let before = sc.instance_index;
                            egui::ComboBox::from_id_salt(("inst", sc.id.as_str())).selected_text(text).width(180.0).show_ui(ui, |ui| {
                                for inst in instances {
                                    let label = format!("{}{}", inst.name, if inst.running { " ●" } else { "" });
                                    ui.selectable_value(&mut sc.instance_index, inst.index, label);
                                }
                            });
                            if sc.instance_index != before {
                                actions.push(RowAction::Save(sc.id.clone()));
                            }
                        });
                        match job {
                            Some(j) => ui.label(RichText::new(&j.status).color(Color32::LIGHT_GREEN)),
                            None => ui.label(RichText::new("Đang tắt").weak()),
                        };
                        ui.horizontal(|ui| {
                            if running {
                                if ui.button("⏹ Dừng").clicked() {
                                    actions.push(RowAction::Stop(sc.id.clone()));
                                }
                            } else {
                                if ui.button("▶ Start").clicked() {
                                    actions.push(RowAction::Start(sc.id.clone()));
                                }
                                if ui.button("Sửa").clicked() {
                                    actions.push(RowAction::Edit(sc.id.clone()));
                                }
                                if ui.add_enabled(sc.recorded, egui::Button::new("Xuất")).on_hover_text("Lưu thành file .omnipilot để chia sẻ").clicked() {
                                    actions.push(RowAction::Export(sc.id.clone()));
                                }
                                if ui.button("Xóa").clicked() {
                                    actions.push(RowAction::Delete(sc.id.clone()));
                                }
                            }
                        });
                        ui.end_row();
                    }
                });
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
                self.selected = Some(id);
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
                    self.message = Some("Chưa cấu hình LDPlayer".into());
                    return;
                };
                let Some(inst) = self.instances.iter().find(|i| i.index == sc.instance_index).cloned() else {
                    self.message = Some("Chọn giả lập cho kịch bản trước".into());
                    return;
                };
                if self.jobs.iter().any(|j| j.instance_index == inst.index && !j.is_finished()) {
                    self.message = Some(format!("Giả lập \"{}\" đang chạy kịch bản khác", inst.name));
                    return;
                }
                if !sc.recorded {
                    self.message = Some("Kịch bản chưa chạy thử nên lần chạy này sẽ dùng AI (chậm hơn).".into());
                } else if let Some(w) = size_warning(&sc, &inst) {
                    self.message = Some(w);
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
                self.message = Some(match sc.export(&path) {
                    Ok(()) => format!("Đã xuất kịch bản ra {}", path.display()),
                    Err(e) => format!("Xuất thất bại: {e:#}"),
                });
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
                let warn = self.instances.iter().find(|i| i.index == sc.instance_index).and_then(|i| size_warning(&sc, i));
                self.message = Some(match warn {
                    Some(w) => format!("Đã nhập \"{}\". {w}", sc.name),
                    None => format!("Đã nhập \"{}\". Chọn giả lập rồi bấm Start.", sc.name),
                });
                self.selected = Some(sc.id.clone());
                self.scenarios.push(sc);
                self.scenarios.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
            }
            Err(e) => self.message = Some(format!("Nhập thất bại: {e:#}")),
        }
    }

    fn delete_dialog(&mut self, ctx: &egui::Context) {
        let Some(id) = self.confirm_delete.clone() else { return };
        let name = self.scenarios.iter().find(|s| s.id == id).map(|s| s.name.clone()).unwrap_or_default();
        egui::Window::new("Xóa kịch bản").collapsible(false).resizable(false).anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0]).show(ctx, |ui| {
            ui.label(format!("Xóa kịch bản \"{name}\"? Không thể hoàn tác."));
            ui.horizontal(|ui| {
                if ui.button("Xóa").clicked() {
                    if let Some(sc) = self.scenarios.iter().find(|s| s.id == id) {
                        sc.delete();
                    }
                    self.scenarios.retain(|s| s.id != id);
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
            ui.label(RichText::new("Chọn một kịch bản để xem nhật ký chạy.").weak());
            return;
        };
        let Some(job) = self.jobs.iter().rev().find(|j| j.scenario_id == id) else {
            ui.label(RichText::new("Kịch bản này chưa chạy lần nào kể từ khi mở app.").weak());
            return;
        };
        if job.screen_gen != self.job_screen.shown_gen {
            if let Some((img, tap)) = &job.screen {
                self.job_screen.set(ctx, img, *tap);
            }
            self.job_screen.shown_gen = job.screen_gen;
        }
        ui.columns(2, |cols| {
            cols[0].label(format!("Số lượt đã xong: {}", job.runs));
            log_ui(&mut cols[0], &job.log);
            self.job_screen.ui(&mut cols[1]);
        });
    }

    fn editor_ui(&mut self, ctx: &egui::Context) {
        let mut close: Option<bool> = None; // Some(saved)
        let instances = self.instances.clone();
        let ld_dir = self.settings.ld_dir.clone();
        let ed = self.editor.as_mut().unwrap();

        if let Some(task) = &ed.task {
            let mut finished = false;
            while let Ok(msg) = task.rx.try_recv() {
                match msg {
                    TaskMsg::Event(Event::Log(l)) => jobs::push_log(&mut ed.log, l),
                    TaskMsg::Event(Event::Screen { img, tap }) => ed.screen.set(ctx, &img, tap),
                    TaskMsg::Compiled(r) => {
                        finished = true;
                        match r {
                            Ok(steps) => {
                                jobs::push_log(&mut ed.log, format!("AI tạo được {} bước. Kiểm tra lại bên dưới rồi bấm Chạy thử.", steps.len()));
                                ed.sc.steps = steps;
                            }
                            Err(e) => ed.error = Some(e),
                        }
                    }
                    TaskMsg::TrialDone(r) => {
                        finished = true;
                        match r {
                            Ok((steps, size)) => {
                                jobs::push_log(&mut ed.log, "Chạy thử xong. Kiểm tra lại game xem đã đúng chưa.".into());
                                ed.sc.steps = steps.clone();
                                ed.sc.screen_size = size;
                                ed.trial = Some(Ok(steps));
                            }
                            Err(e) => {
                                jobs::push_log(&mut ed.log, format!("Chạy thử lỗi: {e}"));
                                ed.trial = Some(Err(e));
                            }
                        }
                    }
                }
            }
            if finished {
                ed.task = None;
            }
        }
        let busy = ed.task.is_some();

        egui::TopBottomPanel::top("ed_top").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading(if ed.is_new { "Tạo kịch bản" } else { "Sửa kịch bản" });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.add_enabled(!busy, egui::Button::new("Hủy")).clicked() {
                        close = Some(false);
                    }
                });
            });
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.columns(2, |cols| {
                let ui = &mut cols[0];
                egui::ScrollArea::vertical().id_salt("form").show(ui, |ui| {
                    ui.add_enabled_ui(!busy, |ui| {
                        ui.label("Tên kịch bản");
                        ui.add(egui::TextEdit::singleline(&mut ed.sc.name).hint_text("vd: Đánh map cốt truyện").desired_width(f32::INFINITY));
                        ui.add_space(6.0);

                        ui.label("Giả lập");
                        let text = instances.iter().find(|i| i.index == ed.sc.instance_index).map(|i| i.name.clone()).unwrap_or("(chọn)".into());
                        egui::ComboBox::from_id_salt("ed_inst").selected_text(text).width(260.0).show_ui(ui, |ui| {
                            for inst in &instances {
                                let label = format!("{}{}", inst.name, if inst.running { " ● đang chạy" } else { "" });
                                ui.selectable_value(&mut ed.sc.instance_index, inst.index, label);
                            }
                        });
                        ui.add_space(6.0);

                        ui.label("Kiểu chạy");
                        let daily = matches!(ed.sc.schedule, Schedule::Daily { .. });
                        ui.horizontal(|ui| {
                            if ui.radio(!daily, "Lặp lại liên tục").clicked() {
                                ed.sc.schedule = Schedule::Repeat { pause_minutes: ed.pause_text.trim().parse().unwrap_or(0) };
                            }
                            if ui.radio(daily, "Chạy theo giờ").clicked() {
                                ed.sc.schedule = Schedule::Daily { times: jobs::parse_times(&ed.times_text) };
                            }
                        });
                        match &mut ed.sc.schedule {
                            Schedule::Repeat { pause_minutes } => {
                                ui.horizontal(|ui| {
                                    ui.label("Nghỉ giữa các lượt (phút, 0 = liên tục):");
                                    if ui.add(egui::TextEdit::singleline(&mut ed.pause_text).desired_width(50.0)).changed() {
                                        *pause_minutes = ed.pause_text.trim().parse().unwrap_or(0);
                                    }
                                });
                            }
                            Schedule::Daily { times } => {
                                ui.horizontal(|ui| {
                                    ui.label("Giờ chạy (HH:MM, cách nhau dấu phẩy):");
                                    if ui.add(egui::TextEdit::singleline(&mut ed.times_text).desired_width(160.0)).changed() {
                                        *times = jobs::parse_times(&ed.times_text);
                                    }
                                });
                                if times.is_empty() {
                                    ui.colored_label(Color32::LIGHT_RED, "Chưa có giờ hợp lệ, vd: 08:00, 20:30");
                                }
                            }
                        }
                        ui.add_space(6.0);

                        ui.label("Mô tả các bước bằng tiếng Việt");
                        let resp = ui.add(
                            egui::TextEdit::multiline(&mut ed.sc.description)
                                .hint_text("vd: click vô button xuất chiến góc phải dưới sau đó chọn map chưa đi bấm tấn công đợi xong đi tiếp map tiếp theo")
                                .desired_rows(4)
                                .desired_width(f32::INFINITY),
                        );
                        if resp.changed() {
                            ed.trial = None;
                        }
                    });

                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        let can = !busy && !ed.sc.description.trim().is_empty();
                        if ui.add_enabled(can, egui::Button::new("🧠 Phân tích mô tả")).clicked() {
                            ed.error = None;
                            ed.trial = None;
                            let (tx, rx) = channel();
                            let ai = ed.ai.clone();
                            let desc = ed.sc.description.clone();
                            std::thread::spawn(move || {
                                let log_tx = tx.clone();
                                let log = move |s: String| {
                                    let _ = log_tx.send(TaskMsg::Event(Event::Log(s)));
                                };
                                let r = compile_with(&ai, &desc, &log).map_err(|e| format!("{e:#}"));
                                let _ = tx.send(TaskMsg::Compiled(r));
                            });
                            ed.task = Some(Task { rx, stop: Arc::new(AtomicBool::new(false)), label: "Đang phân tích..." });
                        }
                        // Steps must be reviewed before anything is tapped in the game.
                        let can_trial = can && !ed.sc.steps.is_empty() && ld_dir.is_some() && instances.iter().any(|i| i.index == ed.sc.instance_index);
                        let trial_btn = ui.add_enabled(can_trial, egui::Button::new("▶ Chạy thử"));
                        let trial_btn = if ed.sc.steps.is_empty() { trial_btn.on_disabled_hover_text("Bấm \"Phân tích mô tả\" và kiểm tra các bước trước") } else { trial_btn };
                        if trial_btn.clicked() {
                            ed.error = None;
                            ed.trial = None;
                            let (tx, rx) = channel();
                            let stop = Arc::new(AtomicBool::new(false));
                            let inst = instances.iter().find(|i| i.index == ed.sc.instance_index).cloned().unwrap();
                            let (ai, dir, stop2) = (ed.ai.clone(), ed.sc.dir(), stop.clone());
                            let mut steps = ed.sc.steps.clone();
                            let ld = ld_dir.clone().unwrap();
                            std::thread::spawn(move || {
                                let r = (|| -> anyhow::Result<(Vec<Step>, Option<[u32; 2]>)> {
                                    steps.iter_mut().for_each(Step::clear_rec);
                                    let (etx, erx) = channel::<Event>();
                                    let fwd = tx.clone();
                                    std::thread::spawn(move || {
                                        while let Ok(e) = erx.recv() {
                                            let _ = fwd.send(TaskMsg::Event(e));
                                        }
                                    });
                                    let mut runner = executor::new_runner(&ld, inst, dir, ai, true, stop2, etx);
                                    runner.ensure_device()?;
                                    runner.run(&mut steps)?;
                                    Ok((steps, runner.screen_size))
                                })();
                                let _ = tx.send(TaskMsg::TrialDone(r.map_err(|e| format!("{e:#}"))));
                            });
                            ed.task = Some(Task { rx, stop, label: "Đang chạy thử..." });
                        }
                        if let Some(t) = &ed.task {
                            ui.spinner();
                            ui.label(t.label);
                            if ui.button("⏹ Dừng").clicked() {
                                t.stop.store(true, Ordering::Relaxed);
                            }
                        }
                    });
                    ui.label(
                        RichText::new(
                            "ℹ Lần chạy thử đầu tiên sẽ chậm vì AI phải nhìn từng màn hình. \
                             Sau khi bạn xác nhận chạy đúng, app lưu lại ảnh mẫu các nút nên những lần sau chạy nhanh và không cần AI.",
                        )
                        .weak(),
                    );
                    if let Some(e) = &ed.error {
                        ui.colored_label(Color32::LIGHT_RED, e);
                    }

                    if !ed.sc.steps.is_empty() {
                        ui.add_space(8.0);
                        ui.separator();
                        ui.strong("Các bước (có thể sửa tên nút)");
                        let mut changed = false;
                        ui.add_enabled_ui(!busy, |ui| steps_ui(ui, &mut ed.sc.steps, 0, &mut changed));
                        if changed {
                            ed.trial = None;
                        }
                        let n = script::dynamic_count(&ed.sc.steps);
                        if n > 0 {
                            ui.label(RichText::new(format!("⚠ {n} bước có mục tiêu thay đổi mỗi lần (vd \"map chưa đi\") nên lần chạy nào cũng cần bật AI cho các bước này.")).color(Color32::YELLOW));
                        }
                    }

                    if let Some(result) = &ed.trial {
                        ui.add_space(8.0);
                        ui.separator();
                        match result {
                            Ok(_) => {
                                ui.strong("Kịch bản đã chạy đúng như bạn muốn chưa?");
                                ui.horizontal(|ui| {
                                    let name_ok = !ed.sc.name.trim().is_empty();
                                    if ui.add_enabled(name_ok, egui::Button::new("✔ Đúng rồi, lưu kịch bản")).clicked() {
                                        close = Some(true);
                                    }
                                    if !name_ok {
                                        ui.colored_label(Color32::LIGHT_RED, "Nhập tên kịch bản trước");
                                    }
                                });
                                ui.label(RichText::new("Chưa đúng? Sửa mô tả hoặc tên nút ở trên rồi bấm Chạy thử lại.").weak());
                            }
                            Err(_) => {
                                ui.colored_label(Color32::LIGHT_RED, "Chạy thử chưa thành công. Sửa mô tả hoặc tên nút rồi chạy thử lại.");
                            }
                        }
                    }
                });

                let ui = &mut cols[1];
                ed.screen.ui(ui);
                ui.add_space(6.0);
                log_ui(ui, &ed.log);
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
                    }
                    Err(e) => self.message = Some(format!("Lưu thất bại: {e:#}")),
                }
            } else if ed.is_new {
                ed.sc.delete(); // drop templates from an abandoned trial
            }
        }
    }
}

fn steps_ui(ui: &mut egui::Ui, steps: &mut Vec<Step>, depth: usize, changed: &mut bool) {
    let mut remove = None;
    for (i, st) in steps.iter_mut().enumerate() {
        ui.horizontal(|ui| {
            ui.add_space(depth as f32 * 20.0);
            ui.label(format!("{}.", i + 1));
            let kind = match st {
                Step::Tap { dynamic: true, .. } => Some("Bấm [AI mỗi lần]:"),
                Step::Tap { .. } => Some("Bấm:"),
                Step::TapAll { .. } => Some("Bấm hết:"),
                Step::WaitUntil { .. } => Some("Đợi thấy:"),
                _ => None,
            };
            match kind {
                Some(k) => {
                    ui.label(k);
                    let desc_len = st.target_mut().map(|t| t.len()).unwrap_or(0);
                    let t = st.target_mut().unwrap();
                    if ui.add(egui::TextEdit::singleline(t).desired_width((desc_len as f32 * 7.0).clamp(120.0, 320.0))).changed() {
                        st.clear_rec();
                        *changed = true;
                    }
                }
                None => {
                    ui.label(st.describe());
                }
            }
            if ui.small_button("✖").on_hover_text("Xóa bước").clicked() {
                remove = Some(i);
            }
        });
        if let Step::Loop { steps: inner, .. } = st {
            steps_ui(ui, inner, depth + 1, changed);
        }
    }
    if let Some(i) = remove {
        steps.remove(i);
        *changed = true;
    }
}

fn sanitize_file_name(name: &str) -> String {
    let s: String = name.chars().map(|c| if r#"\/:*?"<>|"#.contains(c) { '_' } else { c }).collect();
    if s.trim().is_empty() { "kich-ban".into() } else { s.trim().to_string() }
}

fn log_ui(ui: &mut egui::Ui, log: &[String]) {
    egui::ScrollArea::vertical().id_salt("log").stick_to_bottom(true).auto_shrink([false, false]).show(ui, |ui| {
        for line in log {
            ui.label(RichText::new(line).monospace().size(12.0));
        }
    });
}

fn install_fonts(ctx: &egui::Context) {
    // egui's built-in font lacks many Vietnamese glyphs; use Segoe UI from Windows.
    let windir = std::env::var("WINDIR").unwrap_or_else(|_| "C:\\Windows".into());
    let mut fonts = egui::FontDefinitions::default();
    for (name, file) in [("segoe", "segoeui.ttf"), ("consolas", "consola.ttf")] {
        if let Ok(bytes) = std::fs::read(PathBuf::from(&windir).join("Fonts").join(file)) {
            fonts.font_data.insert(name.into(), Arc::new(egui::FontData::from_owned(bytes)));
            let family = if name == "segoe" { egui::FontFamily::Proportional } else { egui::FontFamily::Monospace };
            fonts.families.entry(family).or_default().insert(0, name.into());
        }
    }
    ctx.set_fonts(fonts);
    ctx.style_mut(|s| {
        s.text_styles.insert(egui::TextStyle::Body, egui::FontId::proportional(15.0));
        s.text_styles.insert(egui::TextStyle::Button, egui::FontId::proportional(15.0));
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
