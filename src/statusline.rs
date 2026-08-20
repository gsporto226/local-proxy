//! Sandboxed Rhai template rendering for the Claude Code status line.
//!
//! The status line is driven by `local-proxy statusline`, which passes a
//! session id and a template; this module evaluates the template against the
//! recorded stats. The template is a **Rhai** script, evaluated with no custom
//! plugins registered: Rhai is isolated by default (no file/network/host
//! access) so a template can do arithmetic, comparisons and boolean algebra
//! but is never arbitrary code.
//!
//! Params are bound as variables (numbers as `i64`/`f64`, everything else as
//! strings); the proxy decides which exist. A param that is absent / not
//! numeric binds to a `?` marker string so the template can format it. Any
//! parse/run failure is logged and a static fallback is returned, so the
//! status line keeps rendering even with a broken template.

use std::collections::HashMap;

/// The static line printed when a template fails to parse or run.
const FALLBACK: &str = "statusline: erro";

/// The marker bound to params that have no data for this session.
pub const NO_DATA: &str = "?";

/// Render `template` (a Rhai script) with `params` bound as variables.
///
/// Every key in `params` is bound as a top-level variable of the same name.
/// Values that parse as `i64` or `f64` bind as numbers; anything else binds as
/// a string. Returns the rendered line, or [`FALLBACK`] (plus a logged error)
/// when the template cannot be parsed or evaluated.
#[must_use]
pub fn render<S: std::hash::BuildHasher>(
    template: &str,
    params: &HashMap<String, String, S>,
) -> String {
    let engine = rhai::Engine::new();
    let mut scope = rhai::Scope::new();
    for (name, value) in params {
        if let Ok(i) = value.parse::<i64>() {
            scope.push(name.as_str(), i);
        } else if let Ok(f) = value.parse::<f64>() {
            scope.push(name.as_str(), f);
        } else {
            scope.push(name.as_str(), value.clone());
        }
    }
    match engine.eval_with_scope::<rhai::Dynamic>(&mut scope, template) {
        Ok(out) => out.to_string(),
        Err(e) => {
            tracing::warn!(
                target: crate::LOG_TARGET,
                error = %e,
                template = %template,
                "status line template falhou; usando fallback"
            );
            FALLBACK.to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn params(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn evaluates_arithmetic() {
        let p = params(&[("cost", "0.012"), ("tokens", "1500"), ("ctx", "42")]);
        let out = render("`$${cost} · ${tokens} tok · ${ctx}%`", &p);
        assert_eq!(out, "$0.012 · 1500 tok · 42%");
    }

    #[test]
    fn computes_from_params() {
        // cost expressed in cents with rounding
        let p = params(&[("cost", "0.0123"), ("mult", "100.0")]);
        let out = render("cost * mult", &p);
        assert_eq!(out, "1.23");
    }

    #[test]
    fn unknown_param_becomes_no_data_marker() {
        let p = params(&[("cost", "0.012"), ("requests", NO_DATA)]);
        // `requests` bound as the no-data marker string renders `?`
        let out = render("`${requests}`", &p);
        assert_eq!(out, NO_DATA);
    }

    #[test]
    fn division_and_format() {
        let p = params(&[("total", "90.0"), ("known", "3.0")]);
        let out = render("total / known", &p);
        assert_eq!(out, "30.0");
    }

    #[test]
    fn cost_absent_renders() {
        let p = params(&[("cost", NO_DATA)]);
        let out = render("`cost=${cost}`", &p);
        assert_eq!(out, "cost=?");
    }

    #[test]
    fn broken_template_falls_back() {
        let p = params(&[]);
        // a syntax error -> parser error -> fallback
        let out = render("this is (not valid", &p);
        assert_eq!(out, FALLBACK);
    }
}
