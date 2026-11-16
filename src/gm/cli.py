import argparse
import secrets

from cryptography.fernet import Fernet


def main() -> None:
    parser = argparse.ArgumentParser(prog="gm", description="Google Management")
    sub = parser.add_subparsers(dest="cmd", required=True)
    sub.add_parser("serve", help="run the web server + background workers")
    sub.add_parser("gen-keys", help="print fresh GM_SESSION_SECRET and GM_ENCRYPTION_KEY values")
    args = parser.parse_args()

    if args.cmd == "gen-keys":
        print(f"GM_SESSION_SECRET={secrets.token_urlsafe(48)}")
        print(f"GM_ENCRYPTION_KEY={Fernet.generate_key().decode()}")
        return

    import uvicorn

    from gm.config import get_settings

    settings = get_settings()
    uvicorn.run(
        "gm.main:create_app",
        factory=True,
        host=settings.host,
        port=settings.port,
        proxy_headers=False,
    )
