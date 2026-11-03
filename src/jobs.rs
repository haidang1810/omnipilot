//! Background jobs: one thread per started scenario, following its schedule.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use chrono::{Local, NaiveTime, TimeDelta};
use image::RgbaImage;

use crate::device::{self, Instance};
use crate::executor::{self, Event, Stopped};
use crate::script::{HistoryEntry, Limits, Scenario, Schedule};

pub enum JobMsg {
    Event(Event),
    Status(String),
    /// The scenario was re-learned during a run and saved; the UI should reload it.
    Updated(Scenario),
}

pub struct Job {
    pub scenario_id: String,
    pub instance_index: u32,
    pub instance_name: String,
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
    let name = instance.name.clone();
    register_stop(&stop);
    let stop2 = stop.clone();
    let handle = std::thread::spawn(move || worker(scenario, ld_dir, instance, stop2, tx));
    Job {
        scenario_id: id,
        instance_index: idx,
        instance_name: name,
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

/// Stop flags of every job ever started, for "Dừng tất cả" from the tray or the hotkey
/// (which work even while the window is hidden).
static STOP_FLAGS: Mutex<Vec<std::sync::Weak<AtomicBool>>> = Mutex::new(Vec::new());

fn register_stop(flag: &Arc<AtomicBool>) {
    let mut flags = STOP_FLAGS.lock().unwrap();
    flags.retain(|f| f.strong_count() > 0);
    flags.push(Arc::downgrade(flag));
}

/// Stops every running job.
pub fn stop_all() {
    for f in STOP_FLAGS.lock().unwrap().iter().filter_map(|f| f.upgrade()) {
        f.store(true, Ordering::Relaxed);
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
    let started_at = std::time::Instant::now();
    let mut runs_done = 0u32;
    let limits = scenario.limits.clone();
    let daily = matches!(scenario.schedule, Schedule::Daily { .. });
    loop {
        if let Schedule::Daily { times } = &scenario.schedule {
            let Some(wait) = until_next(times) else {
                status("Chưa có giờ chạy hợp lệ".into());
                return;
            };
            let at = Local::now() + wait;
            status(format!("Chờ đến {}", at.format("%H:%M %d/%m")));
            let wait = wait.to_std().unwrap_or_default();
            // Open the emulator a little early so the run starts on time.
            let early = Duration::from_secs(90);
            if !sleep_checked(wait.saturating_sub(early), &stop) {
                break;
            }
            if wait > early {
                let _ = device::launch_if_closed(&ld_dir, &instance);
                if !sleep_checked(early, &stop) {
                    break;
                }
            }
        }
        // "Chỉ chạy từ … đến …".
        if let Some(wait) = until_window(&limits) {
            status(format!("Ngoài khung giờ, chờ đến {}", limits.from));
            close_if_wanted(&scenario, &ld_dir, &instance, &status);
            if !sleep_checked(wait, &stop) {
                break;
            }
        }

        // Wait for our turn on this emulator, and for a free emulator slot (RAM limit).
        status("Đang chờ đến lượt dùng giả lập".into());
        let Some(_turn) = gate::acquire(instance.index, daily, &stop, &|who| status(format!("Chờ \"{who}\" chạy xong lượt trên giả lập này"))) else {
            break;
        };
        let Some(_slot) = gate::acquire_slot(instance.index, &stop, &|n| status(format!("Chờ bớt giả lập đang chạy (tối đa {n} cùng lúc)"))) else {
            break;
        };
        gate::set_owner(instance.index, &scenario.name);

        // One scheduled run, retried after a successful recovery.
        let mut attempt = 0;
        let mut vars = scenario.load_vars(instance.index);
        let stopped = loop {
            status(if attempt == 0 { "Đang chạy".into() } else { format!("Chạy lại sau khi phục hồi (lần {attempt})") });
            // A fresh, empty AI slot per run: if the run needs the AI it starts it,
            // and it is killed (RAM freed) when `runner` is dropped at the end of the round.
            let ai = Arc::new(Mutex::new(None));
            let dir = scenario.dir();
            let mut runner = executor::new_runner(&ld_dir, instance.clone(), dir.clone(), ai, false, stop.clone(), bridge(&tx));
            runner.popups = scenario.popups.clone();
            runner.popup_dir = dir.clone();
            runner.vars = vars.clone();
            let mut steps = scenario.steps.clone();
            let started = Local::now();
            let _awake = KeepAwake::new();
            let result = runner
                .ensure_device()
                .and_then(|_| runner.ensure_app(&scenario.recovery, &dir))
                .and_then(|_| runner.run_all(&mut steps));
            vars = runner.vars.clone();
            scenario.save_vars(instance.index, &vars);
            let was_stopped = matches!(&result, Err(e) if e.downcast_ref::<Stopped>().is_some());
            if !was_stopped {
                record_history(&scenario, &instance.name, started, &result, runner.last_screen.as_ref());
            }
            if runner.changed {
                scenario.steps = steps;
                if scenario.save().is_ok() {
                    let _ = tx.send(JobMsg::Updated(scenario.clone()));
                }
            }
            match result {
                Ok(()) => {
                    status(format!("Xong lượt lúc {}", Local::now().format("%H:%M:%S")));
                    break false;
                }
                Err(_) if was_stopped => break true,
                Err(e) => {
                    status(format!("Lỗi: {e:#}"));
                    let r = &scenario.recovery;
                    let can_recover = r.home.is_some() || !r.package.trim().is_empty();
                    if !can_recover || attempt >= r.retries {
                        break false;
                    }
                    status("Đang tự phục hồi".into());
                    match runner.recover(r, &dir) {
                        Ok(true) => attempt += 1,
                        Ok(false) => {
                            status("Không phục hồi được".into());
                            break false;
                        }
                        Err(e) if e.downcast_ref::<Stopped>().is_some() => break true,
                        Err(e) => {
                            status(format!("Phục hồi lỗi: {e:#}"));
                            break false;
                        }
                    }
                }
            }
        };
        gate::set_owner(instance.index, "");
        drop(_slot);
        drop(_turn);
        if stopped {
            break;
        }
        runs_done += 1;

        if scenario.schedule == Schedule::Once {
            close_if_wanted(&scenario, &ld_dir, &instance, &status);
            return; // the "Xong lượt" / "Lỗi" status above is the final word
        }
        if limits.max_runs > 0 && runs_done >= limits.max_runs {
            status(format!("Đã chạy đủ {runs_done} lượt"));
            close_if_wanted(&scenario, &ld_dir, &instance, &status);
            return;
        }
        if limits.max_minutes > 0 && started_at.elapsed() >= Duration::from_secs(limits.max_minutes as u64 * 60) {
            status(format!("Đã chạy đủ {} phút", limits.max_minutes));
            close_if_wanted(&scenario, &ld_dir, &instance, &status);
            return;
        }
        if daily {
            close_if_wanted(&scenario, &ld_dir, &instance, &status);
        }
        if let Schedule::Repeat { pause_minutes } = scenario.schedule {
            if pause_minutes > 0 {
                status(format!("Nghỉ {pause_minutes} phút"));
            }
            // Short breather even for back-to-back runs, so an error can't spin the CPU
            // and a waiting daily scenario gets the emulator.
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

/// "Tắt giả lập khi xong".
fn close_if_wanted(sc: &Scenario, ld_dir: &std::path::Path, instance: &Instance, status: &dyn Fn(String)) {
    if !sc.close_emulator_after || gate::busy(instance.index) {
        return;
    }
    status(format!("Tắt giả lập \"{}\"", instance.name));
    if let Err(e) = device::quit_instance(ld_dir, instance.index) {
        status(format!("Không tắt được giả lập: {e:#}"));
    }
}

/// Time until the "chỉ chạy từ … đến …" window opens; None when inside it (or no window).
fn until_window(l: &Limits) -> Option<Duration> {
    let from = NaiveTime::parse_from_str(l.from.trim(), "%H:%M").ok()?;
    let to = NaiveTime::parse_from_str(l.to.trim(), "%H:%M").ok()?;
    let now = Local::now();
    let t = now.time();
    let inside = if from <= to { t >= from && t < to } else { t >= from || t < to };
    if inside {
        return None;
    }
    let today = now.date_naive().and_time(from).and_local_timezone(Local).single()?;
    let next = if today > now { today } else { today + TimeDelta::days(1) };
    (next - now).to_std().ok()
}

/// Keeps Windows from sleeping while a run is in progress. The request belongs to the
/// calling thread, so each job thread sets and clears its own.
struct KeepAwake;

impl KeepAwake {
    fn new() -> Self {
        set_awake(true);
        KeepAwake
    }
}

impl Drop for KeepAwake {
    fn drop(&mut self) {
        set_awake(false);
    }
}

fn set_awake(on: bool) {
    use windows_sys::Win32::System::Power::*;
    // The flag applies to the calling thread; ES_CONTINUOUS keeps it until changed.
    let flags = if on { ES_CONTINUOUS | ES_SYSTEM_REQUIRED } else { ES_CONTINUOUS };
    unsafe {
        SetThreadExecutionState(flags);
    }
}

/// Who may use an emulator now, and how many emulators may run at once.
pub mod gate {
    use std::collections::{HashMap, HashSet, VecDeque};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Condvar, Mutex, OnceLock};
    use std::time::Duration;

    #[derive(Default)]
    struct State {
        /// Per emulator: ticket being served, and tickets waiting (daily ones first).
        current: HashMap<u32, u64>,
        queue: HashMap<u32, VecDeque<u64>>,
        owner: HashMap<u32, String>,
        next_ticket: u64,
        /// Emulators running a scenario right now (for the RAM limit).
        active: HashSet<u32>,
    }

    struct Gate {
        state: Mutex<State>,
        cv: Condvar,
    }

    fn gate() -> &'static Gate {
        static G: OnceLock<Gate> = OnceLock::new();
        G.get_or_init(|| Gate { state: Mutex::new(State::default()), cv: Condvar::new() })
    }

    /// Maximum emulators running at once (0 = unlimited).
    static MAX_PARALLEL: AtomicUsize = AtomicUsize::new(0);

    pub fn set_max_parallel(n: usize) {
        MAX_PARALLEL.store(n, Ordering::Relaxed);
        gate().cv.notify_all();
    }

    pub struct Turn(u32, u64);
    impl Drop for Turn {
        fn drop(&mut self) {
            let g = gate();
            let mut st = g.state.lock().unwrap();
            if st.current.get(&self.0) == Some(&self.1) {
                st.current.remove(&self.0);
            }
            if let Some(q) = st.queue.get_mut(&self.0) {
                q.retain(|t| *t != self.1);
            }
            g.cv.notify_all();
        }
    }

    pub struct Slot(u32);
    impl Drop for Slot {
        fn drop(&mut self) {
            let g = gate();
            g.state.lock().unwrap().active.remove(&self.0);
            g.cv.notify_all();
        }
    }

    /// Waits until this job may use the emulator. Daily (timed) jobs go before repeating ones.
    /// Returns None if stopped while waiting.
    pub fn acquire(instance: u32, priority: bool, stop: &AtomicBool, waiting: &dyn Fn(&str)) -> Option<Turn> {
        let g = gate();
        let mut st = g.state.lock().unwrap();
        st.next_ticket += 1;
        let ticket = st.next_ticket;
        let q = st.queue.entry(instance).or_default();
        if priority { q.push_front(ticket) } else { q.push_back(ticket) }
        let mut told = String::new();
        loop {
            let free = !st.current.contains_key(&instance);
            let first = st.queue.get(&instance).and_then(|q| q.front()) == Some(&ticket);
            if free && first {
                st.queue.get_mut(&instance).unwrap().pop_front();
                st.current.insert(instance, ticket);
                return Some(Turn(instance, ticket));
            }
            if stop.load(Ordering::Relaxed) {
                st.queue.get_mut(&instance).unwrap().retain(|t| *t != ticket);
                g.cv.notify_all();
                return None;
            }
            let who = st.owner.get(&instance).cloned().unwrap_or_default();
            if !who.is_empty() && who != told {
                waiting(&who);
                told = who;
            }
            st = g.cv.wait_timeout(st, Duration::from_millis(500)).unwrap().0;
        }
    }

    /// Waits for a free emulator slot under the "tối đa N giả lập" limit.
    pub fn acquire_slot(instance: u32, stop: &AtomicBool, waiting: &dyn Fn(usize)) -> Option<Slot> {
        let g = gate();
        let mut st = g.state.lock().unwrap();
        let mut told = false;
        loop {
            let max = MAX_PARALLEL.load(Ordering::Relaxed);
            if max == 0 || st.active.contains(&instance) || st.active.len() < max {
                st.active.insert(instance);
                return Some(Slot(instance));
            }
            if stop.load(Ordering::Relaxed) {
                return None;
            }
            if !told {
                waiting(max);
                told = true;
            }
            st = g.cv.wait_timeout(st, Duration::from_millis(500)).unwrap().0;
        }
    }

    pub fn set_owner(instance: u32, name: &str) {
        gate().state.lock().unwrap().owner.insert(instance, name.to_string());
    }

    /// Another job is using or waiting for this emulator.
    pub fn busy(instance: u32) -> bool {
        let st = gate().state.lock().unwrap();
        st.current.contains_key(&instance) || st.queue.get(&instance).is_some_and(|q| !q.is_empty())
    }
}

/// Saves one run in the scenario's history, with the screen at the moment it failed.
pub fn record_history(sc: &Scenario, instance: &str, started: chrono::DateTime<Local>, result: &anyhow::Result<()>, screen: Option<&RgbaImage>) {
    let ended = Local::now();
    let screenshot = match (result, screen) {
        (Err(_), Some(img)) => {
            let name = format!("fail_{}_{}.png", ended.format("%Y%m%d_%H%M%S"), uuid::Uuid::new_v4().simple().to_string().get(..6).unwrap_or(""));
            let _ = std::fs::create_dir_all(sc.history_dir());
            img.save(sc.history_dir().join(&name)).ok().map(|_| name)
        }
        _ => None,
    };
    let entry = HistoryEntry {
        started: started.format("%d/%m %H:%M:%S").to_string(),
        ended: ended.format("%H:%M:%S").to_string(),
        instance: instance.to_string(),
        ok: result.is_ok(),
        message: match result {
            Ok(()) => "Xong".into(),
            Err(e) => format!("{e:#}"),
        },
        screenshot,
    };
    let _ = sc.add_history(entry);
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
            let today = now
                .date_naive()
                .and_time(t)
                .and_local_timezone(Local)
                .single()?;
            let next = if today > now {
                today
            } else {
                today + TimeDelta::days(1)
            };
            Some(next - now)
        })
        .min()
}
