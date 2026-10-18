//! What this node serves through egress and the HTTP clients that reach it, loaded from
//! `egress.toml` when the node starts and again on each `ducktape egress reload`.
use std::collections::BTreeSet;
use std::sync::Arc;

use tokio::sync::watch;

use crate::Workspace;
use crate::egress::TARGET;
use crate::egress::call::Clients;
use crate::egress::config::{Config, Refused};

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

pub struct Station {
    workspace: Workspace,
    serving: watch::Sender<Arc<Serving>>,
}

impl Station {
    pub fn open(workspace: Workspace) -> Result<Station, Refused> {
        let serving = Serving::load(&workspace)?;
        let (serving, _) = watch::channel(Arc::new(serving));
        Ok(Station { workspace, serving })
    }

    pub fn reload(&self) -> Result<(), Refused> {
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
        let changed = now
            .services
            .iter()
            .filter(|(name, upstream)| {
                was.services
                    .get(*name)
                    .is_some_and(|earlier| earlier != *upstream)
            })
            .count();
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
