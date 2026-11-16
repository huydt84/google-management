from pathlib import Path

from fastapi import APIRouter, Depends, Request
from fastapi.responses import JSONResponse, RedirectResponse
from fastapi.templating import Jinja2Templates
from sqlalchemy import select, text

from gm.auth import LoginRequired, current_email
from gm.db import Event, GmailState

router = APIRouter()
templates = Jinja2Templates(directory=Path(__file__).parent / "templates")


async def _recent_events(request: Request, limit: int = 50) -> list[Event]:
    async with request.app.state.deps.sm() as s:
        rows = await s.execute(select(Event).order_by(Event.id.desc()).limit(limit))
        return list(rows.scalars())


@router.get("/")
async def index(request: Request):
    deps = request.app.state.deps
    try:
        email = current_email(request)
    except LoginRequired:
        return templates.TemplateResponse(request, "login.html", {})
    account = await deps.creds.account(email)
    async with deps.sm() as s:
        gmail_state = await s.get(GmailState, email)
    return templates.TemplateResponse(
        request,
        "index.html",
        {
            "email": email,
            "account": account,
            "gmail_state": gmail_state,
            "settings": deps.settings,
            "job_stats": await deps.bus.stats(),
            "events": await _recent_events(request),
        },
    )


@router.post("/api/gmail/sync")
async def sync_now(request: Request, email: str = Depends(current_email)):
    await request.app.state.deps.gmail.sync(email)
    return RedirectResponse("/", status_code=303)


@router.get("/api/events")
async def events_json(request: Request, _: str = Depends(current_email)):
    return [
        {
            "id": e.id, "source": e.source, "kind": e.kind, "status": e.status, "rule": e.rule,
            "data": e.data, "summary": e.summary, "created_at": e.created_at.isoformat(),
        }
        for e in await _recent_events(request, 100)
    ]


@router.get("/healthz")
async def healthz(request: Request):
    try:
        async with request.app.state.deps.sm() as s:
            await s.execute(text("select 1"))
        return {"ok": True}
    except Exception:
        return JSONResponse({"ok": False}, status_code=503)
