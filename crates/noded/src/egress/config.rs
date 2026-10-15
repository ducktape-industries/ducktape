//! What this node serves through the egress role, read from `egress.toml` in its workspace: the
//! relay, the public internet and whether it may reach private addresses, and each registered
//! service it fronts with the base URL its requests go to and the headers carrying its secrets.
use std::collections::BTreeMap;

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
    secrets: BTreeMap<String, String>,
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
    pub fn parse(text: &str, env: impl Fn(&str) -> Option<String>) -> Result<Config, String> {
        let file: File = toml::from_str(text).map_err(|error| error.to_string())?;
        let mut services = BTreeMap::new();
        for (name, service) in file.services {
            if RESERVED.contains(&name.as_str()) {
                return Err(format!(
                    "{name} is egress's own service: turn it on with `{name} = true`"
                ));
            }
            let secrets = service
                .secrets
                .into_iter()
                .map(|(header, variable)| {
                    let value = env(&variable).ok_or_else(|| {
                        format!("{name}'s {header} names {variable}, which is not set")
                    })?;
                    Ok((header.to_ascii_lowercase(), value))
                })
                .collect::<Result<_, String>>()?;
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

fn base(name: &str, text: &str) -> Result<Url, String> {
    let url = Url::parse(text.trim_end_matches('/'))
        .map_err(|error| format!("{name}'s base does not parse: {error}"))?;
    let web = matches!(url.scheme(), "http" | "https");
    let bare = url.query().is_none() && url.fragment().is_none();
    let anonymous = url.username().is_empty() && url.password().is_none();
    if !(web && bare && anonymous) {
        return Err(format!(
            "{name}'s base is an http or https URL with no query, fragment or credentials"
        ));
    }
    Ok(url)
}

#[cfg(test)]
mod tests;
