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
            })
            .collect(),
        rollback: Some(plan.before),
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
