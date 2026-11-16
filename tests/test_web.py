import base64
import json

import pytest
from conftest import OWNER, make_settings
from fastapi import HTTPException
from fastapi.testclient import TestClient
from itsdangerous import TimestampSigner

from gm.auth import _verified_owner_email
from gm.main import create_app


def session_cookie(secret: str, data: dict) -> str:
    # Same encoding as starlette.middleware.sessions.SessionMiddleware
    payload = base64.b64encode(json.dumps(data).encode())
    return TimestampSigner(secret).sign(payload).decode()


@pytest.fixture
def client(engine):
    # `engine` only resets the schema; TestClient runs the app on its own event loop,
    # so the app must create its own engine there.
    settings = make_settings()
    app = create_app(settings)
    with TestClient(app, base_url="http://localhost") as c:
        c.settings = settings
        yield c


def test_anonymous_sees_login_and_api_is_protected(client):
    r = client.get("/")
    assert r.status_code == 200 and "Đăng nhập bằng Google" in r.text
    assert client.get("/api/events").status_code == 401
    assert client.post("/api/gmail/sync").status_code == 401
    assert client.get("/healthz").json() == {"ok": True}


def test_foreign_host_header_rejected(client):
    assert client.get("/healthz", headers={"host": "evil.example"}).status_code == 400


def test_owner_session_sees_dashboard(client):
    client.cookies.set("gm_session", session_cookie(client.settings.session_secret,
                                                    {"email": OWNER}))
    r = client.get("/")
    assert r.status_code == 200 and "Kết nối Gmail" in r.text
    assert client.get("/api/events").json() == []


def test_session_for_other_email_is_not_accepted(client):
    client.cookies.set("gm_session", session_cookie(client.settings.session_secret,
                                                    {"email": "intruder@example.com"}))
    assert client.get("/api/events").status_code == 401


def test_only_verified_owner_can_sign_in():
    s = make_settings()
    ok = {"userinfo": {"email": OWNER.upper(), "email_verified": True}}
    assert _verified_owner_email(ok, s) == OWNER
    for bad in ({"email": "x@example.com", "email_verified": True},
                {"email": OWNER, "email_verified": False}, {}):
        with pytest.raises(HTTPException) as e:
            _verified_owner_email({"userinfo": bad}, s)
        assert e.value.status_code == 403
