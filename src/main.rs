#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod ai;
mod app;
mod cli;
mod device;
mod executor;
mod jobs;
mod paths;
mod script;
mod setup;
mod theme;
mod vision;

fn main() -> eframe::Result {
    let args: Vec<String> = std::env::args().collect();
    if args.len() >= 3 && args[1] == "--analyse" {
        if let Err(e) = cli::analyse(&args[2..]) {
            eprintln!("Lỗi: {e:#}");
            std::process::exit(1);
        }
        return Ok(());
    }
    if args.len() >= 4 && args[1] == "--find" {
        if let Err(e) = cli::find(&args[2], &args[3], args.get(4).map(String::as_str)) {
            eprintln!("Lỗi: {e:#}");
            std::process::exit(1);
        }
        return Ok(());
    }
    if args.len() == 4 && args[1] == "--run" {
        if let Err(e) = cli::run_saved(args[2].parse().unwrap_or(0), &args[3]) {
            eprintln!("Lỗi: {e:#}");
            std::process::exit(1);
        }
        return Ok(());
    }
    if args.len() >= 4 && args[1] == "--cli" {
        if let Err(e) = cli::run(args[2].parse().unwrap_or(0), &args[3..]) {
            eprintln!("Lỗi: {e:#}");
            std::process::exit(1);
        }
        return Ok(());
    }
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("OmniPilot")
            .with_inner_size([1200.0, 780.0])
            .with_min_inner_size([900.0, 600.0]),
        ..Default::default()
    };
    eframe::run_native(
        "OmniPilot",
        options,
        Box::new(|cc| Ok(Box::new(app::App::new(cc)))),
    )
}
