//! Windows integration: tray icon, the emergency hotkey, starting with Windows.

use std::os::windows::process::CommandExt;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Result, bail};
use eframe::egui::{self, ViewportCommand};
use global_hotkey::hotkey::{Code, HotKey, Modifiers};
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

use crate::device::CREATE_NO_WINDOW;
use crate::jobs;

/// Set when the user really wants to quit (closing the window otherwise hides it).
pub static QUITTING: AtomicBool = AtomicBool::new(false);

pub const HOTKEY_LABEL: &str = "Ctrl+F12";

/// Keeps the tray icon and hotkey registration alive.
pub struct Desktop {
    _tray: Option<TrayIcon>,
    _hotkeys: Option<GlobalHotKeyManager>,
}

pub fn show_window(ctx: &egui::Context) {
    ctx.send_viewport_cmd(ViewportCommand::Visible(true));
    ctx.send_viewport_cmd(ViewportCommand::Minimized(false));
    ctx.send_viewport_cmd(ViewportCommand::Focus);
    ctx.request_repaint();
}

pub fn hide_window(ctx: &egui::Context) {
    ctx.send_viewport_cmd(ViewportCommand::Visible(false));
}

/// Tray menu and hotkey. Their handlers run inside the window's event loop, so they work
/// while the window is hidden.
pub fn setup(ctx: &egui::Context, hotkey: bool) -> Desktop {
    let tray = (|| -> Result<TrayIcon> {
        let open = MenuItem::new("Mở OmniPilot", true, None);
        let stop = MenuItem::new(format!("Dừng tất cả ({HOTKEY_LABEL})"), true, None);
        let quit = MenuItem::new("Thoát hẳn", true, None);
        let menu = Menu::new();
        menu.append_items(&[&open, &stop, &PredefinedMenuItem::separator(), &quit])?;
        let (open_id, stop_id, quit_id) = (open.id().clone(), stop.id().clone(), quit.id().clone());
        let c = ctx.clone();
        MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
            if e.id == open_id {
                show_window(&c);
            } else if e.id == stop_id {
                jobs::stop_all();
            } else if e.id == quit_id {
                QUITTING.store(true, Ordering::Relaxed);
                jobs::stop_all();
                show_window(&c);
                c.send_viewport_cmd(ViewportCommand::Close);
            }
        }));
        let c = ctx.clone();
        TrayIconEvent::set_event_handler(Some(move |e: TrayIconEvent| {
            if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = e {
                show_window(&c);
            }
        }));
        Ok(TrayIconBuilder::new().with_menu(Box::new(menu)).with_tooltip("OmniPilot").with_icon(icon()?).build()?)
    })()
    .ok();

    let hotkeys = if hotkey {
        (|| -> Result<GlobalHotKeyManager> {
            let manager = GlobalHotKeyManager::new()?;
            let key = HotKey::new(Some(Modifiers::CONTROL), Code::F12);
            manager.register(key)?;
            let id = key.id();
            GlobalHotKeyEvent::set_event_handler(Some(move |e: GlobalHotKeyEvent| {
                if e.id == id && e.state == HotKeyState::Pressed {
                    jobs::stop_all();
                }
            }));
            Ok(manager)
        })()
        .ok()
    } else {
        None
    };
    Desktop { _tray: tray, _hotkeys: hotkeys }
}

/// The app icon: a purple disc with a white ring, drawn in code.
fn icon() -> Result<Icon> {
    let n = 32u32;
    let mut rgba = Vec::with_capacity((n * n * 4) as usize);
    let c = (n as f32 - 1.0) / 2.0;
    for y in 0..n {
        for x in 0..n {
            let d = ((x as f32 - c).powi(2) + (y as f32 - c).powi(2)).sqrt();
            let px = if d > 15.5 {
                [0, 0, 0, 0]
            } else if (8.0..11.0).contains(&d) {
                [255, 255, 255, 255]
            } else {
                [0x72, 0x5e, 0xfa, 255]
            };
            rgba.extend_from_slice(&px);
        }
    }
    Ok(Icon::from_rgba(rgba, n, n)?)
}

const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";

fn reg(args: &[&str]) -> Result<std::process::Output> {
    Ok(Command::new("reg").args(args).creation_flags(CREATE_NO_WINDOW).output()?)
}

/// Starts OmniPilot (hidden in the tray) when the user logs in.
pub fn set_autostart(on: bool) -> Result<()> {
    if on {
        let exe = std::env::current_exe()?;
        let cmd = format!("\"{}\" --tray", exe.display());
        let out = reg(&["add", RUN_KEY, "/v", "OmniPilot", "/t", "REG_SZ", "/d", &cmd, "/f"])?;
        if !out.status.success() {
            bail!("không ghi được registry: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
    } else {
        let _ = reg(&["delete", RUN_KEY, "/v", "OmniPilot", "/f"]);
    }
    Ok(())
}

pub fn autostart_enabled() -> bool {
    reg(&["query", RUN_KEY, "/v", "OmniPilot"]).map(|o| o.status.success()).unwrap_or(false)
}
