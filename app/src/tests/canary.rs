//! Deployment canary: real native scenes, hydrated WASM trees, and optional GPU pixels.
//! DUCKTAPE_CANARY_PIXELS=1 requires an actual screenshot backend; unsupported
//! platforms fail explicitly. No polling or synthetic screenshot substitutes.
use crate::module_view::canary::{frame, seated_hash, tap, texts};
use gpui_kit::{AppContext, px, size};
use std::path::{Path, PathBuf};
fn component(module: &str) -> Vec<u8> {
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("../target/views/{module}_view.wasm"));
    std::fs::read(&path)
        .unwrap_or_else(|error| panic!("{}: {error}; run make views", path.display()))
}
fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
}
fn marker(module: &str) -> &'static str {
    match module {
        "files" => "+ Folder · B",
        "pages" => "Pages · B",
        "chat" => "CHANNELS · B",
        _ => "CANARY B",
    }
}

struct Transition {
    module: String,
    state: String,
    hash: String,
}

/// `module=… hash=… state=… gen=… reason=…`, as `view_source` logs it.
fn parse(line: &str) -> Option<Transition> {
    let field = |name: &str| {
        line.split_whitespace()
            .find_map(|pair| pair.strip_prefix(name)?.strip_prefix('='))
            .map(str::to_owned)
    };
    Some(Transition {
        module: field("module")?,
        state: field("state")?,
        hash: field("hash")?,
    })
}

fn capture(module: &'static str, path: &Path) -> Vec<u8> {
    let mut cx = crate::frame_probe::headless_context();
    let mut app = super::Ducktape::initial_state();
    app.connected = true;
    app.shell_tab = match module {
        "chat" => super::ShellTab::Chat,
        "pages" => super::ShellTab::Pages,
        "files" => super::ShellTab::Files,
        "members" => super::ShellTab::Members,
        "governance" => super::ShellTab::Governance,
        "forge" => super::ShellTab::Forge,
        _ => panic!("unknown canary module {module}"),
    };
    let (spec, _) = app.native_view();
    let window = cx
        .open_window(size(px(900.), px(600.)), |window, cx| {
            let view = cx.new(|cx| {
                let mut view = crate::module_view::NativeModuleView::new(module);
                view.set_props(spec.props, cx);
                view
            });
            cx.new(|cx| gpui_kit::component::Root::new(view, window, cx))
        })
        .unwrap();
    cx.update_window(window.into(), |_, window, cx| {
        window.draw(cx).clear(cx);
    })
    .unwrap();
    cx.run_until_parked();
    let bytes = ui_lang_wire::encode(&frame(module));
    std::fs::write(path.with_extension("wire"), &bytes).unwrap();
    std::fs::write(path.with_extension("txt"), texts(module).join("\n")).unwrap();
    if std::env::var_os("DUCKTAPE_CANARY_PIXELS").is_some() {
        cx.capture_screenshot(window.into())
            .expect("requested real GPU screenshot unavailable")
            .save(path.with_extension("png"))
            .unwrap();
    }
    bytes
}
fn seal(colour: &str) -> Vec<u8> {
    format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="24" height="24"><circle cx="12" cy="12" r="11" fill="{colour}"/></svg>"#
    )
    .into_bytes()
}

fn artifact(component: &[u8], code: Vec<u8>, seal: Vec<u8>) -> module_artifact::ModuleArtifact {
    module_artifact::ModuleArtifact {
        component: code,
        index: None,
        view: Some(module_artifact::ViewArtifact {
            component: component.to_vec(),
            assets: [("icons/seal.svg".to_owned(), seal)].into(),
        }),
    }
}

#[test]
fn a_capture_draws_the_module_it_names() {
    use crate::backend::view_source::tests::{FakeDeployment, fake_node};
    let runtime = runtime();
    let _turn = runtime.block_on(crate::module_view::canary::connection_turn());
    let governance = artifact(&component("governance"), vec![7], seal("#d00000"));
    let chat = artifact(&component("chat"), vec![8], seal("#d00000"));
    let node = FakeDeployment::serving("governance", &governance);
    node.artifacts.lock().unwrap().push(chat.clone());
    *node.status.lock().unwrap() = serde_json::json!({"module_status":{"modules":[
        {"module_id":"governance","active_code_hash":governance.hash(),"pending":null,"history":[{"height":7,"code_hash":governance.hash()}]},
        {"module_id":"chat","active_code_hash":chat.hash(),"pending":null,"history":[{"height":7,"code_hash":chat.hash()}]}
    ]}});
    let client = runtime.block_on(fake_node(node));
    crate::module_view::connected(&client).joined();
    let out = tempfile::tempdir().unwrap();
    let a = capture("governance", &out.path().join("governance"));
    let b = capture("chat", &out.path().join("chat"));
    assert_eq!(seated_hash("governance"), Some(governance.hash()));
    assert_eq!(seated_hash("chat"), Some(chat.hash()));
    assert_ne!(a, b);
    assert!(texts("chat").iter().any(|text| text == "Channels"));
}
#[test]
fn the_canary_captures_every_transition_of_a_deployment() {
    use crate::backend::view_source::tests::{FakeDeployment, fake_node};
    let runtime = runtime();
    let _turn = runtime.block_on(crate::module_view::canary::connection_turn());
    let component = component("governance");
    let a = artifact(&component, vec![1], seal("#d00000"));
    let b = artifact(&component, vec![2], seal("#d00000"));
    let bp = artifact(&component, vec![2], seal("#0000d0"));
    let removed = module_artifact::ModuleArtifact::component(vec![3]);
    let node = FakeDeployment::serving("governance", &a);
    let client = runtime.block_on(fake_node(node.clone()));
    let transitions = tap();
    let out = tempfile::tempdir().unwrap();
    let mut captures = Vec::new();
    crate::module_view::connected(&client).joined();
    for (index, artifact) in [&a, &b, &bp, &removed].into_iter().enumerate() {
        if index > 0 {
            node.deploy("governance", artifact);
            runtime
                .block_on(crate::module_view::deployments_checked())
                .joined();
        }
        let hash = crate::backend::hex_encode(&artifact.hash());
        let lines = transitions.try_iter().collect::<Vec<_>>().join("\n");
        assert!(
            lines.contains(&format!("module=governance hash={hash}")),
            "{lines}"
        );
        let state = match index {
            0 => "",
            1 | 2 => "state=Swapped",
            _ => "state=Missing",
        };
        assert!(lines.contains(state), "{lines}");
        assert_eq!(seated_hash("governance"), Some(artifact.hash()));
        captures.push(capture(
            "governance",
            &out.path().join(format!("{index}-{hash}")),
        ));
    }
    let has =
        |bytes: &[u8], needle: &[u8]| bytes.windows(needle.len()).any(|slice| slice == needle);
    assert!(
        has(&captures[1], b"#d00000"),
        "B delivers red SVG to native renderer"
    );
    assert!(
        has(&captures[2], b"#0000d0"),
        "asset-only B-prime delivers blue SVG"
    );
    assert!(!has(&captures[2], b"#d00000"));
    assert!(texts("governance").is_empty());
    assert!(frame("governance").is_none());
    assert!(!has(&captures[3], b"#0000d0"));
    if std::env::var_os("DUCKTAPE_CANARY_PIXELS").is_some() {
        let pixels = |index, artifact: &module_artifact::ModuleArtifact, blue| {
            let hash = crate::backend::hex_encode(&artifact.hash());
            image::open(out.path().join(format!("{index}-{hash}.png")))
                .unwrap()
                .into_rgba8()
                .pixels()
                .filter(|p| {
                    if blue {
                        p[2] > 150 && p[0] < 80 && p[1] < 80
                    } else {
                        p[0] > 150 && p[1] < 80 && p[2] < 80
                    }
                })
                .count()
        };
        assert!(pixels(1, &b, false) > 200);
        assert_eq!(pixels(1, &b, true), 0);
        assert!(pixels(2, &bp, true) > 200);
        assert_eq!(pixels(2, &bp, false), 0);
        assert_eq!(pixels(3, &removed, true), 0);
    }
}
/// A live chain is supplied by a disposable fixture, never booted or restarted
/// by this test. The deployment CLI changes only its Chat view artifact.
/// B keeps A's snapshot schema and changes the visible CHANNELS marker. C
/// keeps the manifest/ABI valid but refuses restore: invalid static metadata
/// is rejected by node readiness and never reaches the desktop replacement.
#[test]
#[ignore = "needs a disposable live node, node CLI, and A/B/C view artifacts"]
fn canary_follows_a_live_node() {
    use futures::StreamExt;
    use gpui_kit::test::TestWindowExt as _;
    let node = std::env::var("DUCKTAPE_NODE").expect("DUCKTAPE_NODE");
    let fixture =
        PathBuf::from(std::env::var("DUCKTAPE_CANARY_FIXTURE").expect("DUCKTAPE_CANARY_FIXTURE"));
    let cli = std::env::var("DUCKTAPE_NODE_BIN").expect("DUCKTAPE_NODE_BIN");
    let out = fixture.join("evidence");
    std::fs::create_dir_all(&out).unwrap();
    let runtime = runtime();
    let _runtime = runtime.enter();
    let _turn = runtime.block_on(crate::module_view::canary::connection_turn());
    let transitions = tap();
    let workspace = runtime
        .block_on(crate::backend::connect(node.clone(), 0, 0))
        .expect("connect real node");
    let mut height = workspace.height;
    let mut live = crate::backend::live_events(node.clone());
    let mut cx = crate::frame_probe::headless_context();
    let mut app = super::Ducktape::initial_state();
    app.connected = true;
    app.connected_rpc = node;
    app.shell_tab = super::ShellTab::Chat;
    let (spec, _) = app.native_view();
    let mut entity = None;
    let window = cx
        .open_window(size(px(1100.), px(800.)), |window, cx| {
            let view = cx.new(|cx| {
                let mut view = crate::module_view::NativeModuleView::new("chat");
                view.set_props(spec.props, cx);
                view
            });
            entity = Some(view.clone());
            cx.new(|cx| gpui_kit::component::Root::new(view, window, cx))
        })
        .unwrap();
    let entity = entity.unwrap();
    let identity = entity.entity_id();
    cx.update_window(window.into(), |_, window, cx| window.render_frame(cx))
        .unwrap();
    cx.run_until_parked();
    // A node can transiently refuse one initial module read while still
    // serving the workspace. Let its real block feed drive normal recovery.
    let mut initial_events = 0;
    while frame("chat").is_none() {
        let update = runtime
            .block_on(live.next())
            .expect("live block stream closed");
        height = height.max(update.height);
        drop(update);
        initial_events += 1;
        assert!(
            initial_events < 250,
            "Chat did not become ready on live block events"
        );
        cx.update_window(window.into(), |_, window, cx| window.render_frame(cx))
            .unwrap();
        cx.run_until_parked();
    }
    let input = |label: &str| {
        let mut root = frame("chat").expect("live Chat frame");
        let mut found = None;
        root.for_each_mut(&mut |node| {
            if let ui_lang_wire::Node::Input {
                key,
                options,
                value,
                ..
            } = node
                && options.label == label
            {
                found = Some((key.clone(), value.clone()));
            }
        });
        found.expect("live native input")
    };
    let draft = "retained-live-draft-오리";
    let (key, _) = input("Search messages");
    cx.update_window(window.into(), |_, window, cx| {
        window.click(key.clone(), cx);
        window.input(draft, cx);
        window.press("shift-left", cx);
        window.press("shift-left", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(window.into(), |_, window, cx| window.render_frame(cx))
        .unwrap();
    assert_eq!(
        input("Search messages").1,
        draft,
        "native typing reaches guest state"
    );
    let mut expected_draft = draft.to_owned();
    let mut expected_presentation = cx
        .update_window(window.into(), |_, window, cx| {
            crate::module_view::canary::input_presentation(entity.read(cx), &key, window, cx)
                .expect("focused native Chat field")
        })
        .unwrap();
    assert_eq!(expected_presentation.0, draft);
    assert_eq!(expected_presentation.1, draft.len() - "오리".len());
    assert_eq!(
        expected_presentation.2,
        draft.len() - "오리".len()..draft.len()
    );
    assert!(expected_presentation.3, "native Search field is focused");
    let mut hash = seated_hash("chat").expect("A deployed hash");
    assert!(
        !texts("chat")
            .iter()
            .any(|text| text.contains(marker("chat")))
    );
    let mut log = format!(
        "phase=A height={height} native_entity={identity:?} hash={hash:?} draft_preserved=true focus_preserved=true selection={:?}\n",
        expected_presentation.2
    );
    std::fs::write(out.join("live-canary.log"), &log).unwrap();
    for (variant, expected_state) in [("B", "Swapped"), ("C", "Failed")] {
        let view = fixture.join(format!("view-{variant}.wasm"));
        let component = fixture.join("modules/chat.component.wasm");
        let index = fixture.join("modules/chat.index.wasm");
        let artifact = module_artifact::ModuleArtifact {
            component: std::fs::read(&component).unwrap(),
            index: Some(std::fs::read(&index).unwrap()),
            view: Some(module_artifact::ViewArtifact {
                component: std::fs::read(&view).unwrap(),
                assets: Default::default(),
            }),
        };
        let expected_hash = artifact.hash();
        let deployed = std::process::Command::new(&cli)
            .env("DUCKTAPE_HOME", fixture.join("home"))
            .args(["module", "update", "chat"])
            .arg(component)
            .arg("--index")
            .arg(index)
            .arg("--view")
            .arg(view)
            .args(["--after", "50", "--config"])
            .arg(fixture.join("workspace/node.toml"))
            .output()
            .expect("start deployment CLI");
        std::fs::write(
            out.join(format!("deploy-{variant}.log")),
            [&deployed.stdout[..], &deployed.stderr[..]].concat(),
        )
        .unwrap();
        assert!(
            deployed.status.success(),
            "deployment {variant} refused; see evidence log"
        );
        let before = height;
        let mut blocks = 0;
        loop {
            let update = runtime
                .block_on(live.next())
                .expect("live block stream closed");
            if update.kind == super::LiveKind::Tip {
                height = update.height;
                blocks += 1;
                assert!(
                    blocks < 250,
                    "deployment did not reach host within 250 actual block events"
                );
            }
            drop(update);
            // The production Tip handler initiates deployment checks. No
            // test call requests a reload or replaces the mounted entity.
            cx.update_window(window.into(), |_, window, cx| window.render_frame(cx))
                .unwrap();
            cx.run_until_parked();
            let arrived = transitions.try_iter().any(|line| {
                log.push_str(&line);
                log.push('\n');
                parse(&line).is_some_and(|step| {
                    step.module == "chat"
                        && step.state == expected_state
                        && step.hash == crate::backend::hex_encode(&expected_hash)
                })
            });
            if !arrived {
                continue;
            }
            assert!(height > before, "chain must advance through deployment");
            assert_eq!(entity.entity_id(), identity, "native view was not replaced");
            assert_eq!(
                input("Search messages").1,
                expected_draft,
                "snapshot must preserve typed guest draft"
            );
            let presentation = cx
                .update_window(window.into(), |_, window, cx| {
                    crate::module_view::canary::input_presentation(
                        entity.read(cx),
                        &key,
                        window,
                        cx,
                    )
                    .expect("retained native Chat field")
                })
                .unwrap();
            assert_eq!(
                presentation, expected_presentation,
                "WASM replacement preserves native focus, Unicode selection, and caret"
            );
            assert!(
                texts("chat")
                    .iter()
                    .any(|text| text.contains(marker("chat"))),
                "B's visible marker must remain"
            );
            match variant {
                "B" => {
                    assert_ne!(hash, expected_hash);
                    assert_eq!(seated_hash("chat"), Some(expected_hash));
                    hash = expected_hash;
                    // Editing B proves the replacement field targets its fresh
                    // guest handler table, not A's retired callback identities.
                    cx.update_window(window.into(), |_, window, cx| {
                        window.input("한글", cx);
                        window.press("shift-left", cx);
                        window.press("shift-left", cx);
                    })
                    .unwrap();
                    cx.run_until_parked();
                    cx.update_window(window.into(), |_, window, cx| window.render_frame(cx))
                        .unwrap();
                    expected_draft = draft.replace("오리", "한글");
                    assert_eq!(input("Search messages").1, expected_draft);
                    expected_presentation.0 = expected_draft.clone();
                    let presentation = cx
                        .update_window(window.into(), |_, window, cx| {
                            crate::module_view::canary::input_presentation(
                                entity.read(cx),
                                &key,
                                window,
                                cx,
                            )
                            .expect("B input uses fresh native handlers")
                        })
                        .unwrap();
                    assert_eq!(presentation, expected_presentation);
                }
                "C" => assert_eq!(seated_hash("chat"), Some(hash), "failed C preserves B"),
                _ => unreachable!(),
            }
            log.push_str(&format!("phase={variant} height={height} native_entity={identity:?} seated_hash={hash:?} draft_preserved=true focus_preserved=true selection={:?} fresh_handler_verified=true\n", expected_presentation.2));
            std::fs::write(
                out.join(format!("{variant}.wire")),
                ui_lang_wire::encode(&frame("chat")),
            )
            .unwrap();
            std::fs::write(out.join(format!("{variant}.txt")), texts("chat").join("\n")).unwrap();
            std::fs::write(out.join("live-canary.log"), &log).unwrap();
            break;
        }
    }
}
