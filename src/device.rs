//! LDPlayer discovery and adb control.

use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use anyhow::{Context, Result, bail};
use image::RgbaImage;

pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;

fn run(exe: &Path, args: &[&str]) -> Result<Output> {
    Command::new(exe)
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .with_context(|| format!("không chạy được {}", exe.display()))
}

/// Looks for an LDPlayer install (folder containing ldconsole.exe and adb.exe) on every drive.
pub fn find_ldplayer() -> Option<PathBuf> {
    for drive in b'C'..=b'Z' {
        let root = PathBuf::from(format!("{}:\\", drive as char));
        if !root.exists() {
            continue;
        }
        for parent in [
            root.join("LDPlayer"),
            root.join("Program Files").join("LDPlayer"),
            root.clone(),
        ] {
            let Ok(entries) = std::fs::read_dir(&parent) else {
                continue;
            };
            let mut found: Vec<PathBuf> = entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| {
                    let name = p
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("")
                        .to_lowercase();
                    name.starts_with("ldplayer") && is_ldplayer_dir(p)
                })
                .collect();
            // Prefer the newest version (LDPlayer14 over LDPlayer9).
            found.sort_by_key(|p| std::cmp::Reverse(version_of(p)));
            if let Some(p) = found.into_iter().next() {
                return Some(p);
            }
        }
    }
    None
}

pub fn is_ldplayer_dir(dir: &Path) -> bool {
    dir.join("ldconsole.exe").exists() && dir.join("adb.exe").exists()
}

fn version_of(p: &Path) -> u32 {
    let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
    name.chars()
        .filter(|c| c.is_ascii_digit())
        .collect::<String>()
        .parse()
        .unwrap_or(0)
}

#[derive(Clone, Debug)]
pub struct Instance {
    pub index: u32,
    pub name: String,
    pub running: bool,
    pub size: [u32; 2],
}

impl Instance {
    pub fn serial(&self) -> String {
        format!("emulator-{}", 5554 + 2 * self.index)
    }
}

/// `ldconsole list2` → index,title,top_hwnd,bind_hwnd,android_started,pid,vbox_pid,w,h,dpi
pub fn list_instances(ld_dir: &Path) -> Result<Vec<Instance>> {
    let out = run(&ld_dir.join("ldconsole.exe"), &["list2"])?;
    let text = String::from_utf8_lossy(&out.stdout);
    Ok(text
        .lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.trim().split(',').collect();
            if f.len() < 9 {
                return None;
            }
            let size = [f[7].parse().unwrap_or(0), f[8].parse().unwrap_or(0)];
            Some(Instance {
                index: f[0].parse().ok()?,
                name: f[1].to_string(),
                running: f[4] == "1",
                size,
            })
        })
        .collect())
}

pub fn launch_instance(ld_dir: &Path, index: u32) -> Result<()> {
    run(
        &ld_dir.join("ldconsole.exe"),
        &["launch", "--index", &index.to_string()],
    )?;
    Ok(())
}

/// A captured screen: raw PNG (sent to the AI) and the decoded image.
pub struct Frame {
    pub png: Vec<u8>,
    pub img: RgbaImage,
}

#[derive(Clone)]
pub struct Device {
    adb: PathBuf,
    serial: String,
}

impl Device {
    pub fn new(ld_dir: &Path, instance: &Instance) -> Self {
        Self {
            adb: ld_dir.join("adb.exe"),
            serial: instance.serial(),
        }
    }

    fn adb(&self, args: &[&str]) -> Result<Output> {
        let mut full = vec!["-s", self.serial.as_str()];
        full.extend_from_slice(args);
        let out = run(&self.adb, &full)?;
        if !out.status.success() {
            bail!(
                "adb {:?} lỗi: {}",
                args,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(out)
    }

    pub fn is_online(&self) -> bool {
        self.adb(&["get-state"])
            .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "device")
            .unwrap_or(false)
    }

    pub fn connect(&self) {
        let _ = run(&self.adb, &["connect", &self.serial]);
    }

    pub fn screenshot(&self) -> Result<Frame> {
        let out = self.adb(&["exec-out", "screencap", "-p"])?;
        let img = image::load_from_memory(&out.stdout)
            .context("ảnh chụp màn hình không hợp lệ")?
            .to_rgba8();
        Ok(Frame {
            png: out.stdout,
            img,
        })
    }

    pub fn tap(&self, x: u32, y: u32) -> Result<()> {
        self.adb(&["shell", "input", "tap", &x.to_string(), &y.to_string()])?;
        Ok(())
    }

    pub fn back(&self) -> Result<()> {
        self.adb(&["shell", "input", "keyevent", "4"])?;
        Ok(())
    }

    pub fn swipe(&self, from: (u32, u32), to: (u32, u32), ms: u32) -> Result<()> {
        let args = [from.0, from.1, to.0, to.1, ms].map(|v| v.to_string());
        let mut full = vec!["shell", "input", "swipe"];
        full.extend(args.iter().map(String::as_str));
        self.adb(&full)?;
        Ok(())
    }
}
