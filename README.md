# Google Management

Web server chạy trên máy cá nhân. Nó nhận sự kiện từ các dịch vụ Google, lọc chúng, rồi dùng LLM
local để báo cho bạn những gì đáng quan tâm. Chỉ đúng một tài khoản Google (`GM_ALLOWED_EMAIL`) được
đăng nhập.

**Phase 0 (hiện tại):** Gmail → lọc bằng rule → LLM tóm tắt → Telegram. Chỉ đọc, chưa có hành động.

```
Gmail ──(poll history.list | Pub/Sub pull)──► GmailIngestor
                                                │  events + jobs + cursor: 1 transaction
                                                ▼
                                  gmail.message.received ──► triage (rules.yaml)
                                                               │ notify
                                                               ▼
                                  gmail.message.summarize ──► LLM (OpenAI-compatible)
                                                               │
                                                               ▼
                                  notify.email ─────────────► Telegram / log
```

- **Event bus:** pub/sub bền vững trên Postgres (`src/gm/bus.py`). Mỗi subscriber nhận một row trong
  `jobs`, được claim bằng `FOR UPDATE SKIP LOCKED`, giao at-least-once, retry với exponential backoff,
  quá số lần thì chuyển sang `dead`. Publish nằm trong transaction của caller (outbox pattern).
- **Trạng thái event:** `new → ignored | logged | triaged → summarized → notified`, hoặc `gone` (mail
  bị xoá trước khi xử lý). Mọi handler đều kiểm tra trạng thái trước khi làm nên chạy lại không sao.
- **Không mất mail:** cursor `historyId` và event được ghi cùng nhau. Máy ngủ dậy thì sync bù. Nếu
  history quá cũ (khoảng 1 tuần) thì lấy lại mail của 2 ngày gần nhất, có dedupe.

## Cài đặt

### 1. Hạ tầng local

```bash
docker compose up -d
```

```bash
uv sync
```

Dùng chế độ Pub/Sub thì cài thêm: `uv sync --extra pubsub`.

### 2. Google Cloud (một lần)

1. Tạo project tại <https://console.cloud.google.com>, rồi **bật Gmail API**.
2. **OAuth consent screen**: chọn External, thêm email của bạn vào Test users.
3. **Credentials → Create OAuth client ID → Web application.** Thêm Authorized redirect URIs:
   - `http://localhost:8080/auth/callback`
   - `http://localhost:8080/connect/google/callback`
4. **Quan trọng:** bấm **Publish app** (chuyển sang "In production"). Khi còn ở "Testing", refresh token
   **hết hạn sau 7 ngày**. Không cần submit verification: Google chỉ hiện cảnh báo "unverified app",
   bạn bấm *Advanced → Go to …* để tiếp tục.

### 3. Cấu hình

```bash
cp .env.example .env
```

```bash
uv run gm gen-keys
```

Dán hai giá trị vừa sinh vào `.env`, rồi điền `GM_ALLOWED_EMAIL`, `GM_GOOGLE_CLIENT_ID` và
`GM_GOOGLE_CLIENT_SECRET`.

- **LLM:** chạy gateway của `mlx-server-runtime` (mặc định `http://127.0.0.1:8000/v1`) và đặt
  `GM_LLM_MODEL` đúng với model đã cấu hình. Endpoint nào tương thích OpenAI đều dùng được. Phase 2
  sẽ chuyển sang LiteLLM.
- **Telegram:** tạo bot với @BotFather, nhắn cho bot một tin, rồi lấy `chat.id` từ
  `https://api.telegram.org/bot<TOKEN>/getUpdates`. Bỏ trống thì thông báo chỉ ghi ra log.

### 4. Chạy

```bash
uv run gm serve
```

Mở <http://localhost:8080>, đăng nhập rồi bấm **Kết nối Gmail**. Chỉ mail đến **sau** khi kết nối mới
được xử lý.

Server chỉ bind `127.0.0.1`. Muốn dùng từ điện thoại thì đi qua Tailscale, đừng mở port ra internet.

## Chế độ Pub/Sub (tuỳ chọn, gần realtime)

Mặc định là `poll` (60 giây/lần, không cần setup gì thêm). Với Pub/Sub, Gmail đẩy thông báo vào topic
và server **pull** về, nên không cần URL public:

```bash
gcloud pubsub topics create gmail-events
```

```bash
gcloud pubsub topics add-iam-policy-binding gmail-events \
  --member=serviceAccount:gmail-api-push@system.gserviceaccount.com --role=roles/pubsub.publisher
```

```bash
gcloud pubsub subscriptions create gm-pull --topic=gmail-events --ack-deadline=60
```

```bash
gcloud auth application-default login
```

Sau đó đặt trong `.env`:

```
GM_GMAIL_INGEST_MODE=pubsub
GM_GMAIL_PUBSUB_TOPIC=projects/<project-id>/topics/gmail-events
GM_GMAIL_PUBSUB_SUBSCRIPTION=projects/<project-id>/subscriptions/gm-pull
```

Watch tự gia hạn trước khi hết hạn 7 ngày. Một vòng poll an toàn (15 phút) vẫn chạy song song, phòng
khi Gmail làm rơi thông báo.

## Tuỳ chỉnh bộ lọc

Sửa [`rules.yaml`](rules.yaml). File được tự nạp lại khi thay đổi, không cần restart. Rule đầu tiên
khớp sẽ quyết định. Các action gồm `ignore`, `log` và `notify`.

## Bảo mật

- Đăng nhập UI: Google OIDC, email phải verified và đúng `GM_ALLOWED_EMAIL`. Session cookie được ký,
  `SameSite=Lax`. Có `TrustedHostMiddleware` để chặn DNS rebinding.
- Refresh token được mã hoá Fernet trong DB. Scope chỉ có `gmail.readonly`.
- Nội dung mail là dữ liệu không tin cậy. Nó được bọc trong delimiter (đã chống việc mail tự đóng
  delimiter), và LLM **không có tool nào**: chỉ trả JSON theo schema cố định. Telegram gửi plain text,
  không parse markup.

## Phát triển

```bash
uv run pytest
```

```bash
uv run ruff check src tests
```

Test tích hợp cần Postgres từ `docker compose`, và tự tạo database `gm_test`. Nếu không có Postgres
thì các test đó được skip.

```
src/gm/
  main.py          app factory, lifespan, background tasks
  config.py        GM_* settings
  bus.py           durable pub/sub on Postgres
  db.py            models: google_accounts, gmail_state, events, jobs
  auth.py          /auth/* login, /connect/google API consent
  google_creds.py  encrypted refresh tokens -> google Credentials
  gmail/ingest.py  history sync, poll / Pub/Sub loops, watch renewal
  gmail/message.py Gmail payload -> ParsedMessage
  triage.py        rules.yaml engine
  llm.py           OpenAI-compatible client, prompt, JSON parsing
  pipeline.py      triage -> summarize -> notify handlers
  notify/          Telegram + log notifier
  web.py           dashboard, /api/events, /healthz
```

## Lộ trình

- **Phase 1:** Telegram hai chiều. Bạn trả lời "dịch / phân tích / soạn trả lời / ghi số tiền vào
  Sheets". Hành động có tác động bên ngoài phải qua duyệt (ví dụ draft trong Gmail, bấm duyệt mới gửi).
  Thêm scope `gmail.compose` và `spreadsheets`.
- **Phase 2:** LiteLLM gateway (budget, rate limit, fallback cloud), decision model nhỏ chạy sau rule,
  metrics, Calendar/Drive qua `syncToken`, Alembic migrations.
- **Phase 3:** Thêm tool, tự động hoá những việc an toàn mà không cần duyệt.
