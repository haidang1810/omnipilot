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
use crate::script::{self, Node, Rec, Step};
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

    fn ai_locate(&self, frame: &Frame, q: &Query) -> Result<Option<Rect>> {
        let mut guard = self.ai.lock().unwrap();
        if guard.is_none() {
            self.log("Đang bật AI (lần đầu có thể mất 10-60 giây)...");
            *guard = Some(AiServer::start()?);
            self.log("AI đã sẵn sàng.");
        }
        guard
            .as_ref()
            .unwrap()
            .locate(&frame.png, &frame.img, q, &|s| self.log(s))
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
        let tpl = image::open(self.dir.join(&rec.template)).ok()?.to_rgba8();
        let (x, y, _) = vision::find(&frame.img, &tpl, Some((rec.x, rec.y)))?;
        Some(Rect {
            x,
            y,
            w: tpl.width(),
            h: tpl.height(),
        })
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
            if !node.text.is_empty() {
                self.log(format!("▸ {}", node.text));
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
                            other.text
                        ));
                        return Err(e);
                    }
                    used += 1;
                    self.log(format!(
                        "Không thấy, làm \"{}\" ({used}/{})",
                        other.text, node.otherwise_max
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
            Step::Loop { max, steps } => {
                for i in 0..*max {
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
        }
    }
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
    }
}
