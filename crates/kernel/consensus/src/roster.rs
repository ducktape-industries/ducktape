use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use abi::validators::Member;
use commonware_codec::DecodeExt as _;
use commonware_consensus::simplex::scheme::ed25519::Scheme;
use commonware_consensus::types::Epoch;
use commonware_cryptography::Signer as _;
use commonware_cryptography::certificate::{Provider, Scoped};
use commonware_cryptography::ed25519::{PrivateKey, PublicKey};
use commonware_utils::ordered::Set;

#[derive(Clone)]
pub struct Roster {
    namespace: Vec<u8>,
    me: Option<PrivateKey>,
    epochs: Arc<RwLock<BTreeMap<u64, Set<PublicKey>>>>,
}

impl Roster {
    pub fn new(namespace: Vec<u8>, me: Option<PrivateKey>) -> Roster {
        Roster {
            namespace,
            me,
            epochs: Arc::default(),
        }
    }

    pub fn seat(&self, epoch: u64, validators: Set<PublicKey>) {
        self.epochs
            .write()
            .expect("the roster lock is never poisoned")
            .insert(epoch, validators);
    }

    pub fn seated(&self, epoch: u64) -> Option<Set<PublicKey>> {
        self.epochs
            .read()
            .expect("the roster lock is never poisoned")
            .get(&epoch)
            .cloned()
    }

    pub fn scheme(&self, epoch: u64) -> Option<Scheme> {
        let validators = self.seated(epoch)?;
        let me = self.me.as_ref()?;
        Scheme::signer(&self.namespace, validators, me.clone())
    }

    pub fn verifier(&self, epoch: u64) -> Option<Scheme> {
        let validators = self.seated(epoch)?;
        Some(Scheme::verifier(&self.namespace, validators))
    }

    pub fn participates(&self, epoch: u64) -> bool {
        let Some(me) = &self.me else {
            return false;
        };
        self.seated(epoch)
            .is_some_and(|validators| validators.position(&me.public_key()).is_some())
    }
}

impl Provider for Roster {
    type Scope = Epoch;
    type Scheme = Scheme;

    fn scoped(&self, epoch: Epoch) -> Option<Scoped<Scheme>> {
        let scheme = match self.scheme(epoch.get()) {
            Some(scheme) => Scoped::scheme(Arc::new(scheme)),
            None => Scoped::verifier(Arc::new(self.verifier(epoch.get())?)),
        };
        Some(scheme)
    }
}

pub fn validators_of(members: &[Member]) -> Option<Set<PublicKey>> {
    let keys: Vec<PublicKey> = members
        .iter()
        .map(|member| PublicKey::decode(member.key.as_slice()))
        .collect::<Result<_, _>>()
        .ok()?;
    Set::try_from(keys).ok()
}
