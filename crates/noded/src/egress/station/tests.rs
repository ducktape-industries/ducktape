use super::*;

fn workspace() -> (tempfile::TempDir, Workspace) {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Workspace::at(dir.path());
    (dir, workspace)
}

fn write(workspace: &Workspace, text: &str) {
    std::fs::write(workspace.dir().join("egress.toml"), text).unwrap();
}

fn served(station: &Station) -> Vec<String> {
    station.current().config.served()
}

#[test]
fn a_reload_serves_what_the_file_now_names_and_tells_each_subscriber() {
    let (_dir, workspace) = workspace();
    let station = Station::open(workspace.clone()).unwrap();
    assert!(served(&station).is_empty());
    let mut reloads = station.subscribe();
    write(
        &workspace,
        "relay = true\n[services.pods]\nbase = \"http://127.0.0.1:1\"\n",
    );
    station.reload().unwrap();
    assert!(reloads.has_changed().unwrap());
    assert_eq!(
        reloads.borrow_and_update().config.served(),
        ["pods", "relay"]
    );
    write(&workspace, "relay = true\n");
    station.reload().unwrap();
    assert_eq!(served(&station), ["relay"]);
}

#[test]
fn a_file_that_does_not_load_leaves_the_services_served_before() {
    let (_dir, workspace) = workspace();
    write(&workspace, "relay = true\n");
    let station = Station::open(workspace.clone()).unwrap();
    let mut reloads = station.subscribe();
    reloads.borrow_and_update();
    write(&workspace, "relay = true\nrelya = true\n");
    let unparsed = station.reload();
    assert!(
        matches!(unparsed, Err(Refused::Unparsed(_))),
        "{unparsed:?}"
    );
    let filed = "[services.pods]\nbase = \"http://127.0.0.1:1\"\n\
                 secrets = { authorization = { file = \"pods.key\" } }\n";
    write(&workspace, filed);
    let unopened = station.reload();
    assert!(
        matches!(unopened, Err(Refused::Unopened { .. })),
        "{unopened:?}"
    );
    assert!(!reloads.has_changed().unwrap());
    assert_eq!(served(&station), ["relay"]);
    std::fs::write(workspace.dir().join("pods.key"), "Bearer token\n").unwrap();
    station.reload().unwrap();
    let pods = &station.current().config.services["pods"];
    assert_eq!(
        pods.secrets,
        [("authorization".to_owned(), "Bearer token".to_owned())]
    );
}

#[test]
fn a_tally_counts_names_added_removed_and_services_whose_upstream_changed() {
    let parse = |text: &str| Config::parse(text, |_| None, |_| Ok(String::new())).unwrap();
    let was = parse(
        "relay = true\n[services.a]\nbase = \"http://127.0.0.1:1\"\n\
         [services.b]\nbase = \"http://127.0.0.1:2\"\n",
    );
    let now = parse(
        "http = true\n[services.a]\nbase = \"http://127.0.0.1:9\"\n\
         [services.c]\nbase = \"http://127.0.0.1:3\"\n",
    );
    assert_eq!(
        Tally::between(&was, &now),
        Tally {
            served: 3,
            added: 2,
            removed: 2,
            changed: 1,
        }
    );
}
