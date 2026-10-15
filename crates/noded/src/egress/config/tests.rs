use super::*;

const TEXT: &str = r#"
relay = true
http = true

[services.ccs]
base = "http://127.0.0.1:4141/"
secrets = { X-Api-Key = "CCS_KEY" }
"#;

fn env(variable: &str) -> Option<String> {
    (variable == "CCS_KEY").then(|| "gateway-key".to_owned())
}

#[test]
fn a_config_names_what_it_serves_and_reads_each_services_secrets_from_the_environment() {
    let config = Config::parse(TEXT, env).unwrap();
    assert_eq!(config.served(), ["ccs", HTTP, RELAY]);
    assert_eq!(config.reach, Reach::Public);
    assert_eq!(
        config.services["ccs"],
        Upstream {
            base: Url::parse("http://127.0.0.1:4141").unwrap(),
            secrets: vec![("x-api-key".into(), "gateway-key".into())],
        }
    );
}

#[test]
fn an_unset_variable_a_reserved_name_or_a_base_that_is_not_a_plain_url_is_refused() {
    let unset = Config::parse(TEXT, |_| None).unwrap_err();
    assert!(unset.contains("CCS_KEY"), "{unset}");
    for reserved in RESERVED {
        let text = format!("[services.{reserved}]\nbase = \"https://example.com\"");
        assert!(Config::parse(&text, env).is_err(), "{reserved}");
    }
    for base in [
        "127.0.0.1:4141",
        "ftp://host",
        "https://host/?q=1",
        "https://user:pass@host",
    ] {
        let text = format!("[services.x]\nbase = \"{base}\"");
        assert!(Config::parse(&text, env).is_err(), "{base}");
    }
    assert!(Config::parse("relay = true\nrelya = true", env).is_err());
    let quiet = Config::parse("private = true", env).unwrap();
    assert!(quiet.served().is_empty());
    assert_eq!(quiet.reach, Reach::Private);
}
