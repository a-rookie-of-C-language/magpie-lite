//! Bounded state for repeated, cumulative and independent grounding updates.
//! Native metadata is not inferred to be a delta or a snapshot: each changed
//! object is forwarded intact. Foreign protocols see only new public items.
use std::collections::HashSet;

use serde_json::Value;

use crate::ir::WebSearch;
use crate::stream::{MAX_SSE_EVENT_BYTES, StreamEvent};

use super::{decode_search, get_either};

const MAX_PUBLIC_ITEMS: usize = 4096;

#[derive(Debug)]
pub(super) struct GroundingState {
    last: Option<Value>,
    queries: HashSet<String>,
    urls: HashSet<String>,
    public_bytes: usize,
    byte_limit: usize,
    item_limit: usize,
    failed: bool,
}

impl Default for GroundingState {
    fn default() -> Self {
        Self {
            last: None,
            queries: HashSet::new(),
            urls: HashSet::new(),
            public_bytes: 0,
            byte_limit: MAX_SSE_EVENT_BYTES,
            item_limit: MAX_PUBLIC_ITEMS,
            failed: false,
        }
    }
}

impl GroundingState {
    pub(super) fn failed(&self) -> bool {
        self.failed
    }

    fn fail(&mut self) -> StreamEvent {
        self.last = None;
        self.queries.clear();
        self.urls.clear();
        self.public_bytes = 0;
        self.failed = true;
        StreamEvent::Error {
            message: "upstream grounding state exceeds its budget".into(),
            code: None,
        }
    }

    pub(super) fn observe(&mut self, candidate: &Value) -> Option<StreamEvent> {
        if self.failed {
            return None;
        }
        let metadata = get_either(candidate, "groundingMetadata", "grounding_metadata")?;
        if metadata.as_object().is_none_or(|object| object.is_empty())
            || self.last.as_ref() == Some(metadata)
        {
            return None;
        }
        let mut incoming_queries = HashSet::new();
        let new_queries: Vec<_> = get_either(metadata, "webSearchQueries", "web_search_queries")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .filter(|query| !query.is_empty() && !self.queries.contains(*query))
            .filter(|query| incoming_queries.insert(*query))
            .collect();
        let mut search = decode_search(candidate).unwrap_or_else(|| WebSearch {
            gemini_grounding: Some(metadata.clone()),
            ..Default::default()
        });
        let mut incoming_urls = HashSet::new();
        search
            .hits
            .retain(|hit| !self.urls.contains(&hit.url) && incoming_urls.insert(hit.url.clone()));
        let new_bytes = new_queries
            .iter()
            .map(|query| query.len())
            .sum::<usize>()
            .saturating_add(search.hits.iter().map(|hit| hit.url.len()).sum::<usize>());
        let public_bytes = self.public_bytes.saturating_add(new_bytes);
        let items = self
            .queries
            .len()
            .saturating_add(self.urls.len())
            .saturating_add(new_queries.len())
            .saturating_add(search.hits.len());
        if items > self.item_limit
            || public_bytes.saturating_add(metadata.to_string().len()) > self.byte_limit
        {
            return Some(self.fail());
        }
        let first_public = self.queries.is_empty()
            && self.urls.is_empty()
            && (!new_queries.is_empty() || !search.hits.is_empty());
        search.query = new_queries.join("; ");
        self.queries
            .extend(new_queries.into_iter().map(str::to_owned));
        self.urls
            .extend(search.hits.iter().map(|hit| hit.url.clone()));
        self.public_bytes = public_bytes;
        self.last = Some(metadata.clone());
        let search = Box::new(search);
        Some(if first_public {
            StreamEvent::Search { search }
        } else {
            StreamEvent::SearchUpdate { search }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn candidate(metadata: Value) -> Value {
        json!({"groundingMetadata": metadata})
    }
    fn search() -> Value {
        json!({"webSearchQueries":["private query"],
            "groundingChunks":[{"web":{"uri":"https://example.invalid/private"}}]})
    }
    fn assert_failed(state: &GroundingState, event: Option<StreamEvent>) {
        assert!(matches!(event, Some(StreamEvent::Error { ref message, .. })
            if message == "upstream grounding state exceeds its budget"));
        assert!(state.failed());
        assert!(state.last.is_none());
        assert!(state.queries.is_empty());
        assert!(state.urls.is_empty());
        assert_eq!(state.public_bytes, 0);
    }

    #[test]
    fn exact_retained_budget_and_one_byte_overflow_fail_without_private_data() {
        let metadata = search();
        let size = metadata.to_string().len()
            + "private query".len()
            + "https://example.invalid/private".len();
        let mut state = GroundingState {
            byte_limit: size,
            ..Default::default()
        };
        let input = candidate(metadata.clone());
        assert!(state.observe(&input).unwrap().is_answer());
        assert!(state.observe(&input).is_none());
        let mut updated = metadata;
        updated["extra"] = json!("x");
        let event = state.observe(&candidate(updated));
        assert_failed(&state, event);
        assert!(state.observe(&input).is_none());

        let mut state = GroundingState {
            byte_limit: size - 1,
            ..Default::default()
        };
        let event = state.observe(&input);
        assert_failed(&state, event);
    }

    #[test]
    fn item_limit_counts_unique_queries_and_urls_across_updates() {
        let mut state = GroundingState::default();
        let queries: Vec<_> = (0..MAX_PUBLIC_ITEMS)
            .map(|i| format!("query {i}"))
            .collect();
        let input = candidate(json!({"webSearchQueries":queries}));
        assert!(state.observe(&input).unwrap().is_answer());
        assert_eq!(state.queries.len(), MAX_PUBLIC_ITEMS);
        assert!(state.observe(&input).is_none());
        let event = state.observe(&candidate(json!({"groundingChunks":[
            {"web":{"uri":"https://example.invalid/overflow"}}]})));
        assert_failed(&state, event);
    }

    #[test]
    fn native_updates_replace_retained_metadata_and_do_not_retain_every_version() {
        let mut state = GroundingState {
            byte_limit: 512,
            item_limit: 2,
            ..Default::default()
        };
        assert!(state.observe(&candidate(search())).unwrap().is_answer());
        let public_bytes = state.public_bytes;
        for version in 0..256 {
            let mut metadata = search();
            metadata["groundingSupports"] = json!([{"version":version}]);
            let event = state.observe(&candidate(metadata.clone())).unwrap();
            assert!(matches!(event, StreamEvent::SearchUpdate { .. }));
            assert!(!event.is_content());
            assert_eq!(state.last, Some(metadata));
            assert_eq!(state.public_bytes, public_bytes);
            assert_eq!(state.queries.len() + state.urls.len(), 2);
        }
    }

    #[test]
    fn attribution_before_first_public_search_does_not_consume_or_commit_it() {
        let mut state = GroundingState::default();
        for metadata in [json!({}), json!([]), Value::Null] {
            assert!(state.observe(&candidate(metadata)).is_none());
        }
        let metadata = json!({"searchEntryPoint":{"renderedContent":"private attribution"}});
        let event = state.observe(&candidate(metadata)).unwrap();
        assert!(matches!(event, StreamEvent::SearchUpdate { .. }));
        assert!(!event.is_answer());
        let input = candidate(search());
        assert!(matches!(
            state.observe(&input),
            Some(StreamEvent::Search { .. })
        ));
    }

    #[test]
    fn grounding_overflow_is_terminal_even_with_finish_in_the_same_frame() {
        let mut decoder = super::super::GeminiStreamDecoder {
            grounding: GroundingState {
                byte_limit: 1,
                ..Default::default()
            },
            ..Default::default()
        };
        let frame = json!({"candidates":[{"groundingMetadata":search(),"finishReason":"STOP"}]});
        let events = decoder.decode(&frame);
        assert!(matches!(events.as_slice(), [StreamEvent::Error { .. }]));
        assert!(decoder.decode(&frame).is_empty());
    }
}
