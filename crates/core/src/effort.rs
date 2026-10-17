//! Reasoning-effort translation across protocols, mirroring magpie's
//! `ir.go` `effortOf`/`effortOfBudget`/`budgetOf`: one agent's level in one
//! API's words becomes another's, and an effort a vendor has no words for
//! is asked in the shape it does take, never silently dropped.

use serde_json::{Value, json};

/// An effort a request asks for: `low | medium | high | xhigh | max`,
/// `none` when the client turned reasoning off, or `unspecified` when it
/// asked to think without naming a level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effort {
    Off,
    Low,
    Medium,
    High,
    XHigh,
    Max,
    /// The client asked for reasoning but named no level.
    Unspecified,
}

impl Effort {
    /// A Chat/Responses `reasoning_effort` or `reasoning.effort` string in
    /// the IR's canonical level, `None` when it names none.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "none" => Some(Self::Off),
            "minimal" => Some(Self::Low),
            "low" => Some(Self::Low),
            "medium" => Some(Self::Medium),
            "high" => Some(Self::High),
            "xhigh" => Some(Self::XHigh),
            "max" | "ultra" => Some(Self::Max),
            _ => None,
        }
    }

    /// The level a Chat/Responses upstream is asked at: `None` sends no
    /// reasoning ask at all.
    pub fn wire_level(self) -> Option<&'static str> {
        match self {
            Self::Off => None,
            Self::Low => Some("low"),
            Self::Medium => Some("medium"),
            Self::High => Some("high"),
            Self::XHigh => Some("xhigh"),
            Self::Max => Some("max"),
            Self::Unspecified => None,
        }
    }

    /// Whether the client asked its model to reason at all (level set, or
    /// thinking asked without a level).
    pub fn asked(self) -> bool {
        !matches!(self, Self::Off)
    }
}

/// The effort an Anthropic thinking budget is nearest: the reference's
/// `effortOfBudget`.
fn effort_of_budget(budget: i64) -> Option<Effort> {
    match budget {
        n if n <= 0 => None,
        n if n <= 4096 => Some(Effort::Low),
        n if n <= 12000 => Some(Effort::Medium),
        n if n <= 24000 => Some(Effort::High),
        _ => Some(Effort::XHigh),
    }
}

/// The Anthropic thinking budget an effort asks for: the reference's
/// `budgetOf`.
fn budget_of(effort: Option<Effort>) -> i64 {
    match effort {
        Some(Effort::Low) => 4096,
        Some(Effort::Medium) | None => 10000,
        Some(Effort::High) => 24000,
        Some(Effort::XHigh | Effort::Max) => 32000,
        _ => 10000,
    }
}

/// A Gemini 2.5-style thinking budget an effort asks for (the reference's
/// `thinkingConfig` fallback): every level names a budget; `None` keeps the
/// model's own.
fn gemini_budget_of(effort: Option<Effort>) -> Option<i64> {
    match effort {
        Some(Effort::Low) => Some(1024),
        Some(Effort::Medium) => Some(8192),
        Some(Effort::High) => Some(24576),
        Some(Effort::XHigh | Effort::Max) => Some(32768),
        _ => None,
    }
}

/// What a request asked for, read per protocol: `Ok(None)` when it named no
/// reasoning ask, `Ok(Some(..))` when it did, `Err` when the ask is
/// malformed enough to reject.
pub fn decode(protocol: crate::ir::ToolChoiceProtocol, v: &Value) -> Option<Effort> {
    match protocol {
        crate::ir::ToolChoiceProtocol::Chat => v
            .get("reasoning_effort")
            .and_then(Value::as_str)
            .and_then(Effort::parse),
        crate::ir::ToolChoiceProtocol::Responses => v
            .get("reasoning")
            .and_then(|r| r.get("effort"))
            .and_then(Value::as_str)
            .and_then(Effort::parse),
        crate::ir::ToolChoiceProtocol::Anthropic => {
            let thinking = v.get("thinking")?;
            match thinking.get("type").and_then(Value::as_str)? {
                "disabled" | "between_tools" => Some(Effort::Off),
                "enabled" | "adaptive" => {
                    let by_budget = effort_of_budget(
                        thinking
                            .get("budget_tokens")
                            .and_then(Value::as_i64)
                            .unwrap_or(0),
                    );
                    let by_output = v
                        .get("output_config")
                        .and_then(|oc| oc.get("effort"))
                        .and_then(Value::as_str)
                        .and_then(Effort::parse);
                    Some(by_output.or(by_budget).unwrap_or(Effort::Unspecified))
                }
                _ => None,
            }
        }
        crate::ir::ToolChoiceProtocol::Gemini => {
            let gc = v
                .get("generationConfig")
                .or_else(|| v.get("generation_config"))?;
            let tc = gc
                .get("thinkingConfig")
                .or_else(|| gc.get("thinking_config"))?;
            if let Some(level) = tc
                .get("thinkingLevel")
                .or_else(|| tc.get("thinking_level"))
                .and_then(Value::as_str)
            {
                return Effort::parse(level);
            }
            match tc
                .get("thinkingBudget")
                .or_else(|| tc.get("thinking_budget"))
            {
                Some(Value::Number(b)) => {
                    let b = b.as_i64().unwrap_or(0);
                    if b < 0 {
                        Some(Effort::Medium) // -1: let the model decide
                    } else {
                        effort_of_budget(b)
                    }
                }
                _ => None,
            }
        }
    }
}

/// Write the ask into a body for `protocol`, replacing what decode read.
pub fn encode(
    protocol: crate::ir::ToolChoiceProtocol,
    out: &mut Value,
    effort: Effort,
    max_tokens: Option<u64>,
) {
    match protocol {
        crate::ir::ToolChoiceProtocol::Chat => {
            if let Some(level) = effort.wire_level() {
                out["reasoning_effort"] = json!(level);
            }
        }
        crate::ir::ToolChoiceProtocol::Responses => {
            let reasoning = out
                .as_object_mut()
                .expect("request body")
                .entry("reasoning")
                .or_insert_with(|| json!({}));
            if let Some(level) = effort.wire_level() {
                reasoning["effort"] = json!(level);
            }
            reasoning["summary"] = json!("auto");
        }
        crate::ir::ToolChoiceProtocol::Anthropic => {
            if !effort.asked() {
                out["thinking"] = json!({"type": "disabled"});
                return;
            }
            let mut budget = budget_of(Some(effort));
            let mut cap = max_tokens.unwrap_or(0) as i64;
            if cap > 0 {
                budget = budget.min(cap - 1024);
                if budget < 1024 {
                    out["thinking"] = json!({"type": "disabled"});
                    return;
                }
            } else {
                cap = budget + 4096;
                out["max_tokens"] = json!(cap);
            }
            out["thinking"] = json!({"type": "enabled", "budget_tokens": budget});
        }
        crate::ir::ToolChoiceProtocol::Gemini => {
            let gc = out
                .as_object_mut()
                .expect("request body")
                .entry("generationConfig")
                .or_insert_with(|| json!({}));
            let tc = gc
                .as_object_mut()
                .expect("generationConfig")
                .entry("thinkingConfig")
                .or_insert_with(|| json!({}));
            tc["includeThoughts"] = json!(true);
            if let Some(level) = effort.wire_level() {
                tc["thinkingLevel"] = json!(level);
            } else if let Some(budget) = gemini_budget_of(Some(effort)) {
                tc["thinkingBudget"] = json!(budget);
            }
        }
    }
}
