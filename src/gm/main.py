import asyncio
import logging
import warnings
from collections.abc import Awaitable, Callable
from contextlib import asynccontextmanager
from dataclasses import dataclass
from datetime import timedelta
from urllib.parse import urlparse

from authlib.integrations.starlette_client import OAuth
from fastapi import FastAPI, Request
from fastapi.responses import JSONResponse, RedirectResponse
from sqlalchemy.ext.asyncio import AsyncEngine, async_sessionmaker
from starlette.middleware.sessions import SessionMiddleware
from starlette.middleware.trustedhost import TrustedHostMiddleware

from gm import auth, web
from gm.bus import EventBus
from gm.config import Settings, get_settings
from gm.crypto import SecretBox
from gm.db import create_schema, make_engine, make_sessionmaker
from gm.gmail.ingest import GmailIngestor
from gm.google_creds import GoogleCredentialStore
from gm.llm import LLMClient
from gm.notify.telegram import Notifier, make_notifier
from gm.pipeline import Pipeline
from gm.triage import RuleFile

warnings.filterwarnings("ignore", message=".*httpx module is deprecated.*")
log = logging.getLogger("gm")


@dataclass
class Deps:
    settings: Settings
    engine: AsyncEngine
    sm: async_sessionmaker
    bus: EventBus
    creds: GoogleCredentialStore
    gmail: GmailIngestor
    rules: RuleFile
    llm: LLMClient
    notifier: Notifier
    oauth: OAuth
    pipeline: Pipeline


async def _supervise(name: str, factory: Callable[[], Awaitable[None]], retry_s: float = 30):
    """Keep a background loop alive: log crashes and restart after a pause."""
    while True:
        try:
            await factory()
            return
        except asyncio.CancelledError:
            raise
        except Exception:
            log.exception("background task %s crashed; restarting in %ss", name, retry_s)
            await asyncio.sleep(retry_s)


async def _every(seconds: float, fn: Callable[[], Awaitable[object]]) -> None:
    while True:
        await asyncio.sleep(seconds)
        await fn()


def build_deps(settings: Settings, **overrides) -> Deps:
    engine = overrides.get("engine") or make_engine(settings.database_url)
    sm = make_sessionmaker(engine)
    bus = EventBus(sm, default_max_attempts=settings.job_max_attempts)
    creds = GoogleCredentialStore(settings, sm, SecretBox(settings.encryption_key))
    gmail_factory = overrides.get("gmail_factory") or creds.gmail
    gmail = GmailIngestor(settings, sm, bus, creds, gmail_factory=gmail_factory)
    rules = RuleFile(settings.rules_path)
    llm = overrides.get("llm") or LLMClient(settings)
    notifier = overrides.get("notifier") or make_notifier(settings)
    pipeline = Pipeline(sessionmaker=sm, bus=bus, gmail_factory=gmail_factory, rules=rules,
                        llm=llm, notifier=notifier)
    pipeline.register()
    return Deps(settings, engine, sm, bus, creds, gmail, rules, llm, notifier,
                auth.build_oauth(settings), pipeline)


def _background_tasks(deps: Deps) -> list[asyncio.Task]:
    s = deps.settings
    specs: list[tuple[str, Callable[[], Awaitable[None]]]] = [
        (f"worker-{i}", lambda i=i: deps.bus.run_worker(i)) for i in range(s.worker_concurrency)
    ]
    specs.append(("requeue-stale", lambda: _every(300, deps.bus.requeue_stale)))
    if s.gmail_ingest_mode == "poll":
        specs.append(("gmail-poll", lambda: deps.gmail.poll_loop(s.gmail_poll_interval_s)))
    else:
        specs.append(("gmail-pubsub", deps.gmail.pubsub_loop))
        specs.append(("gmail-watch-renew", deps.gmail.watch_renew_loop))
        specs.append(("gmail-safety-poll",
                      lambda: deps.gmail.poll_loop(s.gmail_safety_poll_interval_s)))
    return [asyncio.create_task(_supervise(n, f), name=n) for n, f in specs]


def create_app(settings: Settings | None = None, **overrides) -> FastAPI:
    settings = settings or get_settings()
    logging.basicConfig(level=logging.INFO,
                        format="%(asctime)s %(levelname)s %(name)s: %(message)s")

    @asynccontextmanager
    async def lifespan(app: FastAPI):
        deps = build_deps(settings, **overrides)
        await create_schema(deps.engine)
        app.state.deps = deps
        tasks: list[asyncio.Task] = []
        if settings.run_background:
            await deps.bus.requeue_stale(older_than=timedelta(0))
            tasks = _background_tasks(deps)
        log.info("Google Management ready on %s (owner: %s)", settings.base_url,
                 settings.allowed_email)
        try:
            yield
        finally:
            for t in tasks:
                t.cancel()
            await asyncio.gather(*tasks, return_exceptions=True)
            await deps.llm.aclose()
            await deps.notifier.aclose()
            await deps.engine.dispose()

    app = FastAPI(lifespan=lifespan, docs_url=None, redoc_url=None, openapi_url=None)

    # Reject requests whose Host header isn't ours (blocks DNS-rebinding attacks on localhost).
    allowed_hosts = {"localhost", "127.0.0.1", urlparse(settings.base_url).hostname or "localhost"}
    app.add_middleware(TrustedHostMiddleware, allowed_hosts=sorted(allowed_hosts))
    app.add_middleware(
        SessionMiddleware,
        secret_key=settings.session_secret,
        session_cookie="gm_session",
        max_age=settings.session_max_age_s,
        same_site="lax",  # cross-site POSTs carry no cookie -> CSRF-safe forms
        https_only=settings.base_url.startswith("https://"),
    )

    @app.exception_handler(auth.LoginRequired)
    async def _login_required(request: Request, exc: auth.LoginRequired):
        if request.url.path.startswith("/api/"):
            return JSONResponse({"detail": "login required"}, status_code=401)
        return RedirectResponse("/", status_code=303)

    app.include_router(auth.router)
    app.include_router(web.router)
    return app
