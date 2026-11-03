//! Developer commands (no window):
//! - `--analyse "<bước>" ...`: how the AI reads each step; taps nothing.
//! - `--find <ảnh.png> "<bước>" [x1,y1,x2,y2]`: reads the step and searches a saved screenshot.
//! - `--cli <giả lập> "<bước 1>" ...`: trial run with the AI, export/import, replay without it.

use std::sync::atomic::AtomicBool;
use std::sync::mpsc::channel;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{Context, Result};

use crate::ai::{AiServer, Query};
use crate::device;
use crate::executor::{self, Event};
use crate::script::{self, Node, Scenario};

fn print_node(n: &Node) {
    match &n.step {
        Some(s) => println!(
            "  \"{}\" -> {}\n      {}",
            n.text,
            s.describe(),
            serde_json::to_string(s).unwrap_or_default()
        ),
        None => println!("  \"{}\" -> (chưa hiểu)", n.text),
    }
    if let Some(o) = &n.otherwise {
        println!(
            "      nếu không thấy thì: \"{}\" -> {}",
            o.text,
            o.step.as_ref().map(|s| s.describe()).unwrap_or_default()
        );
    }
}

fn compile_all(server: &AiServer, nodes: &mut [Node]) -> Result<()> {
    let mut err = None;
    script::for_each_leaf_mut(nodes, &mut |n| {
        if n.step.is_none() && err.is_none() {
            if let Err(e) = server.compile_node(n) {
                err = Some(e);
            }
        }
    });
    err.map_or(Ok(()), Err)
}

pub fn analyse(texts: &[String]) -> Result<()> {
    let t = Instant::now();
    let server = AiServer::start()?;
    println!("AI bật sau {:.1}s", t.elapsed().as_secs_f32());
    let mut nodes: Vec<Node> = texts.iter().map(|t| Node::with_text(t)).collect();
    let t = Instant::now();
    compile_all(&server, &mut nodes)?;
    println!(
        "Đọc {} bước trong {:.1}s",
        nodes.len(),
        t.elapsed().as_secs_f32()
    );
    nodes.iter().for_each(print_node);
    Ok(())
}

pub fn find(png_path: &str, text: &str, region: Option<&str>) -> Result<()> {
    let png = std::fs::read(png_path).with_context(|| format!("không đọc được {png_path}"))?;
    let img = image::load_from_memory(&png)?.to_rgba8();
    let region = region.map(|r| {
        let v: Vec<f32> = r.split(',').filter_map(|x| x.trim().parse().ok()).collect();
        [v[0], v[1], v[2], v[3]]
    });
    let server = AiServer::start()?;
    let mut node = Node::with_text(text);
    server.compile_node(&mut node)?;
    print_node(&node);
    let step = node.step.as_ref().unwrap();
    let (target, hint) = match step {
        script::Step::Tap { target, hint, .. } | script::Step::TapAll { target, hint, .. } => {
            (target.as_str(), hint.as_str())
        }
        script::Step::WaitUntil { target, .. } => (target.as_str(), ""),
        _ => anyhow::bail!("bước này không phải tìm nút"),
    };
    let q = Query {
        target,
        hint,
        exclude: step.exclude(),
        region,
    };
    let t = Instant::now();
    let found = server.locate(&png, &img, &q, &|s| println!("{s}"))?;
    match found {
        Some(r) => println!(
            "=> BẤM tại {:?}  ({:.1}s)",
            r.center(),
            t.elapsed().as_secs_f32()
        ),
        None => {
            let next = if node.otherwise.is_some() {
                " -> làm bước \"nếu không thấy\""
            } else {
                ""
            };
            println!("=> KHÔNG THẤY{next}  ({:.1}s)", t.elapsed().as_secs_f32())
        }
    }
    Ok(())
}

/// `--run <giả lập> <thư mục kịch bản>`: replays a saved scenario like a scheduled run.
pub fn run_saved(index: u32, dir: &str) -> Result<()> {
    let ld = device::find_ldplayer().context("không tìm thấy LDPlayer")?;
    let inst = device::list_instances(&ld)?.into_iter().find(|i| i.index == index).context("không có instance này")?;
    let json = std::fs::read_to_string(std::path::Path::new(dir).join("scenario.json"))?;
    let mut sc: Scenario = serde_json::from_str(json.trim_start_matches('\u{feff}'))?;
    let (tx, rx) = channel::<Event>();
    std::thread::spawn(move || {
        while let Ok(e) = rx.recv() {
            if let Event::Log(l) = e {
                println!("  {l}");
            }
        }
    });
    let ai = Arc::new(Mutex::new(None));
    let t = Instant::now();
    let started = chrono::Local::now();
    let inst_name = inst.name.clone();
    let mut runner = executor::new_runner(&ld, inst, dir.into(), ai.clone(), false, Arc::new(AtomicBool::new(false)), tx);
    runner.popups = sc.popups.clone();
    runner.popup_dir = dir.into();
    runner.ensure_device()?;
    let mut result = runner.ensure_app(&sc.recovery, dir.as_ref()).and_then(|_| runner.run_all(&mut sc.steps));
    if let Err(e) = &result {
        let r = &sc.recovery;
        if r.home.is_some() || !r.package.trim().is_empty() {
            println!("Lỗi: {e:#} -> thử tự phục hồi");
            match runner.recover(r, dir.as_ref()) {
                Ok(ok) => println!("Phục hồi: {}", if ok { "được" } else { "không được" }),
                Err(e2) => println!("Phục hồi lỗi: {e2:#}"),
            }
        }
        result = Err(anyhow::anyhow!("{e:#}"));
    }
    let shot = runner.last_screen.clone();
    drop(runner);
    // Same history as a scheduled run (only for scenarios in the app's folder).
    if std::path::Path::new(dir).starts_with(crate::paths::scenarios_dir()) {
        crate::jobs::record_history(&sc, &inst_name, started, &result, shot.as_ref());
        println!("Lịch sử: {} lượt", sc.history().len());
    }
    println!("Xong sau {:.1}s, AI có bị bật: {}", t.elapsed().as_secs_f32(), ai.lock().unwrap().is_some());
    result
}

/// `--job <giả lập> <thư mục kịch bản>`: runs a saved scenario exactly like the Start
/// button (schedule, recovery and retries, history) and prints its status lines.
pub fn job(index: u32, dir: &str) -> Result<()> {
    let ld = device::find_ldplayer().context("không tìm thấy LDPlayer")?;
    let inst = device::list_instances(&ld)?.into_iter().find(|i| i.index == index).context("không có instance này")?;
    let json = std::fs::read_to_string(std::path::Path::new(dir).join("scenario.json"))?;
    let sc: Scenario = serde_json::from_str(json.trim_start_matches('\u{feff}'))?;
    let mut job = crate::jobs::start(sc, ld, inst);
    let mut shown = 0;
    while !job.is_finished() || shown < job.log.len() {
        job.poll();
        for line in &job.log[shown..] {
            println!("{line}");
        }
        shown = job.log.len();
        std::thread::sleep(std::time::Duration::from_millis(300));
    }
    // The worker's last status may arrive just after it finished.
    std::thread::sleep(std::time::Duration::from_millis(300));
    job.poll();
    for line in &job.log[shown..] {
        println!("{line}");
    }
    Ok(())
}

pub fn run(index: u32, texts: &[String]) -> Result<()> {
    let ld = device::find_ldplayer().context("không tìm thấy LDPlayer")?;
    let inst = device::list_instances(&ld)?
        .into_iter()
        .find(|i| i.index == index)
        .context("không có instance này")?;
    let (tx, rx) = channel::<Event>();
    std::thread::spawn(move || {
        while let Ok(e) = rx.recv() {
            if let Event::Log(l) = e {
                println!("  {l}");
            }
        }
    });

    let mut sc = Scenario::new();
    sc.name = "cli-test".into();
    sc.steps = texts.iter().map(|t| Node::with_text(t)).collect();

    let t = Instant::now();
    let ai = Arc::new(Mutex::new(Some(AiServer::start()?)));
    println!("AI bật sau {:.1}s", t.elapsed().as_secs_f32());
    compile_all(ai.lock().unwrap().as_ref().unwrap(), &mut sc.steps)?;
    sc.steps.iter().for_each(print_node);

    let t = Instant::now();
    let mut runner = executor::new_runner(
        &ld,
        inst.clone(),
        sc.dir(),
        ai.clone(),
        true,
        Arc::new(AtomicBool::new(false)),
        tx.clone(),
    );
    runner.ensure_device()?;
    let result = runner.run_all(&mut sc.steps);
    sc.screen_size = runner.screen_size;
    drop(runner);
    if let Err(e) = result {
        sc.delete();
        return Err(e);
    }
    println!("Chạy thử (AI) xong sau {:.1}s", t.elapsed().as_secs_f32());
    *ai.lock().unwrap() = None;
    println!("Đã tắt AI");

    // Share round-trip: export, import as a new scenario, replay the imported copy.
    sc.recorded = true;
    sc.save()?;
    let file = std::env::temp_dir().join("omnipilot-cli-test.omnipilot");
    sc.export(&file)?;
    let mut imported = Scenario::import(&file)?;
    println!(
        "Xuất/nhập: file {} bytes, kích thước {:?}",
        std::fs::metadata(&file)?.len(),
        imported.screen_size
    );
    sc.delete();
    std::fs::remove_file(&file).ok();

    let t = Instant::now();
    let empty = Arc::new(Mutex::new(None));
    let mut runner = executor::new_runner(
        &ld,
        inst,
        imported.dir(),
        empty.clone(),
        false,
        Arc::new(AtomicBool::new(false)),
        tx,
    );
    let result = runner.run_all(&mut imported.steps);
    drop(runner);
    imported.delete();
    result?;
    println!(
        "Chạy bản đã nhập (không AI) xong sau {:.1}s, AI có bị bật: {}",
        t.elapsed().as_secs_f32(),
        empty.lock().unwrap().is_some()
    );
    Ok(())
}
