# Kế hoạch triển khai — tổng quan

Tài liệu này dành cho các session (người hoặc agent) nhận một phần việc và làm độc lập. Đọc hết file
này trước, rồi mới đọc file của phase bạn nhận.

| Phase | File | Mục tiêu |
|-------|------|----------|
| 0 | (đã xong, commit `145de43`) | Gmail → rule triage → LLM tóm tắt → Telegram (một chiều) |
| 1 | [phase-1.md](phase-1.md) | Vòng hai chiều: bạn ra lệnh qua Telegram, hệ thống dịch / phân tích / soạn trả lời / ghi sổ, có duyệt |
| 2 | [phase-2.md](phase-2.md) | LiteLLM gateway, decision model, học từ phản hồi, metrics, vận hành, Calendar/Drive |
| 3 | [phase-3.md](phase-3.md) | Agent có tool, tự động hoá có kiểm soát, digest, memory, web task center |

## Trạng thái workstream

Cập nhật bảng này **trong cùng commit/PR hoàn thành workstream** (đừng commit riêng chỉ để đổi trạng
thái, tránh conflict giữa các session).

| ID | Tên | Phụ thuộc | Trạng thái |
|----|-----|-----------|------------|
| 1.0 | Nền tảng Phase 1 (Alembic, bảng mới, Google clients, scopes, topics) | — | todo |
| 1A | Telegram hai chiều | 1.0 | todo |
| 1B | Task runtime, intent router, approvals | 1.0 | todo |
| 1C | Gmail recipes: dịch, phân tích, soạn & gửi trả lời | 1.0 (+ interface 1B) | todo |
| 1D | Sổ chi tiêu trên Google Sheets | 1.0 (+ interface 1B) | todo |
| 1.Z | Tích hợp & kiểm thử end-to-end Phase 1 | 1A–1D | todo |
| 2A | LiteLLM gateway + 2 instance mlx-server-runtime | Phase 1 | todo |
| 2B | Decision model (triage tầng 2) | 2A | todo |
| 2C | Phản hồi & rule do người dùng quản lý | Phase 1 | todo |
| 2D | Observability & vận hành (metrics, dead-letter UI, launchd, backup) | Phase 1 | todo |
| 2E | Nguồn Calendar & Drive | Phase 1 | todo |
| 2F | (tuỳ chọn) Event bus trên NATS JetStream | Phase 1 | todo |
| 3A | Agent loop với tool + quarantine | Phase 2 | todo |
| 3B | Standing instructions / tự động hoá | 3A | todo |
| 3C | Digest hằng ngày & nhắc việc | Phase 2 | todo |
| 3D | Preferences & memory | Phase 2 | todo |
| 3E | Web task center & chat | 1B | todo |

Workstream trong cùng một phase được thiết kế để **chạy song song**: mỗi cái sở hữu một nhóm file
riêng (ghi trong plan). Phần dùng chung (schema, topic, interface) được chốt trong workstream nền tảng
hoặc ghi rõ trong plan, để không ai phải đợi ai.

## Cách nhận và làm một workstream

1. Đọc `CLAUDE.md`, file này, và mục workstream của bạn trong file phase.
2. Tạo nhánh `phase<N>/<id>-<slug>`, ví dụ `phase1/1a-telegram`.
3. Chỉ sửa các file workstream của bạn sở hữu. Cần đổi file dùng chung (`db.py`, `main.py`,
   `config.py`, `topics.py`, `templates/`) thì sửa **tối thiểu** và ghi rõ trong mô tả PR.
4. Schema thay đổi → thêm một Alembic revision mới (từ 1.0 trở đi). Trước khi merge, rebase và sửa
   `down_revision` để chỉ có **một head** (`uv run alembic heads`).
5. Hoàn thành khi đạt **Definition of Done** bên dưới, rồi cập nhật bảng trạng thái.

## Definition of Done (mọi workstream)

- `uv run pytest` và `uv run ruff check src tests` đều sạch.
- Test mới cho logic mới, **không gọi mạng thật** (Google, Telegram, LLM): dùng fake/MockTransport như
  `tests/conftest.py` (`FakeGmail`, `RecordingNotifier`, `httpx.MockTransport`).
- Có Alembic migration nếu schema đổi; app khởi động được trên DB trống và trên DB của phase trước.
- Biến môi trường mới có trong `.env.example`. Bước setup mới (scope, API cần bật…) có trong `README.md`.
- Không vi phạm các **bất biến** bên dưới.

## Bất biến kiến trúc (không được phá)

1. **At-least-once + idempotent.** Handler của bus phải chịu được chạy lại: kiểm tra trạng thái trước
   khi làm. Thay đổi trạng thái và `bus.publish(...)` nằm **trong cùng một transaction**.
2. **Nội dung từ Google là dữ liệu không tin cậy** (mail, tên file, mô tả sự kiện…). Không bao giờ
   đưa nó cho một LLM call có quyền gây tác động bên ngoài. Người nhận / đích của một hành động
   (địa chỉ gửi mail, spreadsheet, calendar) **không bao giờ lấy từ output của LLM**; chúng lấy từ
   header của mail gốc, cấu hình, hoặc lời của người dùng.
3. **Mọi tác động bên ngoài phải qua approval** (gửi mail, sửa lịch, chia sẻ file, xoá). Payload của
   approval là bất biến (lưu hash), khi thực thi phải đúng payload đã duyệt, và ghi `audit_log`.
   Ngoại lệ chỉ được thêm ở Phase 3B, theo allowlist rõ ràng.
4. **Scope tối thiểu.** Thêm scope = thêm vào `REQUIRED_SCOPES` + UI báo cần kết nối lại. Ưu tiên
   `drive.file` hơn `drive`, `gmail.compose` hơn `gmail.modify`/full.
5. **Secret** (refresh token, bot token, master key) được mã hoá hoặc chỉ nằm trong `.env`, không bao
   giờ xuất hiện trong log. URL của Telegram chứa bot token: không log URL đó.
6. **Chỉ bind `127.0.0.1`.** Truy cập từ xa đi qua Tailscale. Không mở webhook public.
7. **Mọi lời gọi LLM đi qua `gm.llm.LLMClient`** (chỉ một chỗ để đổi sang LiteLLM). Không phụ thuộc
   vào `tools`/`response_format`: mlx-server-runtime không hỗ trợ. Yêu cầu JSON trong prompt và parse
   bền vững như `parse_summary`.
8. **Telegram gửi plain text** (không `parse_mode`), vì text có thể chứa nội dung từ mail.

## Sự thật về môi trường (đã kiểm tra)

- Python 3.13 qua `uv`; Postgres 17 trong Docker ở `127.0.0.1:5433` (user/pass/db `gm`); test dùng DB
  `gm_test` (tự tạo).
- `mlx-server-runtime` (repo anh em `../mlx-server-runtime`): gateway Rust, `POST /v1/chat/completions`
  chuẩn OpenAI (có `usage`, có SSE). **Mỗi instance chỉ phục vụ một model** (`[worker].model`). **Không
  có** `tools`, `response_format`, hay JSON mode. Khi queue đầy thì trả `429` (bus sẽ retry). Cấu hình
  TOML qua `MLX_RUNTIME_CONFIG`; muốn chạy instance thứ hai thì dùng `server.port` và
  `worker.ipc_path` khác.
- Google: app OAuth phải ở chế độ "In production" (nếu không, refresh token hết hạn sau 7 ngày).
  Tài khoản gmail.com cá nhân **không dùng được Google Keep API** → ghi chú/sổ sách đi vào Sheets.
- Lệnh thường dùng:
  ```bash
  docker compose up -d
  uv run gm serve
  uv run pytest
  uv run ruff check src tests
  ```
