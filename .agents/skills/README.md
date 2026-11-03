# Workspace skills

Reviewed, instructions-only imports for nanovm. No upstream plugins, MCP servers,
install scripts, or evaluation runners are installed.

- **writing-rust:** BeautifulRust `skills/rust-best-practices` and its reference
  digests, from https://github.com/morganlinton/BeautifulRust at
  `edd3e86087eda07983251056508fe3b8856d85d7`. Renamed to Amp's gerund convention;
  final checklist moved to a reference to keep the entry point under 500 lines.
- **grilling:** https://github.com/mattpocock/skills at
  `4ae2317ddb22b046e26455e4f9c9b3aaaf62212d`,
  `skills/productivity/grilling`. The requested `grill-me` skill is only an alias
  that loads this skill; installing the implementation avoids a redundant wrapper.

Each skill includes its upstream MIT license. Repository guidance and the task's
scope still apply: nanovm is application code, not an API-design exercise. Use the
Rust guidance to simplify ownership and responsibilities, not to introduce
speculative abstractions. Use grilling for unresolved design decisions after
looking up facts; existing owner decisions do not need to be asked again.
