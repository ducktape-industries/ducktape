//! What this node serves through egress and the HTTP clients that reach it, loaded from
//! `egress.toml` when the node starts, on each `ducktape egress reload` and from each `egress set`.
use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::watch;

use crate::Workspace;
use crate::egress::TARGET;
use crate::egress::call::Clients;
use crate::egress::config::{Config, Refused};
use crate::wire::Reached;

pub struct Serving {
    pub config: Config,
    pub clients: Clients,
}

impl Serving {
    fn load(workspace: &Workspace) -> Result<Serving, Refused> {
        let config = workspace.egress()?;
        let clients = Clients::new(config.reach)?;
        Ok(Serving { config, clients })
    }
}

pub struct Planned {
    serving: Serving,
    text: String,
    pub tally: Tally,
}

impl Planned {
    pub async fn reach(&self, wait: Duration) -> Vec<(String, Reached)> {
        let clients = &self.serving.clients;
        let calls = self
            .serving
            .config
            .services
            .iter()
            .map(|(name, upstream)| async move {
                (name.clone(), clients.reach(&upstream.base, wait).await)
            });
        futures::future::join_all(calls).await
    }
}

pub struct Station {
    workspace: Workspace,
    serving: watch::Sender<Arc<Serving>>,
    writing: Mutex<()>,
}

impl Station {
    pub fn open(workspace: Workspace) -> Result<Station, Refused> {
        let serving = Serving::load(&workspace)?;
        let (serving, _) = watch::channel(Arc::new(serving));
        Ok(Station {
            workspace,
            serving,
            writing: Mutex::new(()),
        })
    }

    pub fn reload(&self) -> Result<(), Refused> {
        let _writing = self
            .writing
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let serving = match Serving::load(&self.workspace) {
            Ok(serving) => serving,
            Err(refused) => {
                tracing::warn!(
                    target: TARGET,
                    event = "egress_reload_refused",
                    reason = refused.reason(),
                    "egress.toml was refused; the node serves what it served before"
                );
                return Err(refused);
            }
        };
        let now = Arc::new(serving);
        let was = self.serving.send_replace(now.clone());
        let tally = Tally::between(&was.config, &now.config);
        tracing::info!(
            target: TARGET,
            event = "egress_reloaded",
            served = tally.served,
            added = tally.added,
            removed = tally.removed,
            changed = tally.changed,
            "the node re-read egress.toml"
        );
        Ok(())
    }

    pub fn grants(&self) -> std::io::Result<Vec<Vec<u8>>> {
        self.workspace.egress_grants()
    }

    pub fn plan(&self, text: String) -> Result<Planned, Refused> {
        let config = self.workspace.egress_from(&text)?;
        let clients = Clients::new(config.reach)?;
        let tally = Tally::between(&self.current().config, &config);
        Ok(Planned {
            serving: Serving { config, clients },
            text,
            tally,
        })
    }

    pub fn put(&self, planned: Planned) -> Result<Tally, Refused> {
        let _writing = self
            .writing
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.workspace
            .write_egress(&planned.text)
            .map_err(Refused::Unwritten)?;
        self.serving.send_replace(Arc::new(planned.serving));
        let tally = planned.tally;
        tracing::info!(
            target: TARGET,
            event = "egress_set",
            served = tally.served,
            added = tally.added,
            removed = tally.removed,
            changed = tally.changed,
            "the node serves the egress.toml it was sent"
        );
        Ok(tally)
    }

    pub fn current(&self) -> Arc<Serving> {
        self.serving.borrow().clone()
    }

    pub fn subscribe(&self) -> watch::Receiver<Arc<Serving>> {
        self.serving.subscribe()
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct Tally {
    pub served: usize,
    pub added: usize,
    pub removed: usize,
    pub changed: usize,
}

impl Tally {
    pub fn between(was: &Config, now: &Config) -> Tally {
        let before: BTreeSet<String> = was.served().into_iter().collect();
        let after: BTreeSet<String> = now.served().into_iter().collect();
        let services = now.services.iter().filter(|(name, upstream)| {
            was.services
                .get(*name)
                .is_some_and(|earlier| earlier != *upstream)
        });
        let peers = now.crosspond.iter().filter(|(name, peer)| {
            was.crosspond
                .get(*name)
                .is_some_and(|earlier| earlier != *peer)
        });
        let changed = services.count() + peers.count();
        Tally {
            served: after.len(),
            added: after.difference(&before).count(),
            removed: before.difference(&after).count(),
            changed,
        }
    }
}

#[cfg(test)]
mod tests;
