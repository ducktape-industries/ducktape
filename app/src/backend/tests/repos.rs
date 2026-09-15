use super::*;

#[test]
fn highlight_ranges_hold_char_boundaries_on_real_sources() {
    use gpui_kit::component::{
        Rope,
        highlighter::{HighlightTheme, SyntaxHighlighter},
    };
    // Exercise the native syntax engine with real multibyte source text.
    let rust = include_str!("../forge.rs");
    let toml = include_str!("../../../Cargo.toml");
    let theme = HighlightTheme::default_dark();
    for (language, source) in [("rust", rust), ("toml", toml)] {
        let mut highlighter = SyntaxHighlighter::new(language);
        assert!(highlighter.update(None, &Rope::from_str(source), None));
        for (range, _) in highlighter.styles(&(0..source.len()), theme.as_ref()) {
            assert!(
                source.get(range).is_some(),
                "highlight splits a UTF-8 character"
            );
        }
    }
}

#[test]
fn forge_code_expands_tabs_to_the_next_stop() {
    assert_eq!(expand_tabs("\tx\n\t\ty"), "    x\n        y");
    assert_eq!(expand_tabs("ab\tc"), "ab  c");
    assert_eq!(expand_tabs("abcd\tc"), "abcd    c");
    assert_eq!(expand_tabs("한\tc"), "한   c");
}

#[test]
fn forge_code_tokens_follow_the_path_and_rust_really_colors() {
    use gpui_kit::component::{
        Rope,
        highlighter::{HighlightTheme, SyntaxHighlighter},
    };
    assert_eq!(code_token("src/main.rs"), "rs");
    assert_eq!(code_token("a/b/query.SQL"), "sql");
    assert_eq!(code_token("Makefile"), "makefile");
    assert_eq!(code_token(".gitignore"), "gitignore");
    // the stream really tokenizes: one rust line yields more than one ink,
    // and an unknown token degrades to plain text (uniform ink), never an
    // error — exactly the old single-ink reading.
    let colors = |token: &str| -> std::collections::BTreeSet<String> {
        let source = "fn main() { let answer = 42; }";
        let theme = HighlightTheme::default_dark();
        let mut highlighter = SyntaxHighlighter::new(token);
        highlighter.update(None, &Rope::from_str(source), None);
        highlighter
            .styles(&(0..source.len()), theme.as_ref())
            .into_iter()
            // GPUI returns one default span even without a grammar. Count
            // actual ink overrides, not that unstyled coverage span.
            .filter_map(|(_, highlight)| highlight.color.map(|color| format!("{color:?}")))
            .collect()
    };
    assert!(
        colors("rust").len() > 1,
        "rust source highlights with more than one color"
    );
    assert_eq!(
        colors("no-such-language").len(),
        0,
        "an unknown token adds no syntax styles to the plain text"
    );
}

/// A WEB PICTURE IS ONE CAPPED GET. The bytes come back as served; a
/// response that announces more than the viewer takes, one that streams more
/// than it announced (or announced nothing), and one without a body to show
/// all come back as `None` — the image keeps its alt text. This drives the
/// GET behind the host gate: the gate itself refuses loopback, which is
/// where a test server lives.
#[tokio::test(flavor = "current_thread")]
async fn a_web_picture_is_one_capped_get() {
    use super::super::picture::MAX_PICTURE_BYTES;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = vec![0u8; 2048];
            let read = socket.read(&mut request).await.unwrap();
            let head = String::from_utf8_lossy(&request[..read]).into_owned();
            let route = head.split(' ').nth(1).unwrap_or("").to_owned();
            let (status, length, body_len): (&str, Option<usize>, usize) = match route.as_str() {
                "/small" => ("200 OK", Some(9), 9),
                "/announced-huge" => ("200 OK", Some(MAX_PICTURE_BYTES + 1), 0),
                "/streamed-huge" => ("200 OK", None, MAX_PICTURE_BYTES + 1),
                _ => ("404 Not Found", Some(0), 0),
            };
            let mut response = format!("HTTP/1.1 {status}\r\nConnection: close\r\n");
            if let Some(length) = length {
                response.push_str(&format!("Content-Length: {length}\r\n"));
            }
            response.push_str("\r\n");
            socket.write_all(response.as_bytes()).await.unwrap();
            let body = match route.as_str() {
                "/small" => b"PNG-bytes".to_vec(),
                _ => vec![0u8; body_len],
            };
            let _ = socket.write_all(&body).await;
            let _ = socket.shutdown().await;
        }
    });
    assert_eq!(
        fetch_picture_bytes(reqwest::Url::parse(&format!("{origin}/small")).unwrap())
            .await
            .as_deref(),
        Some(&b"PNG-bytes"[..]),
        "a picture under the cap comes back as served"
    );
    assert!(
        fetch_picture_bytes(reqwest::Url::parse(&format!("{origin}/announced-huge")).unwrap())
            .await
            .is_none(),
        "an announced length past the cap is refused before the body"
    );
    assert!(
        fetch_picture_bytes(reqwest::Url::parse(&format!("{origin}/streamed-huge")).unwrap())
            .await
            .is_none(),
        "a body that streams past the cap is refused mid-stream"
    );
    assert!(
        fetch_picture_bytes(reqwest::Url::parse(&format!("{origin}/missing")).unwrap())
            .await
            .is_none(),
        "a miss has no picture"
    );
}

/// A README NAMES THE URL, THE READER'S MACHINE MAKES THE REQUEST. The host
/// gate refuses the machine itself, its link (cloud metadata), nothing and
/// everyone — as an IP literal or as a name that resolves there — and keeps
/// a LAN address, which is where a team's forge lives.
#[tokio::test(flavor = "current_thread")]
async fn a_web_picture_never_points_the_reader_at_its_own_machine() {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
    let blocked = [
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        IpAddr::V4(Ipv4Addr::new(169, 254, 169, 254)),
        IpAddr::V4(Ipv4Addr::UNSPECIFIED),
        IpAddr::V4(Ipv4Addr::BROADCAST),
        IpAddr::V6(Ipv6Addr::LOCALHOST),
        IpAddr::V6(Ipv4Addr::LOCALHOST.to_ipv6_mapped()),
        IpAddr::V6("fe80::1".parse().unwrap()),
    ];
    for ip in blocked {
        assert!(blocked_picture_host(ip), "{ip} is not a picture's address");
    }
    let allowed = [
        IpAddr::V4(Ipv4Addr::new(10, 0, 0, 7)),
        IpAddr::V4(Ipv4Addr::new(192, 168, 1, 20)),
        IpAddr::V4(Ipv4Addr::new(140, 82, 112, 3)),
        IpAddr::V6("2606:4700::1".parse().unwrap()),
    ];
    for ip in allowed {
        assert!(!blocked_picture_host(ip), "{ip} may serve a picture");
    }
    // The gate end to end, without a socket: a literal and the name that
    // resolves to the machine are refused before any request is made.
    assert!(
        web_picture_bytes("http://127.0.0.1:9/logo.png")
            .await
            .is_none(),
        "a loopback literal is refused"
    );
    assert!(
        web_picture_bytes("http://localhost:9/logo.png")
            .await
            .is_none(),
        "a name resolving to loopback is refused"
    );
    assert!(
        web_picture_bytes("ftp://example.com/logo.png")
            .await
            .is_none(),
        "only a web URL is fetched"
    );
}
