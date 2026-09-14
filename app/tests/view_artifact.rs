#[path = "../src/backend/view_artifact.rs"]
mod view_artifact;

#[tokio::test]
async fn a_missing_blob_is_not_held_and_not_a_view_removal() {
    use std::io::{Read as _, Write as _};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let client =
        ducktape_rpc::Client::new(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
    let server = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        let mut request = Vec::new();
        let mut byte = [0];
        while !request.ends_with(b"\r\n\r\n") {
            socket.read_exact(&mut byte).unwrap();
            request.push(byte[0]);
        }
        socket
            .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .unwrap();
    });
    assert!(matches!(
        view_artifact::fetch(&client, [0; 32]).await,
        Err(view_artifact::Error::NotHeld)
    ));
    server.join().unwrap();
}
