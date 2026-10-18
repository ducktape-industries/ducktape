use super::*;

const FILED: &str = r#"
[services.pods]
base = "http://10.43.200.10:8080"
secrets = { Authorization = { file = "pods.key" } }
"#;

#[test]
fn a_secret_named_by_file_is_read_through_the_reader_and_trimmed() {
    let read = |path: &Path| {
        assert_eq!(path, Path::new("pods.key"));
        Ok("Bearer pods-token\n".to_owned())
    };
    let config = Config::parse(FILED, env, read).unwrap();
    assert_eq!(
        config.services["pods"].secrets,
        [("authorization".to_owned(), "Bearer pods-token".to_owned())]
    );
}

#[test]
fn a_secret_file_that_does_not_read_refuses_the_whole_config() {
    let refused = Config::parse(FILED, env, unread).unwrap_err();
    let Refused::Unopened { service, path, .. } = &refused else {
        panic!("not refused for its file: {refused:?}");
    };
    assert_eq!(service, "pods");
    assert_eq!(path, Path::new("pods.key"));
    assert_eq!(refused.reason(), reason::NOT_FOUND);
}

#[test]
fn a_secret_is_a_variable_name_or_a_file_table_and_nothing_else() {
    let stray = FILED.replace("{ file = \"pods.key\" }", "{ path = \"pods.key\" }");
    let refused = Config::parse(&stray, env, unread);
    assert!(matches!(refused, Err(Refused::Unparsed(_))), "{refused:?}");
}
