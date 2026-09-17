//! Per-application secret input buffers, redacted and wiped on replacement or drop.

use std::collections::BTreeMap;

use zeroize::Zeroizing;

pub struct Secret(Zeroizing<String>);

impl Secret {
    pub fn new(text: impl Into<String>) -> Self {
        Self(Zeroizing::new(text.into()))
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Secret(<redacted>)")
    }
}

#[derive(Default)]
pub struct SecretStore {
    slots: BTreeMap<&'static str, Zeroizing<String>>,
}

impl SecretStore {
    pub fn text(&self, slot: &'static str) -> &str {
        self.slots.get(slot).map_or("", |held| held.as_str())
    }

    pub fn set(&mut self, slot: &'static str, text: String) {
        self.slots.insert(slot, Zeroizing::new(text));
    }

    pub fn clear(&mut self, slot: &'static str) {
        self.slots.remove(slot);
    }

    pub fn read(&self, slot: &'static str) -> Secret {
        Secret::new(self.text(slot))
    }
}

impl std::fmt::Debug for SecretStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SecretStore")
            .field("slots", &self.slots.len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::{Secret, SecretStore};

    #[test]
    fn a_secret_never_prints_itself() {
        let secret = Secret::new("abandon abandon about");
        let printed = format!("{secret:?}");
        assert_eq!(printed, "Secret(<redacted>)");
        assert!(!printed.contains("abandon"));
        assert_eq!(secret.expose(), "abandon abandon about");
    }

    #[test]
    fn a_store_answers_facts_and_hands_over_content() {
        let mut store = SecretStore::default();
        assert_eq!(store.text("phrase"), "");

        store.set("phrase", "abandon about".to_owned());
        assert_eq!(store.read("phrase").expose(), "abandon about");
        assert!(!format!("{store:?}").contains("abandon"));

        // A read leaves the slot alone: a derivation that refused the phrase
        // must not cost the owner their typing.
        assert_eq!(store.read("phrase").expose(), "abandon about");

        store.clear("phrase");
        assert_eq!(store.text("phrase"), "");
        assert!(store.read("phrase").expose().is_empty());
    }

    #[test]
    fn slots_do_not_leak_into_each_other() {
        let mut store = SecretStore::default();
        store.set("phrase", "words".to_owned());
        store.set("passphrase", "extra".to_owned());
        store.clear("phrase");
        assert!(store.text("phrase").is_empty());
        assert_eq!(store.text("passphrase"), "extra");
    }
}
