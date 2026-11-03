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
   - Chọn kiểu chạy: **Lặp lại liên tục** (có thể nghỉ X phút giữa các lượt), **Chạy theo giờ** (vd `08:00, 20:30` để tranh tài nguyên, làm nhiệm vụ hằng ngày) hoặc **Chạy 1 lần** (chạy hết các bước một lượt rồi tự dừng).
   - **Thêm bước** có 2 cách:
     - **Làm bằng tay trên ảnh màn hình (không cần AI, chính xác nhất)**: *Bấm vào nút* (khoanh quanh nút, mỗi lần chạy app tìm nút giống vậy rồi bấm), *Bấm hết các nút giống nhau*, *Bấm vào một điểm*, *Đợi đến khi thấy ảnh*, *Vuốt*, *Chờ vài giây*, *Nút Back*. Bấm **Chụp màn hình** để lấy màn hình giả lập rồi bấm/khoanh/kéo trực tiếp lên ảnh.
     - **Viết bằng chữ (AI hiểu)**: mỗi ô là một bước, viết như nói chuyện ("bấm xuất chiến góc phải dưới", "đợi đánh xong", "chờ 3 giây", "nhận hết quà", "thoát ra").
   - **Thêm vòng lặp**: lặp một nhóm bước (vd đánh hết các map). Vòng lặp tự dừng khi một bước bên trong không tìm thấy nút.
   - Điều kiện và trường hợp không thấy, viết luôn trong câu: *"tìm phó bản không có chữ "Hoàn toàn thu hồi" click vào, nếu không có thì click mũi tên màu xanh góc phải để chuyển trang"*. App giữ nguyên câu khi tìm, tự đọc chữ để loại các ô có chữ cấm, và tách phần "nếu không có thì…" thành mục **Nếu không thấy thì** (làm việc đó rồi tìm lại, tối đa N lần).
   - **Vùng tìm**: kéo chuột trên ảnh màn hình để AI chỉ tìm trong vùng đó, tránh nhầm với chỗ khác (vd tab bên cạnh bản đồ).
   - **Thử bước này**: chạy riêng một bước trên giả lập ngay lúc soạn.
   - **Xem AI hiểu thế nào** (tùy chọn): AI đọc từng bước, hiện cách hiểu bên dưới mỗi ô, chưa bấm gì vào game.
   - **Chạy thử**: AI chạy thật trên giả lập, bạn xem màn hình và nhật ký ngay trong app. Lần này chậm vì AI phải nhìn từng màn hình.
   - Chạy đúng → **Lưu kịch bản**. AI tắt, giải phóng RAM.
   - **Khối điều kiện**: *Nếu thấy ảnh … thì … không thì …*, *Nếu số trong vùng … thì …* (vd thể lực đọc từ "97/120" ra 97), vòng lặp có **Dừng sớm khi…**, bước **Dừng kịch bản**.
   - **Kịch bản con**: gọi một kịch bản khác (vd "về màn hình chính") thay vì làm lại các bước.
   - **Popup & tự phục hồi**:
     - *Quy tắc popup*: "thấy ảnh này ở đâu thì bấm vào nó / bấm điểm / nhấn Back", kiểm tra trước mỗi bước.
     - *Tự phục hồi khi lỗi*: chọn một chi tiết của màn hình gốc và game cần mở. Khi lỗi, app nhấn Back để về màn hình gốc, không được thì khởi động lại game, rồi chạy lại (tối đa N lần). Đầu mỗi lượt, game chưa mở thì app tự mở.
3. Ở màn hình chính: chọn **một hoặc nhiều giả lập** cho kịch bản (mỗi giả lập một tài khoản, chạy cùng lúc), bấm **Chạy**. Nút **Dừng tất cả** ở trên cùng.
4. Bấm vào một kịch bản để xem nhật ký và màn hình từng giả lập, hoặc tab **Lịch sử**: các lượt đã chạy, lỗi gì, ảnh màn hình lúc lỗi.

### Treo máy dài ngày

- **Khay hệ thống**: bấm ✕ thì app ẩn xuống khay, kịch bản vẫn chạy. Chuột phải icon để mở lại, dừng tất cả hoặc thoát hẳn.
- **Cài đặt → Khởi động cùng Windows**; kịch bản bật **Tự chạy khi mở app** sẽ tự bắt đầu. Máy không tự ngủ khi đang có kịch bản chạy.
- **Ctrl+F12** ở bất kỳ đâu: dừng tất cả.
- **Khung giờ, giới hạn** (kiểu lặp liên tục): chỉ chạy từ … đến …, dừng sau N phút hoặc N lượt.
- **Nhiều kịch bản trên một giả lập** chạy lần lượt; kịch bản theo giờ được ưu tiên chen vào giữa các lượt của kịch bản lặp.
- **Giả lập**: mở/tắt, sắp xếp cửa sổ; giới hạn số giả lập chạy cùng lúc (tự động theo RAM, ~4GB mỗi cái); kịch bản có thể tự mở giả lập trước giờ chạy và **tắt giả lập khi xong**.

### Sửa kịch bản nhanh

- **Kiểm tra ảnh mẫu**: tìm mọi ảnh nút của kịch bản trên màn hình hiện tại, biết ngay ảnh nào hết khớp sau khi game cập nhật.
- **Độ giống** từng ảnh (mặc định 88%) và nút **Thử khớp**.
- **Chạy từ đây** / **Dừng trước bước này** ở từng bước.
- **Biến**: đếm số trận, lưu số đọc được; giữ trong lượt, trong ngày hoặc mãi mãi. Bước **Đặt biến** và điều kiện **Nếu biến …**.
- **Đọc số nhanh không cần AI** (OCR PaddleOCR, ~10ms): dùng cho "số trong vùng" và "đặt biến = số đọc được". OCR này không có dấu tiếng Việt nên chỉ đọc số/chữ không dấu; không ra số thì app tự nhờ AI.

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
# Đọc bước rồi tìm trên một ảnh chụp có sẵn, có thể kèm vùng tìm (không bấm gì)
omnipilot.exe --find man-hinh.png "tìm phó bản không có chữ \"Đã qua\"" 0.27,0.1,0.97,0.95
# Xem AI hiểu từng bước thế nào (không bấm gì vào game)
omnipilot.exe --analyse "bấm xuất chiến góc phải dưới" "đợi đánh xong"
# Chạy thử bằng AI → xuất/nhập → chạy lại bản đã nhập không cần AI
omnipilot.exe --cli <số thứ tự giả lập> "vô túi đồ" "thoát ra"
# Chạy một kịch bản đã lưu (như khi chạy theo lịch)
omnipilot.exe --run <số thứ tự giả lập> "%LOCALAPPDATA%\OmniPilot\scenarios\<id>"
# Chạy y như nút Chạy (lịch, tự phục hồi, chạy lại, lịch sử) và in trạng thái
omnipilot.exe --job <số thứ tự giả lập> "%LOCALAPPDATA%\OmniPilot\scenarios\<id>"
# Nhiều kịch bản cùng lúc trên một giả lập (thử hàng đợi)
omnipilot.exe --job <số thứ tự giả lập> <thư mục 1> <thư mục 2>
# Chạy từ bước / dừng trước bước (đường dẫn bước tính từ 0, vd 3.0.0 = bước đầu trong khối thứ 4)
omnipilot.exe --run <số thứ tự giả lập> <thư mục kịch bản> from=3.0.0 stop=4
# Bật/tắt khởi động cùng Windows
omnipilot.exe --autostart on
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
| `src/ocr.rs` | Đọc số nhanh bằng PaddleOCR (ONNX) |
| `src/desktop.rs` | Khay hệ thống, phím tắt, khởi động cùng Windows |
| `src/jobs.rs` | Lịch chạy, khung giờ, hàng đợi giả lập, giới hạn RAM, tự phục hồi, lịch sử |
| `src/device.rs` | LDPlayer + adb |
| `src/script.rs` | Kịch bản, lưu, xuất/nhập |

## Lưu ý

Nhiều game cấm dùng công cụ tự động trong điều khoản sử dụng và có thể khóa tài khoản. Bạn tự chịu trách nhiệm khi dùng.
