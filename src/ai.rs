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
            .args([
                "-c",
                "8192",
                "-np",
                "1",
                "-ngl",
                "99",
                "--no-ui",
                "--image-min-tokens",
                "1024",
            ])
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
        let mut server = Self {
            child,
            base: format!("http://127.0.0.1:{port}"),
            agent,
        };
        server.wait_ready()?;
        Ok(server)
    }

    fn wait_ready(&mut self) -> Result<()> {
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(300) {
            if let Some(status) = self.child.try_wait()? {
                bail!(
                    "AI runtime đã thoát ({status}). Xem log: {}",
                    paths::logs_dir().join("llama-server.log").display()
                );
            }
            if let Ok(mut r) = self.agent.get(format!("{}/health", self.base)).call() {
                if r.status() == 200
                    && r.body_mut()
                        .read_json::<Value>()
                        .map(|v| v["status"] == "ok")
                        .unwrap_or(false)
                {
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
        let mut resp = self
            .agent
            .post(format!("{}/v1/chat/completions", self.base))
            .send_json(&body)?;
        let v: Value = resp.body_mut().read_json()?;
        if resp.status() != 200 {
            bail!("AI lỗi: {v}");
        }
        Ok(v["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or("")
            .to_string())
    }

    fn image_part(png: &[u8]) -> Value {
        let b64 = base64::engine::general_purpose::STANDARD.encode(png);
        json!({"type": "image_url", "image_url": {"url": format!("data:image/png;base64,{b64}")}})
    }

    /// Finds an element. Searches only `q.region` if set. With `q.exclude`, every element
    /// the model points at has its text read, and is rejected (the model is asked again
    /// for another one) if it shows a forbidden text – small models can't be trusted to
    /// apply "không có chữ X" themselves when no element qualifies.
    pub fn locate(
        &self,
        png: &[u8],
        img: &RgbaImage,
        q: &Query,
        log: &dyn Fn(String),
    ) -> Result<Option<Rect>> {
        let (area, area_png) = match q.region {
            Some(r) => {
                let a = region_rect(r, img);
                (
                    a,
                    encode_png(&image::imageops::crop_imm(img, a.x, a.y, a.w, a.h).to_image())?,
                )
            }
            None => (
                Rect {
                    x: 0,
                    y: 0,
                    w: img.width(),
                    h: img.height(),
                },
                png.to_vec(),
            ),
        };
        let hint = if q.hint.is_empty() {
            String::new()
        } else {
            format!(" (vị trí: {})", q.hint)
        };
        let mut rejected: Vec<(u32, u32)> = Vec::new();
        let tries = if q.exclude.is_empty() { 1 } else { MAX_REJECTS };
        for _ in 0..tries {
            let skip = if rejected.is_empty() {
                String::new()
            } else {
                let list: Vec<String> =
                    rejected.iter().map(|(x, y)| format!("[{x},{y}]")).collect();
                format!(
                    " Skip the elements at these positions (0-1000), they are wrong: {}.",
                    list.join(", ")
                )
            };
            let prompt = format!(
                "Locate the UI element on this game screen: \"{}\"{hint}.{skip}                  If no such element is visible, reply exactly [].                  Otherwise reply JSON [{{\"bbox_2d\": [x1, y1, x2, y2]}}] in 0-1000 coordinates.",
                q.target
            );
            let answer = self.chat(json!([{"role": "user", "content": [Self::image_part(&area_png), {"type": "text", "text": prompt}]}]), 200, false)?;
            let Some(local) = parse_bbox(&answer, area.w, area.h) else {
                log(format!("  AI: không thấy \"{}\"", q.target));
                return Ok(None);
            };
            let rect = Rect {
                x: area.x + local.x,
                y: area.y + local.y,
                w: local.w,
                h: local.h,
            };

            if !q.exclude.is_empty() {
                let text = self.read_text(img, rect)?;
                match q.exclude.iter().find(|e| contains_folded(&text, e)) {
                    Some(bad) => {
                        log(format!(
                            "  AI chỉ {:?} nhưng ô đó có chữ \"{bad}\", tìm ô khác",
                            rect.center()
                        ));
                        let (cx, cy) = local.center();
                        rejected.push((cx * 1000 / area.w.max(1), cy * 1000 / area.h.max(1)));
                        continue;
                    }
                    None => return Ok(Some(rect)),
                }
            }
            // Close/back buttons are often bare icons the model finds reliably on the full screen
            // but won't recognise in a crop; long conditional descriptions fail a crop check too.
            let skip_confirm =
                crate::script::is_close_target(q.target) || q.target.split_whitespace().count() > 6;
            if skip_confirm || self.confirm(img, rect, q.target)? {
                return Ok(Some(rect));
            }
            log(format!(
                "  AI thấy \"{}\" ở {:?} nhưng kiểm tra lại thấy không đúng, bỏ qua",
                q.target,
                rect.center()
            ));
            return Ok(None);
        }
        log(format!(
            "  Không còn \"{}\" nào hợp lệ trên màn hình",
            q.target
        ));
        Ok(None)
    }

    /// Reads the text on and just above an element (labels/tags often sit on top of it).
    fn read_text(&self, img: &RgbaImage, rect: Rect) -> Result<String> {
        let x = rect.x.saturating_sub(rect.w / 6);
        let y = rect.y.saturating_sub(rect.h);
        let w = (rect.w + rect.w / 3).min(img.width() - x);
        let h = (rect.h * 2 + rect.h / 4).min(img.height() - y);
        let crop = image::imageops::crop_imm(img, x, y, w.max(1), h.max(1)).to_image();
        let big = image::imageops::resize(
            &crop,
            crop.width() * 2,
            crop.height() * 2,
            image::imageops::FilterType::Triangle,
        );
        let prompt = "Đọc toàn bộ chữ trong ảnh này, giữ nguyên dấu. Chỉ trả về chữ đọc được.";
        self.chat(json!([{"role": "user", "content": [Self::image_part(&encode_png(&big)?), {"type": "text", "text": prompt}]}]), 120, false)
    }

    /// Crops the found region and asks the model again, to reject look-alike matches.
    fn confirm(&self, img: &RgbaImage, rect: Rect, target: &str) -> Result<bool> {
        let pad_w = rect.w / 3 + 8;
        let pad_h = rect.h / 3 + 8;
        let x = rect.x.saturating_sub(pad_w);
        let y = rect.y.saturating_sub(pad_h);
        let w = (rect.w + 2 * pad_w).min(img.width() - x);
        let h = (rect.h + 2 * pad_h).min(img.height() - y);
        let png = encode_png(&image::imageops::crop_imm(img, x, y, w, h).to_image())?;
        let prompt = format!(
            "This is a crop of a game screen. Could the element in the middle of it be \"{target}\" \
             (same text, or an icon with that meaning)? Answer only yes or no."
        );
        let answer = self.chat(json!([{"role": "user", "content": [Self::image_part(&png), {"type": "text", "text": prompt}]}]), 5, false)?;
        let a = answer.trim().to_lowercase();
        Ok(a.starts_with("yes") || a.starts_with("có"))
    }

    /// Interprets a node's text. A "nếu không có thì …" part becomes the node's fallback
    /// (unless the user already has one), to be interpreted on its own.
    pub fn compile_node(&self, node: &mut crate::script::Node) -> Result<()> {
        let c = self.compile_step(&node.text)?;
        node.step = Some(c.step);
        if let (Some(text), None) = (c.otherwise, &node.otherwise) {
            node.otherwise = Some(Box::new(crate::script::Node::with_text(&text)));
        }
        Ok(())
    }

    /// Turns one step the user typed ("bấm xuất chiến góc phải dưới") into one action,
    /// plus the "nếu không có thì …" part if the sentence has one.
    pub fn compile_step(&self, text: &str) -> Result<Compiled> {
        if let Some(step) = parse_simple(text) {
            return Ok(Compiled {
                step,
                otherwise: None,
            });
        }
        let answer = self.chat(
            json!([{"role": "system", "content": STEP_PROMPT}, {"role": "user", "content": text}]),
            300,
            true,
        )?;
        let v: Value = serde_json::from_str(strip_fences(&answer))
            .with_context(|| format!("AI không hiểu bước \"{text}\""))?;
        // Small models sometimes wrap the answer in {"steps":[...]}; take the first one.
        let v = match v.get("steps").and_then(|s| s.get(0)) {
            Some(first) => first.clone(),
            None => v,
        };
        let otherwise = v
            .get("otherwise")
            .and_then(|o| o.as_str())
            .map(str::trim)
            .filter(|o| !o.is_empty())
            .map(String::from);
        let step: Step =
            serde_json::from_value(v).with_context(|| format!("AI không hiểu bước \"{text}\""))?;
        if matches!(step, Step::Loop { .. }) {
            bail!("Bước \"{text}\" nên dùng nút \"Thêm vòng lặp\" thay vì viết trong một bước");
        }
        Ok(Compiled {
            step: crate::script::normalize(step),
            otherwise,
        })
    }
}

impl Drop for AiServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_file(pid_file());
    }
}

pub struct Compiled {
    pub step: Step,
    /// Text of the "nếu không có/không thấy thì …" action, if any.
    pub otherwise: Option<String>,
}

/// How many look-alikes may be rejected before concluding nothing qualifies.
const MAX_REJECTS: usize = 4;

/// What to look for on the screen.
pub struct Query<'a> {
    pub target: &'a str,
    pub hint: &'a str,
    pub exclude: &'a [String],
    pub region: Option<[f32; 4]>,
}

fn encode_png(img: &RgbaImage) -> Result<Vec<u8>> {
    let mut png = Vec::new();
    img.write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)?;
    Ok(png)
}

/// Fractions [x1, y1, x2, y2] of the screen → pixels (at least 16px each way).
fn region_rect(r: [f32; 4], img: &RgbaImage) -> Rect {
    let (w, h) = (img.width(), img.height());
    let px = |f: f32, size: u32| (f.clamp(0.0, 1.0) * size as f32) as u32;
    let x1 = px(r[0].min(r[2]), w).min(w.saturating_sub(16));
    let y1 = px(r[1].min(r[3]), h).min(h.saturating_sub(16));
    let x2 = px(r[0].max(r[2]), w).max(x1 + 16).min(w);
    let y2 = px(r[1].max(r[3]), h).max(y1 + 16).min(h);
    Rect {
        x: x1,
        y: y1,
        w: x2 - x1,
        h: y2 - y1,
    }
}

/// Lowercase, no diacritics, single spaces: OCR often drops or garbles Vietnamese marks.
pub fn fold(s: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    let lower = s.to_lowercase().replace('đ', "d");
    let plain: String = lower
        .nfd()
        .filter(|c| !unicode_normalization::char::is_combining_mark(*c))
        .collect();
    plain.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn contains_folded(haystack: &str, needle: &str) -> bool {
    let n = fold(needle);
    !n.is_empty() && fold(haystack).contains(&n)
}

fn strip_fences(s: &str) -> &str {
    let s = s.trim();
    let s = s
        .strip_prefix("```json")
        .or_else(|| s.strip_prefix("```"))
        .unwrap_or(s);
    s.strip_suffix("```").unwrap_or(s).trim()
}

/// Parses Qwen-style `[{"bbox_2d": [x1,y1,x2,y2]}]` (0-1000 scale) into pixels.
fn parse_bbox(answer: &str, width: u32, height: u32) -> Option<Rect> {
    let v: Value = serde_json::from_str(strip_fences(answer)).ok()?;
    let item = if v.is_array() { v.get(0)?.clone() } else { v };
    let b = item
        .get("bbox_2d")
        .or_else(|| item.get("bbox"))?
        .as_array()?;
    let n: Vec<f64> = b.iter().filter_map(|x| x.as_f64()).collect();
    if n.len() != 4 {
        return None;
    }
    let sx = |v: f64| ((v.clamp(0.0, 1000.0) / 1000.0) * width as f64) as u32;
    let sy = |v: f64| ((v.clamp(0.0, 1000.0) / 1000.0) * height as f64) as u32;
    let (x1, y1, x2, y2) = (
        sx(n[0].min(n[2])),
        sy(n[1].min(n[3])),
        sx(n[0].max(n[2])),
        sy(n[1].max(n[3])),
    );
    if x2 <= x1 + 2 || y2 <= y1 + 2 {
        return None;
    }
    Some(Rect {
        x: x1,
        y: y1,
        w: x2 - x1,
        h: y2 - y1,
    })
}

/// "chờ 5 giây", "đợi 2s", "nghỉ 1 phút" – no AI needed.
fn parse_simple(text: &str) -> Option<Step> {
    let t = text.trim().to_lowercase();
    if !["chờ", "đợi", "nghỉ", "wait"]
        .iter()
        .any(|w| t.starts_with(w))
    {
        return None;
    }
    let num: String = t
        .chars()
        .skip_while(|c| !c.is_ascii_digit())
        .take_while(|c| c.is_ascii_digit() || *c == '.' || *c == ',')
        .collect();
    let n: f32 = num.replace(',', ".").parse().ok()?;
    let rest = t.split_once(&num)?.1.trim();
    let seconds = if rest.starts_with("phút") || rest.starts_with("p") || rest.starts_with("min") {
        n * 60.0
    } else if rest.starts_with("giây") || rest.starts_with("s") || rest.is_empty() {
        n
    } else {
        return None;
    };
    Some(Step::Wait { seconds })
}

const STEP_PROMPT: &str = r#"Bạn chuyển MỘT bước thao tác trong game Android (tiếng Việt) thành đúng MỘT object JSON.
Các loại:
- {"op":"tap","target":"<thứ cần tìm>","hint":"<vị trí hoặc rỗng>","dynamic":false,"exclude":[],"otherwise":""}
- {"op":"tap_all","target":"<thứ cần tìm>","hint":"","max":30,"exclude":[],"otherwise":""}  dùng cho "nhận hết", "bấm tất cả".
- {"op":"wait_until","target":"<màn hình/chữ/nút cần xuất hiện>","timeout_sec":300,"exclude":[],"otherwise":""}  dùng cho "đợi xong", "đợi đến khi".
- {"op":"wait","seconds":2}
Quy tắc:
- target: GIỮ NGUYÊN đầy đủ mô tả của người dùng về thứ cần tìm, kể cả điều kiện và đặc điểm (vd "phó bản không có chữ \"Hoàn toàn thu hồi\""). Chỉ bỏ động từ ở đầu ("bấm", "chọn", "click vào", "tìm") và bỏ phần "nếu không có thì …".
- hint: từ chỉ vị trí ("góc phải dưới", "bên trái") nếu có.
- exclude: các chữ mà thứ cần tìm KHÔNG được có, lấy từ các cụm "không có chữ X", "chưa có nhãn X", "không phải X". Ghi đúng chữ X. Không có thì [].
- otherwise: nếu câu có "nếu không có/không thấy/hết thì <việc khác>" thì ghi nguyên <việc khác>, ngược lại "".
- dynamic=true nếu mục tiêu thay đổi mỗi lần chạy (vd "map chưa đi", "ải mới nhất", "nhiệm vụ chưa nhận") hoặc có exclude.
- "đóng", "thoát", "quay lại" không nói rõ nút nào thì target = "thoát".
- Chỉ trả về một object, không thêm gì khác.

Ví dụ:
"bấm xuất chiến góc phải dưới" -> {"op":"tap","target":"Xuất chiến","hint":"góc phải dưới","dynamic":false,"exclude":[],"otherwise":""}
"chọn ải mới nhất" -> {"op":"tap","target":"ải mới nhất chưa hoàn thành","hint":"","dynamic":true,"exclude":[],"otherwise":""}
"tìm phó bản không có chữ \"Đã qua\" click vào, nếu không có thì bấm mũi tên phải để sang trang" -> {"op":"tap","target":"phó bản không có chữ \"Đã qua\"","hint":"","dynamic":true,"exclude":["Đã qua"],"otherwise":"bấm mũi tên phải để sang trang"}
"nhận hết quà" -> {"op":"tap_all","target":"Nhận","hint":"","max":30,"exclude":[],"otherwise":""}
"đợi đánh xong" -> {"op":"wait_until","target":"màn hình kết quả trận đấu (Chiến thắng/Thất bại)","timeout_sec":300,"exclude":[],"otherwise":""}
"thoát ra" -> {"op":"tap","target":"thoát","hint":"","dynamic":false,"exclude":[],"otherwise":""}"#;
