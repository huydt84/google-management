"""Outbound notifications. Phase 0 is one-way; Phase 1 adds replies (long polling)."""

import logging
from typing import Protocol

import httpx

from gm.config import Settings

log = logging.getLogger(__name__)

TELEGRAM_LIMIT = 4096
IMPORTANCE_ICON = {"high": "🔴", "medium": "🟡", "low": "⚪"}


class Notifier(Protocol):
    async def send(self, text: str) -> None: ...
    async def aclose(self) -> None: ...


def format_email_notification(
    *, sender: str, subject: str, link: str, summary: dict | None, error: str | None = None
) -> str:
    imp = (summary or {}).get("importance", "medium")
    lines = [f"{IMPORTANCE_ICON.get(imp, '🟡')} 📧 {subject}", f"Từ: {sender}", ""]
    if summary:
        lines.append(summary.get("summary", "").strip())
        actions = summary.get("suggested_actions") or []
        if actions:
            lines += ["", "Gợi ý: " + " · ".join(actions)]
    else:
        lines.append(f"(Không tóm tắt được: {error or 'LLM unavailable'})")
    lines += ["", link]
    text = "\n".join(lines)
    return text if len(text) <= TELEGRAM_LIMIT else text[: TELEGRAM_LIMIT - 1] + "…"


class TelegramNotifier:
    def __init__(self, token: str, chat_id: str,
                 transport: httpx.AsyncBaseTransport | None = None):
        self._chat_id = chat_id
        self._http = httpx.AsyncClient(
            base_url=f"https://api.telegram.org/bot{token}", timeout=20, transport=transport
        )

    async def send(self, text: str) -> None:
        # No parse_mode: email-derived text is sent as plain text, never as markup.
        resp = await self._http.post(
            "/sendMessage",
            json={"chat_id": self._chat_id, "text": text, "disable_web_page_preview": True},
        )
        if resp.status_code != 200:
            # Don't log the URL: it contains the bot token.
            raise RuntimeError(f"telegram sendMessage failed: {resp.status_code} {resp.text[:200]}")

    async def aclose(self) -> None:
        await self._http.aclose()


class LogNotifier:
    """Used when Telegram is not configured: notifications only go to the log."""

    async def send(self, text: str) -> None:
        log.info("NOTIFICATION\n%s", text)

    async def aclose(self) -> None:
        pass


def make_notifier(settings: Settings) -> Notifier:
    if settings.telegram_enabled:
        return TelegramNotifier(settings.telegram_bot_token, settings.telegram_chat_id)  # type: ignore[arg-type]
    log.warning("Telegram not configured (GM_TELEGRAM_BOT_TOKEN/GM_TELEGRAM_CHAT_ID); "
                "notifications go to the log")
    return LogNotifier()
