//! Compiles the app's JavaScript (the file `$NANOVM_JS` names) into the
//! component.

fn main() {
    println!("cargo:rerun-if-env-changed=NANOVM_JS");
    let src = std::env::var("NANOVM_JS").expect("set NANOVM_JS to the app's .js file (an absolute path)");
    println!("cargo:rerun-if-changed={src}");
    let out = std::path::Path::new(&std::env::var("OUT_DIR").unwrap()).join("app.js");
    std::fs::copy(&src, out).unwrap_or_else(|e| panic!("{src}: {e}"));
}
