//! Gemini's successful wire completion is not always an answer. Keep this
//! classification separate from transport/vendor errors and client dialects.
use serde_json::Value;

pub(crate) const EMPTY_REPLY: &str = "an empty reply (the model answered nothing; try again)";

#[derive(Default)]
pub(crate) struct GeminiReply {
    answered: bool,
    exempt: bool,
}

impl GeminiReply {
    pub(crate) fn observe(&mut self, body: &Value) {
        if body
            .pointer("/promptFeedback/blockReason")
            .or_else(|| body.pointer("/prompt_feedback/block_reason"))
            .and_then(Value::as_str)
            .is_some_and(|reason| !matches!(reason, "" | "BLOCK_REASON_UNSPECIFIED"))
        {
            self.exempt = true;
        }
        let Some(candidate) = body
            .get("candidates")
            .and_then(Value::as_array)
            .and_then(|a| a.first())
        else {
            return;
        };
        if let Some(reason) = candidate
            .get("finishReason")
            .or_else(|| candidate.get("finish_reason"))
            .and_then(Value::as_str)
        {
            // Reference stopFromGemini: all other reasons describe a filter,
            // including image-specific filters and future vendor reasons.
            self.exempt |= !matches!(
                reason.to_ascii_uppercase().as_str(),
                "" | "STOP"
                    | "FINISH_REASON_UNSPECIFIED"
                    | "OTHER"
                    | "MALFORMED_FUNCTION_CALL"
                    | "UNEXPECTED_TOOL_CALL"
            );
        }
        if let Some(parts) = candidate
            .pointer("/content/parts")
            .and_then(Value::as_array)
        {
            self.answered |= parts.iter().any(|p| {
                if p.get("functionCall")
                    .or_else(|| p.get("function_call"))
                    .is_some()
                {
                    return true;
                }
                if p.get("thought").and_then(Value::as_bool).unwrap_or(false) {
                    return false;
                }
                p.get("text")
                    .and_then(Value::as_str)
                    .is_some_and(|s| !s.is_empty())
                    || p.pointer("/inlineData/data")
                        .or_else(|| p.pointer("/inline_data/data"))
                        .and_then(Value::as_str)
                        .is_some_and(|s| !s.is_empty())
                    || p.pointer("/fileData/fileUri")
                        .or_else(|| p.pointer("/file_data/file_uri"))
                        .and_then(Value::as_str)
                        .is_some_and(|s| !s.is_empty())
            });
        }
        if let Some(g) = candidate
            .get("groundingMetadata")
            .or_else(|| candidate.get("grounding_metadata"))
        {
            self.answered |= g
                .get("webSearchQueries")
                .or_else(|| g.get("web_search_queries"))
                .and_then(Value::as_array)
                .is_some_and(|a| a.iter().any(|q| q.as_str().is_some_and(|s| !s.is_empty())))
                || g.get("groundingChunks")
                    .or_else(|| g.get("grounding_chunks"))
                    .and_then(Value::as_array)
                    .is_some_and(|a| {
                        a.iter().any(|c| {
                            c.pointer("/web/uri")
                                .and_then(Value::as_str)
                                .is_some_and(|s| !s.is_empty())
                        })
                    });
        }
    }

    pub(crate) fn has_answer(&self) -> bool {
        self.answered
    }

    pub(crate) fn is_empty(&self) -> bool {
        !self.answered && !self.exempt
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn empty_stop_and_reasoning_are_not_answers() {
        let mut reply = GeminiReply::default();
        reply.observe(&json!({"usageMetadata":{"promptTokenCount":7}}));
        reply.observe(
            &json!({"candidates":[{"content":{"parts":[{"text":"thinking", "thought":true}]}}]}),
        );
        reply.observe(
            &json!({"candidates":[{"content":{"parts":[{"text":""}]},"finishReason":"STOP"}]}),
        );
        assert!(reply.is_empty());
    }

    #[test]
    fn text_tool_image_and_search_are_answers() {
        for body in [
            json!({"candidates":[{"content":{"parts":[{"text":" "}]}}]}),
            json!({"candidates":[{"content":{"parts":[{"functionCall":{"name":"f"}}]}}]}),
            json!({"candidates":[{"content":{"parts":[{"inlineData":{"data":"aGk="}}]}}]}),
            json!({"candidates":[{"content":{"parts":[{"file_data":{"file_uri":"gs://image"}}]}}]}),
            json!({"candidates":[{"groundingMetadata":{"webSearchQueries":["rust"]}}]}),
            json!({"candidates":[{"grounding_metadata":{"grounding_chunks":[{"web":{"uri":"https://example.test"}}]}}]}),
        ] {
            let mut reply = GeminiReply::default();
            reply.observe(&body);
            reply.observe(&json!({"candidates":[{"finishReason":"STOP"}]}));
            assert!(!reply.is_empty(), "{body}");
        }
    }

    #[test]
    fn draft_images_and_empty_search_do_not_count() {
        let mut reply = GeminiReply::default();
        reply.observe(&json!({"candidates":[{"content":{"parts":[{"thought":true,"inlineData":{"data":"aGk="}},{"inlineData":{"data":""}}]},"groundingMetadata":{"webSearchQueries":[],"groundingChunks":[{"web":{"uri":""}}]}}]}));
        assert!(reply.is_empty());
    }

    #[test]
    fn malformed_search_queries_do_not_count_as_answers() {
        for queries in [json!(["", null, 7, {}]), json!(null), json!("rust")] {
            for key in ["webSearchQueries", "web_search_queries"] {
                let mut reply = GeminiReply::default();
                reply.observe(&json!({"candidates":[{"groundingMetadata":{key:queries},"finishReason":"STOP"}]}));
                assert!(reply.is_empty(), "{key}: {queries}");
            }
        }
    }

    #[test]
    fn length_filters_and_prompt_refusals_are_not_retried() {
        for reason in [
            "MAX_TOKENS",
            "SAFETY",
            "RECITATION",
            "BLOCKLIST",
            "PROHIBITED_CONTENT",
            "SPII",
            "IMAGE_SAFETY",
            "NEW_FILTER",
        ] {
            let mut reply = GeminiReply::default();
            reply.observe(&json!({"candidates":[{"finishReason":reason}]}));
            assert!(!reply.is_empty(), "{reason}");
        }
        let mut reply = GeminiReply::default();
        reply.observe(&json!({"promptFeedback":{"blockReason":"SAFETY"}}));
        assert!(!reply.is_empty());
    }
}
