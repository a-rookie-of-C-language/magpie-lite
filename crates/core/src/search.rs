//! Provider-executed search requests. Native configuration is retained; a
//! cross-protocol route must not silently widen a configured search policy.
use serde_json::{Map, Value, json};

use crate::{CoreError, Result, ir::CompletionRequest};

/// The wire dialect in which a built-in search was requested.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchProtocol {
    /// Anthropic's versioned web_search tool.
    Anthropic,
    /// OpenAI Chat Completions web_search_options.
    Chat,
    /// OpenAI Responses web_search tool.
    Responses,
    /// Gemini googleSearch tool.
    Gemini,
}

/// Search availability is separate from agent-executed function declarations.
#[derive(Debug, Clone)]
pub struct WebSearchRequest {
    protocol: SearchProtocol,
    tool: Value,
    // Kept in the original dialect, including Gemini allowlists that the older
    // function-only IR cannot represent without loss.
    choice: Option<Value>,
}

fn invalid(field: &'static str, detail: &'static str) -> CoreError {
    CoreError::Invalid {
        field,
        detail: detail.into(),
    }
}

fn is_search_type(t: &Value) -> bool {
    t.get("type")
        .and_then(Value::as_str)
        .is_some_and(|s| s == "web_search" || s.starts_with("web_search_"))
}

impl WebSearchRequest {
    /// Extract a built-in search definition without treating it as a function.
    /// Duplicate definitions and malformed option objects are request errors.
    pub fn decode(protocol: SearchProtocol, body: &Value) -> Result<Option<Self>> {
        let mut definitions = Vec::new();
        if protocol == SearchProtocol::Chat
            && let Some(options) = body.get("web_search_options").filter(|v| !v.is_null())
        {
            if !options.is_object() {
                return Err(invalid("web_search_options", "expected an object"));
            }
            definitions.push(options.clone());
        }
        for tool in body
            .get("tools")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if protocol == SearchProtocol::Gemini {
                for key in ["googleSearch", "google_search"] {
                    if let Some(options) = tool.get(key) {
                        if !options.is_object() {
                            return Err(invalid("googleSearch", "expected an object"));
                        }
                        definitions.push(json!({key: options}));
                    }
                }
            } else if is_search_type(tool) {
                if protocol == SearchProtocol::Chat {
                    // Some compatible clients use the Responses-style marker.
                    // Chat has no standard built-in tool declaration; only a
                    // bare marker can safely become web_search_options.
                    if tool
                        .as_object()
                        .is_none_or(|m| m.keys().any(|k| k != "type"))
                    {
                        return Err(invalid(
                            "tools",
                            "Chat search markers must be bare; use web_search_options for configuration",
                        ));
                    }
                    definitions.push(json!({}));
                } else {
                    definitions.push(tool.clone());
                }
            }
        }
        if definitions.len() > 1 {
            return Err(invalid(
                "tools",
                "duplicate built-in web search definitions",
            ));
        }
        let choice = if protocol == SearchProtocol::Gemini {
            body.get("toolConfig").or_else(|| body.get("tool_config"))
        } else {
            body.get("tool_choice")
        }
        .filter(|v| !v.is_null())
        .cloned();
        Ok(definitions.pop().map(|tool| Self {
            protocol,
            tool,
            choice,
        }))
    }

    /// An allowed-tools selector constrains offered declarations, including search.
    pub(crate) fn with_allowed_tools(
        &self,
        allow_search: bool,
        required: bool,
        preserve_native: bool,
    ) -> Option<Self> {
        if self.protocol == SearchProtocol::Responses && !allow_search {
            return None;
        }
        let mut restricted = self.clone();
        if !(preserve_native && self.protocol == SearchProtocol::Responses) {
            restricted.choice = Some(json!(if required { "required" } else { "auto" }));
        }
        Some(restricted)
    }

    fn choice(&self) -> Result<Choice> {
        let Some(v) = &self.choice else {
            return Ok(Choice::Auto);
        };
        if self.protocol == SearchProtocol::Gemini {
            let config = v
                .get("functionCallingConfig")
                .or_else(|| v.get("function_calling_config"))
                .ok_or_else(|| {
                    invalid(
                        "tool_choice",
                        "cannot translate this Gemini tool configuration",
                    )
                })?;
            if v.as_object().is_none_or(|m| m.len() != 1)
                || config.as_object().is_none_or(|m| {
                    m.keys().any(|k| {
                        !matches!(
                            k.as_str(),
                            "mode" | "allowedFunctionNames" | "allowed_function_names"
                        )
                    })
                })
            {
                return Err(invalid(
                    "tool_choice",
                    "cannot translate additional Gemini tool configuration",
                ));
            }
            let allowed = config
                .get("allowedFunctionNames")
                .or_else(|| config.get("allowed_function_names"));
            let mode = config.get("mode").and_then(Value::as_str).unwrap_or("AUTO");
            if let Some(names) = allowed {
                if !matches!(mode, "ANY" | "VALIDATED")
                    || names.as_array().is_none_or(|a| a.len() != 1)
                {
                    return Err(invalid(
                        "tool_choice",
                        "cannot translate this Gemini function allowlist",
                    ));
                }
                return names[0]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .map(|s| Choice::Function(s.into()))
                    .ok_or_else(|| invalid("tool_choice", "invalid function name"));
            }
            return match mode {
                "AUTO" => Ok(Choice::Auto),
                "NONE" => Ok(Choice::None),
                "ANY" | "VALIDATED" => Ok(Choice::Required),
                _ => Err(invalid(
                    "tool_choice",
                    "unsupported Gemini function calling mode",
                )),
            };
        }
        if let Some(s) = v.as_str() {
            return match s {
                "auto" => Ok(Choice::Auto),
                "none" => Ok(Choice::None),
                "required" => Ok(Choice::Required),
                _ => Err(invalid("tool_choice", "unsupported search tool choice")),
            };
        }
        let ty = v.get("type").and_then(Value::as_str).unwrap_or_default();
        let allowed_keys: &[&str] = match ty {
            "tool" | "function" => &["type", "name", "function"],
            _ => &["type"],
        };
        if v.as_object()
            .is_none_or(|m| m.keys().any(|k| !allowed_keys.contains(&k.as_str())))
        {
            return Err(invalid(
                "tool_choice",
                "cannot translate additional tool choice restrictions",
            ));
        }
        match ty {
            "auto" => Ok(Choice::Auto),
            "none" => Ok(Choice::None),
            "any" => Ok(Choice::Required),
            t if t == "web_search" || t.starts_with("web_search_") => Ok(Choice::Search),
            "tool" | "function" => {
                let name = v
                    .get("name")
                    .or_else(|| v.pointer("/function/name"))
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| invalid("tool_choice", "missing selected function name"))?;
                if ty == "tool" && name == "web_search" {
                    Ok(Choice::Search)
                } else {
                    Ok(Choice::Function(name.into()))
                }
            }
            _ => Err(invalid("tool_choice", "unsupported search tool choice")),
        }
    }

    fn chooses_none(&self) -> bool {
        // Recognize NONE even if native configuration contains extra options.
        self.choice.as_ref().is_some_and(|v| {
            v == "none"
                || v.get("type").is_some_and(|t| t == "none")
                || v.pointer("/functionCallingConfig/mode")
                    .or_else(|| v.pointer("/function_calling_config/mode"))
                    .is_some_and(|m| m == "NONE")
        })
    }

    fn tool_for(&self, target: SearchProtocol) -> Result<Value> {
        if self.protocol == target {
            return Ok(self.tool.clone());
        }
        let bare = match self.protocol {
            SearchProtocol::Anthropic => self.tool.as_object().is_some_and(|m| {
                m.keys().all(|k| k == "type" || k == "name")
                    && self.tool.get("name").is_none_or(|n| n == "web_search")
            }),
            SearchProtocol::Responses => self.tool.as_object().is_some_and(|m| m.len() == 1),
            SearchProtocol::Chat => self.tool.as_object().is_some_and(Map::is_empty),
            SearchProtocol::Gemini => self
                .tool
                .as_object()
                .is_some_and(|m| m.values().all(|v| v.as_object().is_some_and(Map::is_empty))),
        };
        if !bare {
            return Err(invalid(
                "web_search",
                "configured search options cannot be translated to this provider protocol; use a native-protocol route",
            ));
        }
        Ok(match target {
            SearchProtocol::Anthropic => {
                json!({"type":"web_search_20250305", "name":"web_search", "max_uses":5})
            }
            SearchProtocol::Responses => json!({"type":"web_search"}),
            SearchProtocol::Chat => json!({}),
            SearchProtocol::Gemini => json!({"googleSearch":{}}),
        })
    }
}

#[derive(Debug)]
enum Choice {
    Auto,
    None,
    Required,
    Search,
    Function(String),
}

fn choice_for(choice: Choice, target: SearchProtocol, tool: &Value) -> Result<Value> {
    Ok(match (target, choice) {
        (SearchProtocol::Anthropic, Choice::Auto) => json!({"type":"auto"}),
        (SearchProtocol::Anthropic, Choice::None) => json!({"type":"none"}),
        (SearchProtocol::Anthropic, Choice::Required) => json!({"type":"any"}),
        (SearchProtocol::Anthropic, Choice::Search) => json!({"type":"tool","name":"web_search"}),
        (SearchProtocol::Anthropic, Choice::Function(name)) => json!({"type":"tool","name":name}),
        (SearchProtocol::Chat | SearchProtocol::Responses, Choice::Auto) => json!("auto"),
        (SearchProtocol::Chat | SearchProtocol::Responses, Choice::None) => json!("none"),
        (SearchProtocol::Chat | SearchProtocol::Responses, Choice::Required) => json!("required"),
        (SearchProtocol::Responses, Choice::Search) => json!({"type":tool["type"]}),
        (SearchProtocol::Responses, Choice::Function(name)) => {
            json!({"type":"function","name":name})
        }
        (SearchProtocol::Chat, Choice::Function(name)) => {
            json!({"type":"function","function":{"name":name}})
        }
        (SearchProtocol::Gemini, Choice::Auto) => json!({"functionCallingConfig":{"mode":"AUTO"}}),
        (SearchProtocol::Gemini, Choice::None) => json!({"functionCallingConfig":{"mode":"NONE"}}),
        (SearchProtocol::Gemini, Choice::Function(name)) => {
            json!({"functionCallingConfig":{"mode":"ANY","allowedFunctionNames":[name]}})
        }
        // Gemini functionCallingConfig only controls functions, not googleSearch;
        // Chat tool_choice likewise cannot force hosted search.
        (_, Choice::Search | Choice::Required) => {
            return Err(invalid(
                "tool_choice",
                "this protocol cannot force a built-in search/tool choice",
            ));
        }
    })
}

/// Add a search definition and translate its tool choice to the outgoing body.
/// Called by every request encoder, so direct core users also get errors rather
/// than silently losing policy options during cross-protocol translation.
pub(crate) fn encode(
    req: &CompletionRequest,
    target: SearchProtocol,
    out: &mut Map<String, Value>,
) -> Result<()> {
    let Some(search) = &req.web_search else {
        return Ok(());
    };
    let choice_key = if target == SearchProtocol::Gemini {
        "toolConfig"
    } else {
        "tool_choice"
    };
    let functions_only = matches!(
        search.protocol,
        SearchProtocol::Chat | SearchProtocol::Gemini
    );
    if search.chooses_none() && !functions_only {
        // Anthropic/Responses NONE disables every tool, including hosted search.
        // Omit search even when the target only has function-specific NONE.
        if req.tools.is_empty() {
            out.remove(choice_key);
        } else {
            let choice = if target == search.protocol {
                search
                    .choice
                    .clone()
                    .expect("explicit disabled search choice")
            } else {
                choice_for(Choice::None, target, &Value::Null)?
            };
            out.insert(choice_key.into(), choice);
        }
        return Ok(());
    }
    let tool = search.tool_for(target)?;
    let remove_functions = functions_only && search.chooses_none() && target != search.protocol;
    if remove_functions {
        // Chat/Gemini NONE disables functions, not their separate hosted search.
        // Other dialects cannot express that with a global NONE; omit function
        // declarations instead, leaving only the explicitly requested search.
        out.remove("tools");
        out.remove(choice_key);
    }
    let choice = if target == search.protocol {
        search.choice.clone()
    } else {
        let choice = search.choice()?;
        if functions_only && matches!(choice, Choice::Required) {
            return Err(invalid(
                "tool_choice",
                "cannot translate a function-only requirement to a global tool choice",
            ));
        }
        Some(choice_for(
            if remove_functions {
                Choice::Auto
            } else {
                choice
            },
            target,
            &tool,
        )?)
    };
    let has_functions = !req.tools.is_empty() && !remove_functions;
    if target == SearchProtocol::Chat {
        // tool_choice selects function tools here, not web_search_options.
        if !has_functions {
            if !matches!(search.choice()?, Choice::Auto | Choice::None) {
                return Err(invalid(
                    "tool_choice",
                    "Chat cannot select a built-in search tool",
                ));
            }
            out.remove(choice_key);
        } else if let Some(choice) = choice {
            out.insert(choice_key.into(), choice);
        }
        out.insert("web_search_options".into(), tool);
    } else {
        let tools = out.entry("tools").or_insert_with(|| json!([]));
        tools
            .as_array_mut()
            .expect("request encoder tools array")
            .push(tool);
        if let Some(choice) = choice {
            out.insert(choice_key.into(), choice);
        }
    }
    if target == SearchProtocol::Responses {
        out.insert("include".into(), json!(["web_search_call.action.sources"]));
    }
    Ok(())
}
