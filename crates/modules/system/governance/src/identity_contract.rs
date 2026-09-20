use serde::{Deserialize, Serialize};

use sdk::AccountNumber;

/// The governance consumer only needs the account number resolved from an
/// identity key. Keep the rest of the account reply strict at this boundary,
/// while deliberately not importing the identity module's wire crate.
#[allow(dead_code)]
#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub(crate) struct AccountView {
    pub number: AccountNumber,
    name: String,
    control: serde::de::IgnoredAny,
    keys: Vec<serde::de::IgnoredAny>,
    avatar: Option<String>,
    bio: Option<String>,
    updated_at: u64,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum IdentityQuery {
    Get { number: AccountNumber },
    OfKey { key: Vec<u8> },
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum IdentityReply {
    Account(Option<AccountView>),
}

pub(crate) fn encode_query(query: &IdentityQuery) -> Vec<u8> {
    sdk::wire::encode(query)
}

pub(crate) fn decode_reply(bytes: &[u8]) -> Result<IdentityReply, String> {
    sdk::wire::decode(bytes)
}

pub(crate) fn account_principal(number: AccountNumber) -> Vec<u8> {
    number.to_le_bytes().to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_bytes_match_identity_wire() {
        assert_eq!(
            encode_query(&IdentityQuery::OfKey { key: vec![1, 2] }),
            br#"{"of_key":{"key":[1,2]}}"#
        );
        assert_eq!(
            encode_query(&IdentityQuery::Get { number: 7 }),
            br#"{"get":{"number":7}}"#
        );
    }
}
