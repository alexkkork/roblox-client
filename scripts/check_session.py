#!/usr/bin/env python3
"""Read-only session check against Roblox. Never print or save the cookie."""
from datetime import datetime, timezone
import http.client
import json
from pathlib import Path
import stat


def main():
    root = Path(__file__).resolve().parent.parent
    path = root / "secrets/roblox-cookie.txt"
    result = {"checked_at": datetime.now(timezone.utc).isoformat(),
              "endpoint": "https://users.roblox.com/v1/users/authenticated",
              "method": "GET", "authenticated": False, "cookie_logged": False}
    connection = None
    try:
        from platform_files import read_private
        cookie = read_private(path, 32768).decode('utf-8').strip()
        if cookie.startswith(".ROBLOSECURITY="):
            cookie = cookie[len(".ROBLOSECURITY="):].rstrip(";")
        if not cookie or any(ord(c) < 33 or ord(c) > 126 or c == ";" for c in cookie):
            raise ValueError("Expected only the cookie value, optionally prefixed with .ROBLOSECURITY=")
        connection = http.client.HTTPSConnection("users.roblox.com", timeout=15)
        # HTTPS certificate validation remains enabled. No redirect is followed.
        connection.request("GET", "/v1/users/authenticated", headers={
            "Cookie": ".ROBLOSECURITY=" + cookie,
            "Accept": "application/json", "User-Agent": "LocalRobloxResearch/1.0"})
        response = connection.getresponse()
        result["http_status"] = response.status
        payload = response.read(65536)
        if response.status == 200:
            body = json.loads(payload)
            result["authenticated"] = isinstance(body.get("id"), int) and isinstance(body.get("name"), str)
        # Identity fields, response headers and response bodies are not persisted.
    except ValueError as error:
        result["error"] = str(error) if not isinstance(error, json.JSONDecodeError) else "Invalid JSON response"
    except Exception as error:
        result["error_type"] = type(error).__name__
    finally:
        if connection is not None:
            connection.close()
    (root / "outputs").mkdir(exist_ok=True)
    (root / "outputs/session-check.json").write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps(result, indent=2))
    return 0 if result["authenticated"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
