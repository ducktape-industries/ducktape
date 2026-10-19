use super::*;

mod file;

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

fn unread(path: &Path) -> std::io::Result<String> {
    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        path.display().to_string(),
    ))
}

#[test]
fn a_config_names_what_it_serves_and_reads_each_services_secrets_from_the_environment() {
    let config = Config::parse(TEXT, env, unread).unwrap();
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
fn a_set_names_its_secrets_and_private_reach() {
    let plain = "relay = true\n[services.pods]\nbase = \"http://127.0.0.1:1\"\n";
    assert_eq!(withheld(plain).unwrap(), []);
    assert_eq!(
        withheld(TEXT).unwrap(),
        [Withheld::Secrets {
            service: "ccs".into()
        }]
    );
    let filed = "private = true\n[services.ccs]\nbase = \"https://elsewhere.example\"\n\
                 secrets = { x-seed = { file = \"identity.key\" } }\n";
    let found = withheld(filed).unwrap();
    assert_eq!(
        found,
        [
            Withheld::Private,
            Withheld::Secrets {
                service: "ccs".into(),
            },
        ]
    );
    assert_eq!(found[0].reason(), reason::UNAUTHORIZED);
    assert_eq!(found[1].reason(), reason::INVALID_INPUT);
    assert!(matches!(
        withheld("relya = true"),
        Err(Refused::Unparsed(_))
    ));
}

#[test]
fn an_unset_variable_a_reserved_name_or_a_base_that_is_not_a_plain_url_is_refused() {
    let unset = Config::parse(TEXT, |_| None, unread).unwrap_err();
    let Refused::Unset { variable, .. } = &unset else {
        panic!("not refused for its variable: {unset:?}");
    };
    assert_eq!(variable, "CCS_KEY");
    assert_eq!(unset.reason(), reason::NOT_FOUND);
    for reserved in RESERVED {
        let text = format!("[services.{reserved}]\nbase = \"https://example.com\"");
        let refused = Config::parse(&text, env, unread).unwrap_err();
        assert!(matches!(refused, Refused::Reserved { .. }), "{refused:?}");
    }
    let unparsed = Config::parse("[services.x]\nbase = \"127.0.0.1:4141\"", env, unread);
    assert!(
        matches!(unparsed, Err(Refused::Base { .. })),
        "{unparsed:?}"
    );
    for base in ["ftp://host", "https://host/?q=1", "https://user:pass@host"] {
        let text = format!("[services.x]\nbase = \"{base}\"");
        let refused = Config::parse(&text, env, unread).unwrap_err();
        assert!(matches!(refused, Refused::Unfit { .. }), "{base}");
        assert_eq!(refused.reason(), reason::INVALID_INPUT);
    }
    let misspelled = Config::parse("relay = true\nrelya = true", env, unread);
    assert!(
        matches!(misspelled, Err(Refused::Unparsed(_))),
        "{misspelled:?}"
    );
    let quiet = Config::parse("private = true", env, unread).unwrap();
    assert!(quiet.served().is_empty());
    assert_eq!(quiet.reach, Reach::Private);
}
