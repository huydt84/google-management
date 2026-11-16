# Phase 2 — Gateway, decision model, phản hồi, vận hành, thêm nguồn

**Mục tiêu:** đưa toàn bộ lời gọi LLM qua LiteLLM (quản lý token và throughput), thêm decision model
nhỏ để lọc tốt hơn rule, cho hệ thống học từ phản hồi của bạn, chạy ổn định 24/7 trên máy cá nhân, và
thêm Calendar/Drive.

Điều kiện bắt đầu: Phase 1 đã merge (2A–2E dựa trên bảng, topic và nút Telegram của Phase 1).
Thứ tự gợi ý: **2A trước** (2B cần endpoint model nhỏ). 2C, 2D, 2E chạy song song với 2A. 2F là tuỳ
chọn.

## 2A LiteLLM gateway + hai instance mlx-server-runtime

**Sở hữu:** `infra/litellm/`, `infra/mlx/`, `infra/postgres/`, `scripts/run-mlx.sh`,
`docker-compose.yml`, `src/gm/llm.py`, `src/gm/config.py` (phần LLM).

1. **Hai instance mlx-server-runtime** (mỗi instance một model, xem README của plan):
   - `infra/mlx/agent.toml`: port 8000, `ipc_path=/tmp/mlx-agent.sock`, model chính (ví dụ Qwen2.5-7B
     4-bit).
   - `infra/mlx/triage.toml`: port 8001, `ipc_path=/tmp/mlx-triage.sock`, model nhỏ (1.5B–3B 4-bit).
   - `scripts/run-mlx.sh agent|triage` chạy `mlx-air` (hoặc `cargo run --bin gateway`) với
     `MLX_RUNTIME_CONFIG`. Đặt `limits.max_active_requests` vừa với RAM.
   - Đo và ghi vào README bảng RAM thực tế (model + KV cache khi chạy song song) trên máy này.
2. **LiteLLM proxy** chạy trên `127.0.0.1:4000`:
   - Khuyến nghị chạy trong docker compose, với image `ghcr.io/berriai/litellm` **pin một tag phát hành
     cụ thể**. Container gọi ra host qua `host.docker.internal` (kiểm tra trên Rancher Desktop; không
     được thì chạy native bằng `uvx --from 'litellm[proxy]==<ver>' litellm --config …`).
   - `infra/litellm/config.yaml`:
     - `model_list`: `agent-main` → `openai/<model>` với `api_base: http://host.docker.internal:8000/v1`;
       `triage-small` → `…:8001/v1`. Đặt `rpm`/`max_parallel_requests` khớp với `max_active_requests`
       của mlx.
     - `router_settings`: `num_retries`, `timeout`, `retry_after`.
       `fallbacks: [{agent-main: [cloud-main]}]` **chỉ khi** `GM_ALLOW_CLOUD_FALLBACK=true`. Mặc định
       tắt, vì bật lên thì nội dung mail sẽ rời khỏi máy.
     - `general_settings`: `master_key` (từ env), `database_url` trỏ tới DB `litellm` (cùng container
       Postgres).
   - Tạo DB `litellm`: `infra/postgres/init/01-litellm.sql`. Script init chỉ chạy khi volume còn mới,
     nên với volume đã có thì thêm hướng dẫn hoặc lệnh `gm infra init-litellm-db`.
   - **Virtual keys:** `gm-agent` và `gm-triage`, mỗi key có budget và rpm riêng. Tạo qua API
     `/key/generate` (script `scripts/litellm-keys.sh`), rồi lưu vào `.env`.
3. **Thay đổi trong gm:**
   - Config: `GM_LLM_BASE_URL=http://127.0.0.1:4000/v1`, `GM_LLM_MODEL=agent-main`,
     `GM_LLM_API_KEY=<gm-agent key>`, `GM_TRIAGE_MODEL=triage-small`, `GM_TRIAGE_API_KEY=<gm-triage key>`.
   - `LLMClient.chat(..., model=None, api_key=None, tag=None)`: truyền `user=<tag>` để LiteLLM gắn
     theo dõi chi tiêu. Tạo một factory `llm_for("agent"|"triage")`.
   - `429` hoặc budget vượt: raise lỗi có loại riêng; bus retry với backoff như cũ.
   - Vẫn chạy được khi không có LiteLLM (trỏ thẳng vào mlx): đó chỉ là thay đổi config.
4. Dashboard hiện tên model đang dùng cho từng vai, và link tới LiteLLM UI (`/ui`).

**Xong khi:** gm gọi được cả hai model qua LiteLLM; LiteLLM ghi nhận token theo từng key; tắt instance
triage thì lỗi được retry, không làm sập pipeline.

## 2B Decision model (triage tầng 2)

**Sở hữu:** `src/gm/triage_model.py` (mới), phần triage trong `src/gm/pipeline.py`,
migration thêm `events.decision jsonb`, `src/gm/cli.py` (lệnh eval).

1. **Vị trí trong pipeline:** rule chạy trước. Chỉ khi verdict của rule là `default` (không rule nào
   khớp) mới gọi decision model. Rule khớp rõ ràng (ignore/log/notify) thì quyết định luôn, không tốn
   token.
2. **Input:** sender, subject, labels, có list-unsubscribe hay không, tên file đính kèm, và tối đa 1500
   ký tự đầu của thân thư (bọc delimiter như `llm.py`). **Output** JSON:
   `{action: ignore|log|notify, importance: low|medium|high, reason: string}`. Timeout ngắn
   (`GM_TRIAGE_TIMEOUT_S=15`). Parse lỗi hoặc timeout → dùng `rules.default`, ghi `decision.error`.
3. Lưu `events.decision = {model, action, importance, reason, latency_ms}`. Dashboard hiện lý do.
4. **Few-shot từ phản hồi** (sau khi 2C đã merge): lấy N ví dụ gần nhất từ `triage_feedback` (sender,
   subject, nhãn) đưa vào prompt. Nếu 2C chưa có thì bỏ qua phần này; không được để 2B phụ thuộc
   cứng vào 2C.
5. Cờ `GM_DECISION_ENABLED` (mặc định `true` khi đã cấu hình `GM_TRIAGE_MODEL`).
6. **Đánh giá:** `gm eval triage --days 30` so quyết định của model (chạy lại trên các event có
   feedback) với nhãn của bạn, in precision/recall cho `notify` và ma trận nhầm lẫn. Dùng lệnh này khi
   đổi model hoặc đổi prompt.

**Test:** model trả JSON hợp lệ, JSON hỏng, timeout; rule khớp thì không gọi model.

## 2C Phản hồi & rule do người dùng quản lý

**Sở hữu:** `src/gm/feedback.py` (mới), `src/gm/triage.py`, `src/gm/templates/rules.html` (mới),
migration thêm `triage_feedback` và `triage_rules`; nút feedback trong handler `notify` (phối hợp với
code Telegram của 1A).

1. Bảng:
   ```
   triage_feedback(event_id bigint pk fk events, label varchar(16),  -- important|not_needed
                   created_at)
   triage_rules(id bigserial pk, name text, when_ jsonb, action varchar(8), source varchar(16),
                enabled bool default true, created_at)   -- when_ cùng schema với triage.When
   ```
2. Thêm hàng nút thứ hai trên mỗi thông báo: `[👍 Đúng] [🔕 Không cần báo]` với
   `data = "f:<event_id>:up|down"`. Bấm "Không cần báo" thì hỏi tiếp
   `[Tắt sender này] [Tắt cả domain] [Chỉ lần này]`. Hai nút đầu tạo `triage_rules` (action `log`).
3. Intent mới `mute_sender` trong router của 1B (ví dụ "đừng báo mail từ bên này nữa"), dùng chung
   code tạo rule.
4. `RuleFile.get()` gộp rule theo thứ tự: rule trong DB (enabled) trước, rồi `rules.yaml`. Có cache
   và invalidate khi bảng thay đổi.
5. Trang `/rules`: liệt kê rule từ DB và từ YAML (YAML chỉ đọc), bật/tắt/xoá rule DB (form POST).
6. Mail đã `log` không bao giờ được báo, nên bạn không có dịp sửa sai. Vì vậy `/status` trên Telegram
   và dashboard có mục "Mail đã bỏ qua hôm nay (N)" để xem và bấm "Đáng ra phải báo" (ghi feedback
   `important`).

**Test:** gộp thứ tự rule; tạo rule từ callback; rule bị tắt thì không áp dụng.

## 2D Observability & vận hành

**Sở hữu:** `src/gm/metrics.py` (mới), `src/gm/maintenance.py` (mới), `src/gm/templates/jobs.html`
(mới), `infra/launchd/`, `scripts/backup.sh`, logging trong `main.py`.

1. **Metrics** (`prometheus-client`), endpoint `/metrics`. Yêu cầu header
   `Authorization: Bearer $GM_METRICS_TOKEN` (không qua session):
   - `gm_events_total{source,status}`, `gm_jobs{status}` (gauge, cập nhật định kỳ),
     `gm_job_duration_seconds{subscriber}`, `gm_job_failures_total{subscriber}`
   - `gm_llm_request_seconds{role}`, `gm_llm_errors_total{role,kind}`,
     `gm_notifications_total{channel}`, `gm_ingest_lag_seconds{source}` (now − last_sync_at)
2. **Dead-letter UI** `/jobs?status=dead`: xem lỗi, nút "Chạy lại" (POST, đặt lại `pending`,
   `attempts=0`) và "Bỏ" (`discarded`). `/status` trên Telegram báo số job dead.
3. **Dọn dữ liệu** (background, mỗi ngày): xoá `jobs` đã `done` quá 14 ngày (bảng này tăng nhanh);
   xoá `events.data.snippet`/`summary` quá `GM_RETENTION_DAYS` (mặc định 180), giữ metadata; xoá
   `chat_messages` quá 90 ngày.
4. **Chạy 24/7 bằng launchd:**
   - `infra/launchd/*.plist.template` cho gm, cho hai instance mlx và (nếu chạy native) LiteLLM:
     `KeepAlive`, `RunAtLoad`, `WorkingDirectory`, log ra `~/Library/Logs/google-management/`.
   - Lệnh `gm service install|uninstall|status`: render template vào `~/Library/LaunchAgents`, rồi
     in lệnh `launchctl bootstrap gui/$UID …` để **người dùng tự chạy**.
   - Log: `RotatingFileHandler` (10 MB × 5).
   - Tài liệu về ngủ máy: Mac ngủ thì mọi thứ dừng (ingest sẽ bù khi dậy). Nêu các lựa chọn
     (`caffeinate -s` trong plist, hoặc chỉnh Energy settings) để người dùng chọn. Không tự đổi
     setting hệ thống.
5. **Backup:** `scripts/backup.sh` chạy `pg_dump` qua `docker compose exec` vào
   `~/Backups/google-management/` và giữ 14 bản, có plist hẹn giờ hằng ngày. Ghi cách restore vào
   README.

**Test:** metrics endpoint có auth; dọn dữ liệu đúng ngưỡng; retry job dead.

## 2E Nguồn Calendar & Drive

**Sở hữu:** `src/gm/sources/` (mới: `base.py`, `calendar.py`, `drive.py`), migration thêm
`sync_state`, đăng ký trong `main.py`, handler thông báo cho 2 nguồn mới.

1. **Interface `Source`** (`base.py`): `name`, `scopes`, `poll_interval_s`,
   `async bootstrap(account)`, `async sync(account) -> int`. Cùng nguyên tắc như Gmail: cursor, insert
   event (dedupe theo `(source, external_id)`) và publish **trong một transaction**. Bảng
   `sync_state(account, source, cursor jsonb, last_sync_at, last_error, pk(account, source))`. Gmail
   giữ `gmail_state` cũ; không cần migrate.
2. **Calendar** (scope `calendar.readonly`), poll 2 phút/lần:
   - `events.list(calendarId="primary", syncToken=…, showDeleted=True)`. Bootstrap:
     `timeMin=now`, chạy hết các trang để lấy `nextSyncToken` mà **không publish** sự kiện cũ. Gặp
     `410 Gone` thì bootstrap lại.
   - `external_id = f"{event.id}:{event.updated}"` (mỗi phiên bản là một event).
   - Phân loại trong ingest: `calendar.invitation` (mình là attendee với `responseStatus=needsAction`
     và không phải organizer), `calendar.changed` (đổi giờ hoặc địa điểm của sự kiện trong 7 ngày
     tới), `calendar.cancelled`. Các thay đổi khác chỉ ghi log.
   - Thông báo dùng template, không cần LLM: tiêu đề, giờ (theo timezone của lịch), người mời, link.
     Nút trả lời lời mời để Phase 3 (cần scope ghi và approval).
3. **Drive** (scope `drive.metadata.readonly`), poll 5 phút/lần:
   - `changes.getStartPageToken` rồi `changes.list(pageToken, fields=…)`.
   - Chỉ báo `drive.shared_with_me`: file mới được chia sẻ với mình (`sharedWithMeTime` mới, owner
     không phải mình), gồm tên file, người chia sẻ và link. Tên file là dữ liệu không tin cậy (plain
     text, OK).
4. Scope mới vào `REQUIRED_SCOPES` (bất biến 4). Nguồn bị tắt (`GM_SOURCES=gmail,calendar,drive`)
   thì không xin scope của nó.

**Test:** fake Calendar/Drive; bootstrap không publish dữ liệu cũ; 410 → resync; dedupe theo phiên bản.

## 2F (tuỳ chọn) Event bus trên NATS JetStream

Chỉ làm nếu thật sự cần, ví dụ tách worker thành nhiều process hoặc nhiều máy, hoặc muốn học
JetStream. Với một người dùng, Postgres đủ dùng lâu dài.

- Giữ nguyên `EventBus.publish/subscribe` và chữ ký handler.
- Giữ đảm bảo transaction: publish vẫn ghi vào Postgres (outbox). Một **relay** đẩy outbox sang
  JetStream với `Nats-Msg-Id = job id` để JetStream tự dedupe. Mỗi subscriber là một durable consumer,
  ack sau khi handler xong; dùng `max_deliver` và backoff tương đương với bản Postgres.
- Cờ `GM_BUS_BACKEND=postgres|jetstream`. Toàn bộ test của bus chạy với cả hai backend.

## Phase 2 xong khi

- Mọi lời gọi LLM đi qua LiteLLM, có số liệu token theo key.
- Decision model chạy sau rule và có số đo precision/recall.
- Bấm "Không cần báo" ra được rule.
- gm và các model tự khởi động sau khi máy restart; có backup hằng ngày.
- Lời mời Calendar và file Drive được chia sẻ đều được thông báo.
