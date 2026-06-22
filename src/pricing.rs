//! Built-in, editable model pricing + the cost-estimation formula.
//!
//! These numbers drive the dashboard's "$ saved" estimate. They are **approximate
//! public list prices**, not your actual bill — the README and UI say so. Edit the
//! `BUILTIN` table below (or set `CACHET_PRICING`) to match your plan.

/// Built-in list prices in USD per 1,000,000 tokens: `(model_prefix, input, output)`.
/// Matched by longest prefix, so `gpt-4o-mini` wins over `gpt-4o` for that model.
/// Approximate public list prices as of early 2026 — **edit to taste**.
const BUILTIN: &[(&str, f64, f64)] = &[
    ("gpt-4o-mini", 0.15, 0.60),
    ("gpt-4o", 2.50, 10.00),
    ("gpt-4-turbo", 10.00, 30.00),
    ("gpt-4", 30.00, 60.00),
    ("gpt-3.5-turbo", 0.50, 1.50),
    ("claude-3-5-sonnet", 3.00, 15.00),
    ("claude-3-5-haiku", 0.80, 4.00),
    ("claude-3-opus", 15.00, 75.00),
];

/// Fallback price (USD per 1M in/out) for models not in the table.
const FALLBACK: (f64, f64) = (1.00, 3.00);

/// Characters-per-token heuristic. ~4 chars/token is the common English rule of
/// thumb. This is an estimate, not a real tokenizer — stated plainly in the UI.
const CHARS_PER_TOKEN: usize = 4;

/// Resolved pricing table (built-ins + optional env overrides).
pub struct Pricing {
    /// Sorted longest-prefix-first so matching is a simple linear scan.
    table: Vec<(String, f64, f64)>,
    fallback: (f64, f64),
}

impl Pricing {
    /// Build from the `BUILTIN` table, applying `CACHET_PRICING` overrides if set.
    /// Format: `model=in/out,model=in/out` e.g. `gpt-4o=2.5/10,my-model=1/2`.
    pub fn from_env() -> Self {
        let mut table: Vec<(String, f64, f64)> = BUILTIN
            .iter()
            .map(|(m, i, o)| (m.to_string(), *i, *o))
            .collect();

        if let Ok(raw) = std::env::var("CACHET_PRICING") {
            for entry in raw.split(',') {
                let Some((model, prices)) = entry.split_once('=') else {
                    continue;
                };
                let Some((input, output)) = prices.split_once('/') else {
                    continue;
                };
                if let (Ok(input), Ok(output)) = (input.trim().parse(), output.trim().parse()) {
                    let model = model.trim().to_lowercase();
                    match table.iter_mut().find(|(m, _, _)| *m == model) {
                        Some(existing) => {
                            existing.1 = input;
                            existing.2 = output;
                        }
                        None => table.push((model, input, output)),
                    }
                }
            }
        }

        table.sort_by_key(|entry| std::cmp::Reverse(entry.0.len()));
        Self {
            table,
            fallback: FALLBACK,
        }
    }

    fn price(&self, model: &str) -> (f64, f64) {
        let model = model.to_lowercase();
        for (prefix, input, output) in &self.table {
            if model.starts_with(prefix.as_str()) {
                return (*input, *output);
            }
        }
        self.fallback
    }

    /// Estimate `(tokens_saved, micro_dollars_saved)` for a served cache hit.
    ///
    /// A hit avoids one upstream call, saving both the input tokens (we didn't send
    /// the prompt) and the output tokens (the model didn't regenerate the answer):
    ///
    /// ```text
    /// tokens   ≈ chars / 4
    /// saved($) = input_tokens/1e6 * input_price + output_tokens/1e6 * output_price
    /// ```
    ///
    /// Returned in micro-dollars (USD × 1e6) so totals can be summed in an atomic.
    pub fn saved(&self, model: &str, input_chars: usize, output_chars: usize) -> (u64, u64) {
        let input_tokens = (input_chars / CHARS_PER_TOKEN) as u64;
        let output_tokens = (output_chars / CHARS_PER_TOKEN) as u64;
        let (input_price, output_price) = self.price(model);
        let dollars =
            input_tokens as f64 / 1e6 * input_price + output_tokens as f64 / 1e6 * output_price;
        let micros = (dollars * 1e6).round() as u64;
        (input_tokens + output_tokens, micros)
    }
}
