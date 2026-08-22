//! Build script.
//!
//! `rust-embed` bakes the compiled React SPA (`web/webui/dist`) into the
//! binary at compile time and fails hard if that directory is missing. A fresh
//! checkout has no `dist/` — it is produced by `npm -C web run build`, which a
//! plain `cargo build`/`cargo test` should not require. So: if the directory is
//! absent, drop in a placeholder page explaining how to build the real one.
//! A Docker build copies the real `dist/` in before `cargo build`, so the
//! placeholder never reaches an image.

use std::fs;
use std::path::Path;

const PLACEHOLDER: &str = r#"<!doctype html>
<html lang="en">
  <head>
    <meta charset="utf-8" />
    <title>crust-seed</title>
  </head>
  <body>
    <h1>Web UI not built</h1>
    <p>
      This binary was compiled without the web UI assets. Run
      <code>npm -C web ci &amp;&amp; npm -C web run build</code> and rebuild.
    </p>
  </body>
</html>
"#;

fn main() {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let dist = Path::new(&manifest_dir)
        .join("web")
        .join("webui")
        .join("dist");

    if !dist.join("index.html").exists() {
        fs::create_dir_all(&dist).expect("create web/webui/dist");
        fs::write(dist.join("index.html"), PLACEHOLDER).expect("write placeholder index.html");
    }

    println!("cargo:rerun-if-changed=web/webui/dist");
    println!("cargo:rerun-if-changed=migrations");
    for var in [
        "BUILD_COMMIT_SHA",
        "BUILD_BRANCH",
        "BUILD_VERSION",
        "BUILD_DATE",
    ] {
        println!("cargo:rerun-if-env-changed={var}");
    }
}
