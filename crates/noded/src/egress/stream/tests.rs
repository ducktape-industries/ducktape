use super::*;

fn split(chunks: &[&[u8]]) -> Vec<Vec<u8>> {
    let mut splitter = Splitter::default();
    let mut events: Vec<Vec<u8>> = chunks
        .iter()
        .flat_map(|chunk| splitter.push(chunk))
        .collect();
    events.extend(splitter.finish());
    events
}

#[test]
fn an_event_is_its_lines_as_received_up_to_the_blank_line() {
    let body: &[u8] = b"event: start\ndata: {\"a\":1}\n\ndata: two\n\n";
    assert_eq!(
        split(&[body]),
        [&b"event: start\ndata: {\"a\":1}\n"[..], &b"data: two\n"[..]]
    );
}

#[test]
fn an_event_completes_only_when_its_blank_line_arrives_in_any_chunking() {
    let body: &[u8] = b"data: a\r\n\r\ndata: b\r\rdata: c\n\n";
    let whole = split(&[body]);
    assert_eq!(
        whole,
        [&b"data: a\r\n"[..], &b"data: b\r"[..], &b"data: c\n"[..]]
    );
    for cut in 1..body.len() {
        let (head, tail) = body.split_at(cut);
        assert_eq!(split(&[head, tail]), whole, "cut at {cut}");
    }
    let bytewise: Vec<&[u8]> = body.chunks(1).collect();
    assert_eq!(split(&bytewise), whole);
}

#[test]
fn a_chunk_ending_in_a_carriage_return_waits_to_see_whether_a_line_feed_follows() {
    let mut splitter = Splitter::default();
    assert!(splitter.push(b"data: a\r\n\r").is_empty());
    assert_eq!(splitter.push(b"\n"), [b"data: a\r\n".to_vec()]);
    assert!(splitter.push(b"data: b\r\r").is_empty());
    assert_eq!(splitter.finish(), [b"data: b\r".to_vec()]);
}

#[test]
fn blank_lines_between_events_carry_nothing_and_an_unended_event_is_dropped() {
    let body: &[u8] = b"\n\n: ping\n\n\r\ndata: last";
    assert_eq!(split(&[body]), [b": ping\n".to_vec()]);
}
