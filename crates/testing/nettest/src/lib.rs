//! Raw-HTTP-over-TCP test client + collision-safe port allocation + a coarse
//! event poll — the three helpers every node/daemon/sim integration harness in
//! the tree used to re-roll locally, now in ONE place.
//!
//! The node's app surface trusts localhost callers, so a hand-rolled std-TCP
//! http/1.1 client is a full citizen by design — the daemon's whole promise is
//! that ANY plain http client works. This crate IS that client, deduplicated,
//! so a suite that needs a raw request never re-writes a subtly-different (and
//! racier) variant. Dev/test only: no production crate depends on it.
//!
//! The body/head split is a BYTE-LEVEL scan for the `\r\n\r\n` terminator, never
//! a lossy utf-8 `split("\r\n\r\n")` — the former is correct for a binary chunk
//! body AND for a text body that itself contains a blank line; the latter
//! corrupts both. Port allocation holds every listener at once so the OS can't
//! hand the same port back twice. These are the safe variants; the racy/naive
//! copies are what this crate exists to kill.

use std::io::{Read as _, Write as _};
use std::net::{TcpListener, TcpStream, UdpSocket};
use std::time::{Duration, Instant};

use serde_json::Value;

/// how long a request waits on a loopback peer before giving up. generous: a
/// cold genesis or a slow block legitimately takes seconds, and a real hang is
/// caught by the test's own outer deadline, not this socket timeout.
const IO_TIMEOUT: Duration = Duration::from_secs(30);

/// one raw http/1.1 request over std TCP against a loopback app-surface `port`,
/// returning `(status, body_bytes)`. `content_type` sets the request header;
/// `body` is sent verbatim. panics on a connect/head-write/read failure (a
/// broken loopback is a broken test). the BODY write is best-effort: the server
/// may legally answer 413 and stop reading mid-body, surfacing as a broken pipe.
pub fn http_bytes(
    port: u16,
    method: &str,
    path: &str,
    content_type: &str,
    body: &[u8],
) -> (u16, Vec<u8>) {
    try_http_bytes(port, method, path, content_type, body).expect("app-surface request")
}

/// the fallible core of [`http_bytes`]: an `io::Result` so a readiness loop can
/// treat "connection refused" (node not up yet) as "not ready" instead of a
/// panic.
pub fn try_http_bytes(
    port: u16,
    method: &str,
    path: &str,
    content_type: &str,
    body: &[u8],
) -> std::io::Result<(u16, Vec<u8>)> {
    try_http_bytes_with(port, method, path, content_type, &[], body)
}

/// [`try_http_bytes`] plus extra request headers — what a gated surface needs:
/// `/v1/admin/*` requires the node's operator credential in a header, so a
/// harness that shuts its node down must be able to set one.
pub fn try_http_bytes_with(
    port: u16,
    method: &str,
    path: &str,
    content_type: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> std::io::Result<(u16, Vec<u8>)> {
    let (status, _head, body) = try_http_headed(port, method, path, content_type, headers, body)?;
    Ok((status, body))
}

/// [`try_http_bytes_with`] keeping the response HEAD as well — for a route
/// whose contract is partly in a response header (`/v1/index/{module}/view`
/// answers its fold watermark in `x-ducktape-folded`). Pair it with
/// [`header_of`]; everything else drops the head.
pub fn try_http_headed(
    port: u16,
    method: &str,
    path: &str,
    content_type: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> std::io::Result<(u16, String, Vec<u8>)> {
    let mut stream = TcpStream::connect(("127.0.0.1", port))?;
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    let extra: String = headers
        .iter()
        .map(|(name, value)| format!("{name}: {value}\r\n"))
        .collect();
    let head = format!(
        "{method} {path} HTTP/1.1\r\nhost: 127.0.0.1\r\ncontent-type: {content_type}\r\n{extra}content-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    // best-effort: a 413 refusal closes the read side mid-body.
    let _ = stream.write_all(body);
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw)?;
    // split head/body at the BYTE level — chunk bytes round-trip untouched and a
    // text body's own blank line can't be mistaken for the header terminator.
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "no http header terminator")
        })?;
    let head = String::from_utf8_lossy(&raw[..split]).into_owned();
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    Ok((status, head, raw[split + 4..].to_vec()))
}

/// one response header's value out of a raw head, matched case-insensitively
/// (http/1.1 header names are). `None` = the server did not send it.
pub fn header_of(head: &str, name: &str) -> Option<String> {
    head.lines().find_map(|line| {
        let (field, value) = line.split_once(':')?;
        field
            .trim()
            .eq_ignore_ascii_case(name)
            .then(|| value.trim().to_string())
    })
}

/// a json request/response against a loopback app surface: serialize `body` (if
/// any) as the request, parse the response body as json (`Null` if it isn't).
/// the json twin of [`http_bytes`] — the byte-level split means a json value
/// carrying `\r\n\r\n` inside a string can't corrupt the parse.
pub fn http_json(port: u16, method: &str, path: &str, body: Option<&Value>) -> (u16, Value) {
    try_http_json(port, method, path, body).expect("app-surface json request")
}

/// the fallible core of [`http_json`], for a readiness loop that polls a json
/// route before the node is up.
pub fn try_http_json(
    port: u16,
    method: &str,
    path: &str,
    body: Option<&Value>,
) -> std::io::Result<(u16, Value)> {
    let bytes = body
        .map(|b| serde_json::to_vec(b).expect("request body serializes"))
        .unwrap_or_default();
    let (status, raw) = try_http_bytes(port, method, path, "application/json", &bytes)?;
    let payload = serde_json::from_slice(&raw).unwrap_or(Value::Null);
    Ok((status, payload))
}

/// GET (or any verb) a loopback app surface and return `(status, body_as_text)`
/// — the non-json twin, for text expositions like the Prometheus `/metrics`
/// body or a raw status-body assertion.
pub fn http_text(port: u16, method: &str, path: &str) -> (u16, String) {
    let (status, raw) = http_bytes(port, method, path, "text/plain", &[]);
    (status, String::from_utf8_lossy(&raw).into_owned())
}

/// the http status a loopback app surface answers `method path` with, or `None`
/// if it is not up yet (connect refused) — the readiness/liveness probe every
/// harness polls before driving, and posts its shutdown through.
pub fn http_status(port: u16, method: &str, path: &str) -> Option<u16> {
    http_status_with(port, method, path, &[])
}

/// [`http_status`] carrying extra request headers — the shape an admin route
/// needs, since every one of them requires the node's operator credential.
pub fn http_status_with(
    port: u16,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
) -> Option<u16> {
    try_http_bytes_with(port, method, path, "application/json", headers, &[])
        .ok()
        .map(|(status, _)| status)
}

/// one free localhost port, never one this process already handed out.
pub fn free_port() -> u16 {
    alloc_ports(1)[0]
}

/// every port this process has ever handed out. a probe listener only reserves
/// its port until it is dropped, and the caller does not bind for real until
/// some time later — a spawned daemon takes a whole genesis to get there. In
/// that gap the port is genuinely free, so the OS will hand it to the next
/// `bind(:0)`, and TWO harnesses walk away believing they own it. This is the
/// gap that a per-call listener set cannot see, because the collision is
/// between DIFFERENT calls.
static HANDED_OUT: std::sync::Mutex<std::collections::BTreeSet<u16>> =
    std::sync::Mutex::new(std::collections::BTreeSet::new());

/// `n` free localhost ports, distinct from each other AND from every port this
/// process handed out earlier, and free on BOTH tcp and udp.
///
/// Two guards, for two different collisions. Holding all `n` probes at once
/// stops one call from returning the same port twice. [`HANDED_OUT`] stops it
/// across calls — the case that actually bites, because the window is as long
/// as the caller takes to bind, not as long as this function runs.
///
/// Both protocols, because a caller cannot be asked to pick the right allocator
/// and the number space is shared regardless of which one it binds: a harness
/// that reserved a port here and then handed it to `--wireguard-listen` (udp)
/// would otherwise be holding a reservation on the wrong protocol, and the
/// harnesses that noticed rolled their OWN `UdpSocket::bind(":0")` probe-drop —
/// which reserves nothing at all in [`HANDED_OUT`], so this function could hand
/// the very same number out again seven lines later. One allocator, one number
/// space, no call-site decision.
///
/// A port that loses either check is dropped IMMEDIATELY rather than held aside
/// during the retry: its owner may be about to bind it for real, and squatting
/// on it while we look for another would break the very daemon we are trying
/// not to collide with.
pub fn alloc_ports(n: usize) -> Vec<u16> {
    // one allocation at a time: two threads probing concurrently would each
    // check `HANDED_OUT` before the other inserted, and hand out the same port.
    let mut handed = HANDED_OUT.lock().unwrap_or_else(|e| e.into_inner());
    let mut keep: Vec<(u16, TcpListener, UdpSocket)> = Vec::with_capacity(n);
    // the ephemeral range is tens of thousands wide and cycles, so a repeat is
    // rare and a second probe effectively always advances; the cap turns an
    // exhausted range into a named failure instead of a hang.
    for _ in 0..(n + 1024) {
        if keep.len() == n {
            break;
        }
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind port-0 probe");
        let port = listener.local_addr().expect("probe addr").port();
        // the udp half may legitimately be taken by an unrelated process while
        // tcp is free — that is a port this allocator must not hand out, not an
        // error. mark it used so a later call does not re-probe it either.
        let Some(datagram) = claim_udp(port) else {
            handed.insert(port);
            continue;
        };
        if handed.insert(port) {
            keep.push((port, listener, datagram));
        }
    }
    assert_eq!(
        keep.len(),
        n,
        "could not find {n} localhost ports this process has not already used"
    );
    keep.iter().map(|(port, _, _)| *port).collect()
}

/// The udp half of `port`, held for as long as the caller keeps it — `None`
/// when someone else has it.
///
/// Split out of [`alloc_ports`] because it is the only branch a test can reach
/// deterministically: `alloc_ports` binds `:0`, so no test can steer it at a
/// port it has arranged to be udp-busy, and one that merely checks the returned
/// ports happen to be udp-free passes against a tcp-only probe on any host with
/// no udp listener (measured: 0 of 8000 on a quiet box). See
/// [`tests::a_udp_busy_port_is_unclaimable_even_where_tcp_is_free`].
fn claim_udp(port: u16) -> Option<UdpSocket> {
    UdpSocket::bind(("127.0.0.1", port)).ok()
}

/// Set to `1` to let a test SKIP when the host lacks what it needs. Unset — the
/// default — a missing capability is a FAILURE.
///
/// The default is inverted on purpose, and that inversion is the whole point of
/// this helper. The obvious design is "skip quietly, and let a careful operator
/// opt IN to strictness". It does not work: libtest's default capture takes
/// **stderr as well as stdout**, so a skip printed from a PASSING test is
/// swallowed and the run reads
///
/// ```text
/// test git_push_over_http_lands_in_forge_head ... ok
/// test result: ok. 1 passed; 0 failed; ...
/// ```
///
/// — byte-identical to a real green. A "loud skip" is loud only under
/// `--nocapture`, which is not how anyone runs a suite. That is how five
/// forge-over-http tests, a whole compute plane and a claim lane each spent
/// weeks proving nothing.
///
/// So the two candidate failure modes are "CI is noisy on an under-provisioned
/// box" and "CI lies". Only the first is recoverable by reading the output. A
/// box that genuinely cannot run these sets the variable and gets its skips —
/// as a deliberate act, not as the silent default.
pub const ALLOW_MISSING_TOOLS_ENV: &str = "DUCKTAPE_ALLOW_MISSING_TOOLS";

/// `Some(())` = `test` cannot run on this host and the caller must return;
/// `None` = run it. `why` is `Some(reason)` exactly when the capability is
/// missing — the shape every probe already returns.
///
/// PANICS on a missing capability unless [`ALLOW_MISSING_TOOLS_ENV`] opts out.
pub fn skip_without(test: &str, why: Option<String>) -> Option<()> {
    let skipping_allowed =
        std::env::var_os(ALLOW_MISSING_TOOLS_ENV).is_some_and(|allow| allow == "1");
    decide_skip(test, skipping_allowed, why)
}

/// The decision [`skip_without`] makes, with the environment read out of it —
/// so both arms can be tested without `set_var`, which is process-global and
/// leaks into every other test running on a sibling libtest thread.
fn decide_skip(test: &str, skipping_allowed: bool, why: Option<String>) -> Option<()> {
    let why = why?;
    assert!(
        skipping_allowed,
        "{test} cannot run on this host: {why}\n\
         Install what is missing, or set {ALLOW_MISSING_TOOLS_ENV}=1 to skip \
         instead — and then do not read this suite's green as coverage."
    );
    // fd 2 DIRECTLY, not `eprintln!`: the macro routes through
    // `std::io::_eprint`, which honours libtest's thread-local output capture
    // and would swallow this line on a passing test — the exact defect this
    // helper exists to kill. `Stderr::write_fmt` does not consult it. (The panic
    // arm needs no such care: libtest always prints a failure.)
    let _ = writeln!(
        std::io::stderr(),
        "SKIP {test}: {why} ({ALLOW_MISSING_TOOLS_ENV}=1 is set)"
    );
    Some(())
}

/// `Some(reason)` when `bin` is not runnable here — the probe shape
/// [`skip_without`] takes, for a capability that is just "a tool on PATH".
pub fn missing_tool(bin: &str) -> Option<String> {
    let runs = std::process::Command::new(bin)
        .arg("--version")
        .output()
        .is_ok_and(|out| out.status.success());
    (!runs).then(|| format!("{bin} is not runnable on PATH"))
}

/// poll `probe` every 300ms until it returns `Some`, or panic with `what` past
/// `timeout` — the coarse wait for a harness whose process offers no event to
/// ride. The node cluster harness has one (the block feed behind
/// `await_committed`) and never uses this; the daemon e2e still does.
pub fn poll_until<T>(what: &str, timeout: Duration, mut probe: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(v) = probe() {
            return v;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(300));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead as _, BufReader};

    /// a one-shot loopback server that drains one request head then replies with
    /// `resp` and closes — enough to exercise the client without a real node.
    fn serve_once(resp: &'static [u8]) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(sock.try_clone().unwrap());
            let mut line = String::new();
            loop {
                line.clear();
                let n = reader.read_line(&mut line).unwrap();
                if n == 0 || line == "\r\n" {
                    break;
                }
            }
            sock.write_all(resp).unwrap();
        });
        port
    }

    #[test]
    fn http_bytes_splits_on_header_terminator_not_a_body_blank_line() {
        // the body itself carries a blank line: a naive `split("\r\n\r\n")` would
        // truncate at it; the byte-scan must return the WHOLE body.
        let port = serve_once(
            b"HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\nconnection: close\r\n\r\nline1\r\n\r\nline2",
        );
        let (status, body) = http_bytes(port, "GET", "/", "text/plain", &[]);
        assert_eq!(status, 200);
        assert_eq!(body, b"line1\r\n\r\nline2");
    }

    #[test]
    fn alloc_ports_are_distinct() {
        let ports = alloc_ports(8);
        let mut deduped = ports.clone();
        deduped.sort_unstable();
        deduped.dedup();
        assert_eq!(
            deduped.len(),
            ports.len(),
            "alloc_ports handed back a duplicate"
        );
    }

    /// A reserved port must be free on BOTH protocols: harnesses hand these
    /// numbers to `--wireguard-listen` (udp) as readily as to `--listen` (tcp),
    /// so a tcp-only probe reserves the wrong half of the number space.
    ///
    /// The squatter is what makes this a test. Asserting that
    /// [`alloc_ports`]'s output happens to be udp-free would pass against a
    /// tcp-only probe on any host with no udp listener, and `alloc_ports` binds
    /// `:0` — there is no way to steer it at a port arranged to be busy. So the
    /// branch is exercised where it is reachable: hold the udp half, leave the
    /// tcp half free (the exact state a tcp-only probe misreads), and require
    /// [`claim_udp`] to refuse it. Both sockets stay live through the assertion;
    /// dropping and immediately reclaiming the port tests OS reuse timing, not
    /// this branch.
    #[test]
    fn a_udp_busy_port_is_unclaimable_even_where_tcp_is_free() {
        let squatter = UdpSocket::bind("127.0.0.1:0").expect("udp squatter");
        let port = squatter.local_addr().expect("squatter addr").port();
        // the misreading a tcp-only probe would make: this succeeds.
        let _tcp = TcpListener::bind(("127.0.0.1", port)).expect("the tcp half IS free");

        assert!(
            claim_udp(port).is_none(),
            "port {port} is udp-busy and must not be claimable"
        );
    }

    /// No port is EVER handed out twice in one process, however many calls —
    /// the cross-call collision that made one harness drive another's daemon.
    #[test]
    fn alloc_ports_never_repeats_across_calls() {
        let mut seen = std::collections::BTreeSet::new();
        for _ in 0..16 {
            for port in alloc_ports(2) {
                assert!(seen.insert(port), "alloc_ports re-handed port {port}");
            }
        }
    }

    #[test]
    fn a_present_capability_runs_the_test() {
        assert_eq!(decide_skip("a_test", false, None), None);
        assert_eq!(decide_skip("a_test", true, None), None);
    }

    /// THE default: a missing capability FAILS, because a skip that libtest
    /// captures is indistinguishable from a pass.
    #[test]
    #[should_panic(expected = "a_test cannot run on this host: no widget")]
    fn a_missing_capability_fails_by_default() {
        decide_skip("a_test", false, Some("no widget".into()));
    }

    #[test]
    fn a_missing_capability_skips_only_when_the_operator_asked() {
        assert_eq!(
            decide_skip("a_test", true, Some("no widget".into())),
            Some(())
        );
    }

    #[test]
    fn missing_tool_finds_a_real_one_and_names_a_fake_one() {
        assert_eq!(missing_tool("cargo"), None);
        assert!(missing_tool("ducktape-no-such-tool").is_some());
    }
}
