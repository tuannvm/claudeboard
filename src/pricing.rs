use std::collections::HashMap;
use std::path::PathBuf;

// ============================================================================
// LiteLLM Pricing
// ============================================================================

/// Pricing data for a single model from LiteLLM
#[derive(Debug, Clone, Default)]
pub struct ModelPricing {
    pub input_cost_per_token: f64,
    pub output_cost_per_token: f64,
    pub cache_read_input_token_cost: f64,
    pub cache_creation_input_token_cost: f64,
}

/// Fetched and cached LiteLLM pricing data
#[derive(Debug, Clone, Default)]
pub struct LiteLLMPricing {
    pub models: HashMap<String, ModelPricing>,
}

/// Fallback hardcoded rates for models not in LiteLLM (matching token-usage skill)
pub fn get_fallback_rates(model: &str) -> ModelPricing {
    if model.contains("claude-opus-4") {
        ModelPricing {
            input_cost_per_token: 0.000015,
            output_cost_per_token: 0.000075,
            cache_read_input_token_cost: 0.0000015,
            cache_creation_input_token_cost: 0.000015,
        }
    } else if model.contains("claude-sonnet-4") {
        ModelPricing {
            input_cost_per_token: 0.000003,
            output_cost_per_token: 0.000015,
            cache_read_input_token_cost: 0.0000003,
            cache_creation_input_token_cost: 0.000003,
        }
    } else if model.contains("claude-haiku-4") {
        ModelPricing {
            input_cost_per_token: 0.0000008,
            output_cost_per_token: 0.000004,
            cache_read_input_token_cost: 0.0000001,
            cache_creation_input_token_cost: 0.0000008,
        }
    } else if model.contains("glm-4") {
        ModelPricing {
            input_cost_per_token: 0.0000001,
            output_cost_per_token: 0.0000005,
            cache_read_input_token_cost: 0.0,
            cache_creation_input_token_cost: 0.0,
        }
    } else if model.contains("gemini-2.5-") {
        ModelPricing {
            input_cost_per_token: 0.000000075,
            output_cost_per_token: 0.00015,
            cache_read_input_token_cost: 0.0,
            cache_creation_input_token_cost: 0.0,
        }
    } else if model.contains("minimax") {
        ModelPricing {
            input_cost_per_token: 0.0,
            output_cost_per_token: 0.0,
            cache_read_input_token_cost: 0.0,
            cache_creation_input_token_cost: 0.0,
        }
    } else {
        ModelPricing::default()
    }
}

impl LiteLLMPricing {
    /// Load pricing from cache file, fetching if stale
    pub fn load() -> Self {
        let cache_dir = std::env::var("HOME")
            .ok()
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join(".cache")
            .join("claude-pricing");
        let cache_file = cache_dir.join("litellm-pricing.json");
        let cache_ttl = 86400; // 24 hours

        // Check if cache exists and is fresh
        let need_fetch = if !cache_file.exists() {
            true
        } else if let Ok(metadata) = std::fs::metadata(&cache_file) {
            if let Ok(modified) = metadata.modified() {
                let age = std::time::SystemTime::now()
                    .duration_since(modified)
                    .map(|d| d.as_secs())
                    .unwrap_or(cache_ttl + 1);
                age > cache_ttl
            } else {
                true
            }
        } else {
            true
        };

        // Fetch if needed
        if need_fetch {
            let _ = std::fs::create_dir_all(&cache_dir);
            let url = "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json";
            // Use reqwest for native HTTP (no curl dependency)
            if let Ok(client) = reqwest::blocking::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
            {
                if let Ok(response) = client.get(url).send() {
                    if response.status().is_success() {
                        let _ = std::fs::write(&cache_file, response.bytes().unwrap_or_default());
                    }
                }
            }
        }

        // Parse cache file
        let mut pricing = LiteLLMPricing::default();
        if let Ok(content) = std::fs::read_to_string(&cache_file) {
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) {
                if let Some(obj) = json.as_object() {
                    for (model_name, model_data) in obj {
                        if let Some(data) = model_data.as_object() {
                            let p = ModelPricing {
                                input_cost_per_token: data
                                    .get("input_cost_per_token")
                                    .and_then(|v| v.as_f64())
                                    .unwrap_or(0.0),
                                output_cost_per_token: data
                                    .get("output_cost_per_token")
                                    .and_then(|v| v.as_f64())
                                    .unwrap_or(0.0),
                                cache_read_input_token_cost: data
                                    .get("cache_read_input_token_cost")
                                    .and_then(|v| v.as_f64())
                                    .unwrap_or(0.0),
                                cache_creation_input_token_cost: data
                                    .get("cache_creation_input_token_cost")
                                    .and_then(|v| v.as_f64())
                                    .unwrap_or(0.0),
                            };
                            pricing.models.insert(model_name.clone(), p);
                        }
                    }
                }
            }
        }

        pricing
    }

    /// Get pricing for a model, with fallback to hardcoded rates
    pub fn get(&self, model: &str) -> ModelPricing {
        self.models.get(model).cloned().unwrap_or_else(|| {
            // Try suffix match first (case-insensitive, like token-usage skill)
            let model_lower = model.to_lowercase();
            for (key, p) in &self.models {
                let key_lower = key.to_lowercase();
                if key_lower.ends_with(&format!("/{}", model_lower)) || key_lower == model_lower {
                    return p.clone();
                }
            }
            get_fallback_rates(model)
        })
    }
}

/// Compute cost for given token counts and model
pub fn compute_cost(
    model: &str,
    input_tokens: u64,
    output_tokens: u64,
    cache_read: u64,
    cache_write: u64,
) -> f64 {
    static PRICING: std::sync::LazyLock<LiteLLMPricing> =
        std::sync::LazyLock::new(LiteLLMPricing::load);
    let p = PRICING.get(model);
    (input_tokens as f64 * p.input_cost_per_token)
        + (output_tokens as f64 * p.output_cost_per_token)
        + (cache_read as f64 * p.cache_read_input_token_cost)
        + (cache_write as f64 * p.cache_creation_input_token_cost)
}
