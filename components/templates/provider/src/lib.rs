//! Jinja2-style templates (minijinja) as a component: pure computation, no
//! filesystem. Compiled template sets are cached per instance by content, so
//! an app may compile its templates on every request at little cost.
use std::cell::RefCell;
use std::rc::Rc;

use minijinja::Environment;

mod bindings {
    wit_bindgen::generate!({ path: "../wit", world: "provider" });
}
use bindings::exports::nanovm::templates::render::{Error, Guest, GuestTemplates, Templates as Handle};
bindings::export!(Provider with_types_in bindings);
struct Provider;

type Sources = Vec<(String, String)>;

/// Compiled sets, most recent last. Bounded: an app has a handful of sets.
const MAX_CACHED: usize = 16;

thread_local! {
    static CACHE: RefCell<Vec<(Sources, Rc<Environment<'static>>)>> = const { RefCell::new(Vec::new()) };
}

pub struct Templates {
    env: Rc<Environment<'static>>,
}

impl Guest for Provider {
    type Templates = Templates;
}

impl GuestTemplates for Templates {
    fn compile(sources: Sources) -> Result<Handle, Error> {
        if let Some(env) = CACHE.with_borrow(|c| c.iter().find(|(s, _)| *s == sources).map(|(_, e)| e.clone())) {
            return Ok(Handle::new(Templates { env }));
        }
        // The default escaping follows the name: .html/.htm/.xml for HTML,
        // .json/.js/.yaml for JSON, anything else none.
        let mut env = Environment::new();
        for (name, source) in &sources {
            env.add_template_owned(name.clone(), source.clone())
                .map_err(|e| Error::Syntax(describe(&e)))?;
        }
        let env = Rc::new(env);
        CACHE.with_borrow_mut(|c| {
            if c.len() == MAX_CACHED {
                c.remove(0);
            }
            c.push((sources, env.clone()));
        });
        Ok(Handle::new(Templates { env }))
    }

    fn render(&self, name: String, context: String) -> Result<String, Error> {
        let context: serde_json::Value =
            serde_json::from_str(&context).map_err(|e| Error::InvalidArgument(format!("context: {e}")))?;
        if !context.is_object() {
            return Err(Error::InvalidArgument("the context must be a JSON object".into()));
        }
        let template = self
            .env
            .get_template(&name)
            .map_err(|_| Error::InvalidArgument(format!("no template {name}")))?;
        template
            .render(minijinja::Value::from_serialize(&context))
            .map_err(|e| Error::Render(describe(&e)))
    }
}

/// The error with its location and causes.
fn describe(e: &minijinja::Error) -> String {
    let mut out = e.to_string();
    let mut source = std::error::Error::source(e);
    while let Some(cause) = source {
        out.push_str(": ");
        out.push_str(&cause.to_string());
        source = cause.source();
    }
    out
}
