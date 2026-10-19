//! The error a streamed reply ended with, said in the client's own words.
//!
//! An agent that reads an upstream error's `type`/`code` compacts on an
//! overflow and retries once. A translated stream's error event carries
//! the vendor's code, so a client that checks it isn't left with a
//! generic `upstream_error` (the reference's `writeErrorSaid` says an
//! overflow as a 400 `context_length_exceeded`; `97f264c7` is the same
//! rule for pi).

/// Whether a vendor's error text says the conversation no longer fits
/// its model's context, as `97f264c7`'s `tooLongRe` reads it. A
/// `max_tokens` refusal is about the reply's allowance, not the
/// conversation, so it is left alone.
pub fn overflow_words(message: &str) -> bool {
    let text = message.to_lowercase();
    if ["max_tokens", "max_output_tokens", "max_completion_tokens"]
        .iter()
        .any(|word| text.contains(word))
    {
        return false;
    }
    [
        "context_length_exceeded",
        "prompt is too long",
        "input is too long",
        "maximum context length",
        "too many input tokens",
        "too many prompt tokens",
        "maximum prompt length is",
        "reduce the length of the messages",
        "maximum allowed input length",
        "longer than the model's context length",
        "available context size",
        "greater than the context length",
        "prompt token count of",
        "context window exceeds limit",
        "exceeded model token limit",
        "configured context size",
        "prompt too long",
        "prompt exceeds max length",
        "context_window_exceeded",
        "range of input length should be",
        "token limit exceeded",
        "input token exceed the limit",
        "上下文",
    ]
    .iter()
    .any(|phrase| text.contains(phrase))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overflow_wordings_are_read() {
        for message in [
            "context_length_exceeded",
            "This model's maximum context length is 8192 tokens",
            "prompt is too long: 207000 tokens > 131072 maximum",
            "Input token exceed the limit (request id: 1)",
            "上下文长度超出限制",
        ] {
            assert!(overflow_words(message), "{message}");
        }
        assert!(!overflow_words("max_tokens is too large for this model"));
        assert!(!overflow_words("rate limit reached"));
    }
}
