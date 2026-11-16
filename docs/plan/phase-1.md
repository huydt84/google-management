# Phase 1 — Vòng hai chiều có duyệt

**Mục tiêu:** khi có thông báo về một mail, bạn bấm nút hoặc trả lời tin Telegram ("dịch giúp",
"phân tích", "trả lời là tôi đồng ý, hẹn thứ Ba", "lưu số tiền"), và hệ thống làm việc đó. Hành động
có tác động bên ngoài thì phải đợi bạn duyệt.

**Không làm trong Phase 1:** agent tự chọn tool (Phase 3), decision model (Phase 2), Calendar/Drive
(Phase 2), tự động hoá không cần duyệt (Phase 3B).

## Thiết kế chốt

### Vì sao dùng intent router + recipe, chưa dùng agent tự do

mlx-server-runtime không có tool calling. Quan trọng hơn: một LLM vừa đọc mail không tin cậy vừa cầm
tool gửi mail là bề mặt prompt-injection lớn nhất. Vì vậy Phase 1 làm như sau:

1. **Intent router** chỉ đọc **lời của người dùng** (không đọc nội dung mail) rồi phân loại vào một tập
   intent cố định. Nút bấm thì đã mang sẵn intent, không cần gọi LLM.
2. Mỗi intent có một **recipe** viết bằng code. LLM trong recipe chỉ sinh *nội dung* (bản dịch, phân
   tích, thân thư, số tiền). Đích của hành động (người nhận, thread, spreadsheet) do code xác định.
3. Recipe trả về kết quả dạng text (gửi luôn cho bạn), hoặc một **ProposedAction** (tạo approval, gửi
   bản xem trước kèm nút Duyệt/Huỷ).

### Luồng

```
Telegram update ──► 1A poller ──► user.instruction {event_id, text|intent, chat_message_id}
                                     │
                                     ▼
                     1B: tạo task → intent router → recipe (1C/1D)
                                     │
                    ┌────────────────┴──────────────────┐
                TextResult                         ProposedAction
                    │                                   │ tạo approval (payload + hash)
                    ▼                                   ▼
         task.reply {task_id, text}       task.reply {task_id, text=preview, buttons=approve/reject}
                    │                                   │
                    └──────────► 1A gửi Telegram ◄──────┘
                                                        │ bạn bấm Duyệt
                                     approval.decision {approval_id, decision}
                                                        ▼
                               1B: kiểm hash → executor (1C/1D) → audit_log → task.reply kết quả
```

### Trạng thái

- `tasks.status`: `pending → running → (done | awaiting_approval | needs_input | failed)`;
  `awaiting_approval → executing → done | failed`; `awaiting_approval → rejected | expired`.
- `approvals.status`: `pending → approved → executed | failed`; `pending → rejected | expired`.
  Approval hết hạn sau `GM_APPROVAL_TTL_H` (mặc định 24h).

## 1.0 Nền tảng (làm trước, một session)

**Sở hữu:** `alembic.ini`, `migrations/`, `src/gm/db.py`, `src/gm/topics.py` (mới),
`src/gm/google/` (mới), `src/gm/google_creds.py`, `src/gm/auth.py`, `src/gm/main.py`, `src/gm/config.py`.

Việc cần làm:

1. **Alembic.** `uv add alembic`, cấu hình async (`migrations/env.py` dùng `GM_DATABASE_URL`).
   Revision `0001_baseline` tạo đúng schema Phase 0. Revision `0002_phase1` thêm các bảng bên dưới.
   Thay `create_schema()` khi khởi động bằng `alembic upgrade head` (gọi programmatic trong lifespan,
   có cờ `GM_AUTO_MIGRATE=true`). Thêm lệnh `gm db upgrade`. Fixture test chạy migration thay cho
   `create_all` (hoặc giữ `create_all` và thêm một test riêng kiểm tra migration khớp model).
   DB Phase 0 đã có bảng nhưng chưa có `alembic_version`: phát hiện trường hợp này rồi
   `stamp 0001_baseline` trước khi upgrade.
2. **Bảng mới** (tất cả tạo ngay trong 1.0 để 1A–1D không phải viết migration):
   ```
   tasks(id bigserial pk, event_id bigint null fk events, origin varchar(16),   -- telegram|web
         instruction text null, intent varchar(32) null, args jsonb default '{}',
         status varchar(24), result jsonb null, error text null, created_at, updated_at)
   approvals(id bigserial pk, task_id bigint fk tasks, action varchar(64), payload jsonb,
             payload_hash char(64), preview text, status varchar(16), expires_at timestamptz,
             decided_at timestamptz null, decided_via varchar(16) null, created_at, updated_at)
   chat_messages(id bigserial pk, channel varchar(16), chat_id varchar(64), message_id bigint,
                 event_id bigint null, task_id bigint null, approval_id bigint null,
                 direction varchar(8), created_at, unique(channel, chat_id, message_id))
   audit_log(id bigserial pk, ts timestamptz default now(), actor varchar(16),   -- user|system
             action varchar(64), target text, task_id bigint null, details jsonb)
   kv_state(key varchar(128) pk, value jsonb, updated_at)
   ledger_entries(id bigserial pk, event_id bigint unique null, task_id bigint, spreadsheet_id text,
                  row_range text null, values jsonb, created_at)
   ```
3. **`src/gm/topics.py`**: hằng số topic, payload ghi bằng TypedDict:
   ```python
   GMAIL_RECEIVED = "gmail.message.received"     # đã có (chuyển từ gmail/ingest.py)
   GMAIL_SUMMARIZE = "gmail.message.summarize"   # đã có (chuyển từ pipeline.py)
   NOTIFY_EMAIL = "notify.email"                 # đã có
   USER_INSTRUCTION = "user.instruction"   # {event_id:int|None, text:str|None, intent:str|None,
                                           #  args:dict, channel:str, chat_message_id:int|None}
   TASK_REPLY = "task.reply"               # {task_id:int, text:str, buttons:list[list[Button]]|None,
                                           #  reply_to_event_id:int|None}
   APPROVAL_DECISION = "approval.decision" # {approval_id:int, decision:"approve"|"reject", via:str}
   ```
   `Button = {"text": str, "data": str}`; `data` ≤ 64 byte (giới hạn của Telegram).
4. **`src/gm/google/clients.py`**: gom phần lặp giữa ingest và pipeline:
   `GoogleClients.gmail(email)`, `.sheets(email)`, `.drive(email)`, và
   `async run(email, service, fn) -> dict` (gọi `execute()` trong thread). Ingest/pipeline chuyển sang
   dùng nó; giữ hook `gmail_factory` cho test.
5. **Scopes.** `REQUIRED_SCOPES = [gmail.readonly, gmail.compose, drive.file]`.
   `GoogleCredentialStore.missing_scopes(email)`. Dashboard hiện "Cần cấp thêm quyền" và nút kết nối
   lại. Recipe gặp thiếu scope thì trả lời bạn "cần kết nối lại tại <base_url>".
   Lưu ý: `gmail.compose` cho phép gửi mail trực tiếp, nên cổng duyệt nằm ở code (bất biến 3). Không
   gọi `messages.send`, chỉ `drafts.send` qua executor đã duyệt.
6. **Config mới:** `GM_APPROVAL_TTL_H=24`, `GM_AUTO_MIGRATE=true`, `GM_TELEGRAM_POLL_TIMEOUT_S=50`,
   `GM_LEDGER_SPREADSHEET_TITLE="Google Management – Sổ chi tiêu"`.
7. **Audit helper** `gm/audit.py: async def audit(session, actor, action, target, task_id=None, **details)`.

**Xong khi:** migration chạy được trên DB trống và trên DB Phase 0; toàn bộ test cũ vẫn pass; các bảng
và topic có mặt; dashboard hiện scope còn thiếu.

## 1A Telegram hai chiều

**Sở hữu:** `src/gm/telegram/` (mới: `client.py`, `poller.py`, `handlers.py`), xoá
`src/gm/notify/telegram.py` (chuyển code sang `telegram/`), phần đăng ký trong `pipeline.py` cho
`notify` và `task.reply`.

1. **`TelegramClient`**: `send(text, buttons=None, reply_to_message_id=None) -> message_id`,
   `answer_callback(callback_id, text=None)`, `edit_buttons(message_id, buttons=None)`,
   `get_updates(offset, timeout)`. Plain text, cắt ở 4096 ký tự. Không log URL.
   `LogNotifier` vẫn là fallback khi chưa cấu hình (trả message_id giả).
2. **Notification có nút.** Handler `notify` hiện tại gửi kèm bàn phím:
   `[Dịch] [Phân tích] [Soạn trả lời] [Ghi số tiền]`, với `data = "i:<event_id>:<intent>"`. Ghi
   `chat_messages(direction="out", event_id=…)`.
3. **Poller** (background task, `getUpdates` long polling): offset lưu trong `kv_state["telegram.offset"]`.
   **Chỉ chấp nhận** update có `chat.id == GM_TELEGRAM_CHAT_ID`; còn lại bỏ qua và log cảnh báo
   (không trả lời người lạ).
   - Tin trả lời (reply) một tin của bot → tra `chat_messages` để lấy `event_id`/`task_id` → publish
     `user.instruction{text}`. Lưu ý: ghi `chat_messages(direction="in")` và **publish trong cùng
     transaction**, dùng unique `(channel, chat_id, message_id)` để chống trùng khi poll lại.
   - Callback `i:<event_id>:<intent>` → publish `user.instruction{intent}`; `answer_callback("Đang làm…")`.
   - Callback `a:<approval_id>:y|n` → publish `approval.decision`; gỡ nút trên tin đó (`edit_buttons`).
   - Tin không reply mà gắn với một task đang `needs_input` gần nhất (≤ 30 phút) → publish
     `user.instruction{text, task_id}`. Ngoài ra thì trả lời trợ giúp.
   - Lệnh: `/help`, `/status` (số event hôm nay, job dead, approval đang chờ), `/pause` và `/resume`
     (tắt/bật thông báo, lưu trong `kv_state["notify.paused"]`; handler `notify` tôn trọng cờ này,
     event vẫn ghi `notified_suppressed`).
4. **Handler `task.reply`**: gửi text và nút, reply vào tin thông báo gốc nếu có, ghi `chat_messages`
   kèm `task_id`/`approval_id`.

**Test:** MockTransport giả Telegram Bot API; poller bỏ qua chat lạ; chống trùng update; callback
parsing; offset bền qua restart; pause/resume.

## 1B Task runtime, intent router, approvals

**Sở hữu:** `src/gm/tasks/` (mới: `runtime.py`, `router.py`, `approvals.py`, `registry.py`).

### Interface chốt (1C/1D code theo đúng cái này, có thể làm song song)

```python
# src/gm/tasks/registry.py
@dataclass(frozen=True)
class RecipeContext:
    task_id: int
    account: str
    event: Event | None          # gm.db.Event (data có from/subject/thread_id/link)
    instruction: str | None      # lời người dùng, đáng tin
    args: dict                   # từ router, ví dụ {"target_language": "vi", "tone": "formal"}
    google: GoogleClients
    llm: LLMClient
    settings: Settings

@dataclass(frozen=True)
class TextResult:
    text: str

@dataclass(frozen=True)
class NeedsInput:               # recipe cần hỏi thêm; câu trả lời tiếp theo quay lại cùng task
    question: str

@dataclass(frozen=True)
class ProposedAction:
    action: str                  # tên executor đã đăng ký, ví dụ "gmail.send_draft"
    payload: dict                # JSON-serializable, đủ để thực thi, không cần gọi LLM nữa
    preview: str                 # hiển thị nguyên văn cho người dùng trước khi duyệt

RecipeResult = TextResult | NeedsInput | ProposedAction
Recipe = Callable[[RecipeContext], Awaitable[RecipeResult]]

@dataclass(frozen=True)
class ExecContext:
    task_id: int
    approval_id: int
    account: str
    google: GoogleClients
    settings: Settings

Executor = Callable[[ExecContext, dict], Awaitable[str]]   # trả text kết quả; raise = failed

def recipe(intent: str): ...       # decorator đăng ký
def executor(action: str): ...     # decorator đăng ký
```

Recipe phải idempotent theo `task_id` (ví dụ draft đã tạo thì dùng lại, lưu trong `tasks.result`).

### Việc cần làm

1. **Subscriber `user.instruction`**: tạo `tasks` (hoặc nối tiếp task `needs_input`). Có `intent` thì
   dùng luôn, không thì gọi router. Chạy recipe rồi xử lý kết quả:
   - `TextResult` → `done`, publish `task.reply`.
   - `NeedsInput` → `needs_input`, publish `task.reply` với câu hỏi.
   - `ProposedAction` → tạo `approvals` (`payload_hash = sha256(canonical_json(payload))`), task
     `awaiting_approval`, publish `task.reply` với preview và nút `a:<id>:y` / `a:<id>:n`.
2. **Intent router** (`router.py`): input chỉ gồm lời người dùng và danh sách intent; output JSON
   `{intent, args}`. Intent Phase 1: `translate`, `analyze`, `draft_reply`, `save_amount`, `unknown`.
   `unknown` → `NeedsInput` liệt kê các việc làm được. Có fast-path bằng regex/từ khoá tiếng Việt và
   tiếng Anh ("dịch", "translate", "trả lời", "reply", "lưu", "ghi sổ"…) trước khi gọi LLM.
3. **Subscriber `approval.decision`**: khoá approval (`FOR UPDATE`), kiểm tra `pending` và chưa hết hạn,
   tính lại hash của payload và so khớp. Approve thì chuyển `approved`, chạy executor, ghi
   `audit_log(actor="user", action=<action>)`, chuyển approval sang `executed` và task sang `done`,
   rồi publish `task.reply` với kết quả. Lỗi thì `failed` + reply lỗi. Reject thì `rejected` + reply.
   Executor có thể bị gọi lại khi bus retry, nên executor phải tự idempotent (1C: kiểm tra draft đã gửi
   chưa).
4. **Sweep hết hạn** (background, 10 phút/lần): approval `pending` quá `expires_at` → `expired`, task
   `expired`, báo cho bạn một dòng.
5. **Dashboard**: thêm mục "Đang chờ duyệt" (chỉ hiển thị; duyệt từ web để Phase 3E).

**Test:** router fast-path và JSON fallback; vòng đời task/approval; hash lệch → từ chối; hết hạn;
executor lỗi; approval bị bấm hai lần.

## 1C Gmail recipes

**Sở hữu:** `src/gm/recipes/gmail.py` (mới), `src/gm/recipes/prompts.py` (mới),
`src/gm/gmail/compose.py` (mới).

Mọi recipe lấy lại mail qua API theo `event.external_id`, rồi dùng `parse_message`. Nội dung mail được
bọc như `llm.build_messages` (bọc trong delimiter và chống mail tự đóng delimiter). Mail dài thì chia
đoạn theo `llm_max_input_chars`.

1. **`translate`** → `TextResult`. Ngôn ngữ đích lấy từ `args.target_language`, mặc định
   `GM_SUMMARY_LANGUAGE`. Giữ nguyên số, ngày, tên riêng. Kết quả dài hơn 4096 ký tự thì 1A tự cắt
   thành nhiều tin (nhớ ghi vào 1A).
2. **`analyze`** → `TextResult`: ý chính, việc cần làm và hạn chót, số tiền, rủi ro hoặc dấu hiệu lừa
   đảo (sender lạ, link lạ, yêu cầu chuyển tiền gấp), đề xuất bước tiếp theo.
3. **`draft_reply`** → `ProposedAction("gmail.send_draft")`:
   - Người nhận = `Reply-To` hoặc `From` của mail gốc (**từ header, không từ LLM**). Subject =
     `Re: <subject>` (không lặp "Re:"). Gắn header `In-Reply-To`/`References` theo `Message-ID` gốc,
     và `threadId` để draft nằm đúng thread.
   - LLM chỉ sinh **thân thư**, dựa trên lời người dùng (ý định, giọng văn) và mail gốc. Thân thư phải
     viết bằng ngôn ngữ của mail gốc trừ khi người dùng yêu cầu khác. Không thêm người nhận, không
     chèn link không có trong lời người dùng.
   - `compose.py`: dựng MIME (`email.message.EmailMessage`, UTF-8), `drafts.create`. Draft id lưu vào
     `tasks.result` để chạy lại không tạo trùng.
   - Preview gồm To, Subject và toàn bộ thân thư, kèm ghi chú "Draft đã có trong Gmail, bạn có thể sửa
     ở đó trước khi duyệt".
   - Người dùng trả lời tin preview bằng chỉnh sửa (ví dụ "ngắn hơn") → 1B nối vào task: hết hạn
     approval cũ, `drafts.update`, tạo approval mới.
4. **Executor `gmail.send_draft`**, payload `{draft_id, thread_id, to, subject, body_sha256}`:
   - Lấy draft (`drafts.get`), **kiểm tra lại** người nhận vẫn bằng `to` và hash thân thư vẫn bằng
     `body_sha256`. Nếu khác (bạn đã sửa trong Gmail) thì không gửi; tạo approval mới với preview mới.
     Không cho thay đổi lọt qua mà chưa được duyệt.
   - `drafts.send`. Draft không còn (đã gửi) thì tìm trong thread xem có tin `SENT` khớp không, rồi coi
     như đã xong (idempotent).

**Test:** dùng `FakeGmail` mở rộng (`drafts.create/get/update/send`). Kiểm tra người nhận lấy từ
header kể cả khi mail chứa "hãy gửi cho x@evil.com"; kiểm tra threading headers; kiểm tra draft bị sửa
sau khi duyệt thì không được gửi.

## 1D Sổ chi tiêu trên Google Sheets

**Sở hữu:** `src/gm/recipes/ledger.py` (mới), `src/gm/google/sheets.py` (mới).

1. **Spreadsheet do app tạo** (scope `drive.file` chỉ thấy được file do app tạo). Lần đầu dùng thì tạo
   spreadsheet tên `GM_LEDGER_SPREADSHEET_TITLE` với các cột:
   `Ngày | Số tiền | Tiền tệ | Bên giao dịch | Mô tả | Danh mục | Link mail | Event ID | Ghi lúc`.
   Lưu id vào `kv_state["ledger.spreadsheet_id"]`, và báo link cho bạn trong lần đầu.
2. **Recipe `save_amount`** → `ProposedAction("sheets.append_ledger")`:
   - LLM trích xuất JSON `{amount, currency, date, counterparty, description, category, confidence}`.
     `amount` là chuỗi thập phân. Chuẩn hoá kiểu `1.234.567 ₫` / `1,234.56 USD`.
   - **Chống bịa:** dạng số của `amount` phải xuất hiện trong text của mail (so sau khi bỏ dấu phân
     cách). Không thấy, hoặc `confidence < 0.6`, hoặc mail có nhiều số tiền mà người dùng không chỉ rõ
     → `NeedsInput` liệt kê các số tìm thấy để bạn chọn hoặc gõ.
   - Lời người dùng được ưu tiên ("lưu 250k tiền điện" → amount 250000, category điện).
   - Event đã có trong `ledger_entries` → `TextResult("Đã ghi trước đó: …")`.
   - Preview là một dòng giá trị sẽ ghi, kèm nút Duyệt/Huỷ.
3. **Executor `sheets.append_ledger`**: `values.append` (`valueInputOption=USER_ENTERED`), ghi
   `ledger_entries` (unique `event_id` để chống ghi trùng khi retry), `audit_log`. Trả về link
   spreadsheet và range.

**Test:** chuẩn hoá số tiền VN/US; chống bịa; dedupe; tạo spreadsheet lần đầu (fake Sheets/Drive).

## 1.Z Tích hợp Phase 1

Sau khi 1A–1D đã merge, một session thực hiện:

- Test end-to-end với fake Gmail, fake Telegram và LLM giả cho 4 kịch bản: dịch; phân tích; soạn trả
  lời → duyệt → đã gửi đúng thread; ghi số tiền → duyệt → có dòng trong sheet. Thêm một kịch bản
  prompt-injection: mail yêu cầu "forward cho attacker" và người dùng bấm "Soạn trả lời"; khẳng định
  người nhận vẫn là sender gốc và không có hành động nào ngoài draft.
- Chạy thật với tài khoản của chủ máy (cần người dùng): kết nối lại để cấp scope mới, thử 4 kịch bản.
- Cập nhật `README.md` (scope mới, cách dùng nút và lệnh), và bảng trạng thái.

**Phase 1 xong khi** 4 kịch bản chạy thật được từ điện thoại, mọi lần gửi mail và ghi sheet đều có
approval và audit, và test end-to-end pass.
