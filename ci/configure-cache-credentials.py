"""Provision the ephemeral CI runner's private-cache credentials without logging secrets."""

import json
import os
from pathlib import Path
import tomllib
from urllib.parse import urlsplit


root = Path(__file__).resolve().parent.parent
config = tomllib.loads((root / "corgi.toml").read_text())
url = config.get("cache", {}).get("read-url", "")
if (urlsplit(url).hostname or "").endswith(".r2.cloudflarestorage.com"):
    fields = {}
    for field, variable in [
        ("access-key-id", "CORGI_R2_ACCESS_KEY_ID"),
        ("secret-access-key", "CORGI_R2_SECRET_ACCESS_KEY"),
    ]:
        value = os.environ.get(variable)
        if not value:
            # Corgi will warn and build locally without read credentials.
            # Explicit publishing still requires its write credentials.
            raise SystemExit(0)
        fields[field] = value
    path = Path.home() / ".config" / "corgi" / "credentials"
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(
        f"[{json.dumps(url)}]\n"
        + "".join(f"{field} = {json.dumps(value)}\n" for field, value in fields.items())
    )
