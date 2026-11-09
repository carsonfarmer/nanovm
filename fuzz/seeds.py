#!/usr/bin/env python3
"""Writes the fuzz targets' seed inputs to fuzz/corpus/<target>/ (libFuzzer's
default corpus directory; it adds what it finds next to them). Run from the
repository root before the first `cargo fuzz run`."""

import hashlib
from pathlib import Path
import struct

OUT = Path(__file__).resolve().parent / "corpus"


def varint(v):
    out = bytearray()
    while v >= 0x80:
        out.append(v & 0x7F | 0x80)
        v >>= 7
    out.append(v)
    return bytes(out)


def field(n, value):
    """A length-delimited protobuf field, or a varint one for an int."""
    if isinstance(value, int):
        return varint(n << 3) + varint(value)
    value = value.encode() if isinstance(value, str) else value
    return varint(n << 3 | 2) + varint(len(value)) + value


def kv(key, value):
    return field(1, key) + field(2, field(1, value))


def undo(tag, *fields):
    put = lambda b: struct.pack("<Q", len(b)) + b
    return put(bytes([tag])) + b"".join(put(f if isinstance(f, bytes) else struct.pack("<Q", f)) for f in fields)


DIGEST = "sha256:" + "ab" * 32
forged = kv("nanovm.app", "victim") + kv("service.version", "old")
span = (field(1, b"\x01" * 16) + field(2, b"\x02" * 8) + field(5, "request") + field(9, forged)
        + field(11, field(2, "event") + field(3, forged)) + field(13, field(1, b"\x01" * 16) + field(4, forged)))
scope = field(1, "nanovm") + field(3, forged)
traces = field(1, field(1, field(1, forged)) + field(2, field(1, scope) + field(2, span) + field(2, span)))
logs = field(1, field(1, field(1, forged)) + field(2, field(1, scope) + field(2, field(5, field(1, "line")) + field(6, forged))))

SEEDS = {
    "ws_events": [b"OPEN\r\n", b"TEXT 5\r\nhello\r\n", b"BINARY 3\r\n\x00\x01\x02\r\n", b"CLOSE 2\r\n\x03\xe8\r\n",
                  b"CLOSE\r\n", b"DISCONNECT\r\n", b"OPEN\r\nTEXT 2\r\nhi\r\nTEXT 0\r\n\r\nc:subscribe\r\n"],
    "grip_publish": [
        b'{"items":[{"channel":"room","formats":{"ws-message":{"content":"hi"}}}]}',
        b'{"items":[{"channel":"a","formats":{"ws-message":{"content-bin":"AAEC"}}},{"channel":"b","formats":{}}]}',
        b'{"items":[]}',
    ],
    "txt_record": [
        b'"app=ghcr.io/org/app:prod; trust=https://token.actions.githubusercontent.com repo:org/app:ref:refs/heads/main; min=1.2.0"',
        b'"app=localhost:5000/x/y:latest; " "trust=https://i s1; trust=https://i s2"',
        # Found by fuzzing: a comma inside an identity (fixed in the proxy).
        b'"app=ghcr.io/org/apprp:od; trust=httsp://token,actions.githu mbruepo"',
    ],
    "app_ref": [
        f"ghcr.io/org/app@{DIGEST}\xffhttps://i s1, https://i s2\xff1.2.0\xffexample.com".encode("latin-1"),
        f"localhost:5000/x/y@{DIGEST}\xffdev dev".encode("latin-1"),
        # Found by fuzzing: a host starting with `.` (N-503).
        f"ghgppar/o@{DIGEST}\xffhttps://i s1\xff1.2.0\xff.com".encode("latin-1"),
    ],
    "journal_log": [
        undo(0, b"a/b", 10) + undo(1, b"f", 0, b"xyz") + undo(2, b"d") + undo(3, b"d2") + undo(4, b"a", b"b"),
        undo(5, b"a", b"b") + undo(2, b"c"),
        bytes(range(40)),
    ],
    "telemetry_stamp": [b"\x00" + traces, b"\x01" + logs],
}

for target, seeds in SEEDS.items():
    d = OUT / target
    d.mkdir(parents=True, exist_ok=True)
    for s in seeds:
        (d / hashlib.sha1(s).hexdigest()).write_bytes(s)
    print(f"{target}: {len(seeds)} seeds in {d}")
