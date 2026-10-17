//! The intermediate representation every wire protocol is decoded into and
//! encoded out of. Mirrors the design of magpie's `internal/gateway/ir.go`:
//! N protocols need only N decoders and N encoders, never N-squared
//! pairwise translators.

use serde_json::Value;

/// Who wrote a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// The user side: user text, images, and tool results.
    User,
    /// The assistant side: text, tool calls, and thinking.
    Assistant,
}

impl Role {
    /// Wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            Role::User => "user",
            Role::Assistant => "assistant",
        }
    }

    /// Parse a wire role name; anything unknown maps to the user side.
    pub fn parse(s: &str) -> Role {
        match s {
            "assistant" => Role::Assistant,
            _ => Role::User,
        }
    }
}

/// What one block of a message holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PartKind {
    /// Plain text (or a tool result's output).
    Text,
    /// An image, inline (base64) or by URL.
    Image,
    /// The assistant asking a tool to run.
    ToolCall,
    /// The user side answering a tool call.
    ToolResult,
    /// A web search already executed by the upstream, not a client tool call.
    Search,
    /// Reasoning the model showed (with its signature, when it has one).
    Thinking,
}

/// One public source returned by an upstream web search.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearchHit {
    /// Page title, falling back to its URL when absent.
    pub title: String,
    /// Upstream source URL; the gateway does not fetch it.
    pub url: String,
    /// Provider-reported page age, when available.
    pub page_age: String,
}

/// A search performed by the upstream model, distinct from function tools.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WebSearch {
    /// Query text (Gemini's multiple queries are joined with "; ").
    pub query: String,
    /// Public result titles and URLs; never vendor-encrypted content.
    pub hits: Vec<SearchHit>,
    /// Original Gemini grounding metadata, only re-emitted to Gemini clients.
    /// This retains citation offsets, chunk indices and required attribution.
    /// In SearchUpdate events this is the exact native update; query/hits carry
    /// only new public items, not a reconstructed native metadata object.
    pub gemini_grounding: Option<Value>,
    /// Original Responses search action, only re-emitted to Responses clients.
    pub responses_action: Option<Value>,
    /// One original Anthropic search call/result block, for Anthropic only.
    /// Retaining blocks separately preserves ordering and vendor ciphertext in
    /// native replies/history without exposing it to other protocols.
    pub anthropic_block: Option<Value>,
}

impl WebSearch {
    /// Whether a completed public result can be rendered in another protocol.
    /// Native call blocks are rendered only once their matching result arrives;
    /// native search errors must not become fabricated successful grounding.
    pub fn is_public_result(&self) -> bool {
        self.anthropic_block.as_ref().is_none_or(|block| {
            block
                .get("type")
                .is_some_and(|t| t == "web_search_tool_result")
                && block.get("content").is_some_and(Value::is_array)
        })
    }

    /// Native search activity, a query or a public source prevents replay.
    pub fn is_answer(&self) -> bool {
        self.anthropic_block.is_some()
            || !self.query.is_empty()
            || self.hits.iter().any(|hit| !hit.url.is_empty())
    }
}

/// Wire protocol that owns opaque reasoning. Ciphertext is not portable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReasoningProtocol {
    Anthropic,
    Responses,
}

/// A validated native opaque reasoning object. Its entire JSON value is kept,
/// including unknown fields; only the owning protocol may render it.
#[derive(Debug, Clone, PartialEq)]
pub struct SealedReasoning {
    protocol: ReasoningProtocol,
    value: Value,
}

impl SealedReasoning {
    /// Recognize only the protocol's opaque reasoning shape, never arbitrary
    /// content or another protocol's ciphertext.
    pub fn decode(protocol: ReasoningProtocol, value: &Value) -> Option<Self> {
        let valid = match protocol {
            ReasoningProtocol::Anthropic => {
                value["type"] == "redacted_thinking"
                    && value.get("data").is_some_and(Value::is_string)
            }
            ReasoningProtocol::Responses => {
                value["type"] == "reasoning"
                    && value
                        .get("encrypted_content")
                        .and_then(Value::as_str)
                        .is_some_and(|data| !data.is_empty())
            }
        };
        valid.then(|| Self {
            protocol,
            value: value.clone(),
        })
    }

    /// Get the original object only for its native wire protocol.
    pub fn native(&self, protocol: ReasoningProtocol) -> Option<&Value> {
        (self.protocol == protocol).then_some(&self.value)
    }
}

/// Signature provenance is distinct from public reasoning text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignatureProtocol {
    Anthropic,
    Gemini,
}

/// One block of a message. Fields are shared across kinds; `kind` decides
/// which of them carry meaning.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Part {
    /// Which block this is.
    pub kind: Option<PartKind>,
    /// Text, thinking, or a tool result's output.
    pub text: String,
    /// Image media type, e.g. `image/png`.
    pub media_type: String,
    /// Base64 image bytes, when inline.
    pub data: String,
    /// Image URL, when not inline.
    pub url: String,
    /// Tool call id.
    pub id: String,
    /// Tool name, flattened with `__` when namespaced.
    pub name: String,
    /// Original Responses namespace, when the call was namespaced.
    pub namespace: Option<String>,
    /// Tool call arguments, a JSON object when well-formed.
    pub args: Value,
    /// The call a tool result answers.
    pub call_id: String,
    /// Images returned by a tool beside its text; meaningful for ToolResult only.
    pub images: Vec<Part>,
    /// Whether the tool failed.
    pub is_error: bool,
    /// Thinking signature (Anthropic's, or a Gemini thought's).
    pub signature: String,
    /// Protocol that supplied this signature; None retains legacy IR construction.
    pub signature_protocol: Option<SignatureProtocol>,
    /// Native opaque reasoning; never serialized to a different protocol.
    pub sealed: Option<Box<SealedReasoning>>,
    /// An upstream web search, when the kind is Search.
    pub search: Option<Box<WebSearch>>,
}

impl Part {
    /// A search already performed by the upstream.
    pub fn web_search(search: WebSearch) -> Self {
        Self {
            kind: Some(PartKind::Search),
            search: Some(Box::new(search)),
            ..Default::default()
        }
    }

    /// A text block.
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            kind: Some(PartKind::Text),
            text: text.into(),
            ..Default::default()
        }
    }

    /// An inline image block.
    pub fn image(media_type: impl Into<String>, data: impl Into<String>) -> Self {
        Self {
            kind: Some(PartKind::Image),
            media_type: media_type.into(),
            data: data.into(),
            ..Default::default()
        }
    }

    /// An image block referenced by URL.
    pub fn image_url(url: impl Into<String>) -> Self {
        Self {
            kind: Some(PartKind::Image),
            url: url.into(),
            ..Default::default()
        }
    }

    /// A tool call block.
    pub fn tool_call(id: impl Into<String>, name: impl Into<String>, args: Value) -> Self {
        Self {
            kind: Some(PartKind::ToolCall),
            id: id.into(),
            name: name.into(),
            namespace: None,
            args,
            ..Default::default()
        }
    }

    /// A tool result block.
    pub fn tool_result(
        call_id: impl Into<String>,
        text: impl Into<String>,
        is_error: bool,
    ) -> Self {
        Self {
            kind: Some(PartKind::ToolResult),
            call_id: call_id.into(),
            text: text.into(),
            is_error,
            ..Default::default()
        }
    }

    /// A thinking block.
    pub fn thinking(text: impl Into<String>, signature: impl Into<String>) -> Self {
        Self {
            kind: Some(PartKind::Thinking),
            text: text.into(),
            signature: signature.into(),
            ..Default::default()
        }
    }

    /// Only emit a decoded signature back to the protocol that supplied it.
    pub fn native_signature(&self, protocol: SignatureProtocol) -> &str {
        if self
            .signature_protocol
            .is_none_or(|source| source == protocol)
        {
            &self.signature
        } else {
            ""
        }
    }

    /// The kind, for callers that know every part carries one.
    pub fn kind(&self) -> PartKind {
        self.kind.unwrap_or(PartKind::Text)
    }
}

/// One turn of a conversation.
#[derive(Debug, Clone)]
pub struct Message {
    /// Who wrote the turn.
    pub role: Role,
    /// Its blocks, in order.
    pub parts: Vec<Part>,
}

/// A tool the model may call.
#[derive(Debug, Clone)]
pub struct Tool {
    /// Tool name, flattened with `__` when it belongs to a namespace.
    pub name: String,
    /// Original Responses namespace, when the tool was namespaced.
    pub namespace: Option<String>,
    /// What it does.
    pub description: String,
    /// JSON Schema for its arguments.
    pub parameters: Value,
    /// A Responses free-form (custom) tool. Foreign dialects offer it as a
    /// function with one string `input` argument; its call is restored to
    /// `custom_tool_call` only for Responses clients.
    pub custom: bool,
}

/// Token accounting, as far as the protocols agree.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    /// Input (prompt) tokens.
    pub input: Option<u64>,
    /// Output (completion) tokens.
    pub output: Option<u64>,
    /// Tokens read from a prompt cache.
    pub cache_read: Option<u64>,
    /// Tokens written to a prompt cache.
    pub cache_write: Option<u64>,
}

impl Usage {
    /// Merge cumulative reports: later known counters replace earlier ones,
    /// while absent fields retain their last reported value.
    pub fn merge(&mut self, other: &Usage) {
        if other.input.is_some() {
            self.input = other.input;
        }
        if other.output.is_some() {
            self.output = other.output;
        }
        if other.cache_read.is_some() {
            self.cache_read = other.cache_read;
        }
        if other.cache_write.is_some() {
            self.cache_write = other.cache_write;
        }
    }
}

/// Why a message stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StopReason {
    /// The model finished its turn.
    #[default]
    EndTurn,
    /// The model asked tools to run.
    ToolUse,
    /// A token limit cut the message off.
    MaxTokens,
    /// A stop sequence ended it.
    StopSequence,
    /// The vendor safety/content filter ended the reply.
    ContentFilter,
    /// Anything else; treated as the turn ending.
    Other,
}

impl StopReason {
    /// The OpenAI Chat Completions spelling.
    pub fn to_chat(self) -> &'static str {
        match self {
            StopReason::EndTurn | StopReason::StopSequence | StopReason::Other => "stop",
            StopReason::ToolUse => "tool_calls",
            StopReason::MaxTokens => "length",
            StopReason::ContentFilter => "content_filter",
        }
    }

    /// Parse an OpenAI Chat Completions finish reason.
    pub fn from_chat(s: &str) -> StopReason {
        match s {
            "tool_calls" | "function_call" => StopReason::ToolUse,
            "length" => StopReason::MaxTokens,
            "content_filter" => StopReason::ContentFilter,
            _ => StopReason::EndTurn,
        }
    }

    /// The Anthropic Messages spelling.
    pub fn to_anthropic(self) -> &'static str {
        match self {
            StopReason::EndTurn | StopReason::Other => "end_turn",
            StopReason::ToolUse => "tool_use",
            StopReason::MaxTokens => "max_tokens",
            StopReason::StopSequence => "stop_sequence",
            StopReason::ContentFilter => "refusal",
        }
    }

    /// Parse an Anthropic Messages stop reason.
    pub fn from_anthropic(s: &str) -> StopReason {
        match s {
            "tool_use" => StopReason::ToolUse,
            "max_tokens" => StopReason::MaxTokens,
            "stop_sequence" => StopReason::StopSequence,
            "refusal" => StopReason::ContentFilter,
            _ => StopReason::EndTurn,
        }
    }
}

/// The dialect that supplied a tool-selection policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolChoiceProtocol {
    Chat,
    Responses,
    Anthropic,
    Gemini,
}

/// A completion request, decoded from any wire protocol.
#[derive(Debug, Clone, Default)]
pub struct CompletionRequest {
    /// The model the agent asked for, `provider/model` or bare.
    pub model: String,
    /// The system prompt, as blocks.
    pub system: Vec<Part>,
    /// The conversation so far.
    pub messages: Vec<Message>,
    /// Tools the model may call.
    pub tools: Vec<Tool>,
    /// Provider-executed search and its native configuration, not a function tool.
    pub web_search: Option<crate::search::WebSearchRequest>,
    /// Tool selection in the asking dialect; encoders normalize it and enforce allowlists.
    pub tool_choice: Option<Value>,
    /// Provenance for native selector preservation; None permits manually constructed IR.
    pub tool_choice_protocol: Option<ToolChoiceProtocol>,
    /// A token limit.
    pub max_tokens: Option<u64>,
    /// Sampling temperature.
    pub temperature: Option<f64>,
    /// Nucleus sampling.
    pub top_p: Option<f64>,
    /// Stop sequences.
    pub stop: Vec<String>,
    /// Whether the caller wants a stream.
    pub stream: bool,
}

/// A complete assistant reply.
#[derive(Debug, Clone)]
pub struct CompletionResponse {
    /// The model that answered (as the gateway names it).
    pub model: String,
    /// The assistant's blocks.
    pub message: Message,
    /// Why the message stopped.
    pub stop: StopReason,
    /// Token accounting.
    pub usage: Usage,
}

impl Default for CompletionResponse {
    fn default() -> Self {
        Self {
            model: String::new(),
            message: Message {
                role: Role::Assistant,
                parts: Vec::new(),
            },
            stop: StopReason::EndTurn,
            usage: Usage::default(),
        }
    }
}

/// Join an assistant message that only continues tool calls onto the
/// assistant message before it: Anthropic's Messages API takes one
/// assistant turn's parallel tool_use blocks as one message, while agents
/// on the OpenAI shape can send them split over two (magpie #1275).
pub fn join_split_calls(messages: &[Message]) -> Vec<Message> {
    let mut out: Vec<Message> = Vec::with_capacity(messages.len());
    for m in messages {
        let joins = match out.last() {
            Some(prev) => {
                prev.role == Role::Assistant
                    && m.role == Role::Assistant
                    && prev.parts.iter().any(|p| p.kind() == PartKind::ToolCall)
            }
            None => false,
        };
        if joins {
            let prev = out.last_mut().expect("checked non-empty");
            prev.parts.extend(m.parts.iter().cloned());
        } else {
            out.push(Message {
                role: m.role,
                parts: m.parts.clone(),
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_split_calls_merges_parallel_tool_turns() {
        let messages = vec![
            Message {
                role: Role::User,
                parts: vec![Part::text("hi")],
            },
            Message {
                role: Role::Assistant,
                parts: vec![Part::tool_call(
                    "a",
                    "search",
                    serde_json::json!({"q": "x"}),
                )],
            },
            Message {
                role: Role::Assistant,
                parts: vec![Part::tool_call(
                    "b",
                    "read",
                    serde_json::json!({"path": "/x"}),
                )],
            },
            Message {
                role: Role::User,
                parts: vec![
                    Part::tool_result("a", "found", false),
                    Part::tool_result("b", "data", false),
                ],
            },
        ];
        let joined = join_split_calls(&messages);
        assert_eq!(joined.len(), 3);
        assert_eq!(joined[1].parts.len(), 2);
    }
}
