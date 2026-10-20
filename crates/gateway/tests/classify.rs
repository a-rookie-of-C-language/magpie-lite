//! A group's classifier: which intent a turn's message is, asked once as
//! the turn begins through lite itself. When it cannot say, no intent
//! matches; when it fails or rests on a turn that had one, that intent is
//! kept. Ports the reference's `classify`/`classifyBody`/`readIntent`
//! tests and the intent e2e, without decision providers, effort, or the
//! trace-only fields lite doesn't keep.

use std::collections::HashMap;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use magpie_core::ir::{CompletionRequest, Message, Part, PartKind, Role};
use magpie_gateway::classify::{
    AskError, AskFn, AskFuture, Before, Verdict, classifier_failure, classify, classify_body,
    cut_middle, effort_body, read_effort, read_intent, turn_before, user_said, user_text,
    user_words,
};
use magpie_gateway::rules::{
    Rule, RuleRequest, fit_level, intents, keeps_effort, rule_for, turn_intent,
};

fn text(s: &str) -> Part {
    Part {
        kind: Some(PartKind::Text),
        text: s.into(),
        ..Default::default()
    }
}

fn user_msg(texts: &[&str]) -> Message {
    Message {
        role: Role::User,
        parts: texts.iter().map(|t| text(t)).collect(),
    }
}

fn asst_msg(texts: &[&str]) -> Message {
    Message {
        role: Role::Assistant,
        parts: texts.iter().map(|t| text(t)).collect(),
    }
}

fn req_with(messages: Vec<Message>) -> CompletionRequest {
    CompletionRequest {
        messages,
        ..Default::default()
    }
}

// ---- read_intent ------------------------------------------------------------

#[test]
fn read_intent_names_the_numbered_kind() {
    let intents = vec!["a quick question".to_owned(), "debugging".to_owned()];
    assert_eq!(
        read_intent("1", &intents),
        Ok("a quick question".to_owned())
    );
    assert_eq!(read_intent("2", &intents), Ok("debugging".to_owned()));
    assert_eq!(read_intent("0", &intents), Ok(String::new()));
    // words around the number are read too
    assert_eq!(
        read_intent("Kind 1.", &intents),
        Ok("a quick question".to_owned())
    );
    // out of range, or no number at all, is the classifier not answering
    let err = read_intent("3", &intents).unwrap_err();
    assert!(
        err.contains("not a number from 0 to 2"),
        "unexpected error: {err}"
    );
    let err = read_intent("no idea", &intents).unwrap_err();
    assert!(
        err.contains("not a number from 0 to 2"),
        "unexpected error: {err}"
    );
}

// ---- classify_body ----------------------------------------------------------

#[test]
fn classify_body_asks_for_the_number_only() {
    let intents = vec!["a quick question".to_owned(), "debugging".to_owned()];
    let prev = Before {
        intent: "debugging".to_owned(),
        effort: String::new(),
        user: "why does this panic".to_owned(),
        reply: "because".to_owned(),
    };
    let body = classify_body("c/judge", &intents, &prev, "and now this");
    assert_eq!(body["model"], "c/judge");
    assert_eq!(body["stream"], false);
    assert_eq!(body["temperature"], 0);
    let words = body["messages"][1]["content"].as_str().unwrap();
    assert!(
        words.contains("1. a quick question\n2. debugging\n"),
        "{words}"
    );
    // the turn before's kind: a follow-up carrying on is of its kind too
    assert!(words.contains("was of kind 2"), "{words}");
    // what was said before it goes with the message
    assert!(words.contains("<previous_user_message>"), "{words}");
    assert!(words.contains("<previous_assistant_reply>"), "{words}");
    assert!(
        words.contains("<message>\nand now this\n</message>"),
        "{words}"
    );
    assert!(
        words.ends_with("The number of the kind that fits it best (0 only if none does):"),
        "{words}"
    );
}

#[test]
fn classify_body_says_nothing_of_a_turn_before_it_has_none_of() {
    let intents = vec!["debugging".to_owned()];
    let body = classify_body("c/judge", &intents, &Before::default(), "hi");
    let words = body["messages"][1]["content"].as_str().unwrap();
    assert!(!words.contains("was of kind"), "{words}");
    assert!(!words.contains("<previous_user_message>"), "{words}");
}

// ---- cut_middle -------------------------------------------------------------

#[test]
fn cut_middle_leaves_the_ends_when_long() {
    assert_eq!(cut_middle("short", 3, 3), "short");
    assert_eq!(cut_middle("abcdef", 2, 2), "ab\n…\nef");
}

// ---- user_words -------------------------------------------------------------

const TASK_NOTIFICATION: &str = "<task-notification>\n idioma \n</task-notification>";
const TODO_NUDGE: &str = "The TodoWrite tool hasn't been used recently. Only use it if it's relevant to the current work.";
const AGENT_LIST: &str =
    "Available agent types for the Agent tool:\n- general-purpose: General-purpose agent.";
const ZCODE_AGENT_LIST: &str = "Available agent types and the tools they have access to:\n- general-purpose: General-purpose agent (Tools: *)";
const PLUGIN_REF: &str = "<plugin_reference>\nThe user referenced the following Plugins for this turn.\n</plugin_reference>";
const GOAL_CONTINUATION: &str = "<system-reminder source=\"goal-continuation\">\nKeep working toward the goal.\n</system-reminder>";
const ENV_CONTEXT: &str =
    "<environment_context>\n  <cwd>/Users/me/app</cwd>\n</environment_context>";
const CODEX_AGENTS: &str =
    "# AGENTS.md instructions for /Users/me/app\n\n<INSTRUCTIONS>\nRun the tests.\n</INSTRUCTIONS>";
const IN_APP_BROWSER: &str = "# Context from my IDE setup:\n\n# In app browser:\n<in-app-browser-context source=\"ambient-ui-state\">\nhttps://example.test/\n</in-app-browser-context>\n\n";

#[test]
fn user_words_skips_what_the_agent_put_in() {
    // a task notice, a todo nudge, agent lists and plugin metadata hold no words
    let wrapped = format!("<system-reminder>\n{TODO_NUDGE}\n</system-reminder>");
    for agent in [
        TASK_NOTIFICATION,
        TODO_NUDGE,
        AGENT_LIST,
        ZCODE_AGENT_LIST,
        PLUGIN_REF,
        ENV_CONTEXT,
        CODEX_AGENTS,
        wrapped.as_str(),
    ] {
        assert_eq!(user_words(agent), "", "for {agent:?}");
    }
    // the Codex app's context tail: only the request counts
    assert_eq!(
        user_words(&format!(
            "{IN_APP_BROWSER}## My request for Codex:\nwhat is this page?"
        )),
        "what is this page?"
    );
    // a reminder with a source is stripped, the words kept
    assert_eq!(
        user_words(&format!("{GOAL_CONTINUATION}\nship it")),
        "ship it"
    );
    // plain words pass through
    assert_eq!(user_words("  fix the bug  "), "fix the bug");
}

#[test]
fn user_said_joins_the_words_of_a_message() {
    let parts = vec![text("first"), text(TASK_NOTIFICATION), text("second")];
    assert_eq!(user_said(&parts), "first\nsecond");
    assert_eq!(user_said(&[text(TASK_NOTIFICATION)]), "");
}

// ---- user_text / turn_before ------------------------------------------------

#[test]
fn user_text_is_the_last_user_message_with_words() {
    let q = "what does this plugin do";
    let req = req_with(vec![
        user_msg(&[q]),
        user_msg(&[PLUGIN_REF]),
        user_msg(&[AGENT_LIST]),
    ]);
    assert_eq!(user_text(&req), q);
    // in one message the agent's additions fall away too
    let req = req_with(vec![user_msg(&[q, PLUGIN_REF, ZCODE_AGENT_LIST])]);
    assert_eq!(user_text(&req), q);
    // Codex's context heads the words, the app's context tails them
    let req = req_with(vec![user_msg(&[CODEX_AGENTS, ENV_CONTEXT, "fix the bug"])]);
    assert_eq!(user_text(&req), "fix the bug");
}

#[test]
fn user_text_is_empty_when_nothing_was_said() {
    let req = req_with(vec![user_msg(&[ENV_CONTEXT])]);
    assert_eq!(user_text(&req), "");
    let image = Part {
        kind: Some(PartKind::Image),
        ..Default::default()
    };
    let req = req_with(vec![Message {
        role: Role::User,
        parts: vec![image],
    }]);
    assert_eq!(user_text(&req), "");
}

#[test]
fn turn_before_carries_what_the_follow_up_follows() {
    let req = req_with(vec![
        user_msg(&["first question"]),
        asst_msg(&["first answer"]),
        user_msg(&["go on"]),
    ]);
    assert_eq!(
        turn_before(&req),
        ("first question".to_owned(), "first answer".to_owned())
    );
}

#[test]
fn turn_before_without_words_is_nothing() {
    // a reply of only tool calls is none
    let call = Part {
        kind: Some(PartKind::ToolCall),
        id: "t1".into(),
        name: "Bash".into(),
        ..Default::default()
    };
    let req = req_with(vec![
        user_msg(&["run it"]),
        Message {
            role: Role::Assistant,
            parts: vec![call],
        },
        user_msg(&["go on"]),
    ]);
    assert_eq!(turn_before(&req), ("run it".to_owned(), String::new()));
    // nothing said before the message
    let req = req_with(vec![user_msg(&["hi"])]);
    assert_eq!(turn_before(&req), (String::new(), String::new()));
}

// ---- classify: cache, rest, odd answers -------------------------------------

fn counting_ask(calls: &Arc<AtomicUsize>, out: Result<Verdict, AskError>) -> AskFn {
    let calls = calls.clone();
    Box::new(
        move |_m: String, _i: Vec<String>, _p: Before, _e: bool, _t: String| {
            let calls = calls.clone();
            let out = match &out {
                Ok(v) => Ok(Verdict {
                    intent: v.intent.clone(),
                    effort: v.effort.clone(),
                    score: v.score,
                }),
                Err(AskError::Failed(e)) => Err(AskError::Failed(e.clone())),
                Err(AskError::Answered(e)) => Err(AskError::Answered(e.clone())),
            };
            Box::pin(async move {
                calls.fetch_add(1, Ordering::SeqCst);
                out
            }) as AskFuture
        },
    )
}

fn intents_of(names: &[&str]) -> Vec<String> {
    names.iter().map(|s| s.to_string()).collect()
}

#[tokio::test]
async fn classify_asks_once_for_the_same_message() {
    let calls = Arc::new(AtomicUsize::new(0));
    let ask = counting_ask(
        &calls,
        Ok(Verdict {
            intent: "bug".into(),
            effort: String::new(),
            score: 0.0,
        }),
    );
    let intents = intents_of(&["bug", "refactor"]);
    let (said, cached, err) = classify(
        ask,
        "cache-model-asks-once",
        &intents,
        &Before::default(),
        false,
        "it panics",
    )
    .await;
    assert_eq!((said.intent.as_str(), cached, err), ("bug", false, None));
    let ask = counting_ask(
        &calls,
        Ok(Verdict {
            intent: "bug".into(),
            effort: String::new(),
            score: 0.0,
        }),
    );
    let (said, cached, _) = classify(
        ask,
        "cache-model-asks-once",
        &intents,
        &Before::default(),
        false,
        "it panics",
    )
    .await;
    assert_eq!((said.intent.as_str(), cached), ("bug", true));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    // a failed answer is told, not cached
    assert!(classifier_failure("cache-model-asks-once").is_none());
}

#[tokio::test]
async fn classify_rests_a_failed_classifier() {
    let calls = Arc::new(AtomicUsize::new(0));
    let intents = intents_of(&["bug"]);
    let (said, cached, err) = classify(
        counting_ask(&calls, Err(AskError::Failed("classifier down".into()))),
        "rest-model-rests",
        &intents,
        &Before::default(),
        false,
        "rename this",
    )
    .await;
    assert_eq!(said.intent, "");
    assert!(!cached);
    assert!(err.unwrap().contains("classifier down"));
    // still within its rest: not asked again
    let (said, _, err) = classify(
        counting_ask(&calls, Err(AskError::Failed("classifier down".into()))),
        "rest-model-rests",
        &intents,
        &Before::default(),
        false,
        "rename that",
    )
    .await;
    assert_eq!(said.intent, "");
    assert!(err.unwrap().contains("not asked again"));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    // told on the group's card until it answers
    let (why, _) = classifier_failure("rest-model-rests").unwrap();
    assert!(why.contains("classifier down"), "{why}");
}

#[tokio::test]
async fn classify_asks_again_after_an_answer_that_is_no_number() {
    let calls = Arc::new(AtomicUsize::new(0));
    let intents = intents_of(&["bug"]);
    for _ in 0..2 {
        let (said, cached, err) = classify(
            counting_ask(&calls, Err(AskError::Answered("maybe?".into()))),
            "odd-model-answers-oddly",
            &intents,
            &Before::default(),
            false,
            "rename this",
        )
        .await;
        assert_eq!(said.intent, "");
        assert!(!cached);
        // the odd answer itself is the error; the point is it rests nothing
        assert_eq!(err.unwrap(), "maybe?");
    }
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

// ---- intents ----------------------------------------------------------------

fn intent_rule(use_: &str, intent: &str, members: &[String]) -> Rule {
    let mut rule = Rule {
        use_: use_.into(),
        intent: intent.into(),
        ..Default::default()
    };
    rule.clean(members, 1).unwrap();
    rule
}

#[test]
fn intents_come_from_the_rules_that_could_match_first() {
    let members = vec!["a/one".to_owned(), "b/two".to_owned()];
    let mut plain = Rule {
        use_: "a/one".into(),
        tokens: 1,
        ..Default::default()
    };
    plain.clean(&members, 4).unwrap();
    let rules = vec![
        intent_rule("b/two", "bug", &members),
        intent_rule("a/one", "Bug", &members),
        intent_rule("b/two", "refactor", &members),
        plain,
        intent_rule("a/one", "other", &members),
    ];
    // a tokens rule with no intent ends the ask: "other" is never asked
    let q = RuleRequest {
        tokens: 200,
        ..Default::default()
    };
    assert_eq!(intents(&rules, &q), vec!["bug", "refactor"]);
}

// ---- rule_for keeps the turn's intent ----------------------------------------

#[test]
fn a_turn_keeps_the_intent_it_began_with() {
    let members = vec!["a/one".to_owned(), "b/two".to_owned()];
    let rules = vec![
        intent_rule("b/two", "bug", &members),
        intent_rule("a/one", "bug", &members),
    ];
    let windows = HashMap::new();
    let started = req_with(vec![user_msg(&["it panics"])]);
    let hit = rule_for(
        "keep-g", &rules, &started, "codex", "keep-c", &windows, "bug", "",
    )
    .unwrap();
    assert_eq!(hit.use_, "b/two");
    assert_eq!(turn_intent("keep-g", "keep-c", &started), "bug");
    // within the turn the stored intent goes by, whatever is passed now:
    // the rules after the first that match it too say who comes next
    let result = Part {
        kind: Some(PartKind::ToolResult),
        call_id: "t1".into(),
        text: "done".into(),
        ..Default::default()
    };
    let call = Part {
        kind: Some(PartKind::ToolCall),
        id: "t1".into(),
        name: "Bash".into(),
        ..Default::default()
    };
    let within = req_with(vec![
        user_msg(&["it panics"]),
        Message {
            role: Role::Assistant,
            parts: vec![call],
        },
        Message {
            role: Role::User,
            parts: vec![result],
        },
    ]);
    let hit = rule_for(
        "keep-g", &rules, &within, "codex", "keep-c", &windows, "", "",
    )
    .unwrap();
    assert!(hit.held);
    assert_eq!(hit.use_, "b/two");
    assert_eq!(hit.then, vec!["a/one".to_owned()]);
}

// ---- through the gateway ----------------------------------------------------

use axum::{Json, Router, extract::State, routing::post};
use magpie_gateway::{
    AppState, Config, ConfigStore, GroupConfig, Protocol, ProviderConfig, RoutingPolicy,
    build_router,
};
use reqwest::{Client, StatusCode};
use serde_json::{Value, json};

struct ClsAnswer {
    status: u16,
    text: String,
}

struct Gateway {
    url: String,
    bodies: Vec<Arc<Mutex<Vec<Value>>>>,
    cls_bodies: Arc<Mutex<Vec<Value>>>,
    cls: Arc<Mutex<ClsAnswer>>,
    _tasks: Vec<tokio::task::JoinHandle<()>>,
}

async fn bind(router: Router) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (url, task)
}

async fn mock(bodies: Arc<Mutex<Vec<Value>>>) -> (String, tokio::task::JoinHandle<()>) {
    async fn answer(
        State(bodies): State<Arc<Mutex<Vec<Value>>>>,
        Json(body): Json<Value>,
    ) -> Json<Value> {
        bodies.lock().unwrap().push(body);
        Json(json!({
            "id": "mock", "object": "chat.completion", "model": "mock",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 2, "total_tokens": 12}
        }))
    }
    bind(
        Router::new()
            .route("/v1/chat/completions", post(answer))
            .with_state(bodies),
    )
    .await
}

async fn mock_classifier(
    bodies: Arc<Mutex<Vec<Value>>>,
    answer: Arc<Mutex<ClsAnswer>>,
) -> (String, tokio::task::JoinHandle<()>) {
    type Shared = (Arc<Mutex<Vec<Value>>>, Arc<Mutex<ClsAnswer>>);
    async fn judge(
        State((bodies, answer)): State<Shared>,
        Json(body): Json<Value>,
    ) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
        bodies.lock().unwrap().push(body);
        let answer = answer.lock().unwrap();
        if answer.status != 200 {
            return Err((
                StatusCode::from_u16(answer.status).unwrap(),
                Json(json!({"error": {"message": "classifier down", "type": "server_error"}})),
            ));
        }
        Ok(Json(json!({
            "id": "mock", "object": "chat.completion", "model": "mock",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": answer.text}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 2, "total_tokens": 12}
        })))
    }
    bind(
        Router::new()
            .route("/v1/chat/completions", post(judge))
            .with_state((bodies, answer)),
    )
    .await
}

fn provider(id: &str, upstream: &str, model: &str) -> ProviderConfig {
    ProviderConfig {
        id: id.into(),
        name: id.into(),
        protocol: Protocol::Chat,
        base_url: format!("{upstream}/v1"),
        api_key: "k".into(),
        api_keys: Vec::new(),
        models: vec![model.into()],
        contexts: Default::default(),
        outputs: Default::default(),
        fallback: Vec::new(),
        key_cooldown_ms: None,
        capabilities: Default::default(),
        routing: RoutingPolicy::Order,
        weights: Vec::new(),
        affinity: Default::default(),
        max_concurrency: None,
        max_rpm: None,
        queue_limit: None,
        queue_wait_s: None,
        key_generation: 0,
        key_limits: Vec::new(),
    }
}

async fn gateway(
    group: &str,
    judge: &str,
    rules: Vec<Rule>,
    effort: &str,
    cls: Arc<Mutex<ClsAnswer>>,
) -> Gateway {
    gateway_with_routes(
        group,
        judge,
        rules,
        effort,
        vec!["a/one".into(), "b/two".into()],
        cls,
    )
    .await
}

async fn gateway_with_routes(
    group: &str,
    judge: &str,
    rules: Vec<Rule>,
    effort: &str,
    routes: Vec<String>,
    cls: Arc<Mutex<ClsAnswer>>,
) -> Gateway {
    gateway_with_levels(group, judge, rules, effort, routes, Vec::new(), cls).await
}

async fn gateway_with_levels(
    group: &str,
    judge: &str,
    rules: Vec<Rule>,
    effort: &str,
    routes: Vec<String>,
    levels: Vec<String>,
    cls: Arc<Mutex<ClsAnswer>>,
) -> Gateway {
    let a_bodies = Arc::new(Mutex::new(Vec::new()));
    let b_bodies = Arc::new(Mutex::new(Vec::new()));
    let cls_bodies = Arc::new(Mutex::new(Vec::new()));
    let (a, ta) = mock(a_bodies.clone()).await;
    let (b, tb) = mock(b_bodies.clone()).await;
    let (c, tc) = mock_classifier(cls_bodies.clone(), cls.clone()).await;
    let config = Config {
        provider: vec![
            provider("a", &a, "one"),
            provider("b", &b, "two"),
            provider("c", &c, judge),
        ],
        group: vec![GroupConfig {
            id: group.into(),
            name: group.into(),
            routes,
            rules,
            classifier: format!("c/{judge}"),
            effort: effort.into(),
            levels,
            pick: String::new(),
            auto: false,
            weights: Vec::new(),
            routing: RoutingPolicy::Order,
            first_token_timeout_ms: 0,
            affinity: Default::default(),
            patience_s: None,
        }],
        ..Config::default()
    };
    let state = Arc::new(AppState {
        agents: None,
        config: ConfigStore::new(config, None),
        http: magpie_gateway::upstream::http_client().unwrap(),
        requests: Arc::new(Mutex::new(magpie_gateway::usage::RequestLog::default())),
        key_rotation: magpie_gateway::KeyRotator::default(),
        affinity: magpie_gateway::AffinityStore::default(),
        lanes: magpie_gateway::lanes::Lanes::default(),
        rpms: magpie_gateway::lanes::Rpms::default(),
    });
    let (url, tg) = bind(build_router(state)).await;
    Gateway {
        url,
        bodies: vec![a_bodies, b_bodies],
        cls_bodies,
        cls,
        _tasks: vec![ta, tb, tc, tg],
    }
}

fn intent_group_rule(members: &[String]) -> Rule {
    intent_rule("b/two", "refactoring", members)
}

async fn ask(gw: &Gateway, group: &str, session: &str, messages: Value) -> StatusCode {
    Client::new()
        .post(format!("{}/v1/chat/completions", gw.url))
        .bearer_auth("magpie-lite")
        .header("user-agent", "codex/1.0")
        .header("x-session-id", session)
        .json(&json!({
            "model": format!("group/{group}"), "stream": false,
            "messages": messages,
        }))
        .send()
        .await
        .unwrap()
        .status()
}

fn cls_asked(gw: &Gateway) -> usize {
    gw.cls_bodies.lock().unwrap().len()
}

async fn stats(gw: &Gateway) -> Value {
    Client::new()
        .get(format!("{}/stats", gw.url))
        .bearer_auth("magpie-lite")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

fn last_classified(stats: &Value) -> Value {
    stats["recent_requests"].as_array().unwrap().last().unwrap()["classified"].clone()
}
#[test]
fn an_intent_rule_needs_its_group_classifier() {
    let members = vec!["a/one".to_owned(), "b/two".to_owned()];
    let mut group = GroupConfig {
        id: "g".into(),
        name: "G".into(),
        routes: vec!["a/one".into(), "b/two".into()],
        rules: vec![intent_rule("b/two", "bug", &members)],
        classifier: String::new(),
        effort: String::new(),
        levels: Vec::new(),
        pick: String::new(),
        auto: false,
        weights: Vec::new(),
        routing: RoutingPolicy::Order,
        first_token_timeout_ms: 0,
        affinity: Default::default(),
        patience_s: None,
    };
    let err = group.validate().unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("needs the group's classifier"), "{msg}");
    // answered: clears a classifier nothing waits on
    group.rules.clear();
    group.classifier = "c/judge".into();
    group.validate().unwrap();
    assert_eq!(group.classifier, "");
}

#[test]
fn a_classifier_names_a_model_not_a_group() {
    let config = Config {
        provider: vec![provider("c", "http://127.0.0.1:9/v1", "judge")],
        ..Config::default()
    };
    let mut group = GroupConfig {
        id: "g".into(),
        name: "G".into(),
        routes: vec!["c/judge".into()],
        rules: Vec::new(),
        classifier: "group/g".into(),
        effort: String::new(),
        levels: Vec::new(),
        pick: String::new(),
        auto: false,
        weights: Vec::new(),
        routing: RoutingPolicy::Order,
        first_token_timeout_ms: 0,
        affinity: Default::default(),
        patience_s: None,
    };
    assert!(group.check_classifier(&config).is_err());
    group.classifier = "group/other".into();
    assert!(group.check_classifier(&config).is_err());
    group.classifier = "missing/judge".into();
    assert!(group.check_classifier(&config).is_err());
    group.classifier = "c/judge".into();
    group.check_classifier(&config).unwrap();
}

#[tokio::test]
async fn a_turn_goes_by_what_the_classifier_says_once() {
    let members = vec!["a/one".to_owned(), "b/two".to_owned()];
    let gw = gateway(
        "g-verdict",
        "judge-verdict",
        vec![intent_group_rule(&members)],
        "",
        Arc::new(Mutex::new(ClsAnswer {
            status: 200,
            text: "1".into(),
        })),
    )
    .await;
    // the verdict sends the turn to the intent rule's member, asked once
    let first = json!([{"role": "user", "content": "refactor this package"}]);
    assert_eq!(
        ask(&gw, "g-verdict", "s-verdict", first).await,
        StatusCode::OK
    );
    assert_eq!(gw.bodies[0].lock().unwrap().len(), 0);
    assert_eq!(gw.bodies[1].lock().unwrap().len(), 1);
    assert_eq!(cls_asked(&gw), 1);
    // the gateway asks the resolved upstream model, not the group's spelling
    assert_eq!(gw.cls_bodies.lock().unwrap()[0]["model"], "judge-verdict");
    // within the turn the stored verdict goes by; the classifier rests
    let call = json!({"id": "t1", "type": "function"});
    let within = json!([
        {"role": "user", "content": "refactor this package"},
        {"role": "assistant", "content": Value::Null, "tool_calls": [call]},
        {"role": "tool", "tool_call_id": "t1", "content": "done"},
    ]);
    assert_eq!(
        ask(&gw, "g-verdict", "s-verdict", within).await,
        StatusCode::OK
    );
    assert_eq!(gw.bodies[1].lock().unwrap().len(), 2);
    assert_eq!(cls_asked(&gw), 1);
    // the verdict goes on the request's record; the card stays quiet
    let recent = stats(&gw).await;
    let records = recent["recent_requests"].as_array().unwrap();
    // the classifier's own call leaves a record too; the turn's first
    // agent request carries the verdict
    let started = records.iter().find(|r| !r["classified"].is_null()).unwrap();
    assert_eq!(started["requested_model"], "group/g-verdict");
    assert_eq!(started["classified"]["by"], "c/judge-verdict");
    assert_eq!(started["classified"]["intent"], "refactoring");
    assert_eq!(started["classified"]["intents"], json!(["refactoring"]));
    // within the turn nothing is asked again, so nothing is told either
    let agent: Vec<_> = records
        .iter()
        .filter(|r| r["requested_model"] == "group/g-verdict")
        .collect();
    assert_eq!(agent.len(), 2);
    assert!(
        agent[1]["classified"].is_null(),
        "{}",
        agent[1]["classified"]
    );
    // answered: nothing for the card
    assert!(
        recent["classifier_status"]
            .as_array()
            .unwrap()
            .iter()
            .all(|s| s["model"] != "c/judge-verdict")
    );
}

#[test]
fn classified_keeps_only_what_was_asked_and_said() {
    let full = magpie_gateway::rules::Classified {
        by: "c/judge".into(),
        intents: vec!["bug".into()],
        intent: "bug".into(),
        after: String::new(),
        effort: "high".into(),
        score: 2.0,
        after_effort: "medium".into(),
        cached: true,
        ms: 12,
        error: String::new(),
        fallback: String::new(),
    };
    let value = serde_json::to_value(&full).unwrap();
    assert_eq!(
        value,
        json!({"by": "c/judge", "intents": ["bug"], "intent": "bug",
               "effort": "high", "score": 2.0, "after_effort": "medium",
               "cached": true, "ms": 12})
    );
    // older history lines predate it
    let record: magpie_gateway::usage::RequestRecord = serde_json::from_value(json!({
        "started_unix_ms": 1, "client_protocol": "chat", "requested_model": "m",
        "provider": "a", "upstream_model": "one", "key_slot": 0,
        "stream": false, "status": "ok", "latency_ms": 2,
        "usage": {},
    }))
    .unwrap();
    assert!(record.classified.is_none());
}

#[tokio::test]
async fn a_failed_classifier_leaves_the_turn_to_the_rest_of_the_rules() {
    let members = vec!["a/one".to_owned(), "b/two".to_owned()];
    let gw = gateway(
        "g-down",
        "judge-down",
        vec![intent_group_rule(&members)],
        "",
        Arc::new(Mutex::new(ClsAnswer {
            status: 500,
            text: String::new(),
        })),
    )
    .await;
    // down: no intent matches, the turn goes by the group's order
    let first = json!([{"role": "user", "content": "refactor this package"}]);
    assert_eq!(ask(&gw, "g-down", "s-down-1", first).await, StatusCode::OK);
    assert_eq!(gw.bodies[0].lock().unwrap().len(), 1);
    assert_eq!(cls_asked(&gw), 1);
    // resting: another turn is not asked again
    let second = json!([{"role": "user", "content": "refactor that package"}]);
    assert_eq!(ask(&gw, "g-down", "s-down-2", second).await, StatusCode::OK);
    assert_eq!(gw.bodies[0].lock().unwrap().len(), 2);
    assert_eq!(cls_asked(&gw), 1);
    // the failed ask goes on the request's record and on the card's status
    let record = last_classified(&stats(&gw).await);
    assert_eq!(record["by"], "c/judge-down");
    assert!(record.get("intent").is_none(), "{record}");
    assert!(
        record["error"]
            .as_str()
            .unwrap()
            .contains("classifier down"),
        "{record}"
    );
    let status = &stats(&gw).await["classifier_status"];
    let down = status
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["model"] == "c/judge-down")
        .unwrap()
        .clone();
    assert!(
        down["error"].as_str().unwrap().contains("classifier down"),
        "{down}"
    );
}

#[tokio::test]
async fn a_failed_classifier_keeps_the_turn_before_intent() {
    let members = vec!["a/one".to_owned(), "b/two".to_owned()];
    let gw = gateway(
        "g-keep",
        "judge-keep",
        vec![intent_group_rule(&members)],
        "",
        Arc::new(Mutex::new(ClsAnswer {
            status: 200,
            text: "1".into(),
        })),
    )
    .await;
    let first = json!([{"role": "user", "content": "refactor this package"}]);
    assert_eq!(ask(&gw, "g-keep", "s-keep", first).await, StatusCode::OK);
    assert_eq!(gw.bodies[1].lock().unwrap().len(), 1);
    // down on the next turn of the same conversation: its intent is kept
    gw.cls.lock().unwrap().status = 500;
    let follow = json!([
        {"role": "user", "content": "refactor this package"},
        {"role": "user", "content": "and the tests"},
    ]);
    assert_eq!(
        ask(&gw, "g-keep", "s-keep", follow.clone()).await,
        StatusCode::OK
    );
    assert_eq!(gw.bodies[1].lock().unwrap().len(), 2);
    assert_eq!(cls_asked(&gw), 2);
    // resting after the failure, the turn after keeps it too, unasked
    assert_eq!(ask(&gw, "g-keep", "s-keep", follow).await, StatusCode::OK);
    assert_eq!(gw.bodies[1].lock().unwrap().len(), 3);
    assert_eq!(cls_asked(&gw), 2);
    // the record says the intent was kept from the turn before, not asked
    let record = last_classified(&stats(&gw).await);
    assert_eq!(record["intent"], "refactoring");
    assert_eq!(record["fallback"], "previous");
    assert!(
        record["error"]
            .as_str()
            .unwrap()
            .contains("classifier down"),
        "{record}"
    );
}

// ---- effort -------------------------------------------------------------------

#[test]
fn read_effort_names_the_numbered_level() {
    assert_eq!(read_effort("3"), Ok(("high".to_owned(), 2.0)));
    assert_eq!(read_effort("1"), Ok(("low".to_owned(), 0.0)));
    // words around the number are read too
    assert_eq!(
        read_effort("Level 4 needs deep thought."),
        Ok(("xhigh".to_owned(), 3.0))
    );
    // out of range, or no number at all, is the classifier not answering
    let err = read_effort("5").unwrap_err();
    assert!(err.contains("not a level from 1 to 4"), "{err}");
    let err = read_effort("no idea").unwrap_err();
    assert!(err.contains("not a level from 1 to 4"), "{err}");
}

#[test]
fn effort_body_asks_for_the_level_number_only() {
    let body = effort_body("c/judge", "medium", "redesign the scheduler");
    assert_eq!(body["model"], "c/judge");
    assert_eq!(body["stream"], false);
    assert_eq!(body["temperature"], 0);
    let words = body["messages"][1]["content"].as_str().unwrap();
    assert!(words.contains("1. low:"), "{words}");
    assert!(words.contains("4. xhigh:"), "{words}");
    // the turn before's level: a follow-up carrying on needs it too
    assert!(words.contains("needed level 2"), "{words}");
    assert!(
        words.contains("<message>\nredesign the scheduler\n</message>"),
        "{words}"
    );
    assert!(
        words.ends_with("The number of the level it needs:"),
        "{words}"
    );
    // no reasoning ask rides along: lite knows no per-model levels to ask
    // least with, so the classifier reasons at its default
    assert!(body.get("reasoning_effort").is_none(), "{body}");
    // without a level before, nothing is told of the turn before
    let plain = effort_body("c/judge", "", "hi");
    let words = plain["messages"][1]["content"].as_str().unwrap();
    assert!(!words.contains("needed level"), "{words}");
}

#[test]
fn fit_level_caps_what_no_vendor_takes_without_levels() {
    // lite knows no per-model levels, so the pick goes as judged — except
    // what no vendor takes under an unknown name (fitLevel)
    assert_eq!(fit_level("ultra"), "max");
    assert_eq!(fit_level("xhigh"), "high");
    assert_eq!(fit_level("high"), "high");
    assert_eq!(fit_level("low"), "low");
}

#[test]
fn an_easier_pick_waits_for_a_cold_cache() {
    use std::time::{Duration, Instant};

    let now = Instant::now();
    let warm = now - Duration::from_secs(33);
    let cold = now - Duration::from_secs(6 * 60);
    // lower and warm: the conversation keeps the higher one it had
    assert!(keeps_effort(
        "xhigh", warm, 234_000, warm, "high", 234_000, now
    ));
    // higher: the turn takes what it wants
    assert!(!keeps_effort(
        "medium", warm, 1_000, warm, "high", 1_000, now
    ));
    // same: nothing to keep
    assert!(!keeps_effort(
        "xhigh", warm, 234_000, warm, "xhigh", 234_000, now
    ));
    // cold: the pick is taken
    assert!(!keeps_effort(
        "xhigh", cold, 234_000, cold, "high", 234_000, now
    ));
    // answered since: warm
    assert!(keeps_effort(
        "xhigh",
        cold,
        1_000,
        now - Duration::from_secs(120),
        "low",
        1_000,
        now
    ));
    // compacted: the conversation is under half as long, the pick is taken
    assert!(!keeps_effort(
        "xhigh", warm, 234_000, warm, "high", 40_000, now
    ));
    // nothing before, or no pick: nothing to keep
    assert!(!keeps_effort("", warm, 1_000, warm, "high", 1_000, now));
    assert!(!keeps_effort("xhigh", warm, 1_000, warm, "", 1_000, now));
    // past what levels say: the pick is taken
    assert!(!keeps_effort(
        "xhigh", warm, 1_000, warm, "ultra", 1_000, now
    ));
}

#[test]
fn effort_auto_needs_its_group_classifier() {
    let mut group = GroupConfig {
        id: "g".into(),
        name: "G".into(),
        routes: vec!["a/one".into(), "b/two".into()],
        rules: Vec::new(),
        classifier: String::new(),
        effort: "auto".into(),
        levels: Vec::new(),
        pick: String::new(),
        auto: false,
        weights: Vec::new(),
        routing: RoutingPolicy::Order,
        first_token_timeout_ms: 0,
        affinity: Default::default(),
        patience_s: None,
    };
    let err = group.validate().unwrap_err();
    assert!(
        err.to_string()
            .contains("effort auto needs the group's classifier"),
        "{err:?}"
    );
    // answered: the classifier is kept, something waits on it
    group.classifier = "c/judge".into();
    group.validate().unwrap();
    assert_eq!(group.classifier, "c/judge");
    // blank stays blank, and other words are refused
    group.effort = "  ".into();
    group.validate().unwrap();
    assert_eq!(group.effort, "");
    group.effort = "high".into();
    assert!(group.validate().is_err());
    // cased spellings are put the one way
    group.classifier = "c/judge".into();
    group.effort = "AUTO".into();
    group.validate().unwrap();
    assert_eq!(group.effort, "auto");
}

async fn ask_body(gw: &Gateway, session: &str, body: Value) -> StatusCode {
    Client::new()
        .post(format!("{}/v1/chat/completions", gw.url))
        .bearer_auth("magpie-lite")
        .header("user-agent", "codex/1.0")
        .header("x-session-id", session)
        .json(&body)
        .send()
        .await
        .unwrap()
        .status()
}

fn effort_turn(group: &str, effort: &str, messages: Value) -> Value {
    let mut body = json!({
        "model": format!("group/{group}"), "stream": false,
        "messages": messages,
    });
    if !effort.is_empty() {
        body["reasoning_effort"] = effort.into();
    }
    body
}

#[tokio::test]
async fn an_auto_group_sends_the_turns_pick() {
    let gw = gateway(
        "g-auto",
        "judge-auto",
        Vec::new(),
        "auto",
        Arc::new(Mutex::new(ClsAnswer {
            status: 200,
            text: "3".into(),
        })),
    )
    .await;
    // the agent asks for reasoning: the turn is judged once, for effort
    // alone — the bare group has no intents to ask about
    let first = effort_turn(
        "g-auto",
        "medium",
        json!([{"role": "user", "content": "redesign the scheduler"}]),
    );
    assert_eq!(ask_body(&gw, "s-auto", first).await, StatusCode::OK);
    assert_eq!(cls_asked(&gw), 1);
    // the judge is asked for a level by number, told no level before
    let judged = gw.cls_bodies.lock().unwrap()[0].clone();
    assert!(
        judged["messages"][1]["content"]
            .as_str()
            .unwrap()
            .contains("1. low:"),
        "{judged}"
    );
    // judged high, sent high — not the medium asked for
    assert_eq!(gw.bodies[0].lock().unwrap()[0]["reasoning_effort"], "high");
    assert_eq!(gw.bodies[0].lock().unwrap()[0]["model"], "one");
    // what was asked and said goes on the request's record
    let recent = stats(&gw).await;
    let agent: Vec<_> = recent["recent_requests"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["requested_model"] == "group/g-auto")
        .collect();
    assert_eq!(agent.len(), 1);
    assert_eq!(agent[0]["classified"]["by"], "c/judge-auto");
    assert_eq!(agent[0]["classified"]["effort"], "high");
    assert_eq!(agent[0]["classified"]["score"], json!(2.0));
    assert!(agent[0]["classified"].get("after_effort").is_none());
    // the next turn of the same conversation judges again, told the level
    // the turn before had
    gw.cls.lock().unwrap().text = "2".into();
    let second = effort_turn(
        "g-auto",
        "medium",
        json!([
            {"role": "user", "content": "redesign the scheduler"},
            {"role": "user", "content": "rename the helper"},
        ]),
    );
    assert_eq!(ask_body(&gw, "s-auto", second).await, StatusCode::OK);
    assert_eq!(cls_asked(&gw), 2);
    let judged = gw.cls_bodies.lock().unwrap()[1].clone();
    assert!(
        judged["messages"][1]["content"]
            .as_str()
            .unwrap()
            .contains("needed level 3"),
        "{judged}"
    );
    // the easier pick waits for a cold cache: the turn keeps high
    assert_eq!(gw.bodies[0].lock().unwrap()[1]["reasoning_effort"], "high");
    let recent = stats(&gw).await;
    let agent: Vec<_> = recent["recent_requests"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["requested_model"] == "group/g-auto")
        .collect();
    assert_eq!(agent.len(), 2);
    assert_eq!(agent[1]["classified"]["effort"], "medium");
    assert_eq!(agent[1]["classified"]["after_effort"], "high");
}

#[tokio::test]
async fn an_auto_group_asks_nothing_without_reasoning() {
    let gw = gateway(
        "g-quiet",
        "judge-quiet",
        Vec::new(),
        "auto",
        Arc::new(Mutex::new(ClsAnswer {
            status: 200,
            text: "4".into(),
        })),
    )
    .await;
    // the agent asks for no reasoning: nothing is judged, nothing overridden
    let body = effort_turn(
        "g-quiet",
        "",
        json!([{"role": "user", "content": "redesign the scheduler"}]),
    );
    assert_eq!(ask_body(&gw, "s-quiet", body).await, StatusCode::OK);
    assert_eq!(cls_asked(&gw), 0);
    assert!(
        gw.bodies[0].lock().unwrap()[0]
            .get("reasoning_effort")
            .is_none()
    );
    let recent = stats(&gw).await;
    let agent: Vec<_> = recent["recent_requests"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["requested_model"] == "group/g-quiet")
        .collect();
    assert_eq!(agent.len(), 1);
    assert!(
        agent[0]["classified"].is_null(),
        "{}",
        agent[0]["classified"]
    );
}

#[tokio::test]
async fn a_member_fixed_at_an_effort_sends_it_unasked() {
    // No auto, no rules: the agent asks for no reasoning at all, yet the
    // fixed member still sends its effort, to its own model.
    let gw = gateway_with_routes(
        "g-fixed",
        "judge-fixed",
        Vec::new(),
        "",
        vec!["a/one:high".into(), "b/two".into()],
        Arc::new(Mutex::new(ClsAnswer {
            status: 200,
            text: "1".into(),
        })),
    )
    .await;
    let body = effort_turn(
        "g-fixed",
        "",
        json!([{"role": "user", "content": "Hello there, how are you doing today?"}]),
    );
    assert_eq!(ask_body(&gw, "s-fixed", body).await, StatusCode::OK);
    assert_eq!(cls_asked(&gw), 0);
    let sent = gw.bodies[0].lock().unwrap()[0].clone();
    assert_eq!(sent["model"], "one");
    assert_eq!(sent["reasoning_effort"], "high");
    assert!(gw.bodies[1].lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_member_fixed_at_an_effort_beats_the_turns_pick() {
    // Auto judges the turn high, but the member is fixed at low: low is
    // what its model is asked for.
    let gw = gateway_with_routes(
        "g-fixed-pick",
        "judge-fixed-pick",
        Vec::new(),
        "auto",
        vec!["a/one:low".into(), "b/two".into()],
        Arc::new(Mutex::new(ClsAnswer {
            status: 200,
            text: "3".into(),
        })),
    )
    .await;
    let body = effort_turn(
        "g-fixed-pick",
        "medium",
        json!([{"role": "user", "content": "redesign the scheduler"}]),
    );
    assert_eq!(ask_body(&gw, "s-fixed-pick", body).await, StatusCode::OK);
    assert_eq!(cls_asked(&gw), 1);
    let sent = gw.bodies[0].lock().unwrap()[0].clone();
    assert_eq!(sent["model"], "one");
    assert_eq!(sent["reasoning_effort"], "low");
}

#[tokio::test]
async fn a_rule_names_its_fixed_member_with_the_suffix() {
    // A rule names its member as written, effort and all: the turn goes
    // to a/one first, at high, though it is listed second.
    let members = vec!["b/two".to_owned(), "a/one:high".to_owned()];
    let mut rule = Rule {
        use_: "a/one:high".into(),
        tokens: 1,
        ..Default::default()
    };
    rule.clean(&members, 1).unwrap();
    let gw = gateway_with_routes(
        "g-fixed-rule",
        "judge-fixed-rule",
        vec![rule],
        "",
        members,
        Arc::new(Mutex::new(ClsAnswer {
            status: 200,
            text: "1".into(),
        })),
    )
    .await;
    let body = effort_turn(
        "g-fixed-rule",
        "",
        json!([{"role": "user", "content": "Hello there, how are you doing today?"}]),
    );
    assert_eq!(ask_body(&gw, "s-fixed-rule", body).await, StatusCode::OK);
    assert_eq!(cls_asked(&gw), 0);
    let sent = gw.bodies[0].lock().unwrap()[0].clone();
    assert_eq!(sent["model"], "one");
    assert_eq!(sent["reasoning_effort"], "high");
}

#[tokio::test]
async fn a_rule_sets_the_reasoning_its_member_is_asked_for() {
    // The agent asks medium; the matching rule's member goes first, at
    // the rule's low.
    let members = vec!["b/two".to_owned(), "a/one".to_owned()];
    let mut rule = Rule {
        use_: "a/one".into(),
        tokens: 1,
        use_effort: "low".into(),
        ..Default::default()
    };
    rule.clean(&members, 1).unwrap();
    let gw = gateway_with_routes(
        "g-rule-effort",
        "judge-rule-effort",
        vec![rule],
        "",
        members,
        Arc::new(Mutex::new(ClsAnswer {
            status: 200,
            text: "1".into(),
        })),
    )
    .await;
    let body = effort_turn(
        "g-rule-effort",
        "medium",
        json!([{"role": "user", "content": "Hello there, how are you doing today?"}]),
    );
    assert_eq!(ask_body(&gw, "s-rule-effort", body).await, StatusCode::OK);
    assert_eq!(cls_asked(&gw), 0);
    let sent = gw.bodies[0].lock().unwrap()[0].clone();
    assert_eq!(sent["model"], "one");
    assert_eq!(sent["reasoning_effort"], "low");
}

#[tokio::test]
async fn a_fixed_member_keeps_its_effort_against_a_rule() {
    // The rule names the fixed member and asks low; high still goes.
    let members = vec!["b/two".to_owned(), "a/one:high".to_owned()];
    let mut rule = Rule {
        use_: "a/one:high".into(),
        tokens: 1,
        use_effort: "low".into(),
        ..Default::default()
    };
    rule.clean(&members, 1).unwrap();
    let gw = gateway_with_routes(
        "g-rule-fixed",
        "judge-rule-fixed",
        vec![rule],
        "",
        members,
        Arc::new(Mutex::new(ClsAnswer {
            status: 200,
            text: "1".into(),
        })),
    )
    .await;
    let body = effort_turn(
        "g-rule-fixed",
        "medium",
        json!([{"role": "user", "content": "Hello there, how are you doing today?"}]),
    );
    assert_eq!(ask_body(&gw, "s-rule-fixed", body).await, StatusCode::OK);
    assert_eq!(cls_asked(&gw), 0);
    let sent = gw.bodies[0].lock().unwrap()[0].clone();
    assert_eq!(sent["model"], "one");
    assert_eq!(sent["reasoning_effort"], "high");
}

#[tokio::test]
async fn a_rule_effort_beats_the_turns_pick() {
    // Auto judges the turn high, but the matching rule asks its member
    // for low: low is what goes.
    let members = vec!["b/two".to_owned(), "a/one".to_owned()];
    let mut rule = Rule {
        use_: "a/one".into(),
        tokens: 1,
        use_effort: "low".into(),
        ..Default::default()
    };
    rule.clean(&members, 1).unwrap();
    let gw = gateway_with_routes(
        "g-rule-pick",
        "judge-rule-pick",
        vec![rule],
        "auto",
        members,
        Arc::new(Mutex::new(ClsAnswer {
            status: 200,
            text: "3".into(),
        })),
    )
    .await;
    let body = effort_turn(
        "g-rule-pick",
        "medium",
        json!([{"role": "user", "content": "redesign the scheduler"}]),
    );
    assert_eq!(ask_body(&gw, "s-rule-pick", body).await, StatusCode::OK);
    assert_eq!(cls_asked(&gw), 1);
    let sent = gw.bodies[0].lock().unwrap()[0].clone();
    assert_eq!(sent["model"], "one");
    assert_eq!(sent["reasoning_effort"], "low");
}

#[tokio::test]
async fn a_group_naming_levels_bounds_the_reasoning_its_members_are_asked_for() {
    // The group offers low and high only: low stays, medium ties up to
    // high, xhigh comes down to high, and off stays off.
    let gw = gateway_with_levels(
        "g-levels",
        "judge-levels",
        Vec::new(),
        "",
        vec!["a/one".into(), "b/two".into()],
        vec!["low".into(), "high".into()],
        Arc::new(Mutex::new(ClsAnswer {
            status: 200,
            text: "1".into(),
        })),
    )
    .await;
    for (session, ask) in [
        ("s-levels-low", "low"),
        ("s-levels-medium", "medium"),
        ("s-levels-xhigh", "xhigh"),
        ("s-levels-off", "none"),
    ] {
        let body = effort_turn(
            "g-levels",
            ask,
            json!([{"role": "user", "content": "redesign the scheduler"}]),
        );
        assert_eq!(ask_body(&gw, session, body).await, StatusCode::OK);
    }
    assert_eq!(cls_asked(&gw), 0);
    let sent = gw.bodies[0].lock().unwrap().clone();
    assert_eq!(sent.len(), 4);
    assert_eq!(sent[0]["reasoning_effort"], "low");
    assert_eq!(sent[1]["reasoning_effort"], "high");
    assert_eq!(sent[2]["reasoning_effort"], "high");
    assert!(sent[3].get("reasoning_effort").is_none(), "{}", sent[3]);
}

#[tokio::test]
async fn a_fixed_effort_is_bounded_by_the_levels_its_group_names() {
    // The member is fixed at max, but the group offers low and high: the
    // fixed ask still wins over the agent's, sent at the nearest offered.
    let gw = gateway_with_levels(
        "g-levels-fixed",
        "judge-levels-fixed",
        Vec::new(),
        "",
        vec!["a/one:max".into(), "b/two".into()],
        vec!["low".into(), "high".into()],
        Arc::new(Mutex::new(ClsAnswer {
            status: 200,
            text: "1".into(),
        })),
    )
    .await;
    for (session, ask) in [("s-levels-fixed-quiet", ""), ("s-levels-fixed-low", "low")] {
        let body = effort_turn(
            "g-levels-fixed",
            ask,
            json!([{"role": "user", "content": "redesign the scheduler"}]),
        );
        assert_eq!(ask_body(&gw, session, body).await, StatusCode::OK);
    }
    assert_eq!(cls_asked(&gw), 0);
    let sent = gw.bodies[0].lock().unwrap().clone();
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[0]["reasoning_effort"], "high");
    assert_eq!(sent[1]["reasoning_effort"], "high");
}
