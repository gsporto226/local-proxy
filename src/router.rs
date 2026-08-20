use std::collections::HashMap;
use std::sync::Arc;

use crate::config::{Config, Provider};

/// Errors produced while building or querying a [`Router`].
#[derive(Debug, thiserror::Error)]
pub enum RouterError {
    /// A route referenced a provider that doesn't exist.
    #[error("provider not found: {provider}")]
    ProviderNotFound {
        /// Name of the missing provider.
        provider: String,
    },
    /// No route matched the requested model.
    #[error("model not found: {model}")]
    ModelNotFound {
        /// Requested model that couldn't be resolved.
        model: String,
    },
}

/// A successfully resolved route to an upstream provider.
#[derive(Debug)]
pub struct ResolvedRoute {
    /// The provider to forward the request to.
    pub provider: Arc<Provider>,
    /// Model name to send to the upstream provider.
    pub upstream_model: String,
}

/// Resolves requested model names to upstream providers using configured
/// routes, prefixes, and defaults.
#[derive(Debug)]
pub struct Router {
    config: Arc<Config>,
    exact: HashMap<String, usize>,
    providers: HashMap<String, usize>,
    prefixes: Vec<usize>,
    default_provider: Option<usize>,
}

impl Router {
    /// Build a [`Router`] from a shared configuration.
    ///
    /// # Errors
    ///
    /// Returns [`RouterError::ProviderNotFound`] if any route references an
    /// unknown provider.
    pub fn new(config: Arc<Config>) -> Result<Self, RouterError> {
        let mut providers = HashMap::new();
        for (i, provider) in config.providers.iter().enumerate() {
            providers.insert(provider.name.clone(), i);
        }

        let mut exact = HashMap::new();
        let mut prefixes = Vec::new();
        for (i, route) in config.routes.iter().enumerate() {
            if !providers.contains_key(&route.provider) {
                return Err(RouterError::ProviderNotFound {
                    provider: route.provider.clone(),
                });
            }
            if route.prefix {
                prefixes.push(i);
            } else {
                exact.insert(route.model.clone(), i);
            }
        }

        let default_provider = match config.defaults.provider.as_str() {
            "" => None,
            name => providers.get(name).copied(),
        };

        Ok(Self {
            config,
            exact,
            providers,
            prefixes,
            default_provider,
        })
    }

    /// Resolve `model` to the provider and upstream model that should serve it.
    ///
    /// `is_connected` reports whether a provider currently has a resolvable key;
    /// it is used to prefer connected providers when several serve the same
    /// model. An explicit route, `provider/model` syntax, or prefix always wins;
    /// among the native-model-list matches a connected provider is preferred, and
    /// an unconnected match is returned only so the caller can surface a clear
    /// error instead of forwarding without a key.
    ///
    /// # Errors
    ///
    /// Returns [`RouterError::ModelNotFound`] if no route, provider, or default
    /// matches `model`.
    pub fn resolve_model(
        &self,
        model: &str,
        is_connected: &dyn Fn(&str) -> bool,
    ) -> Result<ResolvedRoute, RouterError> {
        if let Some(&route_idx) = self.exact.get(model) {
            return Ok(self.resolve_route(route_idx, model));
        }

        if let Some((provider_name, upstream)) = model.split_once('/') {
            if let Some(&provider_idx) = self.providers.get(provider_name) {
                return Ok(ResolvedRoute {
                    provider: Arc::new(self.config.providers[provider_idx].clone()),
                    upstream_model: upstream.to_string(),
                });
            }
        }

        let mut best: Option<(usize, usize)> = None;
        for &route_idx in &self.prefixes {
            let route = &self.config.routes[route_idx];
            let len = route.model.len();
            if model.starts_with(&route.model) && best.is_none_or(|(best_len, _)| len > best_len) {
                best = Some((len, route_idx));
            }
        }
        if let Some((_, route_idx)) = best {
            return Ok(self.resolve_route(route_idx, model));
        }

        let mut unconnected: Option<Arc<Provider>> = None;
        for provider in &self.config.providers {
            if provider.models.iter().any(|m| m == model) {
                let p = Arc::new(provider.clone());
                if is_connected(&provider.name) {
                    return Ok(ResolvedRoute {
                        provider: p,
                        upstream_model: model.to_string(),
                    });
                }
                if unconnected.is_none() {
                    unconnected = Some(p);
                }
            }
        }
        if let Some(provider) = unconnected {
            return Ok(ResolvedRoute {
                provider,
                upstream_model: model.to_string(),
            });
        }

        if let Some(provider_idx) = self.default_provider {
            return Ok(ResolvedRoute {
                provider: Arc::new(self.config.providers[provider_idx].clone()),
                upstream_model: model.to_string(),
            });
        }

        Err(RouterError::ModelNotFound {
            model: model.to_string(),
        })
    }

    /// Catalog of models a client may request: exact route models plus each
    /// provider's native model list.
    #[must_use]
    pub fn list_models(&self) -> Vec<String> {
        let mut models: Vec<String> = Vec::new();
        for route in &self.config.routes {
            if !route.prefix && !models.contains(&route.model) {
                models.push(route.model.clone());
            }
        }
        for provider in &self.config.providers {
            for model in &provider.models {
                if !models.contains(model) {
                    models.push(model.clone());
                }
            }
        }
        models
    }

    fn resolve_route(&self, route_idx: usize, model: &str) -> ResolvedRoute {
        let route = &self.config.routes[route_idx];
        let provider_idx = self.providers[&route.provider];
        ResolvedRoute {
            provider: Arc::new(self.config.providers[provider_idx].clone()),
            upstream_model: route
                .upstream_model
                .clone()
                .unwrap_or_else(|| model.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Defaults, ProviderFormat, Route, Server};

    fn config() -> Config {
        Config {
            server: Server::default(),
            providers: vec![
                Provider {
                    name: "anthropic".to_string(),
                    base_url: "https://api.anthropic.com".to_string(),
                    format: ProviderFormat::Anthropic,
                    models: vec!["claude-native-1".to_string()],
                    headers: std::collections::HashMap::new(),
                },
                Provider {
                    name: "openai".to_string(),
                    base_url: "https://api.openai.com/v1".to_string(),
                    format: ProviderFormat::Openai,
                    models: vec!["gpt-native-1".to_string()],
                    headers: std::collections::HashMap::new(),
                },
            ],
            routes: vec![
                Route {
                    model: "gpt-4o".to_string(),
                    provider: "openai".to_string(),
                    prefix: false,
                    upstream_model: None,
                },
                Route {
                    model: "claude".to_string(),
                    provider: "anthropic".to_string(),
                    prefix: true,
                    upstream_model: Some("claude-opus-4-1".to_string()),
                },
                Route {
                    model: "claude-sonnet".to_string(),
                    provider: "openai".to_string(),
                    prefix: true,
                    upstream_model: Some("kimi-k2.6".to_string()),
                },
            ],
            defaults: Defaults {
                provider: "anthropic".to_string(),
                active_model: None,
            },
            exec: crate::config::Exec::default(),
        }
    }

    fn no_default_config() -> Config {
        let mut c = config();
        c.defaults.provider.clear();
        c
    }

    fn router_for(config: Config) -> Router {
        Router::new(Arc::new(config)).unwrap()
    }

    #[test]
    fn exact_route_match_wins() {
        let router = router_for(config());
        let resolved = router.resolve_model("gpt-4o", &|_| true).unwrap();
        assert_eq!(resolved.provider.name, "openai");
        assert_eq!(resolved.upstream_model, "gpt-4o");
    }

    #[test]
    fn exact_route_with_upstream_model() {
        let mut c = config();
        c.routes[0].upstream_model = Some("gpt-4o-mini".to_string());
        let router = router_for(c);
        let resolved = router.resolve_model("gpt-4o", &|_| true).unwrap();
        assert_eq!(resolved.upstream_model, "gpt-4o-mini");
    }

    #[test]
    fn provider_slash_model_syntax() {
        let router = router_for(config());
        let resolved = router
            .resolve_model("openai/gpt-anything", &|_| true)
            .unwrap();
        assert_eq!(resolved.provider.name, "openai");
        assert_eq!(resolved.upstream_model, "gpt-anything");
    }

    #[test]
    fn provider_slash_model_unknown_provider_ignored() {
        let router = router_for(config());
        let resolved = router.resolve_model("nope/foo", &|_| true).unwrap();
        assert_eq!(resolved.provider.name, "anthropic");
        assert_eq!(resolved.upstream_model, "nope/foo");
    }

    #[test]
    fn prefix_longest_match_wins() {
        let router = router_for(config());
        let resolved = router
            .resolve_model("claude-sonnet-4-5", &|_| true)
            .unwrap();
        assert_eq!(resolved.provider.name, "openai");
        assert_eq!(resolved.upstream_model, "kimi-k2.6");

        let resolved = router.resolve_model("claude-opus-3", &|_| true).unwrap();
        assert_eq!(resolved.provider.name, "anthropic");
        assert_eq!(resolved.upstream_model, "claude-opus-4-1");
    }

    #[test]
    fn native_models_list_match() {
        let router = router_for(config());
        let resolved = router.resolve_model("gpt-native-1", &|_| true).unwrap();
        assert_eq!(resolved.provider.name, "openai");
        assert_eq!(resolved.upstream_model, "gpt-native-1");
    }

    #[test]
    fn native_list_prefers_connected_provider() {
        let mut c = config();
        // both anthropic and openai list the same native model
        c.providers[0].models = vec!["shared".to_string()];
        c.providers[1].models = vec!["shared".to_string()];
        let router = router_for(c);

        // openai is connected -> it wins even though anthropic is listed first
        let resolved = router
            .resolve_model("shared", &|name| name == "openai")
            .unwrap();
        assert_eq!(resolved.provider.name, "openai");

        // no connected provider -> falls back to the first (unconnected) match
        let resolved = router.resolve_model("shared", &|_| false).unwrap();
        assert_eq!(resolved.provider.name, "anthropic");
    }

    #[test]
    fn default_provider_fallback() {
        let router = router_for(config());
        let resolved = router
            .resolve_model("totally-unknown-model", &|_| true)
            .unwrap();
        assert_eq!(resolved.provider.name, "anthropic");
        assert_eq!(resolved.upstream_model, "totally-unknown-model");
    }

    #[test]
    fn model_not_found_without_default() {
        let router = router_for(no_default_config());
        let err = router
            .resolve_model("totally-unknown-model", &|_| true)
            .unwrap_err();
        assert!(matches!(err, RouterError::ModelNotFound { .. }));
    }

    #[test]
    fn rejects_route_with_unknown_provider() {
        let mut c = config();
        c.routes.push(Route {
            model: "ghost".to_string(),
            provider: "does-not-exist".to_string(),
            prefix: false,
            upstream_model: None,
        });
        let err = Router::new(Arc::new(c)).unwrap_err();
        assert!(matches!(err, RouterError::ProviderNotFound { .. }));
    }

    #[test]
    fn list_models_catalog() {
        let router = router_for(config());
        let models = router.list_models();
        assert!(models.contains(&"gpt-4o".to_string()));
        assert!(models.contains(&"claude-native-1".to_string()));
        assert!(models.contains(&"gpt-native-1".to_string()));
        assert!(!models.contains(&"claude".to_string()));
    }
}
