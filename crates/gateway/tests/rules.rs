//! A group's rules: the member a matching rule names goes first, the members
//! the rules after it also name behind it, a pause leaves its member out for
//! its hours or agents — and what a turn began with holds for the requests
//! within it (the reference's grouprule/time/pause/rulefolder tests).

use std::sync::{Arc, Mutex};

use axum::{Json, Router, extract::State, routing::post};
use magpie_gateway::rules::{
    Rule, RuleRequest, TimeWindow, agent_of, clean_folder, folder_key, folder_under, match_rule,
    paused_out, then_uses,
};
use magpie_gateway::{
    AppState, Config, ConfigStore, GroupConfig, Protocol, ProviderConfig, RoutingPolicy,
    build_router,
};
use reqwest::{Client, StatusCode};
use serde_json::{Value, json};

fn window(from: &str, to: &str, days: &[&str]) -> TimeWindow {
    TimeWindow {
        from: from.into(),
        to: to.into(),
        days: days.iter().map(|d| d.to_string()).collect(),
    }
}

fn req(q: RuleRequest) -> RuleRequest {
    q
}

// ---- the rule itself -----------------------------------------------------

#[test]
fn a_rule_matches_only_when_every_condition_holds() {
    let members = vec!["a/one".to_owned(), "b/two".to_owned()];
    let mut rule = Rule {
        use_: "b/two".into(),
        tokens: 1000,
        ..Default::default()
    };
    rule.clean(&members, 1).unwrap();
    let long = req(RuleRequest {
        tokens: 2000,
        ..Default::default()
    });
    let short = req(RuleRequest {
        tokens: 500,
        ..Default::default()
    });
    assert!(rule.matches(&long));
    assert!(!rule.matches(&short));

    let mut combo = Rule {
        use_: "b/two".into(),
        images: true,
        effort: "high".into(),
        ..Default::default()
    };
    combo.clean(&members, 1).unwrap();
    assert!(combo.matches(&req(RuleRequest {
        images: true,
        thinking: true,
        effort: "xhigh".into(),
        ..Default::default()
    })));
    assert!(!combo.matches(&req(RuleRequest {
        images: true,
        thinking: true,
        effort: "medium".into(),
        ..Default::default()
    })));
    assert!(!combo.matches(&req(RuleRequest {
        thinking: true,
        effort: "high".into(),
        ..Default::default()
    })));
}

#[test]
fn an_effort_rule_takes_that_level_or_above_and_on_any_reasoning() {
    let members = vec!["a/one".to_owned(), "b/two".to_owned()];
    let mut rule = Rule {
        use_: "b/two".into(),
        effort: "high".into(),
        ..Default::default()
    };
    rule.clean(&members, 1).unwrap();
    for effort in ["high", "xhigh", "max"] {
        assert!(rule.matches(&req(RuleRequest {
            thinking: true,
            effort: effort.into(),
            ..Default::default()
        })));
    }
    for effort in ["", "low", "medium"] {
        assert!(!rule.matches(&req(RuleRequest {
            thinking: true,
            effort: effort.into(),
            ..Default::default()
        })));
    }
    let mut any = Rule {
        use_: "b/two".into(),
        effort: "on".into(),
        ..Default::default()
    };
    any.clean(&members, 1).unwrap();
    assert!(any.matches(&req(RuleRequest {
        thinking: true,
        effort: "low".into(),
        ..Default::default()
    })));
    // thinking asked without a level counts too
    assert!(any.matches(&req(RuleRequest {
        thinking: true,
        ..Default::default()
    })));
    assert!(!any.matches(&req(RuleRequest::default())));
}

#[test]
fn match_rule_takes_the_first_and_then_uses_follow() {
    let members = vec!["a/one".to_owned(), "b/two".to_owned(), "c/three".to_owned()];
    let mut rules = vec![
        Rule {
            use_: "a/one".into(),
            tokens: 100,
            ..Default::default()
        },
        Rule {
            use_: "b/two".into(),
            tokens: 50,
            ..Default::default()
        },
        Rule {
            use_: "c/three".into(),
            tokens: 10,
            ..Default::default()
        },
    ];
    for (i, r) in rules.iter_mut().enumerate() {
        r.clean(&members, i + 1).unwrap();
    }
    let q = req(RuleRequest {
        tokens: 1000,
        ..Default::default()
    });
    assert_eq!(match_rule(&rules, &q), Some(0));
    assert_eq!(then_uses(&rules, &q, 1, "a/one"), vec!["b/two", "c/three"]);
    // a request under the first rule's length starts at the second
    let q = req(RuleRequest {
        tokens: 60,
        ..Default::default()
    });
    assert_eq!(match_rule(&rules, &q), Some(1));
    assert_eq!(then_uses(&rules, &q, 2, "b/two"), vec!["c/three"]);
}

#[test]
fn a_pause_rule_leaves_its_member_out_for_now() {
    let members = vec!["a/one".to_owned(), "b/two".to_owned()];
    let mut pause = Rule {
        use_: "a/one".into(),
        pause: true,
        agents: vec!["claude".into()],
        ..Default::default()
    };
    pause.clean(&members, 1).unwrap();
    let routes = members.clone();
    // paused for claude, not for codex
    assert_eq!(
        paused_out(&[pause.clone()], routes.clone(), "claude", "", None),
        vec!["b/two"]
    );
    assert_eq!(
        paused_out(&[pause], routes, "codex", "", None),
        vec!["a/one", "b/two"]
    );
}

#[test]
fn a_pause_never_matches_as_a_route() {
    let members = vec!["a/one".to_owned(), "b/two".to_owned()];
    let mut pause = Rule {
        use_: "b/two".into(),
        pause: true,
        agents: vec!["claude".into()],
        ..Default::default()
    };
    pause.clean(&members, 1).unwrap();
    let q = req(RuleRequest {
        agent: "claude".into(),
        ..Default::default()
    });
    assert!(!pause.matches(&q));
    assert!(pause.pauses("claude", "", None));
}

// ---- time windows ---------------------------------------------------------

#[test]
fn a_window_holds_between_its_hours_and_past_midnight() {
    let mut w = window("09:00", "18:00", &[]);
    w.clean().unwrap();
    // (minutes, weekday; 3 = Wednesday)
    assert!(w.holds(Some((10 * 60, 3))));
    assert!(!w.holds(Some((8 * 60, 3))));
    assert!(!w.holds(Some((18 * 60, 3))));

    let mut night = window("22:00", "08:00", &[]);
    night.clean().unwrap();
    assert!(night.holds(Some((23 * 60, 3))));
    assert!(night.holds(Some((2 * 60, 3))));
    assert!(!night.holds(Some((12 * 60, 3))));
    // a window past midnight is of the day it began on: Friday's 22–08
    // takes Saturday's early hours
    let mut fri = window("22:00", "08:00", &["fri"]);
    fri.clean().unwrap();
    assert!(fri.holds(Some((2 * 60, 6)))); // Saturday 02:00 counts as Friday's night
    assert!(!fri.holds(Some((2 * 60, 0)))); // Sunday 02:00 is Saturday's night
    assert!(!fri.holds(Some((23 * 60, 6)))); // Saturday 23:00 begins its own night

    let mut whole = window("00:00", "00:00", &["mon"]);
    whole.clean().unwrap();
    assert!(whole.holds(Some((13 * 60, 1))));
    assert!(!whole.holds(Some((13 * 60, 2))));
}

#[test]
fn a_window_cleans_to_one_spelling_and_refuses_bad_times() {
    let mut w = TimeWindow {
        from: "9:05".into(),
        to: "18:00".into(),
        days: vec!["fri".into(), "mon".into(), "monday".into()],
    };
    w.clean().unwrap();
    assert_eq!(w.from, "09:05");
    assert_eq!(w.days, vec!["mon", "fri"]);

    assert!(window("25:00", "18:00", &[]).clean().is_err());
    assert!(window("09:00", "18:00", &["noday"]).clean().is_err());
    // the whole day every day is no window at all — refused by the rule,
    // not the window itself
    let members = vec!["a/one".to_owned()];
    let mut all_day = Rule {
        use_: "a/one".into(),
        time: Some(window("00:00", "00:00", &[])),
        ..Default::default()
    };
    assert!(all_day.clean(&members, 1).is_err());
}

// ---- folders --------------------------------------------------------------

#[test]
fn folder_keys_equal_the_same_path_spelled_each_way() {
    magpie_gateway::rules::set_rule_home("/home/me");
    assert_eq!(folder_key("C:\\Work\\X"), "/mnt/c/work/x");
    assert_eq!(folder_key("/mnt/c/work/x"), "/mnt/c/work/x");
    assert_eq!(folder_key("~/work"), "/home/me/work");
    assert_eq!(folder_key("~/work/"), "/home/me/work");
    assert_eq!(folder_key("\\\\wsl$\\Ubuntu\\home\\x"), "/home/x");
    assert_eq!(folder_key("\\\\wsl.localhost\\Debian\\srv"), "/srv");
    assert_eq!(folder_key("not/a/path"), "");
    assert_eq!(folder_key("relative"), "");
    assert_eq!(folder_key("/a//b/../c"), "/a/c");
    magpie_gateway::rules::reset();
}

#[test]
fn folder_under_matches_at_a_boundary() {
    assert!(folder_under("/a/b", "/a/b"));
    assert!(folder_under("/a/b/c", "/a/b"));
    assert!(!folder_under("/a/bc", "/a/b"));
    assert!(!folder_under("", "/a/b"));
    assert!(!folder_under("/a/b", ""));
    // a Windows drive and its WSL mount are one
    assert!(folder_under("C:\\work", "/mnt/c/work"));
    assert!(folder_under("/mnt/c/work/x", "C:\\work"));
}

#[test]
fn clean_folder_wants_a_whole_path() {
    assert_eq!(clean_folder("~/work").unwrap(), "~/work");
    assert_eq!(clean_folder("/Users/me/work/").unwrap(), "/Users/me/work");
    assert_eq!(clean_folder("C:\\work\\").unwrap(), "C:\\work");
    assert_eq!(clean_folder("/").unwrap(), "/");
    assert!(clean_folder("work/sub").is_err());
    assert!(clean_folder("").is_err());
}

#[test]
fn a_folder_rule_matches_in_or_below_its_folders() {
    let members = vec!["a/one".to_owned(), "b/two".to_owned()];
    let mut rule = Rule {
        use_: "b/two".into(),
        folders: vec!["C:\\work".into()],
        ..Default::default()
    };
    rule.clean(&members, 1).unwrap();
    let in_it = req(RuleRequest {
        folder: "C:\\work\\proj".into(),
        ..Default::default()
    });
    let out = req(RuleRequest {
        folder: "D:\\other".into(),
        ..Default::default()
    });
    let wsl = req(RuleRequest {
        folder: "/mnt/c/work".into(),
        ..Default::default()
    });
    assert!(rule.matches(&in_it));
    assert!(rule.matches(&wsl));
    assert!(!rule.matches(&out));
    // no folder stated: no match
    assert!(!rule.matches(&req(RuleRequest::default())));
}

// ---- agents ----------------------------------------------------------------

#[test]
fn agent_of_names_known_agents_and_keeps_others() {
    assert_eq!(agent_of("claude-cli/2.1.0"), "claude");
    assert_eq!(agent_of("codex/0.5"), "codex");
    assert_eq!(agent_of("kimicli/1.0"), "kimi");
    assert_eq!(agent_of("aider/0.86"), "aider"); // unknown: its product name
    assert_eq!(agent_of(""), "other");
    assert_eq!(agent_of("curl/8.0"), "curl");
    assert_eq!(agent_of("magpie/1.0"), "magpie");
}

#[test]
fn an_agents_rule_matches_by_ua_id() {
    let members = vec!["a/one".to_owned(), "b/two".to_owned()];
    let mut rule = Rule {
        use_: "b/two".into(),
        agents: vec!["claude".into(), "codex".into()],
        ..Default::default()
    };
    rule.clean(&members, 1).unwrap();
    assert!(rule.matches(&req(RuleRequest {
        agent: "codex".into(),
        ..Default::default()
    })));
    assert!(!rule.matches(&req(RuleRequest {
        agent: "kimi".into(),
        ..Default::default()
    })));
    // ids fold to lowercase
    assert!(rule.matches(&req(RuleRequest {
        agent: "Claude".into(),
        ..Default::default()
    })));
}

// ---- validation -------------------------------------------------------------

#[test]
fn group_validation_cleans_rules_and_refuses_bad_ones() {
    let routes = vec!["a/one".to_owned(), "b/two".to_owned()];
    let mut group = GroupConfig {
        id: "g".into(),
        name: String::new(),
        routes: routes.clone(),
        rules: Vec::new(),
        classifier: String::new(),
        effort: String::new(),
        pick: String::new(),
        weights: Vec::new(),
        routing: RoutingPolicy::Order,
        first_token_timeout_ms: 0,
        affinity: Default::default(),
    };
    group.validate().unwrap();

    // a member the group doesn't have
    group.rules = vec![Rule {
        use_: "z/gone".into(),
        tokens: 1,
        ..Default::default()
    }];
    assert!(group.validate().is_err());

    // no condition
    group.rules = vec![Rule {
        use_: "a/one".into(),
        ..Default::default()
    }];
    assert!(group.validate().is_err());

    // a bad effort
    group.rules = vec![Rule {
        use_: "a/one".into(),
        effort: "ludicrous".into(),
        ..Default::default()
    }];
    assert!(group.validate().is_err());

    // a pause may only hold by hours or agents
    group.rules = vec![Rule {
        use_: "a/one".into(),
        pause: true,
        tokens: 10,
        ..Default::default()
    }];
    assert!(group.validate().is_err());

    // cleaning writes back: days ordered, times fixed, agents folded
    group.rules = vec![Rule {
        use_: " a/one ".into(),
        agents: vec![" Claude ".into()],
        time: Some(TimeWindow {
            from: "9:00".into(),
            to: "18:00".into(),
            days: vec!["Friday".into(), "mon".into()],
        }),
        ..Default::default()
    }];
    group.validate().unwrap();
    let r = &group.rules[0];
    assert_eq!(r.use_, "a/one");
    assert_eq!(r.agents, vec!["claude"]);
    assert_eq!(r.time.as_ref().unwrap().from, "09:00");
    assert_eq!(r.time.as_ref().unwrap().days, vec!["mon", "fri"]);
}

// ---- end to end -------------------------------------------------------------

struct Gateway {
    url: String,
    bodies: Vec<Arc<Mutex<Vec<Value>>>>,
    _tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Drop for Gateway {
    fn drop(&mut self) {
        for task in &self._tasks {
            task.abort();
        }
    }
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
    }
}

async fn gateway(rules: Vec<Rule>) -> Gateway {
    let a_bodies = Arc::new(Mutex::new(Vec::new()));
    let b_bodies = Arc::new(Mutex::new(Vec::new()));
    let (a, ta) = mock(a_bodies.clone()).await;
    let (b, tb) = mock(b_bodies.clone()).await;
    let config = Config {
        provider: vec![provider("a", &a, "one"), provider("b", &b, "two")],
        group: vec![GroupConfig {
            id: "g".into(),
            name: "G".into(),
            routes: vec!["a/one".into(), "b/two".into()],
            rules,
            classifier: String::new(),
            effort: String::new(),
            pick: String::new(),
            weights: Vec::new(),
            routing: RoutingPolicy::Order,
            first_token_timeout_ms: 0,
            affinity: Default::default(),
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
        _tasks: vec![ta, tb, tg],
    }
}

async fn ask(gw: &Gateway, body: Value, ua: &str) -> StatusCode {
    Client::new()
        .post(format!("{}/v1/chat/completions", gw.url))
        .bearer_auth("magpie-lite")
        .header("user-agent", ua)
        .json(&body)
        .send()
        .await
        .unwrap()
        .status()
}

fn message() -> Value {
    json!({
        "model": "group/g", "stream": false,
        "messages": [{"role": "user", "content": "hi"}]
    })
}

#[tokio::test]
async fn an_agents_rule_puts_its_member_first() {
    let mut rule = Rule {
        use_: "b/two".into(),
        agents: vec!["codex".into()],
        ..Default::default()
    };
    // config is validated on the way in (ConfigStore::new doesn't validate;
    // the console path does) — clean it here as the CLI save would.
    rule.clean(&["a/one".into(), "b/two".into()], 1).unwrap();
    let gw = gateway(vec![rule]).await;
    magpie_gateway::rules::reset();

    // codex's request goes to b/two first
    assert_eq!(ask(&gw, message(), "codex/1.0").await, StatusCode::OK);
    assert_eq!(gw.bodies[0].lock().unwrap().len(), 0);
    assert_eq!(gw.bodies[1].lock().unwrap().len(), 1);
    assert_eq!(gw.bodies[1].lock().unwrap()[0]["model"], "two");

    // another agent's goes to the group's first member
    assert_eq!(ask(&gw, message(), "claude-cli/2.0").await, StatusCode::OK);
    assert_eq!(gw.bodies[0].lock().unwrap().len(), 1);
    assert_eq!(gw.bodies[1].lock().unwrap().len(), 1);
}

#[tokio::test]
async fn a_pause_rule_leaves_its_member_out_while_it_holds() {
    let mut pause = Rule {
        use_: "a/one".into(),
        pause: true,
        agents: vec!["claude".into()],
        ..Default::default()
    };
    pause.clean(&["a/one".into(), "b/two".into()], 1).unwrap();
    let gw = gateway(vec![pause]).await;
    magpie_gateway::rules::reset();

    // claude's request skips a/one entirely
    assert_eq!(ask(&gw, message(), "claude-cli/2.0").await, StatusCode::OK);
    assert_eq!(gw.bodies[0].lock().unwrap().len(), 0);
    assert_eq!(gw.bodies[1].lock().unwrap().len(), 1);

    // codex's goes to a/one
    assert_eq!(ask(&gw, message(), "codex/1.0").await, StatusCode::OK);
    assert_eq!(gw.bodies[0].lock().unwrap().len(), 1);
}

#[tokio::test]
async fn a_turn_decided_at_its_start_keeps_its_member() {
    // The rule only matches the first request of a conversation: a long one.
    // The tool result going back within the turn stays with the member the
    // rule sent the turn to, though it no longer matches the rule itself.
    let mut rule = Rule {
        use_: "b/two".into(),
        tokens: 1,
        ..Default::default()
    };
    rule.clean(&["a/one".into(), "b/two".into()], 1).unwrap();
    let gw = gateway(vec![rule]).await;
    magpie_gateway::rules::reset();

    let first = json!({
        "model": "group/g", "stream": false,
        "messages": [{"role": "user", "content": "a long message for the rule"}]
    });
    assert_eq!(ask(&gw, first, "codex/1.0").await, StatusCode::OK);
    assert_eq!(gw.bodies[1].lock().unwrap().len(), 1);
    assert_eq!(gw.bodies[0].lock().unwrap().len(), 0);

    // same conversation, tool result back: still b/two — the turn's decision
    let tool_back = json!({
        "model": "group/g", "stream": false,
        "messages": [
            {"role": "user", "content": "a long message for the rule"},
            {"role": "assistant", "content": null,
             "tool_calls": [{"id": "c1", "type": "function",
               "function": {"name": "x", "arguments": "{}"}}]},
            {"role": "tool", "tool_call_id": "c1", "content": "done"}
        ]
    });
    assert_eq!(ask(&gw, tool_back, "codex/1.0").await, StatusCode::OK);
    assert_eq!(gw.bodies[1].lock().unwrap().len(), 2);
    assert_eq!(gw.bodies[0].lock().unwrap().len(), 0);
}

#[tokio::test]
async fn then_uses_are_the_fallbacks_ahead_of_the_groups_others() {
    // The matching rule's member fails over to the members the later rules
    // also name before the group's own order — so with b/two first by rule
    // and a/one the group's first, a b failure should still try a/one (the
    // only other member) — and the rules' order itself is what this group
    // changes, which the member counts show.
    let mut rules = vec![
        Rule {
            use_: "b/two".into(),
            agents: vec!["codex".into()],
            ..Default::default()
        },
        Rule {
            use_: "a/one".into(),
            agents: vec!["codex".into()],
            ..Default::default()
        },
    ];
    for (i, r) in rules.iter_mut().enumerate() {
        r.clean(&["a/one".into(), "b/two".into()], i + 1).unwrap();
    }
    let gw = gateway(rules).await;
    magpie_gateway::rules::reset();
    assert_eq!(ask(&gw, message(), "codex/1.0").await, StatusCode::OK);
    assert_eq!(gw.bodies[1].lock().unwrap().len(), 1);
    assert_eq!(gw.bodies[0].lock().unwrap().len(), 0);
}
