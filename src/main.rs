#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod ai;
mod app;
mod cli;
mod desktop;
mod device;
mod executor;
mod jobs;
mod ocr;
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
    if args.len() >= 4 && args[1] == "--job" {
        if let Err(e) = cli::job(args[2].parse().unwrap_or(0), &args[3..]) {
            eprintln!("Lỗi: {e:#}");
            std::process::exit(1);
        }
        return Ok(());
    }
    if args.len() >= 4 && args[1] == "--run" {
        if let Err(e) = cli::run_saved(args[2].parse().unwrap_or(0), &args[3], &args[4..]) {
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
    if args.len() == 3 && args[1] == "--autostart" {
        let on = args[2] == "on";
        match desktop::set_autostart(on) {
            Ok(()) => println!("Khởi động cùng Windows: {}", desktop::autostart_enabled()),
            Err(e) => eprintln!("Lỗi: {e:#}"),
        }
        return Ok(());
    }
    // Started with Windows: stay in the tray.
    let tray = args.iter().any(|a| a == "--tray");
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_visible(!tray)
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
