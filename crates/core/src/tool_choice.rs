//! Normalize function-tool selection before constructing any provider request.
//! Native-only constraints are retained natively or rejected, never widened.
use std::borrow::Cow;

use serde_json::{Value, json};

use crate::{CoreError, Result, ir::CompletionRequest};

pub(crate) use crate::ir::ToolChoiceProtocol as Protocol;

#[derive(PartialEq, Eq)]
enum Mode {
    Auto,
    None,
    Required,
    Function(String),
    Validated,
}

struct Selection {
    mode: Mode,
    allowed: Option<Vec<String>>,
    search_allowed: Option<bool>,
    native: Option<Protocol>,
    native_only: bool,
}

fn invalid(detail: &'static str) -> CoreError {
    CoreError::Invalid {
        field: "tool_choice",
        detail: detail.into(),
    }
}
fn name(v: Option<&Value>) -> Result<String> {
    v.and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| invalid("missing selected function name"))
}
fn selected_name(v: &Value) -> Result<String> {
    if let Some(nested) = v.get("function") {
        if !nested.is_object() || v.get("name").is_some() {
            return Err(invalid("ambiguous or malformed selected function"));
        }
        return name(nested.get("name"));
    }
    name(v.get("name"))
}
fn mode(v: &str) -> Result<Mode> {
    match v {
        "auto" => Ok(Mode::Auto),
        "none" => Ok(Mode::None),
        "required" | "any" => Ok(Mode::Required),
        _ => Err(invalid("unsupported tool selection mode")),
    }
}
fn extra(v: &Value, known: &[&str]) -> bool {
    v.as_object()
        .is_some_and(|m| m.keys().any(|k| !known.contains(&k.as_str())))
}
fn parse(v: &Value) -> Result<Selection> {
    let mut s = Selection {
        mode: Mode::Auto,
        allowed: None,
        search_allowed: None,
        native: None,
        native_only: false,
    };
    if let Some(m) = v.as_str() {
        s.mode = mode(m)?;
        return Ok(s);
    }
    if !v.is_object() {
        return Err(invalid("expected a tool selection string or object"));
    }
    if v.get("functionCallingConfig").is_some() && v.get("function_calling_config").is_some() {
        return Err(invalid("ambiguous function calling configuration"));
    }
    if let Some(config) = v
        .get("functionCallingConfig")
        .or_else(|| v.get("function_calling_config"))
    {
        if !config.is_object() {
            return Err(invalid("expected a function calling configuration object"));
        }
        if config.get("allowedFunctionNames").is_some()
            && config.get("allowed_function_names").is_some()
        {
            return Err(invalid("ambiguous function name allowlist"));
        }
        let m = match config.get("mode") {
            None => "AUTO",
            Some(v) => v
                .as_str()
                .ok_or_else(|| invalid("expected a function calling mode string"))?,
        };
        s.mode = match m.to_ascii_uppercase().as_str() {
            "AUTO" => Mode::Auto,
            "NONE" => Mode::None,
            "ANY" => Mode::Required,
            "VALIDATED" => Mode::Validated,
            _ => return Err(invalid("unsupported function calling mode")),
        };
        s.native = Some(Protocol::Gemini);
        s.native_only = s.mode == Mode::Validated
            || extra(v, &["functionCallingConfig", "function_calling_config"])
            || extra(
                config,
                &["mode", "allowedFunctionNames", "allowed_function_names"],
            );
        if let Some(allowed) = config
            .get("allowedFunctionNames")
            .or_else(|| config.get("allowed_function_names"))
        {
            let allowed = allowed
                .as_array()
                .ok_or_else(|| invalid("expected a function name allowlist"))?;
            let names: Vec<_> = allowed
                .iter()
                .map(|v| name(Some(v)))
                .collect::<Result<_>>()?;
            // Gemini's absent/empty allowlist means any declared function.
            if !names.is_empty() {
                if s.mode == Mode::Required && names.len() == 1 {
                    s.mode = Mode::Function(names[0].clone());
                }
                s.allowed = Some(names);
            }
        }
        return Ok(s);
    }
    let ty = v
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("missing tool selection type"))?;
    match ty {
        "allowed_tools" => {
            s.mode = mode(match v.get("mode") {
                None => "auto",
                Some(v) => v
                    .as_str()
                    .ok_or_else(|| invalid("expected an allowed tools mode string"))?,
            })?;
            if !matches!(s.mode, Mode::Auto | Mode::Required) {
                return Err(invalid("allowed tools mode must be auto or required"));
            }
            let tools = v
                .get("tools")
                .and_then(Value::as_array)
                .ok_or_else(|| invalid("expected an allowed tools array"))?;
            let mut allowed = Vec::new();
            let mut search = false;
            for tool in tools {
                match tool.get("type").and_then(Value::as_str) {
                    Some("function") => {
                        if tool.get("namespace").is_some() {
                            return Err(invalid("namespaced allowed functions are not supported"));
                        }
                        let nested = tool.get("function");
                        if extra(tool, &["type", "name", "function"])
                            || nested.is_some_and(|f| extra(f, &["name"]))
                        {
                            return Err(invalid("unsupported allowed function restrictions"));
                        }
                        allowed.push(selected_name(tool)?);
                    }
                    Some(t) if t == "web_search" || t.starts_with("web_search_") => {
                        if extra(tool, &["type"]) {
                            return Err(invalid("unsupported allowed search restrictions"));
                        }
                        search = true;
                    }
                    // Unknown built-ins are not callable through the function IR.
                    Some(_) => {}
                    None => return Err(invalid("missing allowed tool type")),
                }
            }
            if extra(v, &["type", "mode", "tools"]) {
                return Err(invalid("unsupported allowed tools restrictions"));
            }
            s.allowed = Some(allowed);
            s.search_allowed = Some(search);
        }
        "function" | "tool" => {
            s.mode = Mode::Function(selected_name(v)?);
            s.native = Some(if ty == "tool" {
                Protocol::Anthropic
            } else if v.get("function").is_some() {
                Protocol::Chat
            } else {
                Protocol::Responses
            });
            s.native_only = extra(v, &["type", "name", "function"])
                || v.get("function").is_some_and(|f| extra(f, &["name"]));
        }
        "auto" | "none" | "any" => {
            s.mode = mode(ty)?;
            s.native = Some(Protocol::Anthropic);
            s.native_only = extra(v, &["type"]);
        }
        _ => return Err(invalid("unsupported tool selection type")),
    }
    Ok(s)
}
fn wire(mode: &Mode, target: Protocol) -> Value {
    match (target, mode) {
        (Protocol::Chat | Protocol::Responses, Mode::Auto) => json!("auto"),
        (Protocol::Chat | Protocol::Responses, Mode::None) => json!("none"),
        (Protocol::Chat | Protocol::Responses, Mode::Required) => json!("required"),
        (Protocol::Chat, Mode::Function(name)) => {
            json!({"type":"function","function":{"name":name}})
        }
        (Protocol::Responses, Mode::Function(name)) => json!({"type":"function","name":name}),
        (Protocol::Anthropic, Mode::Auto) => json!({"type":"auto"}),
        (Protocol::Anthropic, Mode::None) => json!({"type":"none"}),
        (Protocol::Anthropic, Mode::Required) => json!({"type":"any"}),
        (Protocol::Anthropic, Mode::Function(name)) => json!({"type":"tool","name":name}),
        (Protocol::Gemini, Mode::Auto) => json!({"functionCallingConfig":{"mode":"AUTO"}}),
        (Protocol::Gemini, Mode::None) => json!({"functionCallingConfig":{"mode":"NONE"}}),
        (Protocol::Gemini, Mode::Required) => json!({"functionCallingConfig":{"mode":"ANY"}}),
        (Protocol::Gemini, Mode::Function(name)) => {
            json!({"functionCallingConfig":{"mode":"ANY","allowedFunctionNames":[name]}})
        }
        // VALIDATED is never translated; native raw configuration is used instead.
        (_, Mode::Validated) => unreachable!("native-only function calling mode"),
    }
}

pub(crate) fn prepare(
    req: &CompletionRequest,
    target: Protocol,
) -> Result<Cow<'_, CompletionRequest>> {
    let Some(raw) = &req.tool_choice else {
        return Ok(Cow::Borrowed(req));
    };
    let allowed_tools = raw.get("type").is_some_and(|v| v == "allowed_tools");
    let search_owns_choice = req.web_search.is_some() && !allowed_tools;
    let gemini_function_config =
        raw.get("functionCallingConfig").is_some() || raw.get("function_calling_config").is_some();
    if search_owns_choice && !gemini_function_config {
        // Hosted search owns its separate built-in and global/function choices.
        return Ok(Cow::Borrowed(req));
    }
    let selection = parse(raw)?;
    if selection.native_only && selection.native != Some(target) {
        return Err(invalid(
            "native tool selection restrictions cannot be translated to this provider protocol",
        ));
    }
    if search_owns_choice {
        // Validate Gemini policies before delegating search-specific translation.
        // VALIDATED permits text as well as calls; search must not turn it into ANY.
        return Ok(Cow::Borrowed(req));
    }
    let mut prepared = req.clone();
    if let Some(allowed) = &selection.allowed {
        prepared.tools.retain(|t| allowed.contains(&t.name));
    }
    if let Some(allow_search) = selection.search_allowed {
        prepared.web_search = prepared.web_search.as_ref().and_then(|s| {
            s.with_allowed_tools(
                allow_search,
                selection.mode == Mode::Required,
                target == Protocol::Responses,
            )
        });
    }
    match &selection.mode {
        Mode::Required if prepared.tools.is_empty() && prepared.web_search.is_none() => {
            return Err(invalid("required tool selection has no callable tools"));
        }
        Mode::Function(name) if !prepared.tools.iter().any(|t| t.name == *name) => {
            return Err(invalid("selected function is not callable"));
        }
        _ => {}
    }
    prepared.tool_choice = Some(
        if selection.native == Some(target)
            || (allowed_tools && req.tool_choice_protocol == Some(target))
        {
            raw.clone()
        } else {
            wire(&selection.mode, target)
        },
    );
    Ok(Cow::Owned(prepared))
}
