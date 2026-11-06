# Batch 7 — client interoperability and language feasibility

## NH20/NH44: real HTTP bytes and unknown outcomes

```sh
python3 -B experiments/nice-to-haves/wire_fixture.py
```

The fixture owns its loopback HTTP server and temporary files; it accepts no target
URL. Five requests exercise both the Python client and the Node.js example
`operation_client.mjs`. Both send identical file bytes, including composed and
decomposed Unicode, emoji, spaces and a trailing newline. Both report an unknown
outcome when the handler accepts the operation and closes before replying. Explicit
same-ID retries succeed; adding a space to the body conflicts even though its JSON
meaning is unchanged. Exactly one in-memory application event remains. All five
checks passed on Node v26.12.1 and Python 3.11.6; the external-target refusal test
also passed. No JSON canonicalization is promised: **reuse the exact body file**.

The JS example permits only HTTP to literal 127.0.0.1, no redirects/retries, bounded
ID/body/response sizes and a 2-second socket inactivity timeout, not total deadline.
It exits 0 after emitting any outcome, including `unknown`/HTTP errors; inspect the
JSON. Invalid input exits 2. It intentionally does not return response bodies or
implement authentication/TLS. The fixture is not a deployment-ready HTTP service,
crash/recovery test or nanovm integration. Application state is the batch-4 model.

## NH25/NH26: pinned synchronous components really build and run

Only `languages/` inputs are tracked. Tools, generated bindings and binaries live
under `/tmp`; no guest package or shared dependency was touched or published.
Sources: [ComponentizeJS 0.23.0](https://www.npmjs.com/package/@bytecodealliance/componentize-js/v/0.23.0),
[jco 1.41.0](https://www.npmjs.com/package/@bytecodealliance/jco/v/1.41.0),
[componentize-py 0.25.1](https://pypi.org/project/componentize-py/0.25.1/).
Installed package README/help, rather than current upstream main, supplied commands.
Top-level versions are pinned below; transitive npm dependencies are not vendored
or lockfile-pinned in this experiment. No supply-chain/signature verification claim.

```sh
P=/tmp/nanovm-nice-tools
L=experiments/nice-to-haves/languages
npm install --prefix "$P/js-probe" --ignore-scripts --no-audit --no-fund @bytecodealliance/componentize-js@0.23.0 @bytecodealliance/jco@1.41.0
uv venv "$P/python-probe"
uv pip install --python "$P/python-probe/bin/python" componentize-py==0.25.1 wasmtime==38.0.0
"$P/js-probe/node_modules/.bin/jco" componentize "$L/probe.js" --wit "$L/probe.wit" --disable all -o "$P/js-probe/probe.wasm"
"$P/python-probe/bin/componentize-py" -d "$L/probe.wit" -w probe componentize --stub-wasi -p "$L" probe_app -o "$P/python-probe/probe.wasm"
"$P/js-probe/node_modules/.bin/jco" transpile "$P/js-probe/probe.wasm" -o "$P/js-probe/out" --name probe --no-typescript
"$P/python-probe/bin/python" -B -m wasmtime.bindgen "$P/python-probe/probe.wasm" --out-dir "$P/python-probe/host"
node "$L/verify_js.mjs" "$P/js-probe/out/probe.js"
"$P/python-probe/bin/python" -B "$L/verify_py.py" "$P/python-probe"
```

Both verifiers printed **4 string round trips passed**: empty string, Unicode,
newline/quote and embedded NUL. Pinned wasm-tools 1.263.0 `validate --features all`
passed for both binaries. Extracted WIT has no imports because WASI was disabled or
stubbed; both export `greet`, Python additionally exports its `exports` initialization
interface. Python `--stub-wasi` replaces imports with trapping stubs and freezes
build-time PRNG seed: **not suitable for secure randomness or real I/O**.

Observed binary sizes/digests (not reproducible-build promises):

| Toolchain | Bytes | SHA-256 |
| --- | ---: | --- |
| JS 0.23.0 / jco 1.41.0 | 11,952,232 | `6e159031d8485854cae4856a17694d0167703e4260bf88067056e582f36a3d6f` |
| Python 0.25.1 | 18,349,686 | `c55408a6bae98af86e5d4fe30e9c5b7d83f1fe513dc40df5b5b21cec20216eaa` |

Neither size says anything about request latency, resident memory or startup cost;
these were not measured. The Node verifier transpiles to JS/core Wasm; the Python
verifier uses wasmtime-py 38.0.0 generated bindings, not nanovm's host/linker.

## Native async WIT exposes a concrete toolchain boundary

Repeat the two componentization commands with `async-probe.wit` and distinct output
filenames. JS failed with exit 1 in `spidermonkey-embedding-splicer/src/bindgen.rs`:
`not yet implemented`. This is native async WIT, not a JavaScript Promise export
syncified by the JS engine. Python built successfully, and wasm-tools validated
the artifact and extracted `export greet: async func(name: string) -> string`.
However, wasmtime-py 38 bindgen failed with `invalid leading byte (0x43) for component
defined type`. Async execution therefore remains **unverified**, not proven broken
or supported. No nanovm host integration, WASIp3 HTTP, resources, streams, futures,
filesystem, secure entropy, cancellation or startup-cost claim is made.

## NH28: limited portability evidence, not a runtime port

| Surface | Linux x64 orb | Other OS/architectures |
| --- | --- | --- |
| Python stdlib toolbox | Exercised | Not run |
| Node/Python HTTP fixture | Real loopback requests pass | Not run |
| JS synchronous component | Built, validated, executed via jco/Node | Not run |
| Python synchronous component | Built, validated, executed via wasmtime-py | Not run |
| Native async component | JS build failure; Python validates but execution unverified | Not run |
| nanovm Linux jail / deployment | Intentionally outside this batch | Not tested or claimed portable |

The survey does not infer platform support from portable Wasm bytes. Broad host
portability and language adoption need separate consumers, environments and product
decisions; they are not prerequisites for recovery delivery.
