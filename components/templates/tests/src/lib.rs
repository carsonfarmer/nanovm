//! A minimal app over the templates component, driven by
//! crates/nanovm/tests/components.rs. It also links minijinja directly, so
//! the bench can compare the component with the library in one app.
//! Probes panic on violated expectations.
use std::cell::RefCell;

mod bindings {
    wit_bindgen::generate!({ path: "../wit", world: "client", generate_all });
}
use bindings::nanovm::templates::render::{Error, Templates};
use serde_json::json;
use util::wasip3::http::types::{ErrorCode, Request, Response};
util::wasip3::http::service::export!(App);
struct App;

const BASE: &str =
    "<!doctype html><title>{% block title %}{% endblock %}</title>\n<main>{% block body %}{% endblock %}</main>\n";
const PAGE: &str = r#"{% extends "base.html" %}{% block title %}{{ user.name }}'s orders{% endblock %}
{% block body %}<h1>Hello, {{ user.name }}</h1>
<ul>{% for o in orders %}
<li class="{{ loop.cycle('odd', 'even') }}">{{ o.item }} &times; {{ o.qty }}: {{ "%.2f"|format(o.price * o.qty) }}</li>{% endfor %}
</ul>
<p>Total: {{ "%.2f"|format(orders|map(attribute="price")|sum) }} ({{ orders|length }} orders)</p>{% endblock %}"#;

fn sources() -> Vec<(String, String)> {
    vec![("base.html".into(), BASE.into()), ("page.html".into(), PAGE.into())]
}

fn context() -> String {
    let orders: Vec<_> = (0..20)
        .map(|i| json!({"item": format!("Widget <{i}>"), "qty": i % 3 + 1, "price": 9.5 + i as f64}))
        .collect();
    json!({"user": {"name": "Ada & co"}, "orders": orders}).to_string()
}

thread_local! {
    static DIRECT: RefCell<Option<minijinja::Environment<'static>>> = const { RefCell::new(None) };
}

/// The same render with minijinja linked into the app.
fn direct(ctx: &str) -> String {
    DIRECT.with_borrow_mut(|env| {
        let env = env.get_or_insert_with(|| {
            let mut env = minijinja::Environment::new();
            for (name, source) in sources() {
                env.add_template_owned(name, source).unwrap();
            }
            env
        });
        let ctx: serde_json::Value = serde_json::from_str(ctx).unwrap();
        env.get_template("page.html")
            .unwrap()
            .render(minijinja::Value::from_serialize(&ctx))
            .unwrap()
    })
}

impl util::wasip3::exports::http::handler::Guest for App {
    async fn handle(request: Request) -> Result<Response, ErrorCode> {
        let req = util::read_req(request).await;
        let out = match req.path.as_str() {
            // Renders the page `n` times through the component or the library.
            "/page" => {
                let ctx = context();
                let mut html = String::new();
                for _ in 0..req.q_u64("n", 1) {
                    html = match req.q("via") {
                        Some("crate") => direct(&ctx),
                        _ => Templates::compile(&sources())
                            .expect("compile")
                            .render("page.html", &ctx)
                            .expect("render"),
                    };
                }
                return util::respond(200, "text/html", html.into_bytes());
            }
            "/contracts" => {
                let t = Templates::compile(&sources()).expect("compile");
                let html = t.render("page.html", &context()).expect("render");
                assert_eq!(html, direct(&context()), "component and library differ");
                assert!(
                    html.contains("Hello, Ada &amp; co") && html.contains("Widget &lt;3&gt;"),
                    "{html}"
                );
                let set = |name: &str, src: &str| vec![(name.to_string(), src.to_string())];
                let txt = Templates::compile(&set("a.txt", "{{ x }}")).expect("txt");
                assert_eq!(txt.render("a.txt", r#"{"x":"<b>"}"#).expect("raw"), "<b>");
                match Templates::compile(&set("bad.html", "line one\n{% if %}")) {
                    Err(Error::Syntax(m)) => assert!(m.contains("bad.html:2"), "{m}"),
                    _ => panic!("a syntax error compiled"),
                }
                // Filters resolve when rendering.
                let f = Templates::compile(&set("f.html", "{{ x|nosuchfilter }}")).expect("compile");
                assert!(matches!(f.render("f.html", "{}"), Err(Error::Render(m)) if m.contains("unknown filter")));
                let inc = Templates::compile(&set("i.html", "{% include 'missing.html' %}")).expect("include");
                assert!(matches!(inc.render("i.html", "{}"), Err(Error::Render(_))));
                assert!(matches!(txt.render("nope", "{}"), Err(Error::InvalidArgument(_))));
                for bad in ["[]", "{", "3"] {
                    assert!(
                        matches!(txt.render("a.txt", bad), Err(Error::InvalidArgument(_))),
                        "{bad}"
                    );
                }
                assert_eq!(txt.render("a.txt", "{}").expect("undefined is empty"), "");
                json!("ok")
            }
            _ => return util::text(404, "no such route"),
        };
        util::json(200, out.to_string())
    }
}
