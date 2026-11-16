"""Two separate OAuth flows against the same Google client:

/auth/login           OpenID login to this web UI. Only GM_ALLOWED_EMAIL gets a session.
/connect/google       Grants API scopes (offline access -> refresh token) for background work.
"""

import logging
import time

from authlib.integrations.starlette_client import OAuth, OAuthError
from fastapi import APIRouter, HTTPException, Request
from fastapi.responses import HTMLResponse, RedirectResponse

from gm.config import Settings
from gm.google_creds import GMAIL_SCOPES

log = logging.getLogger(__name__)

GOOGLE_METADATA = "https://accounts.google.com/.well-known/openid-configuration"

router = APIRouter()


class LoginRequired(Exception):
    pass


def build_oauth(settings: Settings) -> OAuth:
    oauth = OAuth()
    common = dict(
        server_metadata_url=GOOGLE_METADATA,
        client_id=settings.google_client_id,
        client_secret=settings.google_client_secret,
    )
    oauth.register("google_login", client_kwargs={"scope": "openid email profile"}, **common)
    oauth.register(
        "google_api",
        client_kwargs={"scope": " ".join(["openid", "email", *GMAIL_SCOPES])},
        **common,
    )
    return oauth


def current_email(request: Request) -> str:
    """Dependency: the logged-in owner, or LoginRequired."""
    settings: Settings = request.app.state.deps.settings
    email = request.session.get("email")
    if not email or email != settings.allowed_email:
        raise LoginRequired()
    return email


def _verified_owner_email(token: dict, settings: Settings) -> str:
    info = token.get("userinfo") or {}
    email = str(info.get("email", "")).lower()
    if not info.get("email_verified") or email != settings.allowed_email:
        log.warning("rejected Google sign-in for %r", email or "<unknown>")
        raise HTTPException(status_code=403, detail="This Google account is not allowed.")
    return email


@router.get("/auth/login")
async def login(request: Request):
    deps = request.app.state.deps
    return await deps.oauth.google_login.authorize_redirect(
        request,
        f"{deps.settings.base_url}/auth/callback",
        prompt="select_account",
        login_hint=deps.settings.allowed_email,
    )


@router.get("/auth/callback", name="auth_callback")
async def auth_callback(request: Request):
    deps = request.app.state.deps
    try:
        token = await deps.oauth.google_login.authorize_access_token(request)
    except OAuthError as e:
        raise HTTPException(status_code=400, detail=f"OAuth error: {e.error}") from e
    email = _verified_owner_email(token, deps.settings)
    request.session.clear()
    request.session.update({"email": email, "login_at": int(time.time())})
    return RedirectResponse("/", status_code=303)


@router.post("/auth/logout")
async def logout(request: Request):
    request.session.clear()
    return RedirectResponse("/", status_code=303)


@router.get("/connect/google")
async def connect_google(request: Request):
    current_email(request)
    deps = request.app.state.deps
    return await deps.oauth.google_api.authorize_redirect(
        request,
        f"{deps.settings.base_url}/connect/google/callback",
        access_type="offline",
        prompt="consent",  # forces Google to (re)issue a refresh token
        include_granted_scopes="true",
        login_hint=deps.settings.allowed_email,
    )


@router.get("/connect/google/callback")
async def connect_google_callback(request: Request):
    session_email = current_email(request)
    deps = request.app.state.deps
    try:
        token = await deps.oauth.google_api.authorize_access_token(request)
    except OAuthError as e:
        raise HTTPException(status_code=400, detail=f"OAuth error: {e.error}") from e
    email = _verified_owner_email(token, deps.settings)
    if email != session_email:
        raise HTTPException(status_code=403, detail="Connected account differs from login.")

    refresh_token = token.get("refresh_token")
    if not refresh_token:
        return HTMLResponse(
            "Google did not return a refresh token. Remove the app at "
            "https://myaccount.google.com/permissions and connect again.",
            status_code=400,
        )
    granted = str(token.get("scope", "")).split()
    missing = [s for s in GMAIL_SCOPES if s not in granted]
    if missing:
        return HTMLResponse(f"Missing scopes (untick nothing on the consent screen): {missing}",
                            status_code=400)

    await deps.creds.save(email, refresh_token, granted)
    await deps.gmail.bootstrap(email)
    return RedirectResponse("/", status_code=303)
