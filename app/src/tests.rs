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

fn live_refresh(generation: i64, active_channel: &str) -> backend::LiveRefresh {
    backend::LiveRefresh {
        generation,
        chat_loaded: true,
        channels: Vec::new(),
        active_channel: active_channel.into(),
        active_channel_name: active_channel.into(),
        active_channel_archived: false,
        huddle_roster: Vec::new(),
    }
}

fn posted_delta(channel: &str, row: backend::ChatMessage) -> backend::LiveUpdate {
    backend::LiveUpdate {
        kind: LiveKind::Chat,
        status: "Live".into(),
        height: row.seq.max(1),
        chat: vec![backend::ChatDelta::Head {
            channel_id: channel.into(),
            seq: row.seq,
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
        huddle_roster: Vec::new(),
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
        huddle_roster: Vec::new(),
    }
}

fn room(id: &str, head: i64) -> backend::ChatChannel {
    backend::ChatChannel {
        id: id.into(),
        name: id.into(),
        archived: false,
        members_only: false,
        huddle_count: 0,
        voice: false,
        huddle: Vec::new(),
        head_seq: head,
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
/// One named method of the native update impl, whitespace stripped like a
/// handler body — for the helpers several handlers delegate to.
pub(crate) fn fn_body(name: &str) -> String {
    use quote::ToTokens;
    let source = syn::parse_file(include_str!("ui/app_update.rs")).expect("native update Rust");
    source
        .items
        .iter()
        .filter_map(|item| match item {
            syn::Item::Impl(item) => Some(&item.items),
            _ => None,
        })
        .flatten()
        .find_map(|item| match item {
            syn::ImplItem::Fn(function) if function.sig.ident == name => Some(
                function
                    .block
                    .to_token_stream()
                    .to_string()
                    .chars()
                    .filter(|character| !character.is_whitespace())
                    .collect(),
            ),
            _ => None,
        })
        .unwrap_or_else(|| panic!("missing native method {name}"))
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
