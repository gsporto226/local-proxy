use std::collections::HashMap;

use crate::config::{Config, Provider};

#[derive(Debug, thiserror::Error)]
pub enum RouterError {
    #[error("provider not found: {provider}")]
    ProviderNotFound { provider: String },
    #[error("model not found: {model}")]
    ModelNotFound { model: String },
}

#[derive(Debug)]
pub struct ResolvedRoute<'a> {
    pub provider: &'a Provider,
    pub upstream_model: String,
}

#[derive(Debug)]
pub struct Router<'a> {
    config: &'a Config,
    exact: HashMap<&'a str, usize>,
    providers: HashMap<&'a str, usize>,
    prefixes: Vec<usize>,
    default_provider: Option<usize>,
}

impl<'a> Router<'a> {
    pub fn new(config: &'a Config) -> Result<Self, RouterError> {
        let mut providers = HashMap::new();
        for (i, provider) in config.providers.iter().enumerate() {
            providers.insert(provider.name.as_str(), i);
        }

        let mut exact = HashMap::new();
        let mut prefixes = Vec::new();
        for (i, route) in config.routes.iter().enumerate() {
            if !providers.contains_key(route.provider.as_str()) {
                return Err(RouterError::ProviderNotFound {
                    provider: route.provider.clone(),
                });
            }
            if route.prefix {
                prefixes.push(i);
            } else {
                exact.insert(route.model.as_str(), i);
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

    pub fn resolve_model(&self, model: &str) -> Result<ResolvedRoute<'a>, RouterError> {
        if let Some(&route_idx) = self.exact.get(model) {
            return self.resolve_route(route_idx, model);
        }

        if let Some((provider_name, upstream)) = model.split_once('/') {
            if let Some(&provider_idx) = self.providers.get(provider_name) {
                return Ok(ResolvedRoute {
                    provider: &self.config.providers[provider_idx],
                    upstream_model: upstream.to_string(),
                });
            }
        }

        let mut best: Option<(usize, usize)> = None;
        for &route_idx in &self.prefixes {
            let route = &self.config.routes[route_idx];
            let len = route.model.len();
            if model.starts_with(&route.model)
                && best.map(|(best_len, _)| len > best_len).unwrap_or(true)
            {
                best = Some((len, route_idx));
            }
        }
        if let Some((_, route_idx)) = best {
            return self.resolve_route(route_idx, model);
        }

        for provider in &self.config.providers {
            if provider.models.iter().any(|m| m == model) {
                return Ok(ResolvedRoute {
                    provider,
                    upstream_model: model.to_string(),
                });
            }
        }

        if let Some(provider_idx) = self.default_provider {
            return Ok(ResolvedRoute {
                provider: &self.config.providers[provider_idx],
                upstream_model: model.to_string(),
            });
        }

        Err(RouterError::ModelNotFound {
            model: model.to_string(),
        })
    }

    fn resolve_route(
        &self,
        route_idx: usize,
        model: &str,
    ) -> Result<ResolvedRoute<'a>, RouterError> {
        let route = &self.config.routes[route_idx];
        let provider_idx = self.providers[route.provider.as_str()];
        Ok(ResolvedRoute {
            provider: &self.config.providers[provider_idx],
            upstream_model: route
                .upstream_model
                .clone()
                .unwrap_or_else(|| model.to_string()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Defaults, Provider, ProviderFormat, Route, Server};

    fn config() -> Config {
        Config {
            server: Server::default(),
            providers: vec![
                Provider {
                    name: "anthropic".to_string(),
                    base_url: "https://api.anthropic.com".to_string(),
                    api_key_env: Some("ANTHROPIC_API_KEY".to_string()),
                    format: ProviderFormat::Anthropic,
                    models: vec!["claude-native-1".to_string()],
                },
                Provider {
                    name: "openai".to_string(),
                    base_url: "https://api.openai.com/v1".to_string(),
                    api_key_env: Some("OPENAI_API_KEY".to_string()),
                    format: ProviderFormat::Openai,
                    models: vec!["gpt-native-1".to_string()],
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
            },
        }
    }

    fn no_default_config() -> Config {
        let mut c = config();
        c.defaults.provider.clear();
        c
    }

    #[test]
    fn exact_route_match_wins() {
        let config = config();
        let router = Router::new(&config).unwrap();
        let resolved = router.resolve_model("gpt-4o").unwrap();
        assert_eq!(resolved.provider.name, "openai");
        assert_eq!(resolved.upstream_model, "gpt-4o");
    }

    #[test]
    fn exact_route_with_upstream_model() {
        let mut c = config();
        c.routes[0].upstream_model = Some("gpt-4o-mini".to_string());
        let router = Router::new(&c).unwrap();
        let resolved = router.resolve_model("gpt-4o").unwrap();
        assert_eq!(resolved.upstream_model, "gpt-4o-mini");
    }

    #[test]
    fn provider_slash_model_syntax() {
        let config = config();
        let router = Router::new(&config).unwrap();
        let resolved = router.resolve_model("openai/gpt-anything").unwrap();
        assert_eq!(resolved.provider.name, "openai");
        assert_eq!(resolved.upstream_model, "gpt-anything");
    }

    #[test]
    fn provider_slash_model_unknown_provider_ignored() {
        let config = config();
        let router = Router::new(&config).unwrap();
        let resolved = router.resolve_model("nope/foo").unwrap();
        assert_eq!(resolved.provider.name, "anthropic");
        assert_eq!(resolved.upstream_model, "nope/foo");
    }

    #[test]
    fn prefix_longest_match_wins() {
        let config = config();
        let router = Router::new(&config).unwrap();
        let resolved = router.resolve_model("claude-sonnet-4-5").unwrap();
        assert_eq!(resolved.provider.name, "openai");
        assert_eq!(resolved.upstream_model, "kimi-k2.6");

        let resolved = router.resolve_model("claude-opus-3").unwrap();
        assert_eq!(resolved.provider.name, "anthropic");
        assert_eq!(resolved.upstream_model, "claude-opus-4-1");
    }

    #[test]
    fn native_models_list_match() {
        let config = config();
        let router = Router::new(&config).unwrap();
        let resolved = router.resolve_model("gpt-native-1").unwrap();
        assert_eq!(resolved.provider.name, "openai");
        assert_eq!(resolved.upstream_model, "gpt-native-1");
    }

    #[test]
    fn default_provider_fallback() {
        let config = config();
        let router = Router::new(&config).unwrap();
        let resolved = router.resolve_model("totally-unknown-model").unwrap();
        assert_eq!(resolved.provider.name, "anthropic");
        assert_eq!(resolved.upstream_model, "totally-unknown-model");
    }

    #[test]
    fn model_not_found_without_default() {
        let config = no_default_config();
        let router = Router::new(&config).unwrap();
        let err = router.resolve_model("totally-unknown-model").unwrap_err();
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
        let err = Router::new(&c).unwrap_err();
        assert!(matches!(err, RouterError::ProviderNotFound { .. }));
    }
}
