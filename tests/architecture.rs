//! Keeps the hexagon's dependency rule: inner layers never name outer ones.
//!
//! `domain` may not use `ports`, `application`, `adapters` or `bootstrap`;
//! `ports` and `application` may not use `adapters` or `bootstrap`. Test
//! modules (after `#[cfg(test)]`) and comments are exempt.

use std::path::Path;

fn rust_files(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("read src dir") {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

fn violations(layer: &str, forbidden: &[&str]) -> Vec<String> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join(layer);
    let mut files = Vec::new();
    rust_files(&root, &mut files);
    let mut found = Vec::new();
    for file in files {
        let text = std::fs::read_to_string(&file).expect("read source");
        let production = text.split("#[cfg(test)]").next().unwrap_or_default();
        for (n, line) in production.lines().enumerate() {
            if line.trim_start().starts_with("//") {
                continue;
            }
            for module in forbidden {
                if line.contains(&format!("crate::{module}")) {
                    found.push(format!("{}:{}: {}", file.display(), n + 1, line.trim()));
                }
            }
        }
    }
    found
}

#[test]
fn domain_depends_on_nothing_outside_it() {
    let found = violations("domain", &["ports", "application", "adapters", "bootstrap"]);
    assert!(
        found.is_empty(),
        "domain reaches outward:\n{}",
        found.join("\n")
    );
}

#[test]
fn ports_and_application_never_name_adapters() {
    for layer in ["ports", "application"] {
        let found = violations(layer, &["adapters", "bootstrap"]);
        assert!(
            found.is_empty(),
            "{layer} reaches outward:\n{}",
            found.join("\n")
        );
    }
}
