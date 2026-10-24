use super::*;

#[test]
fn preparation_preserves_repeated_chunks_and_frames_one_complete_bundle() {
    let body = b"ababxyz".to_vec();
    let request = Preparation {
        body: body.clone(),
        hash: HashKind::Sha256,
        kind: "chunk".into(),
        chunk_bytes: 2,
    };
    let (product, bundle) = prepare(request).unwrap();
    let mut stream = bundle.framed.as_slice();
    let mut bodies = Vec::new();
    for item in &bundle.entries {
        let (framed, rest) = stream.split_at(item.len as usize);
        assert_eq!(blob::id_of(HashKind::Sha256, framed).unwrap(), item.id);
        bodies.push(blob::parse(framed).unwrap().1.to_vec());
        stream = rest;
    }
    assert!(stream.is_empty());
    let reply = bodies.pop().unwrap();
    let prepared: Prepared = abi::decode(&reply).unwrap();
    assert_eq!(prepared.size, body.len() as u64);
    assert_eq!(prepared.chunks, product.produced);
    assert_eq!(prepared.chunks[0], prepared.chunks[1]);
    assert_eq!(bodies.concat(), body);
    assert_eq!(bundle.entries.last().unwrap().id, product.reply.id);
}

#[test]
fn empty_preparation_has_no_chunks_and_refuses_malformed_parameters() {
    let request = Preparation {
        body: Vec::new(),
        hash: HashKind::Sha256,
        kind: "chunk".into(),
        chunk_bytes: 2,
    };
    let (product, bundle) = prepare(request.clone()).unwrap();
    assert!(product.produced.is_empty());
    assert_eq!(bundle.entries.len(), 1);
    let mut zero = request.clone();
    zero.chunk_bytes = 0;
    assert_eq!(prepare(zero).err().unwrap().reason, reason::INVALID_INPUT);
    let mut invalid_kind = request;
    invalid_kind.kind = "two words".into();
    assert_eq!(
        prepare(invalid_kind).err().unwrap().reason,
        reason::INVALID_INPUT
    );
}
