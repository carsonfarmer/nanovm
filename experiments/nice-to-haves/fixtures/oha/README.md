# Archived oha 1.16.0 parser fixtures

These four JSON files are unmodified stdout from real oha 1.16.0 runs against the
[local synthetic handler](../../local_load_fixture.py), captured 2026-11-06.
Each used `--no-tui --output-format json -n 12 -c 2 -t 1s -r 0` with a disposable
loopback port. No nanovm, guest, proxy, Collector or production target participated.
The recorded timings are fixture observations, **not performance comparisons**.

| File | HTTP200 | HTTP503 | Disconnected before response | Application errors if only 200 is accepted |
| --- | ---: | ---: | ---: | ---: |
| [success.json](success.json) | 12 | 0 | 0 | 0 |
| [http-error.json](http-error.json) | 0 | 12 | 0 | 12 |
| [mixed.json](mixed.json) | 4 | 4 | 4 | 8 |
| [transport-error.json](transport-error.json) | 0 | 0 | 12 | 12 |

The HTTP503-only file reports `summary.successRate: 1.0`. That is not application
success. The transport-error file's latency percentiles are `null`, not zero.
The importer preserves both facts without trusting rounded `metrics.latency_ms`
or deriving request counts from floating-point rates.

SHA-256 values of the raw files:

```text
464ef5718e2eaddc3a7070fae19313ec73f40d1a12fa5daca7280f7087324d6d  success.json
da77e6c1bc96f139db6bdd3eca78e7fb76bfed09c3b1f375ad1ba5e1d2eff28d  http-error.json
d3a8e461188d79ceb60575846ae8344f28c5ac480f552f21fc89b59b4077c54d  mixed.json
fe8f2cd260521adf62d5cce34759ca68bf04fff46095364133808ed6ab4d1b44  transport-error.json
```

Future fixture runs write to a new directory and refuse overwrite. They produce
fresh raw JSON, metadata, normalized rows and an independently counted server
report; their elapsed times and machine metadata will differ.
