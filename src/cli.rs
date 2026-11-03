//! Headless test mode: `omnipilot --cli <instance index> "<mô tả>"`.
//! Compiles the description, does a trial run with the AI, then replays without it.

use std::sync::atomic::AtomicBool;
use std::sync::mpsc::channel;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{Context, Result};

use crate::ai::AiServer;
use crate::device;
use crate::executor::{self, Event};
use crate::script::Scenario;

pub fn run(index: u32, description: &str) -> Result<()> {
    let ld = device::find_ldplayer().context("không tìm thấy LDPlayer")?;
    let inst = device::list_instances(&ld)?.into_iter().find(|i| i.index == index).context("không có instance này")?;
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
    sc.description = description.into();

    let t = Instant::now();
    let ai = Arc::new(Mutex::new(Some(AiServer::start()?)));
    println!("AI bật sau {:.1}s", t.elapsed().as_secs_f32());
    sc.steps = ai.lock().unwrap().as_ref().unwrap().compile(description)?;
    println!("Kịch bản:\n{}", serde_json::to_string_pretty(&sc.steps)?);

    let t = Instant::now();
    let mut runner = executor::new_runner(&ld, inst.clone(), sc.dir(), ai.clone(), true, Arc::new(AtomicBool::new(false)), tx.clone());
    runner.ensure_device()?;
    runner.run(&mut sc.steps)?;
    sc.screen_size = runner.screen_size;
    drop(runner);
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
        "Xuất/nhập: file {} bytes, id mới {} (cũ {}), kích thước {:?}",
        std::fs::metadata(&file)?.len(),
        imported.id,
        sc.id,
        imported.screen_size
    );
    sc.delete();
    std::fs::remove_file(&file).ok();

    let t = Instant::now();
    let empty = Arc::new(Mutex::new(None));
    let mut runner = executor::new_runner(&ld, inst, imported.dir(), empty.clone(), false, Arc::new(AtomicBool::new(false)), tx);
    runner.run(&mut imported.steps)?;
    println!("Chạy bản đã nhập (không AI) xong sau {:.1}s, AI có bị bật: {}", t.elapsed().as_secs_f32(), empty.lock().unwrap().is_some());
    imported.delete();
    Ok(())
}
