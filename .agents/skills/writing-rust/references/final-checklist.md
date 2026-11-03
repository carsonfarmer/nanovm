# Final checklist

Before returning Rust code, confirm:

- `cargo fmt`, `cargo clippy -- -D warnings`, and `cargo test` are clean.
- No `unwrap()` outside tests and `main`; every `expect`, `assert`, and `panic` has a
  message with the relevant values.
- Public functions take `&str`, `&[T]`, `impl AsRef<_>`, or `impl IntoIterator`, not
  `&String` or `&Vec<T>`.
- Every public type derives `Debug` and the applicable common traits; error types
  implement `Error + Display + Send + Sync + 'static`.
- No `bool` parameters in public signatures; no hand-written `Ord` without matching
  `PartialEq` and `Hash`.
- No `.clone()` whose only job is to satisfy the borrow checker.
- No globs (bar a framework's prelude), no preludes of your own, no leaked third-party types.
- Every `unsafe` block has a `// SAFETY:` comment; every `unsafe fn` has `# Safety` docs.
- Public items have docs with a one-line summary and `# Errors` / `# Panics` where relevant.
- Application code stayed simple; library code followed the API rules.
