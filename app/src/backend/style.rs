use super::*;

pub(crate) fn live_update(kind: crate::LiveKind, status: &str, height: i64) -> LiveUpdate {
    LiveUpdate {
        kind,
        status: status.into(),
        height,
        module: String::new(),
        load_chat: kind == crate::LiveKind::Ready,
        debounce: false,
        chat: Vec::new(),
        bell: BellDelta::default(),
        permit: LivePermit::default(),
    }
}

pub(crate) fn live_retry(_message: String) -> LiveUpdate {
    live_update(crate::LiveKind::Retry, "Reconnecting…", -1)
}

/// One plane's module committed something. The handler refetches that plane and
/// nothing else — `module` is the whole payload.
pub(crate) fn live_plane(module: &str, height: i64) -> LiveUpdate {
    let mut update = live_update(
        crate::LiveKind::Plane,
        &format!("Live · block {height}"),
        height,
    );
    update.module = module.to_string();
    update
}

/// A module's replay is unavailable or unfoldable — the handler reloads that
/// module's slices instead of folding.
pub(crate) fn live_resync(module: &str, height: i64) -> LiveUpdate {
    let mut update = live_update(crate::LiveKind::Resync, "Live · resyncing", height);
    update.module = module.to_string();
    update.load_chat = module == "chat";
    update
}

pub(crate) const fn block_kind_name(kind: BlockKind) -> &'static str {
    match kind {
        BlockKind::Page => "Page",
        BlockKind::Paragraph => "Text",
        BlockKind::Heading1 => "Heading 1",
        BlockKind::Heading2 => "Heading 2",
        BlockKind::Heading3 => "Heading 3",
        BlockKind::Bulleted => "Bullet",
        BlockKind::Numbered => "Number",
        BlockKind::Todo => "Todo",
        BlockKind::Toggle => "Toggle",
        BlockKind::Quote => "Quote",
        BlockKind::Code => "Code",
        BlockKind::Callout => "Callout",
        BlockKind::Divider => "Divider",
    }
}

pub(crate) fn next_sequence() -> u64 {
    static SEQUENCE: OnceLock<AtomicU64> = OnceLock::new();
    SEQUENCE
        .get_or_init(|| AtomicU64::new(epoch_nanos() as u64))
        .fetch_add(1, Ordering::Relaxed)
}

pub(crate) fn fresh_id(prefix: &str) -> String {
    format!("{prefix}-{}-{}", epoch_nanos(), next_sequence())
}

fn epoch_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default()
}

pub(crate) fn hex_encode(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(output, "{byte:02x}");
    }
    output
}

pub(crate) fn hex_decode(value: &str) -> Result<Vec<u8>, String> {
    let valid = !value.is_empty()
        && value.len() <= ::node::MAX_FRAME_HEX_BYTES
        && value.len().is_multiple_of(2)
        && value.bytes().all(|byte| byte.is_ascii_hexdigit());
    if !valid {
        return Err("ducktape signer returned an invalid frame".into());
    }
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let pair = std::str::from_utf8(pair).expect("validated ASCII hex");
            u8::from_str_radix(pair, 16).map_err(|_| "ducktape signer returned invalid hex".into())
        })
        .collect()
}
