//! Every frame the CLI signs names it as its client (`crate::CLIENT`, `ducktape-cli/<version>`):
//! no source file of the binary signs with `Frame::sign` or `Frame::sign_query`, which name none.
use std::path::{Path, PathBuf};

fn sources(dir: &Path, found: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            sources(&path, found);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            found.push(path);
        }
    }
}

#[test]
fn every_frame_the_cli_signs_names_the_cli_as_its_client() {
    let mut found = Vec::new();
    sources(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
        &mut found,
    );
    let unnamed: Vec<String> = found
        .iter()
        .flat_map(|path| {
            let text = std::fs::read_to_string(path).unwrap();
            let lines: Vec<String> = text
                .lines()
                .enumerate()
                .filter(|(_, line)| {
                    line.contains("Frame::sign(") || line.contains("Frame::sign_query(")
                })
                .map(|(at, _)| format!("{}:{}", path.display(), at + 1))
                .collect();
            lines
        })
        .collect();
    assert!(
        unnamed.is_empty(),
        "these sign a frame that names no client; sign it with Frame::sign_as(crate::CLIENT, ..) or \
         Frame::sign_query_as: {unnamed:?}"
    );
}
