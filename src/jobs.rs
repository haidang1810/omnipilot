//! Background jobs: one thread per started scenario, following its schedule.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use chrono::{Local, NaiveTime, TimeDelta};
use image::RgbaImage;

use crate::device::Instance;
use crate::executor::{self, Event, Stopped};
use crate::script::{Scenario, Schedule};

pub enum JobMsg {
    Event(Event),
    Status(String),
    /// The scenario was re-learned during a run and saved; the UI should reload it.
    Updated(Scenario),
}

pub struct Job {
    pub scenario_id: String,
    pub instance_index: u32,
    pub stop: Arc<AtomicBool>,
    rx: Receiver<JobMsg>,
    handle: Option<JoinHandle<()>>,
    pub status: String,
    pub log: Vec<String>,
    pub screen: Option<(RgbaImage, Option<(u32, u32)>)>,
    /// Bumped on every new screenshot so the UI knows to re-upload the texture.
    pub screen_gen: u64,
    pub runs: u32,
}

impl Job {
    pub fn is_finished(&self) -> bool {
        self.handle.as_ref().is_none_or(|h| h.is_finished())
    }

    /// Drains messages from the worker. Returns a scenario that needs reloading, if any.
    pub fn poll(&mut self) -> Option<Scenario> {
        let mut updated = None;
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                JobMsg::Event(Event::Log(l)) => push_log(&mut self.log, l),
                JobMsg::Event(Event::Screen { img, tap }) => {
                    self.screen = Some((img, tap));
                    self.screen_gen += 1;
                }
                JobMsg::Status(s) => {
                    if s.starts_with("Xong lượt") {
                        self.runs += 1;
                    }
                    push_log(&mut self.log, format!("[{}]", s));
                    self.status = s;
                }
                JobMsg::Updated(s) => updated = Some(s),
            }
        }
        updated
    }
}

pub fn push_log(log: &mut Vec<String>, line: String) {
    log.push(format!("{} {}", Local::now().format("%H:%M:%S"), line));
    if log.len() > 500 {
        log.drain(..100);
    }
}

pub fn start(scenario: Scenario, ld_dir: PathBuf, instance: Instance) -> Job {
    let stop = Arc::new(AtomicBool::new(false));
    let (tx, rx) = channel();
    let id = scenario.id.clone();
    let idx = instance.index;
    let stop2 = stop.clone();
    let handle = std::thread::spawn(move || worker(scenario, ld_dir, instance, stop2, tx));
    Job {
        scenario_id: id,
        instance_index: idx,
        stop,
        rx,
        handle: Some(handle),
        status: "Đang khởi động".into(),
        log: Vec::new(),
        screen: None,
        screen_gen: 0,
        runs: 0,
    }
}

/// Forwards executor events into the job channel.
fn bridge(tx: &Sender<JobMsg>) -> Sender<Event> {
    let (etx, erx) = channel::<Event>();
    let tx = tx.clone();
    std::thread::spawn(move || {
        while let Ok(e) = erx.recv() {
            if tx.send(JobMsg::Event(e)).is_err() {
                break;
            }
        }
    });
    etx
}

fn worker(mut scenario: Scenario, ld_dir: PathBuf, instance: Instance, stop: Arc<AtomicBool>, tx: Sender<JobMsg>) {
    let status = |s: String| {
        let _ = tx.send(JobMsg::Status(s));
    };
    loop {
        if let Schedule::Daily { times } = &scenario.schedule {
            let Some(wait) = until_next(times) else {
                status("Chưa có giờ chạy hợp lệ".into());
                return;
            };
            let at = Local::now() + wait;
            status(format!("Chờ đến {}", at.format("%H:%M %d/%m")));
            if !sleep_checked(wait.to_std().unwrap_or_default(), &stop) {
                break;
            }
        }

        status("Đang chạy".into());
        // A fresh, empty AI slot per run: if the run needs the AI it starts it,
        // and it is killed (RAM freed) as soon as `runner` is dropped below.
        let ai = Arc::new(Mutex::new(None));
        let mut runner = executor::new_runner(&ld_dir, instance.clone(), scenario.dir(), ai, false, stop.clone(), bridge(&tx));
        let mut steps = scenario.steps.clone();
        let result = runner.ensure_device().and_then(|_| runner.run(&mut steps));
        let changed = runner.changed;
        drop(runner);

        if changed {
            scenario.steps = steps;
            if scenario.save().is_ok() {
                let _ = tx.send(JobMsg::Updated(scenario.clone()));
            }
        }
        match result {
            Ok(()) => status(format!("Xong lượt lúc {}", Local::now().format("%H:%M:%S"))),
            Err(e) if e.downcast_ref::<Stopped>().is_some() => break,
            Err(e) => status(format!("Lỗi: {e:#}")),
        }

        if let Schedule::Repeat { pause_minutes } = scenario.schedule {
            if pause_minutes > 0 {
                status(format!("Nghỉ {pause_minutes} phút"));
            }
            // Short breather even for back-to-back runs, so an error can't spin the CPU.
            let pause = Duration::from_secs(pause_minutes as u64 * 60).max(Duration::from_secs(3));
            if !sleep_checked(pause, &stop) {
                break;
            }
        }
        if stop.load(Ordering::Relaxed) {
            break;
        }
    }
    status("Đã dừng".into());
}

/// Sleeps up to `d`; returns false if stopped meanwhile.
fn sleep_checked(d: Duration, stop: &AtomicBool) -> bool {
    let end = std::time::Instant::now() + d;
    while std::time::Instant::now() < end {
        if stop.load(Ordering::Relaxed) {
            return false;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    !stop.load(Ordering::Relaxed)
}

pub fn parse_times(text: &str) -> Vec<String> {
    text.split([',', ';', ' '])
        .filter_map(|t| NaiveTime::parse_from_str(t.trim(), "%H:%M").ok())
        .map(|t| t.format("%H:%M").to_string())
        .collect()
}

/// Time until the next "HH:MM" slot (today or tomorrow).
fn until_next(times: &[String]) -> Option<TimeDelta> {
    let now = Local::now();
    times
        .iter()
        .filter_map(|t| NaiveTime::parse_from_str(t, "%H:%M").ok())
        .filter_map(|t| {
            let today = now.date_naive().and_time(t).and_local_timezone(Local).single()?;
            let next = if today > now { today } else { today + TimeDelta::days(1) };
            Some(next - now)
        })
        .min()
}
