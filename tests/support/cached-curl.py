#!/usr/bin/env python3
"""Cache successful test downloads until target/curl-cache is cleared."""

import fcntl
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile


def cache_request(arguments):
    """Extract the destination from Corgi's single-GET invocations."""
    forwarded = []
    output = None
    urls = 0
    arguments = iter(arguments)
    for argument in arguments:
        if argument in ("-o", "--output"):
            if output is not None:
                return None
            output = next(arguments, None)
            if output is None:
                return None
        elif argument in ("--proto", "--proto-redir"):
            value = next(arguments, None)
            if value is None:
                return None
            forwarded.extend((argument, value))
        else:
            if argument.startswith(("https://", "http://")):
                urls += 1
            elif argument not in ("-sSfL", "--silent", "--show-error", "--fail", "--location"):
                return None
            forwarded.append(argument)
    # Error and redirect bodies must not become cached artifacts.
    if urls != 1 or not ("-sSfL" in forwarded or {"--fail", "--location"}.issubset(forwarded)):
        return None
    return forwarded, output


def main(arguments):
    request = cache_request(arguments)
    if request is None:
        return subprocess.call(["curl", "-q", *arguments])
    forwarded, output = request
    key = hashlib.sha256(json.dumps(forwarded).encode()).hexdigest()
    cache = Path(os.environ.get(
        "CORGI_CURL_CACHE", Path(__file__).resolve().parents[2] / "target" / "curl-cache"
    )).resolve()
    cache.mkdir(parents=True, exist_ok=True)
    artifact = cache / key
    # Lock files persist: unlinking them can create multiple locks for one key.
    with open(cache / (key + ".lock"), "a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        if not artifact.is_file():
            descriptor, temporary = tempfile.mkstemp(prefix=key + ".", suffix=".tmp", dir=cache)
            os.close(descriptor)
            try:
                # Ignore curlrc so test requests are defined by their arguments.
                status = subprocess.call(["curl", "-q", *forwarded, "--output", temporary])
                if status:
                    return status
                os.replace(temporary, artifact)
            finally:
                if os.path.exists(temporary):
                    os.unlink(temporary)
    try:
        with artifact.open("rb") as source:
            if output is None or output == "-":
                shutil.copyfileobj(source, sys.stdout.buffer)
                sys.stdout.buffer.flush()
            else:
                with open(output, "wb") as destination:
                    shutil.copyfileobj(source, destination)
    except OSError as error:
        print(f"cached-curl: {error}", file=sys.stderr)
        return 23  # curl's write-error status
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
