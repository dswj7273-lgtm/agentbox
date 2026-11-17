#[derive(Debug, Clone, Copy)]
pub struct ModelPricing {
    pub input_per_million: f64,
    pub output_per_million: f64,
    pub cache_write_per_million: f64,
    pub cache_read_per_million: f64,
}

/// Returns approximate USD pricing per 1M tokens for known models.
/// Supports substring matching so dated snapshots (e.g. claude-sonnet-4-5-20250929) resolve cleanly.
pub fn pricing_for_model(model: &str) -> ModelPricing {
    let m = model.to_lowercase();
    if m.contains("opus") {
        // Claude Opus 4 / 4.5 / 4.6 tier ($15 in / $75 out or $5/$25 for 4.5+ — use conservative $15/$75 or check 4-5)
        if m.contains("4-5") || m.contains("4.5") || m.contains("4-6") || m.contains("4.6") {
            ModelPricing {
                input_per_million: 5.0,
                output_per_million: 25.0,
                cache_write_per_million: 6.25,
                cache_read_per_million: 0.50,
            }
        } else {
            ModelPricing {
                input_per_million: 15.0,
                output_per_million: 75.0,
                cache_write_per_million: 18.75,
                cache_read_per_million: 1.50,
            }
        }
    } else if m.contains("haiku") {
        ModelPricing {
            input_per_million: 1.0,
            output_per_million: 5.0,
            cache_write_per_million: 1.25,
            cache_read_per_million: 0.10,
        }
    } else if m.contains("sonnet") {
        ModelPricing {
            input_per_million: 3.0,
            output_per_million: 15.0,
            cache_write_per_million: 3.75,
            cache_read_per_million: 0.30,
        }
    } else if m.contains("gpt-5") || m.contains("o3") || m.contains("codex") {
        ModelPricing {
            input_per_million: 2.0,
            output_per_million: 8.0,
            cache_write_per_million: 2.0,
            cache_read_per_million: 0.50,
        }
    } else if m.contains("gemini") {
        ModelPricing {
            input_per_million: 1.25,
            output_per_million: 5.0,
            cache_write_per_million: 1.25,
            cache_read_per_million: 0.31,
        }
    } else {
        // Fallback default (Sonnet-equivalent)
        ModelPricing {
            input_per_million: 3.0,
            output_per_million: 15.0,
            cache_write_per_million: 3.75,
            cache_read_per_million: 0.30,
        }
    }
}

pub fn estimate_cost_usd(
    model: &str,
    input_tokens: i64,
    output_tokens: i64,
    cache_write_tokens: i64,
    cache_read_tokens: i64,
) -> f64 {
    let p = pricing_for_model(model);
    let in_cost = (input_tokens.max(0) as f64 / 1_000_000.0) * p.input_per_million;
    let out_cost = (output_tokens.max(0) as f64 / 1_000_000.0) * p.output_per_million;
    let cw_cost = (cache_write_tokens.max(0) as f64 / 1_000_000.0) * p.cache_write_per_million;
    let cr_cost = (cache_read_tokens.max(0) as f64 / 1_000_000.0) * p.cache_read_per_million;
    in_cost + out_cost + cw_cost + cr_cost
}
