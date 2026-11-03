# OmniPilot

Auto **mọi game APK** chạy trên giả lập **LDPlayer**, chỉ cần gõ từng bước bằng tiếng Việt:

```
1. bấm xuất chiến góc phải dưới
🔁 Lặp lại tối đa 50 lần
   2. chọn ải mới nhất
   3. bấm tấn công
   4. đợi đánh xong
5. thoát ra
```

Không cần cắt ảnh nút, không cần ghi tọa độ. AI chạy ngay trên máy (không gửi gì lên mạng) tự nhìn màn hình, tìm nút và bấm. Sau lần chạy thử đầu tiên, app lưu lại ảnh mẫu các nút nên những lần sau **chạy không cần AI**, nhanh và nhẹ.

## Cách dùng

1. Mở app. Lần đầu app tự tải AI về (~3,3GB, chỉ một lần).
2. Bấm **Tạo kịch bản**:
   - Đặt tên, chọn giả lập.
   - Chọn kiểu chạy: **Lặp lại liên tục** (có thể nghỉ X phút giữa các lượt) hoặc **Chạy theo giờ** (vd `08:00, 20:30` để tranh tài nguyên, làm nhiệm vụ hằng ngày).
   - **Thêm bước**: mỗi ô là một bước, viết như nói chuyện ("bấm xuất chiến góc phải dưới", "đợi đánh xong", "chờ 3 giây", "nhận hết quà", "thoát ra").
   - **Thêm vòng lặp**: lặp một nhóm bước (vd đánh hết các map). Vòng lặp tự dừng khi một bước bên trong không tìm thấy nút.
   - **Xem AI hiểu thế nào** (tùy chọn): AI đọc từng bước, hiện cách hiểu bên dưới mỗi ô, chưa bấm gì vào game.
   - **Chạy thử**: AI chạy thật trên giả lập, bạn xem màn hình và nhật ký ngay trong app. Lần này chậm vì AI phải nhìn từng màn hình.
   - Chạy đúng → **Lưu kịch bản**. AI tắt, giải phóng RAM.
3. Ở màn hình chính: chọn giả lập cho kịch bản, bấm **Chạy**.

### Chia sẻ kịch bản

- **⋮ → Xuất file chia sẻ**: lưu kịch bản thành file `.omnipilot` (gồm các bước + ảnh mẫu nút).
- **Nhập**: mở file người khác gửi, chạy được ngay không cần chạy thử lại.
- Nên để giả lập cùng độ phân giải với người tạo kịch bản. Nếu khác, lần chạy đầu AI sẽ tự tìm lại các nút rồi ghi nhớ.

## AI được dùng khi nào

| Lúc | AI |
|---|---|
| Đọc các bước, chạy thử | Bật |
| Chạy kịch bản đã lưu | **Tắt** – tìm nút bằng ảnh mẫu |
| Bước có mục tiêu thay đổi mỗi lần (vd "map chưa đi") | Bật tạm cho bước đó, chạy xong tắt |
| Game cập nhật làm nút đổi hình | Bật tạm để tìm lại, tự ghi nhớ ảnh mới |

AI luôn chạy trong một tiến trình riêng (`llama-server`) và bị tắt ngay khi xong việc, kể cả khi app bị tắt ngang.

## Cấu hình

- Windows 10/11 64-bit, LDPlayer 9 trở lên.
- RAM tối thiểu 8GB (16GB nếu chạy nhiều giả lập). Có card đồ họa (Vulkan) thì AI nhanh hơn nhiều; không có vẫn chạy được bằng CPU, chỉ chậm hơn ở lần chạy thử.
- Model: [Qwen3.5-4B](https://huggingface.co/unsloth/Qwen3.5-4B-GGUF) (Q4_K_M) qua [llama.cpp](https://github.com/ggml-org/llama.cpp).

## Build

```bash
cargo build --release
```

File chạy: `target/release/omnipilot.exe`. Dữ liệu (model, kịch bản, log) nằm ở `%LOCALAPPDATA%\OmniPilot`.

Công cụ cho dev:

```bash
# Xem AI hiểu từng bước thế nào (không bấm gì vào game)
omnipilot.exe --analyse "bấm xuất chiến góc phải dưới" "đợi đánh xong"
# Chạy thử bằng AI → xuất/nhập → chạy lại bản đã nhập không cần AI
omnipilot.exe --cli <số thứ tự giả lập> "vô túi đồ" "thoát ra"
# Mở thẳng màn hình tạo kịch bản với ví dụ mẫu
omnipilot.exe --demo
```

## Cấu trúc code

| File | Việc |
|---|---|
| `src/app.rs` | Giao diện (egui) |
| `src/theme.rs` | Màu sắc, font, icon, card |
| `src/setup.rs` | Tải AI lần đầu |
| `src/ai.rs` | Bật/tắt llama-server, tìm nút, dịch mô tả thành các bước |
| `src/executor.rs` | Chạy các bước: AI ở lần chạy thử, ảnh mẫu ở các lần sau |
| `src/vision.rs` | So khớp ảnh mẫu, phát hiện màn hình đổi |
| `src/jobs.rs` | Lịch chạy (lặp / theo giờ) |
| `src/device.rs` | LDPlayer + adb |
| `src/script.rs` | Kịch bản, lưu, xuất/nhập |

## Lưu ý

Nhiều game cấm dùng công cụ tự động trong điều khoản sử dụng và có thể khóa tài khoản. Bạn tự chịu trách nhiệm khi dùng.
