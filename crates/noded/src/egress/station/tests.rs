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
fn a_plan_serves_nothing_and_a_put_writes_the_file_and_serves_it() {
    let (_dir, workspace) = workspace();
    write(&workspace, "relay = true\n");
    let station = Station::open(workspace.clone()).unwrap();
    let text = "http = true\n[services.pods]\nbase = \"http://127.0.0.1:1\"\n";
    let planned = station.plan(text.to_owned()).unwrap();
    assert_eq!(
        planned.tally,
        Tally {
            served: 2,
            added: 2,
            removed: 1,
            changed: 0,
        }
    );
    assert_eq!(served(&station), ["relay"]);
    let mut sets = station.subscribe();
    sets.borrow_and_update();
    station.put(planned).unwrap();
    assert!(sets.has_changed().unwrap());
    assert_eq!(served(&station), ["http", "pods"]);
    let written = std::fs::read_to_string(workspace.dir().join("egress.toml")).unwrap();
    assert_eq!(written, text);
    let unparsed = station.plan("relya = true\n".to_owned());
    assert!(
        matches!(unparsed, Err(Refused::Unparsed(_))),
        "{:?}",
        unparsed.err()
    );
}

#[test]
fn sets_put_at_once_each_land_and_the_file_is_what_the_node_serves() {
    let (_dir, workspace) = workspace();
    let station = Station::open(workspace.clone()).unwrap();
    let texts: Vec<String> = (1..=16)
        .map(|port| format!("[services.s{port}]\nbase = \"http://127.0.0.1:{port}\"\n"))
        .collect();
    std::thread::scope(|scope| {
        let puts: Vec<_> = texts
            .iter()
            .map(|text| scope.spawn(|| station.put(station.plan(text.clone()).unwrap())))
            .collect();
        for put in puts {
            put.join().unwrap().unwrap();
        }
    });
    let written = std::fs::read_to_string(workspace.dir().join("egress.toml")).unwrap();
    assert!(texts.contains(&written));
    assert_eq!(workspace.egress().unwrap().served(), served(&station));
}

#[test]
fn reloads_and_sets_at_once_leave_the_file_as_what_the_node_serves() {
    let (_dir, workspace) = workspace();
    let station = Station::open(workspace.clone()).unwrap();
    let texts: Vec<String> = (1..=8)
        .map(|port| format!("[services.s{port}]\nbase = \"http://127.0.0.1:{port}\"\n"))
        .collect();
    std::thread::scope(|scope| {
        let mut done = Vec::new();
        for text in &texts {
            done.push(scope.spawn(|| station.put(station.plan(text.clone()).unwrap()).map(|_| ())));
            done.push(scope.spawn(|| station.reload()));
        }
        for one in done {
            one.join().unwrap().unwrap();
        }
    });
    assert_eq!(workspace.egress().unwrap().served(), served(&station));
}

#[test]
fn the_grants_are_read_from_the_workspace_at_each_ask() {
    let (_dir, workspace) = workspace();
    let station = Station::open(workspace.clone()).unwrap();
    assert!(station.grants().unwrap().is_empty());
    let grants = workspace.dir().join("egress-grants");
    std::fs::write(&grants, "0a0b\n\n  0c  \n").unwrap();
    assert_eq!(station.grants().unwrap(), [vec![0x0a, 0x0b], vec![0x0c]]);
    std::fs::write(&grants, "zz\n").unwrap();
    let unread = station.grants().unwrap_err();
    assert_eq!(unread.kind(), std::io::ErrorKind::InvalidData);
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
