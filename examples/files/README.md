# Files: uploads, downloads and ranges

File buckets with quotas, streamed downloads, range requests and ETags, in
about 130 lines of Rust ([src/lib.rs](src/lib.rs)) on
[nanovm-sdk](../../sdk/rust).

```sh
tools/build-app.sh examples/files -o examples/target/files.wasm --run --manifest examples/files/nanovm.json
```

```sh
curl -XPUT http://127.0.0.1:8080/photos/files/cat.jpg -H 'content-type: image/jpeg' --data-binary @cat.jpg
curl -O http://127.0.0.1:8080/photos/files/cat.jpg
curl -H 'range: bytes=0-1023' http://127.0.0.1:8080/photos/files/cat.jpg   # 206, the first KiB
curl http://127.0.0.1:8080/photos/                                         # {"files":[…],"bytes":…}
```

What it shows:

- **Quotas do their job.** A bucket (`/{bucket}/`) keeps its files in its
  own volume, so they count against the app's `disk_mb` and `max_files`. A
  write past them fails with `insufficient-space` (or `quota`), and the app
  answers `507`. A body over `max_request_body_kb` never reaches it: the
  node answers `413` (`x-nanovm-error: request-too-large`).
- **When write-then-rename still matters.** A request's writes commit
  together, so a failed upload that returned an error would roll back by
  itself, but its client would get a `500`. To answer `507` (an answer
  commits) and still keep the old file, an upload is written aside and
  renamed over the old one only once it is whole. The cost: replacing a
  file needs room for both copies while it is written (W-600 would remove
  the need).
- **Streamed downloads.** A download is read and sent 64 KiB at a time
  (`Body::stream`, `fs::read_at`), so a large file never sits in the
  object's memory; it has `content-length`, `accept-ranges` and an `ETag`
  (`If-None-Match` gives `304`). The object serves nothing else while it
  streams, for at most `post_return_timeout_ms`.
- **Range requests.** `bytes=a-b`, `bytes=a-` and `bytes=-n` give `206` with
  `content-range`; a range past the end `416`; several ranges the whole
  file (`200`, as the spec allows).

Uploads are not streamed: the node reads a request's body whole (up to
`max_request_body_kb`) before the object sees it. `nanovm run` caps every
limit at its node's defaults (1 MiB bodies, 200 ms CPU), so this manifest
asks for no more.

API: `PUT /{bucket}/files/{name}` (a name is 1-100 letters, digits, `.`,
`_`, `-`, not starting with `.`) → `201 {"name","size","etag","type"}` or
`507`; `GET`/`HEAD /{bucket}/files/{name}` (`Range`, `If-None-Match`);
`DELETE /{bucket}/files/{name}` → `204`; `GET /{bucket}/` →
`{"files":[…],"bytes"}`.

Test: `files_upload_within_quota_and_serve_ranges` in
[crates/nanovm/tests/examples.rs](../../crates/nanovm/tests/examples.rs).
