// Source lint: the modules an RPC request runs through name no lock and never reach marshal's
// mailbox, and a handler takes the request context, never the daemon.

const REQUEST_PATH: [&str; 27] = [
    "server.rs",
    "query.rs",
    "ctx.rs",
    "view.rs",
    "changes.rs",
    "pool.rs",
    "logs.rs",
    "relay.rs",
    "blocks.rs",
    "network.rs",
    "writer.rs",
    "lib.rs",
    "link.rs",
    "live/mod.rs",
    "live/table.rs",
    "live/lane.rs",
    "live/session.rs",
    "live/groups.rs",
    "live/grants.rs",
    "live/tracks.rs",
    "activity.rs",
    "snapshot.rs",
    "snapshot/stream.rs",
    "standing.rs",
    "census.rs",
    "forward.rs",
    "health.rs",
];

const LOCKS: [&str; 4] = ["Mutex", "RwLock", "Semaphore", ".lock("];

fn source(module: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join(module);
    std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

#[test]
fn no_module_on_the_request_path_names_a_lock() {
    let named: Vec<(&str, &str)> = REQUEST_PATH
        .iter()
        .flat_map(|module| {
            let text = source(module);
            LOCKS
                .iter()
                .filter(move |lock| text.contains(**lock))
                .map(move |lock| (*module, *lock))
                .collect::<Vec<_>>()
        })
        .collect();
    assert!(named.is_empty(), "locks on the request path: {named:?}");
}

#[test]
fn no_module_on_the_request_path_reaches_marshal() {
    let reaching: Vec<(&str, &str)> = REQUEST_PATH
        .iter()
        .flat_map(|module| {
            let text = source(module);
            ["MarshalMailbox", "get_finalization", "get_block("]
                .into_iter()
                .filter(move |name| text.contains(name))
                .map(move |name| (*module, name))
                .collect::<Vec<_>>()
        })
        .collect();
    assert!(
        reaching.is_empty(),
        "modules on the request path that ask marshal: {reaching:?}"
    );
}

#[test]
fn every_handler_takes_the_request_context_and_none_takes_the_daemon() {
    let server = source("server.rs");
    assert!(
        !server.contains("State<"),
        "a handler extracts router state instead of the request context"
    );
    let handlers: Vec<&str> = server
        .split("async fn ")
        .skip(1)
        .filter(|handler| {
            handler
                .split_once('(')
                .is_some_and(|(name, _)| name.ends_with("<E: Context>"))
        })
        .collect();
    assert!(!handlers.is_empty());
    let unbound: Vec<&str> = handlers
        .iter()
        .filter(|handler| {
            let arguments = handler.split(')').next().unwrap_or_default();
            !arguments.contains("Ctx<E>")
        })
        .map(|handler| handler.split('<').next().unwrap_or_default())
        .collect();
    assert!(
        unbound.is_empty(),
        "handlers without the context: {unbound:?}"
    );
}

const DISPATCHES: [(&str, &str); 9] = [
    ("live/table.rs", "match event {"),
    ("pool.rs", "let flow = match event {"),
    ("changes.rs", "let flow = match event {"),
    ("run.rs", "let flow = match event {"),
    ("run.rs", "match command {"),
    ("activity.rs", "let flow = match event {"),
    ("server.rs", "match heard {"),
    ("standing.rs", "let flow = match next {"),
    ("census.rs", "match heard {"),
];

fn body_after(text: &str, head: &str) -> String {
    let start = text
        .find(head)
        .unwrap_or_else(|| panic!("no `{head}` to check"))
        + head.len();
    let mut depth = 1usize;
    let mut body = String::new();
    for c in text[start..].chars() {
        match c {
            '{' => depth += 1,
            '}' => depth -= 1,
            _ => {}
        }
        if depth == 0 {
            return body;
        }
        body.push(c);
    }
    panic!("`{head}` never closes");
}

#[test]
fn every_owner_task_dispatches_its_events_in_one_match_of_named_handlers() {
    let broken: Vec<(&str, &str, &str)> = DISPATCHES
        .iter()
        .flat_map(|(module, head)| {
            let body = body_after(&source(module), head);
            [
                ("_ =>", "a wildcard arm"),
                (" if ", "a match guard"),
                ("=> {", "a block arm"),
            ]
            .into_iter()
            .filter(move |(shape, _)| body.contains(shape))
            .map(move |(_, what)| (*module, *head, what))
        })
        .collect();
    assert!(
        broken.is_empty(),
        "dispatches that are not one match of named handlers: {broken:?}"
    );
}
