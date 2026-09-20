//! The sim's small, consumer-owned views of module wires.
//!
//! These types are deliberately kept beside the scenario node.  The sim sends
//! the same JSON bytes as the guests, but it does not need to link producer
//! crates merely to build a scenario or decode the echo worker's two effects.

pub mod dispatch {
    use serde::{Deserialize, Serialize};
    use std::collections::BTreeMap;

    pub const WORK_SPEC_KIND: &str = "dispatch-work-v1";

    #[derive(Default, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum AdmissionPolicy {
        #[default]
        Queue,
        FailFast,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    pub struct WorkSpec {
        pub kind: String,
        pub dispatch_id: String,
        pub capability: String,
        pub payload: Vec<u8>,
        pub demands: BTreeMap<String, u64>,
        #[serde(default, skip_serializing_if = "is_queue")]
        pub admission: AdmissionPolicy,
    }

    fn is_queue(policy: &AdmissionPolicy) -> bool {
        matches!(policy, AdmissionPolicy::Queue)
    }

    pub fn encode_work_spec(spec: &WorkSpec) -> Vec<u8> {
        sdk::wire::encode(spec)
    }

    pub fn decode_work_spec(bytes: &[u8]) -> Result<WorkSpec, String> {
        sdk::wire::decode(bytes)
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn work_spec_fixture_is_stable() {
            let spec = WorkSpec {
                kind: WORK_SPEC_KIND.into(),
                dispatch_id: "d1".into(),
                capability: "echo".into(),
                payload: b"hi".to_vec(),
                demands: BTreeMap::new(),
                admission: AdmissionPolicy::Queue,
            };
            assert_eq!(
                encode_work_spec(&spec),
                br#"{"kind":"dispatch-work-v1","dispatch_id":"d1","capability":"echo","payload":[104,105],"demands":{}}"#
            );
        }
    }
}

pub mod saga {
    use serde::{Deserialize, Serialize};

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum SagaMsg {
        Accept {
            saga_id: String,
            attempt: u32,
        },
        OracleResult {
            saga_id: String,
            attempt: u32,
            outcome: Result<Vec<u8>, String>,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            usage: Option<TokenUsage>,
        },
    }

    #[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    pub struct TokenUsage {
        pub input_tokens: u64,
        pub cached_input_tokens: u64,
        pub cache_write_input_tokens: u64,
        pub output_tokens: u64,
        pub reasoning_output_tokens: u64,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    pub struct WorkerRequest {
        pub saga_id: String,
        pub attempt: u32,
        pub spec: Vec<u8>,
        pub deadline: Option<u64>,
        pub assignee: Option<Vec<u8>>,
    }

    pub fn namespaced_id(origin: &sdk::Origin, local_id: &str) -> String {
        format!("{}{}{local_id}", origin.actor_string(), sdk::KEY_SEP)
    }

    pub fn encode_msg(message: &SagaMsg) -> Vec<u8> {
        sdk::wire::encode(message)
    }

    pub fn decode_worker_request(bytes: &[u8]) -> Result<WorkerRequest, String> {
        sdk::wire::decode(bytes)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn accept_fixture_is_stable() {
            assert_eq!(
                encode_msg(&SagaMsg::Accept {
                    saga_id: "s".into(),
                    attempt: 1,
                }),
                br#"{"accept":{"saga_id":"s","attempt":1}}"#
            );
        }
    }
}

pub mod files {
    use serde::{Deserialize, Serialize};

    pub use duckfs_core::{Actor, Change};

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(rename_all = "snake_case")]
    pub enum FilesMsg {
        Commit {
            base_snapshot: Option<String>,
            message: String,
            changes: Vec<Change>,
        },
    }

    pub fn encode_msg(message: &FilesMsg) -> Vec<u8> {
        sdk::wire::encode(message)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn commit_fixture_is_stable() {
            assert_eq!(
                encode_msg(&FilesMsg::Commit {
                    base_snapshot: None,
                    message: "m".into(),
                    changes: vec![Change::Mkdir { path: "/x".into() }],
                }),
                br#"{"commit":{"base_snapshot":null,"message":"m","changes":[{"mkdir":{"path":"/x"}}]}}"#
            );
        }
    }
}

pub mod gateway {
    use serde::{Deserialize, Serialize};

    pub const GATEWAY_ROUTE_NS: &[u8] = b"ducktape-gateway-route-v1";

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
    #[serde(deny_unknown_fields)]
    pub struct RouteName {
        pub label: Option<String>,
    }

    impl RouteName {
        pub const fn apex() -> Self {
            Self { label: None }
        }

        pub fn named(label: impl Into<String>) -> Self {
            Self {
                label: Some(label.into()),
            }
        }
    }

    #[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum RouteMethod {
        Get,
        Head,
        Post,
        Put,
        Patch,
        Delete,
    }

    impl RouteMethod {
        pub const fn permits_body(self) -> bool {
            matches!(self, Self::Post | Self::Put | Self::Patch | Self::Delete)
        }
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
    pub enum RouteAudience {
        Owner,
        Network,
        Accounts { account_ids: Vec<u64> },
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    pub struct RoutePolicy {
        pub audience: RouteAudience,
        pub methods: Vec<RouteMethod>,
        pub max_request_bytes: Option<u64>,
        pub max_response_bytes: u64,
        pub allow_authorization: bool,
        pub allow_upgrade: bool,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
    pub enum RouteTarget {
        DuckFs { manifest_sha256: String },
        LoopbackHttp,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    pub struct RouteDefinition {
        pub target: RouteTarget,
        pub policy: RoutePolicy,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    pub struct RouteStatement {
        pub chain_id: String,
        pub account_id: u64,
        pub name: RouteName,
        pub publisher_node: Vec<u8>,
        pub revision: u64,
        pub route: Option<RouteDefinition>,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    pub struct MemberAuthorization {
        pub signer: Vec<u8>,
        pub signature: Vec<u8>,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum GatewayMsg {
        SetRoute {
            statement: RouteStatement,
            authorization: MemberAuthorization,
        },
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum GatewayQuery {
        Get { account_id: u64, name: RouteName },
        List { account_id: u64 },
    }

    pub fn route_signing_preimage(statement: &RouteStatement) -> Result<Vec<u8>, String> {
        let mut out = Vec::new();
        sdk::codec::push_bytes(&mut out, statement.chain_id.as_bytes());
        out.extend_from_slice(&statement.account_id.to_le_bytes());
        match statement.name.label.as_deref() {
            Some(label) => {
                out.push(1);
                sdk::codec::push_bytes(&mut out, label.as_bytes());
            }
            None => out.push(0),
        }
        sdk::codec::push_bytes(&mut out, &statement.publisher_node);
        out.extend_from_slice(&statement.revision.to_le_bytes());
        let Some(route) = &statement.route else {
            out.push(0);
            return Ok(out);
        };
        out.push(1);
        encode_policy(&mut out, &route.policy);
        match &route.target {
            RouteTarget::DuckFs { manifest_sha256 } => {
                out.push(1);
                let bytes = decode_hex_32(manifest_sha256)?;
                out.extend_from_slice(&bytes);
            }
            RouteTarget::LoopbackHttp => out.push(2),
        }
        Ok(out)
    }

    fn encode_policy(out: &mut Vec<u8>, policy: &RoutePolicy) {
        match &policy.audience {
            RouteAudience::Owner => out.push(1),
            RouteAudience::Network => out.push(2),
            RouteAudience::Accounts { account_ids } => {
                out.push(3);
                out.extend_from_slice(&(account_ids.len() as u64).to_le_bytes());
                for account in account_ids {
                    out.extend_from_slice(&account.to_le_bytes());
                }
            }
        }
        out.extend_from_slice(&(policy.methods.len() as u64).to_le_bytes());
        for method in &policy.methods {
            out.push(match method {
                RouteMethod::Get => 1,
                RouteMethod::Head => 2,
                RouteMethod::Post => 3,
                RouteMethod::Put => 4,
                RouteMethod::Patch => 5,
                RouteMethod::Delete => 6,
            });
        }
        match policy.max_request_bytes {
            Some(bytes) => {
                out.push(1);
                out.extend_from_slice(&bytes.to_le_bytes());
            }
            None => out.push(0),
        }
        out.extend_from_slice(&policy.max_response_bytes.to_le_bytes());
        out.push(u8::from(policy.allow_authorization));
        out.push(u8::from(policy.allow_upgrade));
    }

    fn decode_hex_32(value: &str) -> Result<[u8; 32], String> {
        if value.len() != 64 {
            return Err("manifest hash must be 32 bytes".into());
        }
        let mut out = [0; 32];
        for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
            let high = hex_nibble(pair[0])?;
            let low = hex_nibble(pair[1])?;
            out[index] = high << 4 | low;
        }
        Ok(out)
    }

    fn hex_nibble(byte: u8) -> Result<u8, String> {
        match byte {
            b'0'..=b'9' => Ok(byte - b'0'),
            b'a'..=b'f' => Ok(byte - b'a' + 10),
            _ => Err("manifest hash must be lowercase hex".into()),
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn query_fixture_and_route_preimage_are_stable() {
            let query = GatewayQuery::Get {
                account_id: 1,
                name: RouteName::named("api"),
            };
            assert_eq!(
                sdk::wire::encode(&query),
                br#"{"get":{"account_id":1,"name":{"label":"api"}}}"#
            );
            let statement = RouteStatement {
                chain_id: "local".into(),
                account_id: 1,
                name: RouteName::apex(),
                publisher_node: vec![7; 32],
                revision: 1,
                route: None,
            };
            let mut expected = vec![5, 0, 0, 0, 0, 0, 0, 0];
            expected.extend_from_slice(b"local");
            expected.extend_from_slice(&1u64.to_le_bytes());
            expected.push(0);
            expected.extend_from_slice(&(32u64).to_le_bytes());
            expected.extend_from_slice(&[7; 32]);
            expected.extend_from_slice(&1u64.to_le_bytes());
            expected.push(0);
            assert_eq!(route_signing_preimage(&statement).unwrap(), expected);
        }
    }
}

pub mod governance {
    use commonware_cryptography::{Signer as _, Verifier as _, ed25519};
    use serde::{Deserialize, Serialize};

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum GovAction {
        AddResident {
            key: Vec<u8>,
        },
        RemoveValidator {
            key: Vec<u8>,
        },
        Signal {
            text: String,
        },
        UpdateModule {
            name: String,
            module_id: String,
            activation_lead: u64,
            code_hash: Vec<u8>,
        },
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum GovMsg {
        Propose {
            proposal_id: String,
            action: GovAction,
            voting_period: u64,
        },
        Vote {
            proposal_id: String,
            approve: bool,
        },
        Execute {
            proposal_id: String,
        },
    }

    pub fn encode_msg(message: &GovMsg) -> Vec<u8> {
        sdk::wire::encode(message)
    }

    pub mod invite {
        use super::*;

        pub const INVITE_GRANT_NAMESPACE: &[u8] = b"ducktape-invite-grant-v1";
        pub const INVITE_JOIN_NAMESPACE: &[u8] = b"ducktape-invite-join-v1";
        pub const INVITE_NONCE_LEN: usize = 16;

        #[derive(Clone, Debug, PartialEq)]
        pub struct InviteToken {
            pub issuer: ed25519::PublicKey,
            pub nonce: [u8; INVITE_NONCE_LEN],
            pub expires_unix_secs: u64,
            pub sig: ed25519::Signature,
        }

        pub fn sign_join_proof(
            joiner: &ed25519::PrivateKey,
            binding: &[u8],
            token: &InviteToken,
        ) -> ed25519::Signature {
            let msg = [
                binding,
                token.nonce.as_slice(),
                joiner.public_key().as_ref(),
            ]
            .concat();
            joiner.sign(INVITE_JOIN_NAMESPACE, &msg)
        }

        pub fn verify_invite_token(token: &InviteToken, binding: &[u8]) -> bool {
            let msg = [
                binding,
                token.nonce.as_slice(),
                &token.expires_unix_secs.to_le_bytes(),
            ]
            .concat();
            token
                .issuer
                .verify(INVITE_GRANT_NAMESPACE, &msg, &token.sig)
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn proposal_fixture_is_stable() {
            let message = GovMsg::Propose {
                proposal_id: "p".into(),
                action: GovAction::Signal { text: "ok".into() },
                voting_period: 1,
            };
            assert_eq!(
                encode_msg(&message),
                br#"{"propose":{"proposal_id":"p","action":{"signal":{"text":"ok"}},"voting_period":1}}"#
            );
        }
    }
}

pub mod tasks {
    pub const MAX_ATTEMPTS: u64 = 8;
    pub const MIN_LEASE_VIEWS: u64 = 10;
}

pub mod runs {
    use serde_json::{Value, json};

    pub const MAX_ACTIONS_PER_SESSION: u32 = 32;

    fn object(fields: impl IntoIterator<Item = (&'static str, Value)>) -> Value {
        let fields: serde_json::Map<String, Value> = fields
            .into_iter()
            .map(|(key, value)| (key.to_string(), value))
            .collect();
        json!({ "map": fields })
    }

    fn reference(path: &[&str]) -> Value {
        json!({ "ref": path })
    }

    fn text(value: &str) -> Value {
        json!({ "text": value })
    }

    fn number(value: u64) -> Value {
        json!({ "number": value })
    }

    fn equals(left: Value, right: Value) -> Value {
        json!({ "equals": { "left": left, "right": right } })
    }

    fn operation(
        name: &'static str,
        fields: impl IntoIterator<Item = (&'static str, Value)>,
    ) -> Value {
        object([(name, object(fields))])
    }

    fn call(module: &str, msg: Value, bind: &str, failure: u64) -> Value {
        json!({
            "call": {
                "module": module,
                "msg": msg,
                "bind": bind,
                "decode": "json",
                "on_failure": { "step": failure },
            }
        })
    }

    fn branch(test: Value, then: u64, or: u64) -> Value {
        json!({ "branch": { "test": test, "then": then, "or": or } })
    }

    fn all(values: impl IntoIterator<Item = Value>) -> Value {
        json!({ "all": values.into_iter().collect::<Vec<_>>() })
    }

    fn any(values: impl IntoIterator<Item = Value>) -> Value {
        json!({ "any": values.into_iter().collect::<Vec<_>>() })
    }

    /// The exact default model program, represented in the agent wire's JSON
    /// form so the sim does not link the agent producer crate.
    pub fn model_program(agent_id: &str) -> Value {
        let request = || reference(&["change", "source", "object"]);
        let mut steps = vec![
            branch(
                all([
                    equals(reference(&["change", "source", "module"]), text("runs")),
                    equals(
                        reference(&["change", "source", "kind"]),
                        text("action_request"),
                    ),
                    equals(reference(&["change", "kind"]), text("added")),
                ]),
                1,
                0,
            ),
            json!({
                "query": {
                    "module": "runs",
                    "query": operation("action_plan", [("request_id", request())]),
                    "bind": "proposal",
                }
            }),
        ];
        for module in ["chat", "pages", "tasks", "files", "forge", "runs"] {
            let route = steps.len() as u64;
            let claim = route + 1;
            let target = route + 2;
            let complete = route + 3;
            let finish = route + 4;
            steps.push(branch(
                equals(
                    reference(&["proposal", "action_request", "target"]),
                    text(module),
                ),
                claim,
                finish + 1,
            ));
            steps.push(call(
                "runs",
                operation(
                    "claim_action_request",
                    [("request_id", request()), ("target_step", number(target))],
                ),
                "plan",
                finish,
            ));
            steps.push(call(
                module,
                reference(&["plan", "applied", "output", "payload"]),
                "effect",
                complete,
            ));
            steps.push(call(
                "runs",
                operation(
                    "complete_action_request",
                    [
                        ("request_id", request()),
                        ("result", reference(&["effect"])),
                        (
                            "call",
                            object([
                                (
                                    "requester",
                                    reference(&["plan", "applied", "output", "requester"]),
                                ),
                                (
                                    "invocation",
                                    reference(&["plan", "applied", "output", "invocation"]),
                                ),
                                ("step", number(target)),
                            ]),
                        ),
                    ],
                ),
                "receipt",
                finish,
            ));
            steps.push(json!("finish"));
        }
        let unsupported = steps.len() as u64;
        steps.push(call(
            "runs",
            operation(
                "reject_action_request",
                [
                    ("request_id", request()),
                    (
                        "reason",
                        text("the model program has no route for this action target"),
                    ),
                ],
            ),
            "rejection",
            unsupported + 1,
        ));
        steps.push(json!("finish"));
        let mention = steps.len() as u64;
        let mention_intake = all([
            equals(reference(&["change", "kind"]), text("added")),
            any([
                all([
                    equals(reference(&["change", "reason"]), text("mention")),
                    any([
                        equals(reference(&["change", "source", "module"]), text("chat")),
                        equals(reference(&["change", "source", "module"]), text("pages")),
                    ]),
                ]),
                all([
                    equals(reference(&["change", "source", "module"]), text("runs")),
                    equals(
                        reference(&["change", "source", "kind"]),
                        text("run_request"),
                    ),
                ]),
            ]),
        ]);
        steps.push(branch(mention_intake, mention + 1, mention + 2));
        steps.push(call(
            "runs",
            operation(
                "request_attributed_run",
                [
                    ("agent_id", text(agent_id)),
                    ("change_seq", reference(&["change", "seq"])),
                ],
            ),
            "run",
            mention + 2,
        ));
        steps.push(json!("finish"));
        if let Some(Value::Object(branch)) = steps.first_mut()
            && let Some(Value::Object(fields)) = branch.get_mut("branch")
        {
            fields.insert("or".into(), json!(mention));
        }
        json!({ "steps": steps })
    }
}
