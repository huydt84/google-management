# Phase 3 — Agent có tool, tự động hoá có kiểm soát, digest, memory

**Mục tiêu:** ra lệnh tự do được ("tìm mail hoá đơn tháng này rồi tổng tiền", "dời cuộc họp với An
sang thứ Sáu và báo cho An"), giao việc lặp lại cho hệ thống tự làm, và nhận bản tổng hợp mỗi sáng.
Tất cả vẫn giữ các bất biến an toàn.

Điều kiện bắt đầu: Phase 2 đã merge (cần LiteLLM, các nguồn và feedback). 3A là nền cho 3B. 3C, 3D và
3E có thể chạy song song với 3A.

Phase này có nhiều quyết định thiết kế hơn các phase trước. Session nhận việc nên viết một design note
ngắn trong `docs/design/` (vài đoạn) và cho người dùng xem trước khi code phần lớn.

## 3A Agent loop với tool + quarantine

**Sở hữu:** `src/gm/agent/` (mới: `loop.py`, `tools.py`, `policy.py`, `quarantine.py`), intent
`agent` trong router của 1B.

### Mô hình an toàn: privileged planner + quarantined reader

- **Planner** (LLM có quyền gọi tool): chỉ thấy lời người dùng, preferences (3D), và **kết quả có cấu
  trúc** của tool. Nó **không bao giờ thấy text thô** của mail, sự kiện hay file.
- **Quarantined reader** (LLM không có tool): là người duy nhất đọc nội dung không tin cậy. Nó trả về
  dữ liệu theo schema cố định (ví dụ `{sender, date, amounts:[…], summary}`), và text tự do của nó bị
  giới hạn độ dài.
- **Taint:** mọi giá trị đến từ quarantined reader được đánh dấu `tainted`. Policy chặn mọi hành động
  bên ngoài mà **đích** (người nhận, attendee, quyền chia sẻ) là giá trị tainted, trừ khi giá trị đó
  khớp với một nguồn tin cậy (header `From` của mail người dùng đang hỏi, danh bạ, lời người dùng).
  Nội dung (thân thư) được phép tainted, vì người dùng sẽ duyệt preview.

### Tool calling không cần server hỗ trợ

mlx-server-runtime không có `tools`. Agent dùng giao thức JSON qua text: mỗi bước model trả về đúng
một object, hoặc `{"tool": "<name>", "args": {...}}`, hoặc `{"final": "<trả lời người dùng>"}`. Parse
bền vững (như `parse_summary`). Sai format thì nhắc lại một lần rồi dừng. Giới hạn
`GM_AGENT_MAX_STEPS=8` và một ngân sách token cho mỗi task.

Có thể thêm native tool calling vào `mlx-server-runtime` như một việc **ở repo đó**, làm trong session
riêng. Khi đã có thì `LLMClient` dùng native tool calling nếu model hỗ trợ, còn giao thức JSON vẫn là
fallback.

### Tool registry

Mỗi tool khai báo `name`, `description`, JSON schema của args, **mức rủi ro**, và scope cần dùng.

| Mức | Ví dụ | Chính sách |
|-----|-------|------------|
| `read` | `gmail.search` (metadata), `calendar.list`, `ledger.query` | Tự chạy |
| `read_untrusted` | `gmail.read`, `drive.read_doc` | Chỉ chạy qua quarantined reader; trả dữ liệu có cấu trúc kèm taint |
| `write_local` | `ledger.append`, `notes.add` (Sheets/Docs của app), `gmail.label` | Tự chạy nếu automation cho phép (3B); mặc định cần approval; luôn ghi audit và có hoàn tác |
| `external` | `gmail.send_draft`, `calendar.create/update/respond`, `drive.share`, `tasks.create` | Luôn cần approval, kiểm tra taint của đích |
| cấm | xoá mail/file vĩnh viễn, forward hàng loạt, đổi cài đặt tài khoản, mọi thứ liên quan đến tiền | Không có tool |

Approval vẫn dùng bảng và executor của 1B. Agent có thể gom nhiều hành động vào một preview (duyệt
một lần, thực thi theo thứ tự, dừng ở lỗi đầu tiên).

Scope mới phát sinh (ví dụ `calendar.events`, `tasks`) phải qua bất biến 4; mỗi scope thêm vào cần
người dùng đồng ý.

**Test:** kịch bản injection (mail yêu cầu gửi dữ liệu ra ngoài → planner không thấy câu đó; nếu đích
tainted thì policy chặn); vượt giới hạn bước; JSON hỏng; nhiều hành động trong một approval.

## 3B Standing instructions / tự động hoá

**Sở hữu:** `src/gm/automations.py` (mới), `src/gm/templates/automations.html` (mới), migration
thêm `automations`.

1. Người dùng nói "mail hoá đơn EVN thì tự ghi sổ, không cần hỏi". Hệ thống đề xuất một automation:
   điều kiện `match` (cùng schema `When`), recipe hoặc chuỗi tool, và `auto_approve`. Người dùng
   **duyệt chính automation đó** một lần.
2. Chỉ được `auto_approve` cho hành động trong allowlist: `write_local` có hoàn tác (ghi sổ, gắn nhãn,
   dịch rồi gửi cho mình). **Không bao giờ** auto cho mức `external`.
3. Mỗi lần chạy tự động đều gửi một dòng thông báo kèm nút `[Hoàn tác]` (xoá dòng sheet, gỡ nhãn).
   Bảng `automation_runs` ghi lại để hoàn tác và để thống kê.
4. Tự động tạm dừng automation khi hoàn tác nhiều lần liên tiếp (ví dụ 3), rồi hỏi lại người dùng.
5. Trang `/automations` để xem, bật/tắt và xoá automation; có lịch sử chạy.

## 3C Digest hằng ngày & nhắc việc

**Sở hữu:** `src/gm/digest.py` (mới), `src/gm/scheduler.py` (mới).

1. **Scheduler trong process:** lịch cron-like lưu trong DB, mỗi lần đến hạn publish một job (dùng
   `run_after` của bus). Chịu được máy ngủ: lần chạy bị lỡ thì chạy một lần khi dậy, không chạy bù
   nhiều lần.
2. **Digest buổi sáng** (`GM_DIGEST_AT=07:30`, theo timezone của người dùng):
   - Lịch hôm nay
   - Mail đã `log` hôm qua, gom theo sender hoặc danh mục
   - Approval đang chờ
   - Chi tiêu tuần này (từ `ledger_entries`)
   - Lỗi hệ thống (job dead, token cần kết nối lại)

   LLM chỉ đọc dữ liệu đã có cấu trúc. Gửi qua Telegram và hiện trên dashboard.
3. **Nhắc việc:** từ hạn chót mà `analyze` (1C) hoặc quarantined reader trích ra, đề xuất "nhắc tôi
   lúc …" (nút). Người dùng chấp nhận thì tạo lịch nhắc nội bộ; Google Tasks là tuỳ chọn, cần scope.

## 3D Preferences & memory

**Sở hữu:** `src/gm/preferences.py` (mới), `src/gm/templates/preferences.html` (mới), migration
thêm `preferences` và `contacts_profile`.

1. **Preferences** do người dùng đặt: ngôn ngữ mặc định, giọng văn và chữ ký khi trả lời, giờ yên lặng
   (không thông báo trừ khi `high`), danh sách VIP. Sửa được trên web và qua Telegram ("từ giờ trả lời
   mail công việc bằng giọng trang trọng").
2. **Contact profile** học từ hành vi: sender nào bạn hay bấm 👍, hay trả lời, hay tắt → điểm quan
   trọng, đưa vào decision model (2B) và digest. Bạn xem và sửa được; không lưu nội dung mail.
3. Inject vào prompt có giới hạn độ dài. Preferences là dữ liệu tin cậy (do bạn đặt). Contact profile
   suy ra từ dữ liệu không tin cậy, nên **chỉ** được dùng để xếp hạng, không làm chỉ dẫn.
4. Quiet hours do 1A/notify tôn trọng: dồn lại và gửi khi hết giờ yên lặng hoặc gộp vào digest.

## 3E Web task center & chat

**Sở hữu:** `src/gm/templates/tasks.html`, `src/gm/templates/chat.html`, route trong `src/gm/web.py`
hoặc router mới `src/gm/web_tasks.py`.

1. `/tasks`: danh sách task và approval (lọc theo trạng thái), xem preview đầy đủ, **duyệt hoặc từ
   chối từ web** (POST, SameSite cookie + kiểm tra session; publish `approval.decision` với
   `via="web"`).
2. `/chat`: gửi lệnh cho agent từ trình duyệt (publish `user.instruction` với `channel="web"`). Nhận
   trả lời qua polling hoặc SSE. Kênh `web` có notifier riêng, dùng chung `task.reply`.
3. Trang chi tiết event: mail đã triage, quyết định của rule/model, các task liên quan, audit log.

## Phase 3 xong khi

- Lệnh tự do nhiều bước chạy được, với mọi hành động bên ngoài đều qua approval và kiểm tra taint.
- Có ít nhất một automation chạy thật kèm hoàn tác.
- Digest buổi sáng đến đúng giờ.
- Preferences ảnh hưởng tới trả lời và thông báo.
- Duyệt được từ cả Telegram lẫn web.
