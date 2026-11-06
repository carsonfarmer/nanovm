#!/usr/bin/env python3
"""One HTTP attempt with a caller-owned operation ID; never automatically retry."""

import argparse
import http.client
import json
import math
from pathlib import Path
import re
import sys
import urllib.error
import urllib.request
from urllib.parse import urlsplit


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


def attempt(url, operation_id, body, timeout=5, response_limit=65536):
    parsed = urlsplit(url)
    if parsed.scheme not in ("http", "https") or not parsed.hostname or parsed.username or parsed.password or parsed.fragment:
        raise ValueError("use an HTTP(S) URL without credentials or fragment")
    if not re.fullmatch(r"[A-Za-z0-9_-]{1,128}", operation_id):
        raise ValueError("operation ID must contain 1–128 ASCII letters, digits, underscores or hyphens")
    if not math.isfinite(timeout) or timeout <= 0:
        raise ValueError("timeout must be positive and finite")
    if type(response_limit) is not int or response_limit < 1:
        raise ValueError("response_limit must be a positive integer")
    request = urllib.request.Request(url, data=body, method="POST", headers={
        "Content-Type": "application/json", "Idempotency-Key": operation_id,
    })
    # Do not inherit proxy credentials/routes, follow redirects, or install retries.
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), NoRedirect())
    try:
        try:
            response = opener.open(request, timeout=timeout)
        except urllib.error.HTTPError as error:
            response = error
        with response:
            status = response.status
            expected = getattr(response, "length", None)
            data = response.read(response_limit + 1)
            if expected is not None and len(data) < min(expected, response_limit + 1):
                raise http.client.IncompleteRead(data)
            return {"operation_id": operation_id, "outcome": "http-response", "status": status,
                    "body_utf8": data[:response_limit].decode("utf-8", errors="replace"),
                    "body_truncated": len(data) > response_limit,
                    "retry": "manual; same operation ID only if application implements deduplication"}
    except (OSError, urllib.error.URLError, http.client.HTTPException) as error:
        # No URL/body/exception string: these may contain credentials or private data.
        return {"operation_id": operation_id, "outcome": "unknown", "error_type": type(error).__name__,
                "retry": "query application operation status or retry same ID under its deduplication contract"}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("url")
    parser.add_argument("--operation-id", required=True)
    parser.add_argument("--body", type=Path, required=True, help="JSON body file; not a command-line secret")
    parser.add_argument("--timeout", type=float, default=5, help="socket I/O timeout, not total wall-clock deadline")
    args = parser.parse_args()
    try:
        body = args.body.read_bytes()
        json.loads(body)
        result = attempt(args.url, args.operation_id, body, args.timeout)
        print(json.dumps(result, indent=2))
        return 0 if result.get("status", 0) in range(200, 300) else 1
    except (OSError, ValueError) as error:
        parser.exit(2, f"invalid input: {type(error).__name__}\n")


if __name__ == "__main__":
    sys.exit(main())
