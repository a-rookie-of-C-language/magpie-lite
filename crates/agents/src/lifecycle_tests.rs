use super::*;
const TOKEN: &str = "gateway-private-token-for-tests";
const MODEL: &str = "mock/test-model";
fn gateway() -> Gateway<'static> {
    Gateway {
        port: 45678,
        token: TOKEN,
    }
}
fn setup() -> (tempfile::TempDir, Manager) {
    let root = tempfile::tempdir().unwrap();
    let manager =
        Manager::at_home(&root.path().join("home"), root.path().join("agent-state")).unwrap();
    (root, manager)
}
fn write(path: &Path, bytes: impl AsRef<[u8]>) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
}
fn connect(manager: &Manager, id: AgentId, model: &str) -> Status {
    let p = manager
        .preview(id, Action::Connect, Some(model), gateway())
        .unwrap();
    manager
        .apply(id, Action::Connect, Some(model), &p.revision, gateway())
        .unwrap()
}
fn disconnect(manager: &Manager, id: AgentId) -> Status {
    let p = manager
        .preview(id, Action::Disconnect, None, gateway())
        .unwrap();
    manager
        .apply(id, Action::Disconnect, None, &p.revision, gateway())
        .unwrap()
}
fn images(manager: &Manager, id: AgentId) -> Vec<Image> {
    manager
        .paths(id)
        .unwrap()
        .iter()
        .map(|p| Image::capture(p).unwrap())
        .collect()
}
fn original(id: AgentId, index: usize) -> &'static [u8] {
    match (id, index) {
        (AgentId::Codex, _) => b"# preserve comments\r\nmodel = 'original'\r\n[features]\r\nreview = true\r\n",
        (AgentId::Goose, 0) => b"# original settings\r\nGOOSE_PROVIDER: original\r\nGOOSE_MODEL: prior\r\ncustom: kept\r\n",
        (AgentId::Gemini, 1) => b"# original keys\r\nGEMINI_API_KEY=old-secret\r\nUNRELATED=kept\r\n",
        (AgentId::Goose, 1) => b"{\"original-secret\":\"custom-provider-secret\"}\r\n",
        _ => b"{\r\n  \"custom\": \"kept\", \"unrelated\": [1, 2]\r\n}\r\n",
    }
}
fn assert_safe(value: impl Serialize) {
    let text = serde_json::to_string(&value).unwrap();
    for secret in [TOKEN, "old-secret", "custom-provider-secret"] {
        assert!(!text.contains(secret));
    }
}
#[test]
fn seven_agents_preview_apply_rewire_and_exact_restore() {
    let (_root, m) = setup();
    for id in AgentId::ALL {
        let paths = m.paths(id).unwrap();
        for (index, path) in paths.iter().enumerate() {
            write(path, original(id, index));
        }
        let baseline = images(&m, id);
        let p = m
            .preview(id, Action::Connect, Some(MODEL), gateway())
            .unwrap();
        assert_eq!(images(&m, id), baseline, "preview touched {id:?}");
        assert_eq!(p.revision.len(), 64);
        assert_eq!(p.files.len(), paths.len());
        assert_safe(&p);
        let status = m
            .apply(id, Action::Connect, Some(MODEL), &p.revision, gateway())
            .unwrap();
        assert!(status.connected && !status.drifted && !status.recovery_needed);
        assert_safe(&status);
        for (image, file) in images(&m, id).iter().zip(&p.files) {
            assert_eq!(image.len(), file.after_bytes);
        }
        connect(&m, id, "mock/changed-model");
        assert_eq!(
            m.load(id)
                .unwrap()
                .unwrap()
                .files
                .iter()
                .map(|e| e.baseline.clone())
                .collect::<Vec<_>>(),
            baseline
        );
        assert!(!disconnect(&m, id).connected);
        assert_eq!(images(&m, id), baseline, "restore changed {id:?}");
    }
}
#[test]
fn seven_agents_remove_originally_missing_files() {
    let (_root, m) = setup();
    for id in AgentId::ALL {
        let before = images(&m, id);
        assert!(before.iter().all(|i| i.data.is_none()));
        let p = m
            .preview(id, Action::Connect, Some(MODEL), gateway())
            .unwrap();
        assert!(p.files.iter().all(|f| f.operation == "create"));
        connect(&m, id, MODEL);
        assert!(disconnect(&m, id).model.is_none());
        assert_eq!(images(&m, id), before);
    }
}
#[test]
fn same_file_crush_layout_is_journaled_once() {
    let (root, _) = setup();
    let path = root.path().join("crush.json");
    let mut targets = BTreeMap::new();
    targets.insert(AgentId::Crush, vec![path.clone(), path.clone()]);
    let m = Manager::new(root.path().join("other-state"), targets).unwrap();
    write(&path, b"{\"custom\":true}");
    let baseline = Image::capture(&path).unwrap();
    assert_eq!(
        m.preview(AgentId::Crush, Action::Connect, Some(MODEL), gateway())
            .unwrap()
            .files
            .len(),
        1
    );
    connect(&m, AgentId::Crush, MODEL);
    let value: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(value["models"]["large"]["model"], MODEL);
    assert_eq!(value["providers"]["magpie-lite"]["api_key"], TOKEN);
    disconnect(&m, AgentId::Crush);
    assert_eq!(Image::capture(&path).unwrap(), baseline);
}
#[test]
fn changed_files_gateway_or_model_invalidate_preview() {
    let (_root, m) = setup();
    let id = AgentId::Claude;
    let p = m
        .preview(id, Action::Connect, Some(MODEL), gateway())
        .unwrap();
    let path = m.paths(id).unwrap().remove(0);
    write(&path, b"{\"user\":true}");
    assert_eq!(
        m.apply(id, Action::Connect, Some(MODEL), &p.revision, gateway())
            .unwrap_err()
            .kind,
        ErrorKind::Conflict
    );
    assert_eq!(fs::read(&path).unwrap(), b"{\"user\":true}");
    assert!(m.load(id).unwrap().is_none());
    let p = m
        .preview(id, Action::Connect, Some(MODEL), gateway())
        .unwrap();
    let changed = Gateway {
        port: 12345,
        token: "other-gateway-secret",
    };
    assert_eq!(
        m.apply(id, Action::Connect, Some(MODEL), &p.revision, changed)
            .unwrap_err()
            .kind,
        ErrorKind::Conflict
    );
    assert_eq!(
        m.apply(
            id,
            Action::Connect,
            Some("mock/changed-model"),
            &p.revision,
            gateway()
        )
        .unwrap_err()
        .kind,
        ErrorKind::Conflict
    );
    connect(&m, id, MODEL);
    assert_eq!(
        m.apply(id, Action::Disconnect, None, &p.revision, gateway())
            .unwrap_err()
            .kind,
        ErrorKind::Conflict
    );
}
#[test]
fn drift_refuses_reconnect_and_disconnect_without_overwriting_user_bytes() {
    let (_root, m) = setup();
    let id = AgentId::Gemini;
    connect(&m, id, MODEL);
    write(&m.paths(id).unwrap()[1], b"# user's edit\nOTHER=value\n");
    let after = images(&m, id);
    let status = m.status(id).unwrap();
    assert!(status.connected && status.drifted);
    for action in [Action::Connect, Action::Disconnect] {
        assert_eq!(
            m.preview(id, action, Some(MODEL), gateway())
                .unwrap_err()
                .kind,
            ErrorKind::Conflict
        );
        assert_eq!(images(&m, id), after);
    }
}
// This is exactly the journal durable before apply's first file publish.
fn interrupted_connect(m: &Manager, id: AgentId, model: &str) -> Journal {
    let plan = m.plan(id, Action::Connect, Some(model), gateway()).unwrap();
    let journal = Journal {
        schema: 1,
        agent: id,
        model: model.into(),
        phase: Phase::Connecting,
        files: plan
            .paths
            .iter()
            .zip(&plan.before)
            .zip(&plan.after)
            .enumerate()
            .map(|(index, ((path, before), after))| Entry {
                path: path.clone(),
                baseline: plan
                    .journal
                    .as_ref()
                    .filter(|j| j.phase == Phase::Connected)
                    .map_or_else(|| before.clone(), |j| j.files[index].baseline.clone()),
                applied: after.clone(),
                owned: None,
                restore_base: None,
            })
            .collect(),
        rollback: Some(plan.before),
        restore: None,
        prior_model: plan
            .journal
            .filter(|j| j.phase == Phase::Connected)
            .map(|j| j.model),
    };
    m.save(&journal).unwrap();
    atomic(&journal.files[0].path, &journal.files[0].applied).unwrap();
    journal
}
#[test]
fn pending_first_connect_rolls_back_after_restart() {
    let (root, m) = setup();
    let id = AgentId::Gemini;
    write(&m.paths(id).unwrap()[0], b"{\"custom\":true}");
    let baseline = images(&m, id);
    interrupted_connect(&m, id, MODEL);
    assert!(m.status(id).unwrap().recovery_needed);
    assert_eq!(
        m.preview(id, Action::Connect, Some(MODEL), gateway())
            .unwrap_err()
            .kind,
        ErrorKind::Conflict
    );
    drop(m);
    let m = Manager::at_home(&root.path().join("home"), root.path().join("agent-state")).unwrap();
    let status = m.recover(id).unwrap();
    assert!(!status.connected && !status.recovery_needed);
    assert_eq!(images(&m, id), baseline);
}
#[test]
fn interrupted_rewire_restores_prior_connection_then_original_baseline() {
    let (_root, m) = setup();
    let id = AgentId::Goose;
    let baseline = images(&m, id);
    connect(&m, id, MODEL);
    let prior = images(&m, id);
    interrupted_connect(&m, id, "mock/changed-model");
    let status = m.recover(id).unwrap();
    assert!(status.connected && !status.recovery_needed && !status.drifted);
    assert_eq!(status.model.as_deref(), Some(MODEL));
    assert_eq!(images(&m, id), prior);
    disconnect(&m, id);
    assert_eq!(images(&m, id), baseline);
}
#[test]
fn interrupted_disconnect_completes_restore() {
    let (_root, m) = setup();
    let id = AgentId::Crush;
    let baseline = images(&m, id);
    connect(&m, id, MODEL);
    let mut j = m.load(id).unwrap().unwrap();
    j.rollback = Some(images(&m, id));
    j.restore = Some(j.files.iter().map(|e| e.baseline.clone()).collect());
    j.phase = Phase::Disconnecting;
    m.save(&j).unwrap();
    atomic(&j.files[0].path, &j.files[0].baseline).unwrap();
    assert!(!m.recover(id).unwrap().connected);
    assert_eq!(images(&m, id), baseline);
}
#[test]
fn recovery_checks_all_files_before_any_overwrite() {
    let (_root, m) = setup();
    let id = AgentId::Gemini;
    let j = interrupted_connect(&m, id, MODEL);
    write(&j.files[1].path, b"USER_EDIT=keep\n");
    let current = images(&m, id);
    assert_eq!(m.recover(id).unwrap_err().kind, ErrorKind::Conflict);
    assert_eq!(images(&m, id), current);
    assert!(m.status(id).unwrap().recovery_needed);
}
#[test]
fn malformed_config_is_rejected_without_panics_or_writes() {
    let (_root, m) = setup();
    for id in AgentId::ALL {
        let bad = match id {
            AgentId::Codex => "model = [",
            AgentId::Goose => "- a\n- list\n",
            _ => "[\"gateway-private-token-for-tests\"]",
        };
        write(&m.paths(id).unwrap()[0], bad);
        let before = images(&m, id);
        let e = m
            .preview(id, Action::Connect, Some(MODEL), gateway())
            .unwrap_err();
        assert_eq!(e.kind, ErrorKind::Invalid);
        assert!(!e.to_string().contains(TOKEN));
        assert_eq!(images(&m, id), before);
    }
}
#[test]
fn invalid_models_credentials_and_unmanaged_disconnect_are_rejected() {
    let (_root, m) = setup();
    for model in ["", "has spaces", "has\nnewline", TOKEN] {
        assert_eq!(
            m.preview(AgentId::Claude, Action::Connect, Some(model), gateway())
                .unwrap_err()
                .kind,
            ErrorKind::Invalid
        );
    }
    for g in [
        Gateway {
            port: 0,
            token: TOKEN,
        },
        Gateway {
            port: 1234,
            token: "bad\nsecret",
        },
        Gateway {
            port: 1234,
            token: "",
        },
    ] {
        assert_eq!(
            m.preview(AgentId::Claude, Action::Connect, Some(MODEL), g)
                .unwrap_err()
                .kind,
            ErrorKind::Invalid
        );
    }
    assert_eq!(
        m.preview(AgentId::Claude, Action::Disconnect, None, gateway())
            .unwrap_err()
            .kind,
        ErrorKind::Conflict
    );
}
#[test]
fn corrupt_journal_paths_base64_and_rollback_are_rejected() {
    let (root, m) = setup();
    let id = AgentId::Goose;
    let j = interrupted_connect(&m, id, MODEL);
    let mut bad = j.clone();
    bad.files[0].path = root.path().join("not-an-agent-file");
    m.save(&bad).unwrap();
    assert_eq!(m.recover(id).unwrap_err().kind, ErrorKind::Conflict);
    let mut bad = j.clone();
    bad.rollback.as_mut().unwrap()[0].data = Some("not base64 $$$".into());
    m.save(&bad).unwrap();
    assert_eq!(m.recover(id).unwrap_err().kind, ErrorKind::Conflict);
    let mut bad = j;
    bad.rollback = None;
    m.save(&bad).unwrap();
    assert_eq!(m.recover(id).unwrap_err().kind, ErrorKind::Conflict);
}
#[test]
fn file_size_limit_and_nonregular_files_fail_without_writes() {
    let (_root, m) = setup();
    let path = m.paths(AgentId::Claude).unwrap().remove(0);
    write(&path, vec![b' '; FILE_LIMIT as usize + 1]);
    assert_eq!(
        m.preview(AgentId::Claude, Action::Connect, Some(MODEL), gateway())
            .unwrap_err()
            .kind,
        ErrorKind::Invalid
    );
    fs::remove_file(&path).unwrap();
    fs::create_dir(&path).unwrap();
    assert!(
        m.preview(AgentId::Claude, Action::Connect, Some(MODEL), gateway())
            .is_err()
    );
    assert!(path.is_dir());
}
#[test]
fn different_managers_contend_on_the_os_lock() {
    let (root, m) = setup();
    let other =
        Manager::at_home(&root.path().join("home"), root.path().join("agent-state")).unwrap();
    let lock = m.lock().unwrap();
    assert_eq!(other.statuses().unwrap_err().kind, ErrorKind::Conflict);
    drop(lock);
    assert_eq!(other.statuses().unwrap().len(), 7);
}
#[test]
fn opencode_live_precedence_changes_allow_restore_after_restart() {
    let (root, m) = setup();
    let id = AgentId::Opencode;
    let json = m.paths(id).unwrap().remove(0);
    connect(&m, id, MODEL);
    let jsonc = json.with_extension("jsonc");
    write(&jsonc, b"{// keep comments\n}");
    assert!(m.status(id).unwrap().drifted);
    assert_eq!(
        m.preview(id, Action::Connect, Some(MODEL), gateway())
            .unwrap_err()
            .kind,
        ErrorKind::Conflict
    );
    drop(m);
    let m = Manager::at_home(&root.path().join("home"), root.path().join("agent-state")).unwrap();
    assert_eq!(m.paths(id).unwrap(), vec![json.clone()]);
    disconnect(&m, id);
    assert!(!json.exists());
    assert!(jsonc.exists());
    assert_eq!(m.paths(id).unwrap(), vec![jsonc.clone()]);
    assert_eq!(
        m.preview(id, Action::Connect, Some(MODEL), gateway())
            .unwrap_err()
            .kind,
        ErrorKind::Invalid
    );
    fs::remove_file(jsonc).unwrap();
    assert!(
        m.preview(id, Action::Connect, Some(MODEL), gateway())
            .is_ok()
    );
}
#[test]
fn opencode_new_selected_path_invalidates_unapplied_preview() {
    let (_root, m) = setup();
    let id = AgentId::Opencode;
    let p = m
        .preview(id, Action::Connect, Some(MODEL), gateway())
        .unwrap();
    let default = m.paths(id).unwrap().remove(0);
    let legacy = default.with_file_name("config.json");
    write(&legacy, b"{}");
    assert_eq!(m.paths(id).unwrap(), vec![legacy.clone()]);
    assert_eq!(
        m.apply(id, Action::Connect, Some(MODEL), &p.revision, gateway())
            .unwrap_err()
            .kind,
        ErrorKind::Conflict
    );
    assert_eq!(fs::read(legacy).unwrap(), b"{}");
    assert!(!default.exists());
}
#[cfg(unix)]
#[test]
fn unix_secrets_are_private_and_original_modes_are_restored() {
    use std::os::unix::fs::PermissionsExt;
    let (_root, m) = setup();
    let path = m.paths(AgentId::Claude).unwrap().remove(0);
    write(&path, b"{}");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
    connect(&m, AgentId::Claude, MODEL);
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        fs::metadata(m.journal_path(AgentId::Claude))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert_eq!(
        fs::metadata(&m.state_dir).unwrap().permissions().mode() & 0o777,
        0o700
    );
    disconnect(&m, AgentId::Claude);
    assert_eq!(
        fs::metadata(path).unwrap().permissions().mode() & 0o777,
        0o640
    );
}
#[cfg(unix)]
#[test]
fn symlinked_agent_parent_is_rejected() {
    let (root, m) = setup();
    let target = root.path().join("elsewhere");
    fs::create_dir_all(&target).unwrap();
    let home = root.path().join("home");
    fs::create_dir_all(&home).unwrap();
    std::os::unix::fs::symlink(&target, home.join(".claude")).unwrap();
    assert_eq!(
        m.preview(AgentId::Claude, Action::Connect, Some(MODEL), gateway())
            .unwrap_err()
            .kind,
        ErrorKind::Conflict
    );
    assert!(!target.join("settings.json").exists());
}
#[cfg(windows)]
#[test]
fn windows_new_secrets_have_private_acls_and_existing_acls_are_preserved() {
    let (_root, m) = setup();
    let path = m.paths(AgentId::Claude).unwrap().remove(0);
    write(&path, b"{}");
    let original = crate::windows::dacl_sddl(&path).unwrap();
    connect(&m, AgentId::Claude, MODEL);
    assert_eq!(crate::windows::dacl_sddl(&path).unwrap(), original);
    disconnect(&m, AgentId::Claude);
    assert_eq!(crate::windows::dacl_sddl(&path).unwrap(), original);
    connect(&m, AgentId::Codex, MODEL);
    for path in [
        m.state_dir.clone(),
        m.journal_path(AgentId::Claude),
        m.paths(AgentId::Codex).unwrap().remove(0),
    ] {
        let dacl = crate::windows::dacl_sddl(&path).unwrap();
        assert!(dacl.starts_with("D:P"), "{dacl}");
        assert_eq!(dacl.matches("(A;").count(), 1, "{dacl}");
    }
}

#[test]
fn goose_comments_and_crlf_survive_wiring() {
    let (_root, m) = setup();
    let paths = m.paths(AgentId::Goose).unwrap();
    write(&paths[0], original(AgentId::Goose, 0));
    super::super::goose::wire_at(&paths[0], MODEL, gateway().port, TOKEN).unwrap();
    let text = fs::read_to_string(&paths[0]).unwrap();
    assert!(text.contains("# original settings\r\n"));
}

fn add_unrelated(id: AgentId, index: usize, path: &Path, value: &str) {
    if id == AgentId::Codex {
        let mut doc: toml_edit::DocumentMut = fs::read_to_string(path).unwrap().parse().unwrap();
        doc["user_note"] = toml_edit::value(value);
        doc["model_providers"]["magpie-lite"]["user_option"] = toml_edit::value(value);
        write(path, format!("{doc}\n# later comment\n").as_bytes());
    } else if id == AgentId::Goose && index == 0 {
        let text = fs::read_to_string(path).unwrap();
        let mut yaml: serde_yaml::Value = serde_yaml::from_str(&text).unwrap();
        yaml["user_note"] = value.into();
        let edited = super::super::selective::edit_yaml(&text, &yaml).unwrap();
        write(path, format!("{edited}\n# later comment\n").as_bytes());
    } else if id == AgentId::Gemini && index == 1 {
        let text = fs::read_to_string(path).unwrap();
        write(
            path,
            format!("{text}\r\n# later comment\r\nUSER_NOTE={value}\r\n").as_bytes(),
        );
    } else {
        let mut v: serde_json::Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        v["user_note"] = value.into();
        if v.get("custom").is_some() {
            v["custom"] = "user changed this".into();
        }
        v.as_object_mut().unwrap().remove("unrelated");
        let provider = match id {
            AgentId::Claude => "/env",
            AgentId::Gemini => "/security/auth",
            AgentId::Crush if index == 0 => "/providers/magpie-lite",
            AgentId::Opencode => "/provider/magpie-lite/options",
            AgentId::Zed => "/language_models/openai_compatible/magpie-lite",
            AgentId::Goose => "/headers",
            _ => "",
        };
        if !provider.is_empty() {
            v.pointer_mut(provider).unwrap()["user_option"] = value.into();
        }
        write(path, serde_json::to_vec_pretty(&v).unwrap());
    }
}
fn assert_unrelated(id: AgentId, index: usize, path: &Path, value: &str) {
    let text = fs::read_to_string(path).unwrap();
    if id == AgentId::Codex {
        let v: toml::Value = text.parse().unwrap();
        assert_eq!(v["user_note"].as_str(), Some(value));
        assert_eq!(
            v["model_providers"]["magpie-lite"]["user_option"].as_str(),
            Some(value)
        );
        assert!(text.contains("# later comment"));
    } else if id == AgentId::Goose && index == 0 {
        let v: serde_yaml::Value = serde_yaml::from_str(&text).unwrap();
        assert_eq!(v["user_note"].as_str(), Some(value));
        assert!(text.contains("# later comment"));
    } else if id == AgentId::Gemini && index == 1 {
        assert!(text.contains(&format!("USER_NOTE={value}\r\n")));
        assert!(text.contains("# later comment\r\n"));
    } else {
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["user_note"].as_str(), Some(value));
        assert!(v.get("unrelated").is_none());
        if v.get("custom").is_some() {
            assert_eq!(v["custom"], "user changed this");
        }
    }
    assert!(!text.contains(TOKEN));
}
#[test]
fn seven_agents_rewire_and_restore_preserve_later_unrelated_edits() {
    for existed in [true, false] {
        let (_root, m) = setup();
        for id in AgentId::ALL {
            let paths = m.paths(id).unwrap();
            if existed {
                for (i, p) in paths.iter().enumerate() {
                    write(p, original(id, i));
                }
            }
            connect(&m, id, MODEL);
            for (i, p) in paths.iter().enumerate() {
                add_unrelated(id, i, p, "before_rewire");
            }
            assert!(
                !m.status(id).unwrap().drifted,
                "unrelated edits flagged {id:?}"
            );
            connect(&m, id, "mock/rewired");
            for (i, p) in paths.iter().enumerate() {
                add_unrelated(id, i, p, "after_rewire");
            }
            let before = images(&m, id);
            let p = m.preview(id, Action::Disconnect, None, gateway()).unwrap();
            assert_eq!(before, images(&m, id), "preview touched files");
            m.apply(id, Action::Disconnect, None, &p.revision, gateway())
                .unwrap();
            for (i, p) in paths.iter().enumerate() {
                assert_unrelated(id, i, p, "after_rewire");
            }
            if existed && id == AgentId::Goose {
                assert!(
                    fs::read_to_string(&paths[1])
                        .unwrap()
                        .contains("custom-provider-secret")
                );
            }
        }
    }
}
#[test]
fn seven_agents_immediate_disconnect_after_rewire_preserves_adopted_edits() {
    for existed in [true, false] {
        let (_root, m) = setup();
        for id in AgentId::ALL {
            let paths = m.paths(id).unwrap();
            if existed {
                for (i, p) in paths.iter().enumerate() {
                    write(p, original(id, i));
                }
            }
            connect(&m, id, MODEL);
            for (i, p) in paths.iter().enumerate() {
                add_unrelated(id, i, p, "before_rewire");
            }
            connect(&m, id, "mock/rewired");
            // Exercise the exact applied-image fast path, without any later edits.
            let p = m.preview(id, Action::Disconnect, None, gateway()).unwrap();
            m.apply(id, Action::Disconnect, None, &p.revision, gateway())
                .unwrap();
            for (i, p) in paths.iter().enumerate() {
                assert_unrelated(id, i, p, "before_rewire");
            }
        }
    }
}
#[test]
fn seven_agents_managed_values_and_atomic_lists_cannot_be_overwritten() {
    let (_root, m) = setup();
    for id in AgentId::ALL {
        connect(&m, id, MODEL);
        let paths = m.paths(id).unwrap();
        match id {
            AgentId::Codex => {
                let mut doc: toml_edit::DocumentMut =
                    fs::read_to_string(&paths[0]).unwrap().parse().unwrap();
                doc["model"] = toml_edit::value("external/model");
                write(&paths[0], doc.to_string().as_bytes());
            }
            AgentId::Goose => {
                let text = fs::read_to_string(&paths[0])
                    .unwrap()
                    .replace(MODEL, "external/model");
                write(&paths[0], text.as_bytes());
            }
            _ => {
                let mut v: serde_json::Value =
                    serde_json::from_slice(&fs::read(&paths[0]).unwrap()).unwrap();
                let ptr = match id {
                    AgentId::Claude => "/env/ANTHROPIC_BASE_URL",
                    AgentId::Gemini => "/security/auth/selectedType",
                    AgentId::Crush => "/providers/magpie-lite/models",
                    AgentId::Opencode => "/provider/magpie-lite/models",
                    AgentId::Zed => {
                        "/language_models/openai_compatible/magpie-lite/available_models"
                    }
                    _ => unreachable!(),
                };
                *v.pointer_mut(ptr).unwrap() = serde_json::json!({"external": true});
                write(&paths[0], serde_json::to_vec(&v).unwrap());
            }
        }
        let before = images(&m, id);
        assert!(m.status(id).unwrap().drifted);
        for action in [Action::Connect, Action::Disconnect] {
            assert_eq!(
                m.preview(id, action, Some(MODEL), gateway())
                    .unwrap_err()
                    .kind,
                ErrorKind::Conflict
            );
        }
        assert_eq!(before, images(&m, id));
    }
}
#[test]
fn replaced_container_shapes_restore_or_refuse_added_children() {
    for (id, raw) in [
        (
            AgentId::Gemini,
            b"{\"model\":\"prior\",\"security\":false}".as_slice(),
        ),
        (AgentId::Crush, b"{\"providers\":[],\"models\":null}"),
        (AgentId::Zed, b"{\"language_models\":\"prior\"}"),
    ] {
        let (_root, m) = setup();
        let path = &m.paths(id).unwrap()[0];
        write(path, raw);
        connect(&m, id, MODEL);
        let mut v: serde_json::Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        v["unrelated_addition"] = true.into();
        write(path, serde_json::to_vec(&v).unwrap());
        assert!(!m.status(id).unwrap().drifted);
        disconnect(&m, id);
        let restored: serde_json::Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        let mut expected: serde_json::Value = serde_json::from_slice(raw).unwrap();
        expected["unrelated_addition"] = true.into();
        assert_eq!(restored, expected);
        connect(&m, id, MODEL);
        let mut v: serde_json::Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        let parent = match id {
            AgentId::Gemini => "security",
            AgentId::Crush => "providers",
            _ => "language_models",
        };
        v[parent]["later"] = true.into();
        write(path, serde_json::to_vec(&v).unwrap());
        assert_eq!(
            m.preview(id, Action::Disconnect, None, gateway())
                .unwrap_err()
                .kind,
            ErrorKind::Conflict
        );
    }
}
#[test]
fn gemini_export_duplicate_multiline_dotenv_and_crlf_are_reversible() {
    let (_root, m) = setup();
    let paths = m.paths(AgentId::Gemini).unwrap();
    let baseline = "\u{feff}# prior\r\n export GEMINI_API_KEY = 'one'\r\nGEMINI_API_KEY=\"two\r\nlines\"\r\nOTHER='keep#literal'\r\n";
    write(&paths[1], baseline.as_bytes());
    connect(&m, AgentId::Gemini, MODEL);
    let wired = fs::read_to_string(&paths[1]).unwrap();
    assert!(wired.starts_with("\u{feff}# prior\r\n"));
    assert_eq!(wired.matches(TOKEN).count(), 2);
    assert!(!wired.contains("two"));
    write(
        &paths[1],
        format!("{wired}# new comment\r\nNEW=value\r\n").as_bytes(),
    );
    disconnect(&m, AgentId::Gemini);
    assert_eq!(
        fs::read_to_string(&paths[1]).unwrap(),
        format!("{baseline}# new comment\r\nNEW=value\r\n")
    );
}
#[test]
fn selective_disconnect_recovery_uses_durable_merged_targets() {
    let (root, m) = setup();
    let id = AgentId::Gemini;
    connect(&m, id, MODEL);
    for (i, path) in m.paths(id).unwrap().iter().enumerate() {
        add_unrelated(id, i, path, "kept");
    }
    let plan = m.plan(id, Action::Disconnect, None, gateway()).unwrap();
    let mut j = plan.journal.unwrap();
    j.phase = Phase::Disconnecting;
    j.rollback = Some(plan.before);
    j.restore = Some(plan.after.clone());
    m.save(&j).unwrap();
    atomic(&j.files[0].path, &plan.after[0]).unwrap();
    drop(m);
    let m = Manager::at_home(&root.path().join("home"), root.path().join("agent-state")).unwrap();
    assert!(!m.recover(id).unwrap().connected);
    assert_eq!(images(&m, id), plan.after);
    for (i, path) in m.paths(id).unwrap().iter().enumerate() {
        assert_unrelated(id, i, path, "kept");
    }
}
#[test]
fn selective_recovery_still_refuses_new_edits_after_the_pending_journal() {
    let (_root, m) = setup();
    let id = AgentId::Crush;
    connect(&m, id, MODEL);
    for (i, path) in m.paths(id).unwrap().iter().enumerate() {
        add_unrelated(id, i, path, "kept");
    }
    let plan = m.plan(id, Action::Disconnect, None, gateway()).unwrap();
    let mut j = plan.journal.unwrap();
    j.phase = Phase::Disconnecting;
    j.rollback = Some(plan.before);
    j.restore = Some(plan.after);
    m.save(&j).unwrap();
    add_unrelated(id, 1, &j.files[1].path, "after_pending");
    let before = images(&m, id);
    assert_eq!(m.recover(id).unwrap_err().kind, ErrorKind::Conflict);
    assert_eq!(images(&m, id), before);
}
#[test]
fn stale_selective_disconnect_preview_is_rejected_even_for_unrelated_edits() {
    let (_root, m) = setup();
    let id = AgentId::Claude;
    connect(&m, id, MODEL);
    let p = m.preview(id, Action::Disconnect, None, gateway()).unwrap();
    add_unrelated(id, 0, &m.paths(id).unwrap()[0], "kept");
    let before = images(&m, id);
    assert_eq!(
        m.apply(id, Action::Disconnect, None, &p.revision, gateway())
            .unwrap_err()
            .kind,
        ErrorKind::Conflict
    );
    assert_eq!(images(&m, id), before);
    disconnect(&m, id);
    assert_unrelated(id, 0, &m.paths(id).unwrap()[0], "kept");
}

#[test]
fn schema_one_backups_upgrade_without_losing_old_goose_provider_fields() {
    let (_root, m) = setup();
    let id = AgentId::Goose;
    let paths = m.paths(id).unwrap();
    let original = br#"{"foreign":"original", "headers":{"Authorization":"prior","X-Old":"keep"}}"#;
    write(&paths[1], original);
    connect(&m, id, MODEL);
    let mut j = m.load(id).unwrap().unwrap();
    j.schema = 1;
    for entry in &mut j.files {
        entry.owned = None;
        entry.restore_base = None;
    }
    // The old adapter dropped these fields when it rewrote the custom file.
    let mut applied: serde_json::Value =
        serde_json::from_slice(&j.files[1].applied.bytes().unwrap().unwrap()).unwrap();
    applied.as_object_mut().unwrap().remove("foreign");
    applied["headers"].as_object_mut().unwrap().remove("X-Old");
    j.files[1].applied.data = Some(STANDARD.encode(serde_json::to_vec(&applied).unwrap()));
    atomic(&paths[1], &j.files[1].applied).unwrap();
    m.save(&j).unwrap();
    applied["user"] = "later".into();
    write(&paths[1], serde_json::to_vec(&applied).unwrap());
    assert!(!m.status(id).unwrap().drifted);
    connect(&m, id, "mock/rewired");
    assert_eq!(m.load(id).unwrap().unwrap().schema, 2);
    disconnect(&m, id);
    let mut expected: serde_json::Value = serde_json::from_slice(original).unwrap();
    expected["user"] = "later".into();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&fs::read(&paths[1]).unwrap()).unwrap(),
        expected
    );
}
#[test]
fn schema_one_pending_disconnect_still_finishes_exact_baseline_restore() {
    let (_root, m) = setup();
    let id = AgentId::Crush;
    connect(&m, id, MODEL);
    let mut j = m.load(id).unwrap().unwrap();
    j.schema = 1;
    j.phase = Phase::Disconnecting;
    for entry in &mut j.files {
        entry.owned = None;
        entry.restore_base = None;
    }
    m.save(&j).unwrap();
    atomic(&j.files[0].path, &j.files[0].baseline).unwrap();
    assert!(!m.recover(id).unwrap().connected);
    assert!(images(&m, id).iter().all(|i| i.data.is_none()));
    assert_eq!(m.load(id).unwrap().unwrap().schema, 2);
}
#[test]
fn schema_two_pending_images_and_ownership_are_validated_before_writing() {
    let (_root, m) = setup();
    let id = AgentId::Claude;
    connect(&m, id, MODEL);
    let j = m.load(id).unwrap().unwrap();
    let before = images(&m, id);
    let mut bad = j.clone();
    bad.files[0].owned = Some(vec![vec![]]);
    m.save(&bad).unwrap();
    assert_eq!(m.status(id).unwrap_err().kind, ErrorKind::Conflict);
    let mut bad = j.clone();
    bad.phase = Phase::Disconnecting;
    m.save(&bad).unwrap();
    assert_eq!(m.recover(id).unwrap_err().kind, ErrorKind::Conflict);
    let mut bad = j.clone();
    bad.phase = Phase::Disconnecting;
    bad.rollback = Some(before.clone());
    bad.restore = Some(vec![]);
    m.save(&bad).unwrap();
    assert_eq!(m.recover(id).unwrap_err().kind, ErrorKind::Conflict);
    let mut bad = j;
    bad.phase = Phase::Disconnecting;
    bad.rollback = Some(before.clone());
    bad.restore = Some(vec![Image {
        data: Some("bad base64 $".into()),
        mode: None,
    }]);
    m.save(&bad).unwrap();
    assert_eq!(m.recover(id).unwrap_err().kind, ErrorKind::Conflict);
    assert_eq!(images(&m, id), before);
}
#[test]
fn seven_agents_selective_pending_disconnects_recover_after_restart() {
    let (root, m) = setup();
    let mut targets = Vec::new();
    for id in AgentId::ALL {
        connect(&m, id, MODEL);
        for (i, path) in m.paths(id).unwrap().iter().enumerate() {
            add_unrelated(id, i, path, "kept");
        }
        let plan = m.plan(id, Action::Disconnect, None, gateway()).unwrap();
        let mut j = plan.journal.unwrap();
        j.phase = Phase::Disconnecting;
        j.rollback = Some(plan.before);
        j.restore = Some(plan.after.clone());
        m.save(&j).unwrap();
        atomic(&j.files[0].path, &plan.after[0]).unwrap();
        targets.push((id, plan.after));
    }
    drop(m);
    let m = Manager::at_home(&root.path().join("home"), root.path().join("agent-state")).unwrap();
    for (id, target) in targets {
        assert!(!m.recover(id).unwrap().connected);
        assert_eq!(images(&m, id), target);
    }
}
#[test]
fn interrupted_rewire_keeps_unrelated_edits_and_first_owned_baseline() {
    let (_root, m) = setup();
    let id = AgentId::Gemini;
    let paths = m.paths(id).unwrap();
    write(&paths[0], b"{\"model\":{\"name\":\"original\"},\"security\":{\"auth\":{\"selectedType\":\"native\"}}}");
    connect(&m, id, MODEL);
    for (i, path) in paths.iter().enumerate() {
        add_unrelated(id, i, path, "kept");
    }
    let before = images(&m, id);
    // Reproduce a schema 2 interrupted rewire using the actual staged plan.
    let plan = m
        .plan(id, Action::Connect, Some("mock/next"), gateway())
        .unwrap();
    let mut j = plan.journal.unwrap();
    j.phase = Phase::Connecting;
    j.prior_model = Some(j.model.clone());
    j.model = "mock/next".into();
    j.rollback = Some(plan.before);
    for ((entry, after), retained) in j
        .files
        .iter_mut()
        .zip(plan.after)
        .zip(plan.restored.unwrap())
    {
        entry.applied = after;
        entry.restore_base = Some(retained);
    }
    m.save(&j).unwrap();
    atomic(&j.files[0].path, &j.files[0].applied).unwrap();
    assert!(m.recover(id).unwrap().connected);
    assert_eq!(images(&m, id), before);
    disconnect(&m, id);
    let restored: serde_json::Value =
        serde_json::from_slice(&fs::read(&paths[0]).unwrap()).unwrap();
    assert_eq!(restored["model"]["name"], "original");
    assert_eq!(restored["security"]["auth"]["selectedType"], "native");
    for (i, path) in paths.iter().enumerate() {
        assert_unrelated(id, i, path, "kept");
    }
}
#[test]
fn toml_and_yaml_comments_inside_removed_generated_containers_are_retained() {
    for id in [AgentId::Codex, AgentId::Goose] {
        let (_root, m) = setup();
        let path = &m.paths(id).unwrap()[0];
        if id == AgentId::Goose {
            write(path, b"active_provider: original\nproviders: {}\n");
        }
        connect(&m, id, MODEL);
        let wired = fs::read_to_string(path).unwrap();
        let with_comment = if id == AgentId::Codex {
            wired.replace("base_url =", "# user note inside provider\nbase_url =")
        } else {
            wired.replace(
                "\"enabled\":",
                "\n # user note inside provider\n \"enabled\":",
            )
        };
        assert!(with_comment.contains("# user note inside provider"));
        write(path, with_comment.as_bytes());
        assert!(!m.status(id).unwrap().drifted);
        disconnect(&m, id);
        let restored = fs::read_to_string(path).unwrap();
        assert!(restored.contains("# user note inside provider"));
        assert!(!restored.contains(TOKEN));
    }
}
#[test]
fn goose_nested_flow_yaml_preserves_unrelated_comments_and_block_values() {
    let (_root, m) = setup();
    let paths = m.paths(AgentId::Goose).unwrap();
    let original = "# top\r\nactive_provider: native # active\r\nproviders:\r\n  magpie-lite: {model: original, custom: 'keep#literal'}\r\n  native:\r\n    text: |\r\n      # not a comment: literal data\r\n      keep this\r\n";
    write(&paths[0], original.as_bytes());
    connect(&m, AgentId::Goose, MODEL);
    let wired = fs::read_to_string(&paths[0]).unwrap();
    assert!(wired.contains("# top\r\n"));
    assert!(wired.contains("# active"));
    assert!(wired.contains("      # not a comment: literal data\r\n      keep this\r\n"));
    write(
        &paths[0],
        format!("{wired}# later\r\nnew: kept\r\n").as_bytes(),
    );
    disconnect(&m, AgentId::Goose);
    let restored = fs::read_to_string(&paths[0]).unwrap();
    let v: serde_yaml::Value = serde_yaml::from_str(&restored).unwrap();
    assert_eq!(v["active_provider"], "native");
    assert_eq!(v["providers"]["magpie-lite"]["model"], "original");
    assert_eq!(v["providers"]["magpie-lite"]["custom"], "keep#literal");
    assert_eq!(v["new"], "kept");
    assert!(restored.contains("# later\r\n"));
}
#[test]
fn comments_only_and_bom_text_configs_connect_and_selectively_restore() {
    for id in [AgentId::Codex, AgentId::Goose] {
        let (_root, m) = setup();
        let path = &m.paths(id).unwrap()[0];
        write(path, "\u{feff}# prior\r\n".as_bytes());
        connect(&m, id, MODEL);
        let wired = fs::read_to_string(path).unwrap();
        assert!(wired.starts_with("\u{feff}"));
        assert!(wired.contains("# prior\r\n"));
        write(path, format!("{wired}\r\n# later\r\n").as_bytes());
        disconnect(&m, id);
        let restored = fs::read_to_string(path).unwrap();
        assert!(restored.starts_with("\u{feff}"));
        assert!(restored.contains("# prior\r\n"));
        assert!(restored.contains("# later\r\n"));
        assert!(!restored.contains("magpie-lite"));
    }
}
