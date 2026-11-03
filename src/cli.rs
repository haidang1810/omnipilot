//! Headless test mode: `omnipilot --cli <instance index> "<bước 1>" "<bước 2>" ...`.
//! Analyses each step, does a trial run with the AI, exports/imports the scenario,
//! then replays the imported copy without the AI.

use std::sync::atomic::AtomicBool;
use std::sync::mpsc::channel;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{Context, Result};

use crate::ai::AiServer;
use crate::device;
use crate::executor::{self, Event};
use crate::script::{self, Node, Scenario};

/// `omnipilot --analyse "<bước 1>" ...`: shows how the AI reads each step, taps nothing.
pub fn analyse(texts: &[String]) -> Result<()> {
    let t = Instant::now();
    let server = AiServer::start()?;
    println!("AI bật sau {:.1}s", t.elapsed().as_secs_f32());
    for text in texts {
        let t = Instant::now();
        match server.compile_step(text) {
            Ok(s) => println!("  \"{text}\" -> {}  ({:.1}s)\n      {}", s.describe(), t.elapsed().as_secs_f32(), serde_json::to_string(&s)?),
            Err(e) => println!("  \"{text}\" -> LỖI: {e:#}"),
        }
    }
    Ok(())
}

pub fn run(index: u32, texts: &[String]) -> Result<()> {
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
    sc.steps = texts.iter().map(|t| Node { text: t.clone(), step: None }).collect();

    let t = Instant::now();
    let ai = Arc::new(Mutex::new(Some(AiServer::start()?)));
    println!("AI bật sau {:.1}s", t.elapsed().as_secs_f32());
    {
        let guard = ai.lock().unwrap();
        let server = guard.as_ref().unwrap();
        let mut err = None;
        script::for_each_leaf_mut(&mut sc.steps, &mut |n| match server.compile_step(&n.text) {
            Ok(s) => {
                println!("  \"{}\" -> {}", n.text, s.describe());
                n.step = Some(s);
            }
            Err(e) => err = Some(e),
        });
        if let Some(e) = err {
            return Err(e);
        }
    }

    let t = Instant::now();
    let mut runner = executor::new_runner(&ld, inst.clone(), sc.dir(), ai.clone(), true, Arc::new(AtomicBool::new(false)), tx.clone());
    runner.ensure_device()?;
    let result = runner.run(&mut sc.steps);
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
    println!("Xuất/nhập: file {} bytes, kích thước {:?}", std::fs::metadata(&file)?.len(), imported.screen_size);
    sc.delete();
    std::fs::remove_file(&file).ok();

    let t = Instant::now();
    let empty = Arc::new(Mutex::new(None));
    let mut runner = executor::new_runner(&ld, inst, imported.dir(), empty.clone(), false, Arc::new(AtomicBool::new(false)), tx);
    let result = runner.run(&mut imported.steps);
    drop(runner);
    imported.delete();
    result?;
    println!("Chạy bản đã nhập (không AI) xong sau {:.1}s, AI có bị bật: {}", t.elapsed().as_secs_f32(), empty.lock().unwrap().is_some());
    Ok(())
}
