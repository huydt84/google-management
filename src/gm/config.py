from functools import lru_cache
from pathlib import Path
from typing import Literal

from pydantic import field_validator
from pydantic_settings import BaseSettings, SettingsConfigDict


class Settings(BaseSettings):
    """All configuration comes from environment variables prefixed with GM_ (or .env)."""

    model_config = SettingsConfigDict(env_file=".env", env_prefix="GM_", extra="ignore")

    # --- Web server (bind to localhost only; use Tailscale if you need remote access) ---
    host: str = "127.0.0.1"
    port: int = 8080
    base_url: str = "http://localhost:8080"
    session_secret: str
    session_max_age_s: int = 7 * 24 * 3600

    # --- Identity: the only Google account allowed to log in ---
    allowed_email: str

    # --- Secrets at rest (Fernet key, generate with `gm gen-keys`) ---
    encryption_key: str

    # --- Storage ---
    database_url: str = "postgresql+asyncpg://gm:gm@localhost:5433/gm"

    # --- Google OAuth client (type: Web application) ---
    google_client_id: str
    google_client_secret: str

    # --- Gmail ingestion ---
    # poll:   history.list every gmail_poll_interval_s (no GCP Pub/Sub needed)
    # pubsub: users.watch -> Pub/Sub topic, pulled from gmail_pubsub_subscription
    gmail_ingest_mode: Literal["poll", "pubsub"] = "poll"
    gmail_poll_interval_s: int = 60
    gmail_safety_poll_interval_s: int = 900
    gmail_pubsub_topic: str | None = None
    gmail_pubsub_subscription: str | None = None
    gmail_label_ids: list[str] = ["INBOX"]

    # --- Triage ---
    rules_path: Path = Path("rules.yaml")

    # --- LLM (any OpenAI-compatible endpoint: mlx-server-runtime now, LiteLLM later) ---
    llm_base_url: str = "http://127.0.0.1:8000/v1"
    llm_api_key: str = "not-needed"
    llm_model: str = "mlx-community/Qwen2.5-7B-Instruct-4bit"
    llm_timeout_s: float = 120.0
    llm_max_tokens: int = 600
    llm_max_input_chars: int = 8000
    summary_language: str = "Vietnamese"

    # --- Notifications ---
    telegram_bot_token: str | None = None
    telegram_chat_id: str | None = None

    # --- Background processing ---
    run_background: bool = True
    worker_concurrency: int = 2
    job_max_attempts: int = 5

    @field_validator("allowed_email")
    @classmethod
    def _lower_email(cls, v: str) -> str:
        return v.strip().lower()

    @field_validator("base_url", "llm_base_url")
    @classmethod
    def _strip_slash(cls, v: str) -> str:
        return v.rstrip("/")

    @property
    def telegram_enabled(self) -> bool:
        return bool(self.telegram_bot_token and self.telegram_chat_id)


@lru_cache
def get_settings() -> Settings:
    return Settings()  # type: ignore[call-arg]
