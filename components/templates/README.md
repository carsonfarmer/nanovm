# Templates: Jinja2-style rendering (minijinja)

A Wasm component that renders [minijinja](https://docs.rs/minijinja)
(Jinja2-syntax) templates: inheritance, includes, macros, loops, filters, and
HTML escaping chosen by the template's name. Compose it with `wac plug`, like
the other components.

```sh
bash components/templates/build.sh   # target/components/templates/{provider,tests}.wasm
wac plug app.wasm --plug target/components/templates/provider.wasm -o app-with-templates.wasm
```

The interface is [`nanovm:templates/render@0.1.0`](wit/templates.wit):
`templates.compile(list<(name, source)>)` and `render(name, context-json)`.
Names ending in `.html`, `.htm` or `.xml` escape for HTML; `.json`, `.js` and
`.yaml` for JSON; anything else not at all. Errors name the template and line.

It is pure computation: no filesystem, no state but a per-instance cache of
compiled sets keyed by their sources (up to 16), so compiling the same set on
every request costs a comparison, not a parse. With nothing stored, the
persistence, rollback and quota tests of the other components don't apply.

## Component or library?

Measured in one test app that has both (`components_bench -- templates`;
see [notes/components.md](../../notes/components.md)): rendering a 20-row
page costs about 10% more through the component than through minijinja
linked into the app (139 vs 125 µs CPU per render, medians of three rounds;
the call copies the context in and the HTML out). Size is similar: the
provider is 886 KB, and linking minijinja into an app adds roughly 700 KB.

So for **Rust apps the library is the simpler choice**: no composition step,
no JSON round trip of the context. The component is for guests in languages
that can import WIT but have no Jinja engine of their own, or for keeping a
large dependency out of the app's own build. It is not composed by default.

## Tests

`crates/nanovm/tests/components.rs`, `templates_contracts`: inheritance,
loops, filters, HTML escaping, plain-text templates, syntax errors with line,
unknown filters and missing includes at render, bad contexts and names, and
identical output from the component and the library.
