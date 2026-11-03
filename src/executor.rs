//! Runs a scenario's steps on a device.
//!
//! Trial run (`record = true`): every element is found by the AI, and a crop of it is saved.
//! Later runs: elements are found by template matching; the AI is only started for
//! dynamic steps or when a saved template no longer matches (and then re-recorded).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use image::RgbaImage;

use crate::ai::{AiServer, Query, Rect};
use crate::device::{self, Device, Frame, Instance};
use crate::script::{self, Cond, Node, PopupAction, PopupRule, Rec, Step};
use crate::vision;

pub type SharedAi = Arc<Mutex<Option<AiServer>>>;

pub enum Event {
    Log(String),
    Screen {
        img: RgbaImage,
        tap: Option<(u32, u32)>,
    },
}

/// A step's target was not on screen. Inside a loop this ends the loop normally.
#[derive(Debug)]
pub struct NotFound(pub String);
impl std::fmt::Display for NotFound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "không tìm thấy \"{}\"", self.0)
    }
}
impl std::error::Error for NotFound {}

/// "Dừng kịch bản": the run ends here, successfully.
#[derive(Debug)]
pub struct Finished;
impl std::fmt::Display for Finished {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "kịch bản tự dừng")
    }
}
impl std::error::Error for Finished {}

#[derive(Debug)]
pub struct Stopped;
impl std::fmt::Display for Stopped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "đã dừng")
    }
}
impl std::error::Error for Stopped {}

pub struct Runner {
    pub device: Device,
    pub ld_dir: PathBuf,
    pub instance: Instance,
    /// Scenario folder where templates are stored.
    pub dir: PathBuf,
    pub ai: SharedAi,
    pub record: bool,
    pub stop: Arc<AtomicBool>,
    pub events: Sender<Event>,
    /// Set when a replay had to re-learn a step, so the caller saves the scenario.
    pub changed: bool,
    /// Resolution of the last screenshot.
    pub screen_size: Option<[u32; 2]>,
    /// The last screenshot, kept to save it when a run fails.
    pub last_screen: Option<RgbaImage>,
    /// "Thấy ảnh này thì …" rules, checked before every step.
    pub popups: Vec<PopupRule>,
    /// Folder of the popup pictures (the top scenario's, also inside kịch bản con).
    pub popup_dir: PathBuf,
    /// Nesting of kịch bản con, to stop a scenario that calls itself.
    depth: u32,
}

const TAP_SETTLE: Duration = Duration::from_millis(1500);
// Some games need several seconds to open a screen (entering a stage took > 5s).
const CHANGE_TIMEOUT: Duration = Duration::from_secs(10);

impl Runner {
    fn log(&self, msg: impl Into<String>) {
        let _ = self.events.send(Event::Log(msg.into()));
    }

    fn check_stop(&self) -> Result<()> {
        if self.stop.load(Ordering::Relaxed) {
            Err(Stopped.into())
        } else {
            Ok(())
        }
    }

    /// Sleeps in small slices so Stop reacts quickly.
    fn sleep(&self, d: Duration) -> Result<()> {
        let end = Instant::now() + d;
        while Instant::now() < end {
            self.check_stop()?;
            std::thread::sleep(Duration::from_millis(100).min(end - Instant::now()));
        }
        Ok(())
    }

    fn shot(&mut self) -> Result<Frame> {
        let f = self.device.screenshot()?;
        self.screen_size = Some([f.img.width(), f.img.height()]);
        self.last_screen = Some(f.img.clone());
        let _ = self.events.send(Event::Screen {
            img: f.img.clone(),
            tap: None,
        });
        Ok(f)
    }

    pub fn ensure_device(&self) -> Result<()> {
        self.device.connect();
        if self.device.is_online() {
            return Ok(());
        }
        self.log(format!("Đang mở giả lập \"{}\"...", self.instance.name));
        device::launch_instance(&self.ld_dir, self.instance.index)?;
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(180) {
            self.sleep(Duration::from_secs(3))?;
            self.device.connect();
            if self.device.is_online() {
                self.sleep(Duration::from_secs(10))?; // let Android finish booting
                return Ok(());
            }
        }
        bail!("không kết nối được giả lập \"{}\"", self.instance.name)
    }

    /// Starts the AI on first use (it is killed when the run ends).
    fn with_ai<T>(&self, f: impl FnOnce(&AiServer) -> Result<T>) -> Result<T> {
        let mut guard = self.ai.lock().unwrap();
        if guard.is_none() {
            self.log("Đang bật AI (lần đầu có thể mất 10-60 giây)...");
            *guard = Some(AiServer::start()?);
            self.log("AI đã sẵn sàng.");
        }
        f(guard.as_ref().unwrap())
    }

    fn ai_locate(&self, frame: &Frame, q: &Query) -> Result<Option<Rect>> {
        self.with_ai(|ai| ai.locate(&frame.png, &frame.img, q, &|s| self.log(s)))
    }

    /// Clears popups matching a rule: up to 5 in a row (one may open another).
    fn handle_popups(&mut self) -> Result<()> {
        if self.popups.is_empty() {
            return Ok(());
        }
        for _ in 0..5 {
            self.check_stop()?;
            let frame = self.device.screenshot()?;
            let hit = self.popups.iter().find_map(|p| {
                let r = match_in(&self.popup_dir, &frame, &p.rec)?;
                Some((p.clone(), r))
            });
            let Some((rule, r)) = hit else { return Ok(()) };
            let name = if rule.note.trim().is_empty() { "popup".to_string() } else { rule.note.clone() };
            self.log(format!("Gặp {name}, xử lý"));
            match rule.action {
                PopupAction::TapImage => self.tap_at(&frame, r, &name)?,
                PopupAction::TapPoint { x, y } => {
                    let (px, py) = to_pixels(&frame, [x, y]);
                    self.tap_at(&frame, Rect { x: px, y: py, w: 1, h: 1 }, &name)?
                }
                PopupAction::Back => {
                    self.device.back()?;
                    self.wait_for_change(&frame)?
                }
            }
        }
        Ok(())
    }

    /// Checks a condition on the current screen.
    fn check(&mut self, cond: &Cond) -> Result<bool> {
        match cond {
            c if c.needs_pick() => bail!("có điều kiện chưa chọn trên màn hình"),
            Cond::SeeImage { rec, region } => {
                let seen = self.find_image(rec, *region, 1)?.is_some();
                self.log(if seen { "  → thấy ảnh" } else { "  → không thấy ảnh" });
                Ok(seen)
            }
            Cond::Number { region, op, value } => {
                let frame = self.shot()?;
                let crop = crop_fraction(&frame.img, *region);
                let text = self.with_ai(|ai| ai.read_text_plain(&crop))?;
                let n = first_number(&text).ok_or_else(|| anyhow!("không đọc được số trong vùng (AI đọc: \"{}\")", text.trim()))?;
                let ok = op.test(n, *value);
                self.log(format!("  → đọc được {n}, {n} {} {value}: {}", op.symbol(), if ok { "đúng" } else { "sai" }));
                Ok(ok)
            }
        }
    }

    /// Before a run: opens the game if it is not on screen.
    pub fn ensure_app(&mut self, r: &script::Recovery, home_dir: &Path) -> Result<()> {
        let pkg = r.package.trim();
        if pkg.is_empty() || self.device.foreground_package().as_deref() == Some(pkg) {
            return Ok(());
        }
        self.log(format!("Game chưa mở, đang mở {pkg}..."));
        self.device.launch_app(pkg)?;
        self.wait_home(r, home_dir, Duration::from_secs(120))?;
        Ok(())
    }

    /// Waits for the start screen after (re)opening the game, clearing popups meanwhile.
    /// Without a start-screen picture, just gives the game time to load.
    fn wait_home(&mut self, r: &script::Recovery, home_dir: &Path, limit: Duration) -> Result<bool> {
        let Some(home) = &r.home else {
            self.sleep(Duration::from_secs(20))?;
            return Ok(true);
        };
        let start = Instant::now();
        while start.elapsed() < limit {
            self.handle_popups()?;
            let frame = self.shot()?;
            if match_in(home_dir, &frame, home).is_some() {
                self.log("Đã thấy màn hình gốc");
                return Ok(true);
            }
            self.sleep(Duration::from_secs(2))?;
        }
        Ok(false)
    }

    /// After a failed run: gets back to the start screen. Clears popups and presses Back;
    /// if that does not work (many games ignore Back), restarts the game.
    pub fn recover(&mut self, r: &script::Recovery, home_dir: &Path) -> Result<bool> {
        let pkg = r.package.trim().to_string();
        if !pkg.is_empty() && self.device.foreground_package().as_deref() != Some(pkg.as_str()) {
            self.log("Game không còn mở (có thể bị văng), mở lại...");
            self.device.launch_app(&pkg)?;
            return self.wait_home(r, home_dir, Duration::from_secs(120));
        }
        if let Some(home) = &r.home {
            for i in 0..=r.max_back {
                self.handle_popups()?;
                let frame = self.shot()?;
                if match_in(home_dir, &frame, home).is_some() {
                    self.log("Đã về màn hình gốc");
                    return Ok(true);
                }
                if i < r.max_back {
                    self.log(format!("Nhấn Back để về màn hình gốc ({}/{})", i + 1, r.max_back));
                    self.device.back()?;
                    self.sleep(Duration::from_millis(1500))?;
                }
            }
        }
        if pkg.is_empty() {
            self.log("Không về được màn hình gốc (chưa chọn game để mở lại)");
            return Ok(false);
        }
        self.log("Khởi động lại game...");
        self.device.stop_app(&pkg)?;
        self.sleep(Duration::from_secs(2))?;
        self.device.launch_app(&pkg)?;
        self.wait_home(r, home_dir, Duration::from_secs(120))
    }

    /// Runs a node list as a whole scenario: "Dừng kịch bản" counts as success.
    pub fn run_all(&mut self, nodes: &mut [Node]) -> Result<()> {
        match self.run(nodes) {
            Err(e) if e.downcast_ref::<Finished>().is_some() => {
                self.log("Kịch bản tự dừng.");
                Ok(())
            }
            r => r,
        }
    }

    fn save_template(&self, frame: &Frame, rect: Rect) -> Result<Rec> {
        std::fs::create_dir_all(&self.dir)?;
        let name = format!("tpl_{}.png", uuid::Uuid::new_v4().simple());
        image::imageops::crop_imm(&frame.img, rect.x, rect.y, rect.w, rect.h)
            .to_image()
            .save(self.dir.join(&name))?;
        Ok(Rec {
            template: name,
            x: rect.x,
            y: rect.y,
        })
    }

    fn match_template(&self, frame: &Frame, rec: &Rec) -> Option<Rect> {
        match_in(&self.dir, frame, rec)
    }

    /// Finds an element, trying the saved template first (replay) and the AI otherwise.
    /// `tries` screenshots are taken, `gap` apart, before giving up. Without `ai_fallback`
    /// a missing template simply means "not there" (e.g. no more rewards to claim).
    /// Steps with an `exclude` filter never use templates: the right element moves.
    fn find(
        &mut self,
        q: &Query,
        rec: &mut Option<Rec>,
        learn: bool,
        ai_fallback: bool,
        tries: u32,
        gap: Duration,
    ) -> Result<Option<(Frame, Rect)>> {
        let use_template = !self.record && rec.is_some() && q.exclude.is_empty();
        let target = q.target;
        for attempt in 0..tries {
            self.check_stop()?;
            if attempt > 0 {
                self.sleep(gap)?;
            }
            let frame = self.shot()?;
            if use_template {
                if let Some(r) = self.match_template(&frame, rec.as_ref().unwrap()) {
                    return Ok(Some((frame, r)));
                }
                // Keep trying the template a few times (screen may still be loading),
                // then fall back to the AI on the last attempt.
                if attempt + 1 < tries || !ai_fallback {
                    continue;
                }
                self.log(format!(
                    "Ảnh mẫu của \"{target}\" không khớp, nhờ AI tìm lại..."
                ));
            }
            if let Some(r) = self.ai_locate(&frame, q)? {
                if learn && q.exclude.is_empty() {
                    *rec = Some(self.save_template(&frame, r)?);
                    if !self.record {
                        self.changed = true;
                    }
                }
                return Ok(Some((frame, r)));
            }
        }
        Ok(None)
    }

    fn tap_at(&self, before: &Frame, rect: Rect, target: &str) -> Result<()> {
        let (x, y) = rect.center();
        let _ = self.events.send(Event::Screen {
            img: before.img.clone(),
            tap: Some((x, y)),
        });
        self.log(format!("Bấm \"{target}\" tại ({x}, {y})"));
        self.device.tap(x, y)?;
        self.wait_for_change(before)
    }

    /// Games often take a few seconds to open a screen, so instead of a fixed delay
    /// wait until the screen actually changes (then a short moment for it to settle).
    fn wait_for_change(&self, before: &Frame) -> Result<()> {
        let start = Instant::now();
        self.sleep(Duration::from_millis(500))?;
        while start.elapsed() < CHANGE_TIMEOUT {
            if let Ok(now) = self.device.screenshot() {
                if vision::changed(&before.img, &now.img) {
                    return self.sleep(Duration::from_millis(700));
                }
            }
            self.sleep(Duration::from_millis(400))?;
        }
        self.log(
            "  ⚠ màn hình không đổi sau khi bấm, có thể bấm trượt hoặc nút này không có hiệu ứng",
        );
        Ok(())
    }

    pub fn run(&mut self, nodes: &mut [Node]) -> Result<()> {
        for node in nodes.iter_mut() {
            self.check_stop()?;
            self.handle_popups()?;
            if !node.is_block() {
                self.log(format!("▸ {}", label(node)));
            }
            self.run_node(node)?;
        }
        Ok(())
    }

    /// Runs a step; when it finds nothing and the node has a "nếu không thấy thì…" action,
    /// runs that action (e.g. next page) and tries again, up to `otherwise_max` times.
    fn run_node(&mut self, node: &mut Node) -> Result<()> {
        let Some(step) = node.step.as_mut() else {
            bail!("bước \"{}\" chưa được AI phân tích", node.text)
        };
        let mut used = 0;
        loop {
            match self.run_step(step, node.region) {
                Err(e) if e.downcast_ref::<NotFound>().is_some() => {
                    let Some(other) = node.otherwise.as_deref_mut() else {
                        return Err(e);
                    };
                    if used >= node.otherwise_max {
                        self.log(format!(
                            "Đã làm \"{}\" {used} lần mà vẫn không thấy",
                            label(other)
                        ));
                        return Err(e);
                    }
                    used += 1;
                    self.log(format!(
                        "Không thấy, làm \"{}\" ({used}/{})",
                        label(other),
                        node.otherwise_max
                    ));
                    let Some(other_step) = other.step.as_mut() else {
                        bail!("bước \"{}\" chưa được AI phân tích", other.text)
                    };
                    // If even this fails (e.g. no next-page arrow left) the whole step is "not found".
                    self.run_step(other_step, other.region)?;
                }
                result => return result,
            }
        }
    }

    fn run_step(&mut self, step: &mut Step, region: Option<[f32; 4]>) -> Result<()> {
        match step {
            Step::Tap {
                target,
                hint,
                dynamic,
                exclude,
                rec,
            } => {
                let learn = !*dynamic;
                let (tries, gap) = if !exclude.is_empty() {
                    (1, Duration::ZERO) // the filter already asks the AI several times
                } else if !self.record && rec.is_some() && learn {
                    (8, Duration::from_secs(1))
                } else {
                    (3, Duration::from_secs(2))
                };
                let mut tmp = None;
                let rec_ref = if learn { rec } else { &mut tmp };
                let q = Query {
                    target,
                    hint,
                    exclude,
                    region,
                };
                match self.find(&q, rec_ref, learn, true, tries, gap)? {
                    Some((frame, r)) => self.tap_at(&frame, r, target),
                    None if script::is_close_target(target) => {
                        self.log("Không thấy nút đóng trên màn hình, nhấn Back của Android");
                        self.device.back()?;
                        self.sleep(TAP_SETTLE)
                    }
                    None => Err(NotFound(target.clone()).into()),
                }
            }
            Step::TapAll {
                target,
                hint,
                max,
                exclude,
                rec,
            } => {
                let mut count = 0;
                let q = Query {
                    target,
                    hint,
                    exclude,
                    region,
                };
                while count < *max {
                    match self.find(&q, rec, true, false, 2, Duration::from_secs(1))? {
                        Some((frame, r)) => {
                            self.tap_at(&frame, r, target)?;
                            count += 1;
                        }
                        None => break,
                    }
                }
                self.log(format!("Đã bấm \"{target}\" {count} lần"));
                Ok(())
            }
            Step::WaitUntil {
                target,
                timeout_sec,
                exclude,
                rec,
            } => {
                self.log(format!("Đợi đến khi thấy \"{target}\"..."));
                let gap = if !self.record && rec.is_some() {
                    Duration::from_secs(1)
                } else {
                    Duration::from_secs(3)
                };
                let tries = (*timeout_sec as u64 / gap.as_secs()).max(1) as u32;
                let q = Query {
                    target,
                    hint: "",
                    exclude,
                    region,
                };
                match self.find(&q, rec, true, true, tries, gap)? {
                    Some(_) => {
                        self.log(format!("Đã thấy \"{target}\""));
                        Ok(())
                    }
                    None => Err(anyhow!("hết {timeout_sec}s mà chưa thấy \"{target}\"")),
                }
            }
            Step::Wait { seconds } => {
                self.log(format!("Chờ {seconds}s"));
                self.sleep(Duration::from_secs_f32(*seconds))
            }
            Step::Back => {
                self.log("Nhấn Back");
                self.device.back()?;
                self.sleep(TAP_SETTLE)
            }
            Step::If { cond, then_steps, else_steps } => {
                self.log(format!("▸ Nếu {}", cond.describe()));
                let yes = self.check(cond)?;
                self.run(if yes { then_steps } else { else_steps })
            }
            Step::StopRun => Err(Finished.into()),
            Step::Call { scenario_id, name } => {
                if self.depth >= 4 {
                    bail!("kịch bản con lồng nhau quá sâu (có thể đang tự gọi chính nó)");
                }
                let mut sub = script::load_all()
                    .into_iter()
                    .find(|s| s.id == *scenario_id)
                    .ok_or_else(|| anyhow!("không tìm thấy kịch bản con \"{name}\" (đã bị xóa?)"))?;
                self.log(format!("— Vào kịch bản con \"{}\"", sub.name));
                let parent_dir = std::mem::replace(&mut self.dir, sub.dir());
                self.depth += 1;
                let result = self.run(&mut sub.steps);
                self.depth -= 1;
                self.dir = parent_dir;
                self.log(format!("— Xong kịch bản con \"{}\"", sub.name));
                result
            }
            Step::Loop { max, steps, until } => {
                for i in 0..*max {
                    if let Some(c) = until {
                        if self.check(c)? {
                            self.log("Điều kiện dừng vòng lặp đã đúng");
                            break;
                        }
                    }
                    self.log(format!("— Vòng {}/{}", i + 1, max));
                    match self.run(steps) {
                        Ok(()) => {}
                        Err(e) if e.downcast_ref::<NotFound>().is_some() => {
                            self.log(format!("Kết thúc vòng lặp ({e})"));
                            break;
                        }
                        Err(e) => return Err(e),
                    }
                }
                Ok(())
            }
            s if s.needs_pick() => bail!("có bước chưa chọn vị trí trên màn hình"),
            Step::TapPoint { x, y } => {
                let frame = self.shot()?;
                let (px, py) = to_pixels(&frame, [*x, *y]);
                self.tap_at(&frame, Rect { x: px, y: py, w: 1, h: 1 }, "điểm đã chọn")
            }
            Step::TapImage { all, max, rec } => {
                let mut count = 0;
                while count < if *all { *max } else { 1 } {
                    // The first look waits for the screen to load; later ones only check.
                    let tries = if count == 0 { 5 } else { 2 };
                    match self.find_image(rec, region, tries)? {
                        Some((frame, r)) => {
                            self.tap_at(&frame, r, "nút giống ảnh")?;
                            count += 1;
                        }
                        None if count == 0 && !*all => return Err(NotFound("nút giống ảnh".into()).into()),
                        None => break,
                    }
                }
                if *all {
                    self.log(format!("Đã bấm {count} nút giống ảnh"));
                }
                Ok(())
            }
            Step::WaitImage { timeout_sec, rec } => {
                self.log("Đợi đến khi thấy ảnh...");
                match self.find_image(rec, region, (*timeout_sec).max(1))? {
                    Some(_) => {
                        self.log("Đã thấy ảnh");
                        Ok(())
                    }
                    None => Err(anyhow!("hết {timeout_sec}s mà chưa thấy ảnh")),
                }
            }
            Step::Swipe { from, to, ms } => {
                let frame = self.shot()?;
                let (a, b) = (to_pixels(&frame, *from), to_pixels(&frame, *to));
                self.log(format!("Vuốt từ {a:?} đến {b:?}"));
                self.device.swipe(a, b, *ms)?;
                self.wait_for_change(&frame)
            }
        }
    }

    /// Looks for a hand-picked button picture, once a second, `tries` times.
    /// With a search area, matches outside it are ignored.
    fn find_image(&mut self, rec: &Rec, region: Option<[f32; 4]>, tries: u32) -> Result<Option<(Frame, Rect)>> {
        for attempt in 0..tries {
            self.check_stop()?;
            if attempt > 0 {
                self.sleep(Duration::from_secs(1))?;
            }
            if attempt > 0 {
                self.handle_popups()?;
            }
            let frame = self.shot()?;
            if let Some(r) = self.match_template(&frame, rec) {
                let (cx, cy) = r.center();
                let inside = region.is_none_or(|g| {
                    let (fx, fy) = (cx as f32 / frame.img.width() as f32, cy as f32 / frame.img.height() as f32);
                    fx >= g[0] && fx <= g[2] && fy >= g[1] && fy <= g[3]
                });
                if inside {
                    return Ok(Some((frame, r)));
                }
            }
        }
        Ok(None)
    }
}

/// What to call a step in the log: the user's words, or how a hand-made step works.
fn label(node: &Node) -> String {
    match (&node.step, node.text.trim()) {
        (Some(s), "") => s.describe(),
        (_, t) => t.to_string(),
    }
}

fn match_in(dir: &Path, frame: &Frame, rec: &Rec) -> Option<Rect> {
    let tpl = image::open(dir.join(&rec.template)).ok()?.to_rgba8();
    let (x, y, _) = vision::find(&frame.img, &tpl, Some((rec.x, rec.y)))?;
    Some(Rect { x, y, w: tpl.width(), h: tpl.height() })
}

fn crop_fraction(img: &RgbaImage, r: [f32; 4]) -> RgbaImage {
    let (w, h) = (img.width() as f32, img.height() as f32);
    let x = (r[0].clamp(0.0, 1.0) * w) as u32;
    let y = (r[1].clamp(0.0, 1.0) * h) as u32;
    let cw = (((r[2] - r[0]).clamp(0.0, 1.0) * w) as u32).clamp(1, img.width() - x);
    let ch = (((r[3] - r[1]).clamp(0.0, 1.0) * h) as u32).clamp(1, img.height() - y);
    image::imageops::crop_imm(img, x, y, cw, ch).to_image()
}

/// First number in OCR text: "97/120" → 97, "1.250.000" → 1250000, "3,5" → 3.5.
pub fn first_number(text: &str) -> Option<f64> {
    let start = text.find(|c: char| c.is_ascii_digit())?;
    let raw: String = text[start..].chars().take_while(|c| c.is_ascii_digit() || *c == '.' || *c == ',').collect();
    let raw = raw.trim_end_matches(['.', ',']);
    let groups: Vec<&str> = raw.split(['.', ',']).collect();
    let digits = if groups.len() > 1 && groups[1..].iter().all(|g| g.len() == 3) {
        groups.concat() // thousands separators
    } else if groups.len() == 2 {
        format!("{}.{}", groups[0], groups[1]) // decimal
    } else {
        groups[0].to_string()
    };
    digits.parse().ok()
}

/// Fractions of the screen → pixels of this frame.
fn to_pixels(frame: &Frame, f: [f32; 2]) -> (u32, u32) {
    let (w, h) = (frame.img.width(), frame.img.height());
    (((f[0].clamp(0.0, 1.0) * w as f32) as u32).min(w - 1), ((f[1].clamp(0.0, 1.0) * h as f32) as u32).min(h - 1))
}

pub fn new_runner(
    ld_dir: &Path,
    instance: Instance,
    dir: PathBuf,
    ai: SharedAi,
    record: bool,
    stop: Arc<AtomicBool>,
    events: Sender<Event>,
) -> Runner {
    Runner {
        device: Device::new(ld_dir, &instance),
        ld_dir: ld_dir.to_path_buf(),
        instance,
        dir,
        ai,
        record,
        stop,
        events,
        changed: false,
        screen_size: None,
        last_screen: None,
        popups: Vec::new(),
        popup_dir: PathBuf::new(),
        depth: 0,
    }
}
