//! Local AI: a llama-server child process that only lives while it is needed.
//! Dropping `AiServer` kills the process so its RAM/VRAM is returned immediately.

use std::fs::{self, File};
use std::io::Cursor;
use std::net::TcpListener;
use std::os::windows::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use base64::Engine;
use image::RgbaImage;
use serde_json::{Value, json};

use crate::device::CREATE_NO_WINDOW;
use crate::script::Step;
use crate::{paths, setup};

pub struct AiServer {
    child: Child,
    base: String,
    agent: ureq::Agent,
}

/// Bounding box in screen pixels.
#[derive(Clone, Copy, Debug)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

impl Rect {
    pub fn center(&self) -> (u32, u32) {
        (self.x + self.w / 2, self.y + self.h / 2)
    }
}

fn pid_file() -> std::path::PathBuf {
    paths::runtime_dir().join("llama-server.pid")
}

/// Kills a llama-server left behind by a previous crash of the app.
pub fn kill_stale() {
    if let Ok(pid) = fs::read_to_string(pid_file()) {
        let _ = Command::new("taskkill")
            .args(["/PID", pid.trim(), "/F"])
            .creation_flags(CREATE_NO_WINDOW)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = fs::remove_file(pid_file());
    }
}

/// Puts the child in a job object that is closed when this app exits (even on a crash),
/// which makes Windows kill llama-server so it never keeps holding RAM.
fn kill_with_app(child: &Child) {
    use std::os::windows::io::AsRawHandle;
    use std::sync::OnceLock;
    use windows_sys::Win32::System::JobObjects::*;

    static JOB: OnceLock<usize> = OnceLock::new();
    let job = *JOB.get_or_init(|| unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const _,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        );
        job as usize
    });
    unsafe {
        AssignProcessToJobObject(job as _, child.as_raw_handle() as _);
    }
}

impl AiServer {
    pub fn start() -> Result<Self> {
        kill_stale();
        let port = TcpListener::bind("127.0.0.1:0")?.local_addr()?.port();
        fs::create_dir_all(paths::logs_dir())?;
        let log = File::create(paths::logs_dir().join("llama-server.log"))?;
        let child = Command::new(setup::llama_server_exe())
            .args(["-m", &setup::model_path().to_string_lossy()])
            .args(["--mmproj", &setup::mmproj_path().to_string_lossy()])
            .args(["--host", "127.0.0.1", "--port", &port.to_string()])
            .args(["-c", "8192", "-np", "1", "-ngl", "99", "--no-ui", "--image-min-tokens", "1024"])
            .stdout(Stdio::from(log.try_clone()?))
            .stderr(Stdio::from(log))
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .context("không khởi động được AI runtime")?;
        fs::write(pid_file(), child.id().to_string()).ok();
        kill_with_app(&child);

        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(300)))
            .http_status_as_error(false)
            .build()
            .into();
        let mut server = Self { child, base: format!("http://127.0.0.1:{port}"), agent };
        server.wait_ready()?;
        Ok(server)
    }

    fn wait_ready(&mut self) -> Result<()> {
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(300) {
            if let Some(status) = self.child.try_wait()? {
                bail!("AI runtime đã thoát ({status}). Xem log: {}", paths::logs_dir().join("llama-server.log").display());
            }
            if let Ok(mut r) = self.agent.get(format!("{}/health", self.base)).call() {
                if r.status() == 200 && r.body_mut().read_json::<Value>().map(|v| v["status"] == "ok").unwrap_or(false) {
                    return Ok(());
                }
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        bail!("AI runtime khởi động quá lâu")
    }

    fn chat(&self, messages: Value, max_tokens: u32, json_mode: bool) -> Result<String> {
        let mut body = json!({
            "messages": messages,
            "temperature": 0,
            "max_tokens": max_tokens,
            "chat_template_kwargs": {"enable_thinking": false},
        });
        if json_mode {
            body["response_format"] = json!({"type": "json_object"});
        }
        let mut resp = self.agent.post(format!("{}/v1/chat/completions", self.base)).send_json(&body)?;
        let v: Value = resp.body_mut().read_json()?;
        if resp.status() != 200 {
            bail!("AI lỗi: {v}");
        }
        Ok(v["choices"][0]["message"]["content"].as_str().unwrap_or("").to_string())
    }

    fn image_part(png: &[u8]) -> Value {
        let b64 = base64::engine::general_purpose::STANDARD.encode(png);
        json!({"type": "image_url", "image_url": {"url": format!("data:image/png;base64,{b64}")}})
    }

    /// Finds `target` on the screen. Returns None if the model says it is not there,
    /// or if the double-check on the cropped region disagrees.
    pub fn locate(&self, png: &[u8], img: &RgbaImage, target: &str, hint: &str, log: &dyn Fn(String)) -> Result<Option<Rect>> {
        let hint = if hint.is_empty() { String::new() } else { format!(" (vị trí: {hint})") };
        let prompt = format!(
            "Locate the UI element on this game screen: \"{target}\"{hint}. \
             If no such element is visible, reply exactly []. \
             Otherwise reply JSON [{{\"bbox_2d\": [x1, y1, x2, y2]}}] in 0-1000 coordinates."
        );
        let answer = self.chat(json!([{"role": "user", "content": [Self::image_part(png), {"type": "text", "text": prompt}]}]), 200, false)?;
        let Some(rect) = parse_bbox(&answer, img.width(), img.height()) else {
            log(format!("  AI: không thấy \"{target}\""));
            return Ok(None);
        };
        // Close/back buttons are often bare icons (a green arrow...) that the model finds
        // reliably on the full screen but won't recognise in a crop, so skip the double-check.
        if crate::script::is_close_target(target) || self.confirm(img, rect, target)? {
            Ok(Some(rect))
        } else {
            log(format!("  AI thấy \"{target}\" ở {:?} nhưng kiểm tra lại thấy không đúng, bỏ qua", rect.center()));
            Ok(None)
        }
    }

    /// Crops the found region and asks the model again, to reject look-alike matches.
    fn confirm(&self, img: &RgbaImage, rect: Rect, target: &str) -> Result<bool> {
        let pad_w = rect.w / 3 + 8;
        let pad_h = rect.h / 3 + 8;
        let x = rect.x.saturating_sub(pad_w);
        let y = rect.y.saturating_sub(pad_h);
        let w = (rect.w + 2 * pad_w).min(img.width() - x);
        let h = (rect.h + 2 * pad_h).min(img.height() - y);
        let crop = image::imageops::crop_imm(img, x, y, w, h).to_image();
        let mut png = Vec::new();
        crop.write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)?;
        let prompt = format!(
            "This is a crop of a game screen. Could the element in the middle of it be \"{target}\" \
             (same text, or an icon with that meaning)? Answer only yes or no."
        );
        let answer = self.chat(json!([{"role": "user", "content": [Self::image_part(&png), {"type": "text", "text": prompt}]}]), 5, false)?;
        let a = answer.trim().to_lowercase();
        Ok(a.starts_with("yes") || a.starts_with("có"))
    }

    /// Turns a Vietnamese description into a list of steps.
    pub fn compile(&self, description: &str) -> Result<Vec<Step>> {
        let answer = self.chat(
            json!([{"role": "system", "content": COMPILE_PROMPT}, {"role": "user", "content": description}]),
            2000,
            true,
        )?;
        let v: Value = serde_json::from_str(strip_fences(&answer)).context("AI trả về kịch bản không đúng định dạng")?;
        let steps = v.get("steps").cloned().unwrap_or(v);
        let mut steps: Vec<Step> = serde_json::from_value(steps).context("AI trả về kịch bản không đúng định dạng")?;
        crate::script::normalize(&mut steps);
        Ok(steps)
    }
}

impl Drop for AiServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_file(pid_file());
    }
}

fn strip_fences(s: &str) -> &str {
    let s = s.trim();
    let s = s.strip_prefix("```json").or_else(|| s.strip_prefix("```")).unwrap_or(s);
    s.strip_suffix("```").unwrap_or(s).trim()
}

/// Parses Qwen-style `[{"bbox_2d": [x1,y1,x2,y2]}]` (0-1000 scale) into pixels.
fn parse_bbox(answer: &str, width: u32, height: u32) -> Option<Rect> {
    let v: Value = serde_json::from_str(strip_fences(answer)).ok()?;
    let item = if v.is_array() { v.get(0)?.clone() } else { v };
    let b = item.get("bbox_2d").or_else(|| item.get("bbox"))?.as_array()?;
    let n: Vec<f64> = b.iter().filter_map(|x| x.as_f64()).collect();
    if n.len() != 4 {
        return None;
    }
    let sx = |v: f64| ((v.clamp(0.0, 1000.0) / 1000.0) * width as f64) as u32;
    let sy = |v: f64| ((v.clamp(0.0, 1000.0) / 1000.0) * height as f64) as u32;
    let (x1, y1, x2, y2) = (sx(n[0].min(n[2])), sy(n[1].min(n[3])), sx(n[0].max(n[2])), sy(n[1].max(n[3])));
    if x2 <= x1 + 2 || y2 <= y1 + 2 {
        return None;
    }
    Some(Rect { x: x1, y: y1, w: x2 - x1, h: y2 - y1 })
}

const COMPILE_PROMPT: &str = r#"Bạn chuyển mô tả tiếng Việt về thao tác trong game Android thành danh sách bước JSON.
Các loại bước:
- {"op":"tap","target":"<mô tả nút>","hint":"<vị trí gợi ý hoặc rỗng>","dynamic":false}
  dynamic=true nếu mục tiêu thay đổi mỗi lần chạy (vd "map chưa đi", "map tiếp theo", "nhiệm vụ chưa nhận").
- {"op":"tap_all","target":"<mô tả nút>","hint":"","max":30}  bấm lần lượt mọi nút khớp cho đến khi không còn (dùng cho "nhận hết", "bấm tất cả").
- {"op":"wait_until","target":"<màn hình/phần tử cần xuất hiện>","timeout_sec":300}
- {"op":"wait","seconds":2}
- {"op":"back"}
- {"op":"loop","max":50,"steps":[...]}  lặp các bước bên trong; tự dừng khi một bước bên trong không tìm thấy nút.
Quy tắc:
- Các cụm "đi tiếp", "tiếp theo", "lặp lại", "cho đến khi" nghĩa là phải bọc các bước lặp trong loop.
- "đóng", "đóng lại", "thoát", "quay lại" mà không nói rõ nút nào thì dùng target "nút đóng hoặc quay lại (dấu X hoặc mũi tên quay lại)".
- target là tên nút như hiện trên màn hình; bỏ các từ chỉ vị trí ("bên phải", "góc trên") khỏi target và đưa vào hint.

Ví dụ 1: "vào phó bản bấm quét nhanh 3 lần rồi thoát"
{"steps":[{"op":"tap","target":"nút Phó bản","hint":"","dynamic":false},{"op":"loop","max":3,"steps":[{"op":"tap","target":"nút Quét nhanh","hint":"","dynamic":false},{"op":"wait","seconds":2}]},{"op":"back"}]}
Ví dụ 2: "vào thư nhận hết quà rồi đóng"
{"steps":[{"op":"tap","target":"biểu tượng Thư","hint":"","dynamic":false},{"op":"tap_all","target":"nút Nhận quà","hint":"","max":30},{"op":"tap","target":"nút Đóng","hint":"","dynamic":false}]}

TUYỆT ĐỐI không thêm bước mà mô tả không nói tới. Mỗi hành động trong mô tả là đúng một bước.
Ví dụ 3: "mở túi đồ rồi đóng lại"
{"steps":[{"op":"tap","target":"Túi đồ","hint":"","dynamic":false},{"op":"tap","target":"nút đóng hoặc quay lại (dấu X hoặc mũi tên quay lại)","hint":"","dynamic":false}]}

Viết target bằng tiếng Việt ngắn gọn."#;
