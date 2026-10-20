//! What this node serves through the egress role, read from `egress.toml` in its workspace.
//! Loaded by `station.rs` when the node starts and on each `ducktape egress reload`.
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use abi::reason;
use abi::role::egress::{HTTP, RELAY, RESERVED};
use serde::Deserialize;
use url::Url;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    #[serde(default)]
    relay: bool,
    #[serde(default)]
    http: bool,
    #[serde(default)]
    private: bool,
    #[serde(default)]
    services: BTreeMap<String, Service>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Service {
    base: String,
    #[serde(default)]
    secrets: BTreeMap<String, Secret>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Secret {
    Variable(String),
    File(SecretFile),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SecretFile {
    file: PathBuf,
}

#[derive(Debug, thiserror::Error)]
pub enum Refused {
    #[error("egress.toml does not read: {0}")]
    Unread(#[source] std::io::Error),
    #[error("egress.toml does not parse: {0}")]
    Unparsed(#[from] toml::de::Error),
    #[error("{service} is egress's own service: turn it on with `{service} = true`")]
    Reserved { service: String },
    #[error("{service}'s {header} names the variable {variable}, which is not set")]
    Unset {
        service: String,
        header: String,
        variable: String,
    },
    #[error("{service}'s {header} names the file {}, which does not read: {source}", .path.display())]
    Unopened {
        service: String,
        header: String,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{service}'s base does not parse: {source}")]
    Base {
        service: String,
        #[source]
        source: url::ParseError,
    },
    #[error("{service}'s base is an http or https URL with no query, fragment or credentials")]
    Unfit { service: String },
    #[error("the HTTP client does not build: {0}")]
    Client(#[source] reqwest::Error),
    #[error("egress.toml was not written: {0}")]
    Unwritten(#[source] std::io::Error),
}

impl Refused {
    pub fn reason(&self) -> &'static str {
        match self {
            Refused::Unread(_) | Refused::Unset { .. } | Refused::Unopened { .. } => {
                reason::NOT_FOUND
            }
            Refused::Unparsed(_)
            | Refused::Reserved { .. }
            | Refused::Base { .. }
            | Refused::Unfit { .. } => reason::INVALID_INPUT,
            Refused::Client(_) | Refused::Unwritten(_) => reason::UNSUPPORTED,
        }
    }
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum Withheld {
    #[error(
        "{service} carries secrets, which a set never names: a secret stays with a proxy in the cluster that holds it"
    )]
    Secrets { service: String },
    #[error(
        "private = true lets http reach private addresses, which only the node's own key may set"
    )]
    Private,
}

impl Withheld {
    pub fn reason(&self) -> &'static str {
        match self {
            Withheld::Secrets { .. } => reason::INVALID_INPUT,
            Withheld::Private => reason::UNAUTHORIZED,
        }
    }
}

pub fn withheld(text: &str) -> Result<Vec<Withheld>, Refused> {
    let file: File = toml::from_str(text)?;
    let private = file.private.then_some(Withheld::Private);
    let secreted = file
        .services
        .into_iter()
        .filter(|(_, settings)| !settings.secrets.is_empty())
        .map(|(service, _)| Withheld::Secrets { service });
    Ok(private.into_iter().chain(secreted).collect())
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Reach {
    #[default]
    Public,
    Private,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Upstream {
    pub base: Url,
    pub secrets: Vec<(String, String)>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Config {
    pub relay: bool,
    pub http: bool,
    pub reach: Reach,
    pub services: BTreeMap<String, Upstream>,
}

impl Config {
    pub fn parse(
        text: &str,
        env: impl Fn(&str) -> Option<String>,
        read: impl Fn(&Path) -> std::io::Result<String>,
    ) -> Result<Config, Refused> {
        let file: File = toml::from_str(text)?;
        let mut services = BTreeMap::new();
        for (name, service) in file.services {
            if RESERVED.contains(&name.as_str()) {
                return Err(Refused::Reserved { service: name });
            }
            let secrets = service
                .secrets
                .into_iter()
                .map(|(header, secret)| {
                    let value = reveal(&name, &header, secret, &env, &read)?;
                    Ok((header.to_ascii_lowercase(), value))
                })
                .collect::<Result<_, Refused>>()?;
            let upstream = Upstream {
                base: base(&name, &service.base)?,
                secrets,
            };
            services.insert(name, upstream);
        }
        let reach = match file.private {
            true => Reach::Private,
            false => Reach::Public,
        };
        Ok(Config {
            relay: file.relay,
            http: file.http,
            reach,
            services,
        })
    }

    pub fn served(&self) -> Vec<String> {
        let own = [(self.relay, RELAY), (self.http, HTTP)];
        let mut served: Vec<String> = own
            .into_iter()
            .filter(|(on, _)| *on)
            .map(|(_, name)| name.to_owned())
            .chain(self.services.keys().cloned())
            .collect();
        served.sort();
        served
    }
}

fn reveal(
    service: &str,
    header: &str,
    secret: Secret,
    env: impl Fn(&str) -> Option<String>,
    read: impl Fn(&Path) -> std::io::Result<String>,
) -> Result<String, Refused> {
    match secret {
        Secret::Variable(variable) => env(&variable).ok_or_else(|| Refused::Unset {
            service: service.to_owned(),
            header: header.to_owned(),
            variable,
        }),
        Secret::File(SecretFile { file }) => match read(&file) {
            Ok(text) => Ok(text.trim().to_owned()),
            Err(source) => Err(Refused::Unopened {
                service: service.to_owned(),
                header: header.to_owned(),
                path: file,
                source,
            }),
        },
    }
}

fn base(name: &str, text: &str) -> Result<Url, Refused> {
    let url = Url::parse(text.trim_end_matches('/')).map_err(|source| Refused::Base {
        service: name.to_owned(),
        source,
    })?;
    let web = matches!(url.scheme(), "http" | "https");
    let bare = url.query().is_none() && url.fragment().is_none();
    let anonymous = url.username().is_empty() && url.password().is_none();
    if !(web && bare && anonymous) {
        return Err(Refused::Unfit {
            service: name.to_owned(),
        });
    }
    Ok(url)
}

#[cfg(test)]
mod tests;
