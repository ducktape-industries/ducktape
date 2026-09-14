//! App state regressions and native UI/kernel contracts.
use super::*;
mod bell;
mod canary;
mod connection;
mod design;
mod font_fallback;
mod huddle_live;
mod messages;
mod rooms;
mod sends;
mod shell;
mod stream;
mod window_lifecycle;
use crate::composer_surface::testing as composer;
const RAIL_THREAD_SEQ: i64 = 7;
fn message(seq: i64, body: &str, deleted: bool) -> backend::ChatMessage {
    backend::ChatMessage {
        id: format!("message-{seq}"),
        view_key: seq,
        seq,
        author: "user".into(),
        meta: format!("#{seq}"),
        edit_body: body.into(),
        body: body.into(),
        blocks: backend::paragraph_blocks(body),
        pending: false,
        rev: 2,
        edited: false,
        deleted,
        reply_count: 0,
        thread_seq: 0,
        show_author: true,
        initial: "U".into(),
        avatar_kind: "human".into(),
        height: 0,
        time: 0,
        reactions: Vec::new(),
        render_rev: 0,
    }
}

fn composer_scope(app: &Ducktape) -> String {
    backend::composer_scope(&app.connected_rpc, &app.active_channel)
}

fn reply_composer_scope(app: &Ducktape, thread_seq: i64) -> String {
    backend::thread_scope(&app.connected_rpc, &app.active_channel, thread_seq)
}

fn submit(app: &mut Ducktape, kind: ComposerKind, body: &str) -> String {
    let id = backend::fresh_operation_id(composer_op_prefix(kind));
    let scope = match kind {
        ComposerKind::Message => composer_scope(app),
        ComposerKind::Reply => reply_composer_scope(app, RAIL_THREAD_SEQ),
        ComposerKind::Edit | ComposerKind::ThreadEdit => {
            backend::edit_scope(&app.connected_rpc, &app.active_channel, app.chat_edit_seq)
        }
    };
    let _ = app.update(AppMessage::ComposerSubmitted(
        kind,
        body.to_owned(),
        id.clone(),
        scope,
    ));
    id
}

fn composer_intent(scope: &str, kind: &str, body: &str) -> module_view::ModuleViewEvent {
    module_view::ModuleViewEvent {
        kind: "composer".into(),
        detail: serde_json::json!({
            "scope": scope,
            "kind": kind,
            "body": body,
            "id": backend::fresh_operation_id(kind.to_owned()),
        })
        .to_string(),
    }
}

fn composer_text(scope: &str) -> String {
    composer::text(scope).trim().to_owned()
}

fn composer_stash(scope: &str) -> String {
    composer::failed(scope)
}

fn live_refresh(generation: i64, active_channel: &str) -> backend::LiveRefresh {
    backend::LiveRefresh {
        generation,
        chat_loaded: true,
        channels: Vec::new(),
        active_channel: active_channel.into(),
        active_channel_name: active_channel.into(),
        active_channel_archived: false,
        active_channel_members_only: false,
        huddle_roster: Vec::new(),
        channel_members: Vec::new(),
    }
}

fn posted_delta(channel: &str, row: backend::ChatMessage) -> backend::LiveUpdate {
    backend::LiveUpdate {
        kind: LiveKind::Chat,
        status: "Live".into(),
        height: row.seq.max(1),
        chat: vec![backend::ChatDelta::Posted {
            channel_id: channel.into(),
            seq: row.seq,
            message: row,
        }],
        ..backend::LiveUpdate::default()
    }
}

fn chat_data(active_channel: &str) -> backend::ChatData {
    backend::ChatData {
        generation: 0,
        channels: Vec::new(),
        active_channel: active_channel.into(),
        active_channel_name: active_channel.into(),
        active_channel_archived: false,
        active_channel_members_only: false,
        huddle_roster: Vec::new(),
        channel_members: Vec::new(),
    }
}

fn stale_chat_hit() -> backend::ChatSearchHit {
    backend::ChatSearchHit {
        channel_id: "old".into(),
        seq: 1,
        root_seq: 1,
        author: "user".into(),
        text: "stale".into(),
        meta: "#1".into(),
    }
}

fn stale_page_hit() -> backend::PageSearchHit {
    backend::PageSearchHit {
        page_id: "old".into(),
        page_title: "Old".into(),
        block_id: "old-block".into(),
        kind: "Text".into(),
        text: "stale".into(),
    }
}

fn workspace(active_channel: &str) -> backend::WorkspaceData {
    backend::WorkspaceData {
        generation: 0,
        rpc: "http://node".into(),
        status: "current".into(),
        height: 1,
        channels: Vec::new(),
        active_channel: active_channel.into(),
        active_channel_name: active_channel.into(),
        active_channel_archived: false,
        active_channel_members_only: false,
        huddle_roster: Vec::new(),
        channel_members: Vec::new(),
    }
}

fn room(id: &str, head: i64) -> backend::ChatChannel {
    backend::ChatChannel {
        id: id.into(),
        name: id.into(),
        archived: false,
        members_only: false,
        huddle_count: 0,
        huddle: Vec::new(),
        head_seq: head,
    }
}

fn type_into(scope: &str, text: &str) {
    composer::append(scope, text);
}
fn seed_composer(scope: &str, text: &str) {
    composer::replace(scope, text);
}
fn restore_composer(scope: &str, blocked: bool) {
    composer::restore(scope, blocked);
}
fn submit_composer(app: &mut Ducktape, scope: &str, kind: ComposerKind, blocked: bool) {
    let Some(value) = composer::submit(scope, &composer_op_prefix(kind), blocked) else {
        return;
    };
    let event = composer_surface::intent(&value).expect("native composer submit intent");
    let task = app.update(AppMessage::ChatViewEvent(event));
    pump(app, task);
}
/// Drain only this task's messages. Follow-up I/O belongs to the caller's fixture.
fn pump(app: &mut Ducktape, task: ducktape_view_guest::Task<AppMessage>) {
    use futures::StreamExt;
    let messages: Vec<_> = futures::executor::block_on(task.into_stream().collect());
    for message in messages {
        let _ = app.update(message);
    }
}
fn command_chord(key: &str) -> crate::shell::KeyPress {
    crate::shell::KeyPress {
        key: key.into(),
        modifiers: gpui_kit::Modifiers {
            platform: cfg!(target_os = "macos"),
            control: !cfg!(target_os = "macos"),
            ..Default::default()
        },
    }
}

/// Parse Rust tokens so formatting and comments cannot satisfy a source rule.
pub(crate) fn rust_tokens(source: &str) -> String {
    std::thread::scope(|scope| {
        std::thread::Builder::new()
            .stack_size(16 * 1024 * 1024)
            .spawn_scoped(scope, || rust_tokens_on_stack(source))
            .unwrap()
            .join()
            .unwrap()
    })
}

fn rust_tokens_on_stack(source: &str) -> String {
    use quote::ToTokens;
    syn::parse_file(source)
        .expect("valid Rust source")
        .to_token_stream()
        .to_string()
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect()
}
pub(crate) fn handler_bodies() -> Vec<(String, String)> {
    use quote::ToTokens;
    use syn::visit::Visit;
    struct Handlers {
        bodies: Vec<(String, String)>,
        methods: std::collections::BTreeMap<String, syn::Block>,
    }
    impl<'ast> Visit<'ast> for Handlers {
        fn visit_arm(&mut self, arm: &'ast syn::Arm) {
            let path = match &arm.pat {
                syn::Pat::Path(path) => Some(&path.path),
                syn::Pat::TupleStruct(tuple) => Some(&tuple.path),
                _ => None,
            };
            if let Some(path) = path {
                let parts: Vec<_> = path
                    .segments
                    .iter()
                    .map(|part| part.ident.to_string())
                    .collect();
                if parts.len() == 2 && parts[0] == "AppMessage" {
                    assert!(arm.guard.is_none(), "message dispatch has no match guards");
                    let expression = match arm.body.as_ref() {
                        syn::Expr::Block(block) => match block.block.stmts.as_slice() {
                            [syn::Stmt::Expr(expression, None)] => expression,
                            _ => panic!("a dispatch arm contains only its handler call"),
                        },
                        expression => expression,
                    };
                    let syn::Expr::MethodCall(call) = expression else {
                        panic!("each message delegates to its named handler");
                    };
                    let handler = self
                        .methods
                        .get(&call.method.to_string())
                        .expect("the dispatched handler exists");
                    self.bodies.push((
                        parts[1].clone(),
                        handler
                            .to_token_stream()
                            .to_string()
                            .chars()
                            .filter(|character| !character.is_whitespace())
                            .collect(),
                    ));
                }
            }
            syn::visit::visit_arm(self, arm);
        }
    }
    let source = syn::parse_file(include_str!("ui/app_update.rs")).expect("native update Rust");
    let methods = source
        .items
        .iter()
        .filter_map(|item| match item {
            syn::Item::Impl(item) => Some(&item.items),
            _ => None,
        })
        .flatten()
        .filter_map(|item| match item {
            syn::ImplItem::Fn(function) => {
                Some((function.sig.ident.to_string(), function.block.clone()))
            }
            _ => None,
        })
        .collect();
    let mut found = Handlers {
        bodies: Vec::new(),
        methods,
    };
    found.visit_file(&source);
    assert!(!found.bodies.is_empty(), "real native handlers are present");
    found.bodies
}
pub(crate) fn handler_body(variant: &str) -> String {
    let mut found = handler_bodies()
        .into_iter()
        .filter(|(name, _)| name == variant);
    let (_, body) = found
        .next()
        .unwrap_or_else(|| panic!("missing native handler {variant}"));
    assert!(found.next().is_none(), "one handler per message variant");
    body
}

fn composer_op_prefix(kind: ComposerKind) -> String {
    match kind {
        ComposerKind::Message => "message",
        ComposerKind::Reply => "reply",
        ComposerKind::Edit | ComposerKind::ThreadEdit => "edit",
    }
    .into()
}
