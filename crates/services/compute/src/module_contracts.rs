//! The compute service's small, local slices of module-owned wire contracts.
//!
//! These types intentionally stop at the values this host consumes or emits.
//! Their JSON shapes are producer-owned: the fixtures below are pinned to
//! ducktape-sdk at commit `736865710dcfa7c56f9834747287881c1c25d45d`. The
//! consumer tests compare local codecs and decoders with those committed bytes;
//! independent producer-codec execution is recorded in the review evidence.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub(crate) const WORK_SPEC_KIND: &str = "dispatch-work-v1";
pub(crate) const RESOURCE_UNAVAILABLE_RESULT: &[u8] = br#"{"code":"RESOURCE_UNAVAILABLE"}"#;
pub(crate) const MAX_RESULT_BYTES: usize = 256 * 1024;
pub const SKILL_LIBRARY_PREFIX: &str = "/shared/skills";
pub const MAX_SKILLS_PER_AGENT: usize = 64;
pub(crate) const WORKER_CONTROL_KIND: &str = "ducktape_worker_control";

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct WorkSpec {
    pub(crate) kind: String,
    pub(crate) dispatch_id: String,
    pub(crate) capability: String,
    pub(crate) payload: Vec<u8>,
    pub(crate) demands: BTreeMap<String, u64>,
    #[serde(default, skip_serializing_if = "AdmissionPolicy::is_queue")]
    pub(crate) admission: AdmissionPolicy,
}

#[derive(Serialize, Deserialize, Debug, Default, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum AdmissionPolicy {
    #[default]
    Queue,
    FailFast,
}

impl AdmissionPolicy {
    fn is_queue(&self) -> bool {
        *self == Self::Queue
    }
}

#[cfg(test)]
pub(crate) fn encode_work_spec(spec: &WorkSpec) -> Vec<u8> {
    sdk::wire::encode(spec)
}

pub(crate) fn decode_work_spec(bytes: &[u8]) -> Result<WorkSpec, String> {
    let spec: WorkSpec = sdk::wire::decode(bytes)?;
    if spec.kind != WORK_SPEC_KIND {
        return Err(format!("not a dispatch work spec (kind {:?})", spec.kind));
    }
    Ok(spec)
}

pub(crate) type SagaId = String;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct WorkerRequest {
    pub(crate) saga_id: SagaId,
    pub(crate) attempt: u32,
    pub(crate) spec: Vec<u8>,
    pub(crate) deadline: Option<u64>,
    pub(crate) assignee: Option<Vec<u8>>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct WorkerControl {
    pub(crate) kind: String,
    pub(crate) command: WorkerControlCommand,
}

impl WorkerControl {
    #[cfg(test)]
    pub(crate) fn cancel_attempt(saga_id: SagaId, attempt: u32, assignee: Vec<u8>) -> Self {
        Self {
            kind: WORKER_CONTROL_KIND.into(),
            command: WorkerControlCommand::CancelAttempt {
                saga_id,
                attempt,
                assignee,
            },
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum WorkerControlCommand {
    CancelAttempt {
        saga_id: SagaId,
        attempt: u32,
        assignee: Vec<u8>,
    },
}

#[cfg(test)]
pub(crate) fn encode_worker_request(request: &WorkerRequest) -> Vec<u8> {
    sdk::wire::encode(request)
}

pub(crate) fn decode_worker_request(bytes: &[u8]) -> Result<WorkerRequest, String> {
    sdk::wire::decode(bytes)
}

#[cfg(test)]
pub(crate) fn encode_worker_control(control: &WorkerControl) -> Vec<u8> {
    sdk::wire::encode(control)
}

pub(crate) fn decode_worker_control(bytes: &[u8]) -> Result<WorkerControl, String> {
    let control: WorkerControl = sdk::wire::decode(bytes)?;
    if control.kind != WORKER_CONTROL_KIND {
        return Err(format!("not a worker control (kind {:?})", control.kind));
    }
    Ok(control)
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum SagaMsg {
    OracleResult {
        saga_id: SagaId,
        attempt: u32,
        outcome: Result<Vec<u8>, String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<TokenUsage>,
    },
    RenewLease {
        saga_id: SagaId,
        attempt: u32,
    },
    Accept {
        saga_id: SagaId,
        attempt: u32,
    },
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, Default, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct TokenUsage {
    pub(crate) input_tokens: u64,
    pub(crate) cached_input_tokens: u64,
    pub(crate) cache_write_input_tokens: u64,
    pub(crate) output_tokens: u64,
    pub(crate) reasoning_output_tokens: u64,
}

pub(crate) fn encode_saga_msg(message: &SagaMsg) -> Vec<u8> {
    sdk::wire::encode(message)
}

#[cfg(test)]
pub(crate) fn decode_saga_msg(bytes: &[u8]) -> Result<SagaMsg, String> {
    sdk::wire::decode(bytes)
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReplyBlock {
    pub(crate) kind: String,
    pub(crate) text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) lang: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct ActionEnvelope {
    pub(crate) operation: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) target: Option<Value>,
    #[serde(default)]
    pub(crate) input: Value,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct AgentResponse {
    #[serde(default)]
    pub(crate) reply_blocks: Vec<ReplyBlock>,
    #[serde(default)]
    pub(crate) actions: Vec<ActionEnvelope>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) commit_message: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn producer_fixtures_cover_work_spec_defaults_and_admission_tag() {
        let queue = WorkSpec {
            kind: WORK_SPEC_KIND.into(),
            dispatch_id: "d".into(),
            capability: "c".into(),
            payload: b"input".to_vec(),
            demands: BTreeMap::new(),
            admission: AdmissionPolicy::Queue,
        };
        let queue_bytes = br#"{"kind":"dispatch-work-v1","dispatch_id":"d","capability":"c","payload":[105,110,112,117,116],"demands":{}}"#;
        assert_eq!(encode_work_spec(&queue), queue_bytes);
        assert_eq!(decode_work_spec(queue_bytes).unwrap(), queue);

        let fail_fast = WorkSpec {
            admission: AdmissionPolicy::FailFast,
            ..queue.clone()
        };
        let fail_fast_bytes = br#"{"kind":"dispatch-work-v1","dispatch_id":"d","capability":"c","payload":[105,110,112,117,116],"demands":{},"admission":"fail_fast"}"#;
        assert_eq!(encode_work_spec(&fail_fast), fail_fast_bytes);
        assert_eq!(decode_work_spec(fail_fast_bytes).unwrap(), fail_fast);
    }

    #[test]
    fn producer_fixtures_cover_worker_and_control_shapes() {
        let request = WorkerRequest {
            saga_id: "s".into(),
            attempt: 2,
            spec: vec![1, 2],
            deadline: Some(9),
            assignee: Some(vec![3, 4]),
        };
        let request_bytes =
            br#"{"saga_id":"s","attempt":2,"spec":[1,2],"deadline":9,"assignee":[3,4]}"#;
        assert_eq!(encode_worker_request(&request), request_bytes);
        assert_eq!(decode_worker_request(request_bytes).unwrap(), request);

        let control = WorkerControl {
            kind: WORKER_CONTROL_KIND.into(),
            command: WorkerControlCommand::CancelAttempt {
                saga_id: "s".into(),
                attempt: 2,
                assignee: vec![3, 4],
            },
        };
        let control_bytes = br#"{"kind":"ducktape_worker_control","command":{"cancel_attempt":{"saga_id":"s","attempt":2,"assignee":[3,4]}}}"#;
        assert_eq!(encode_worker_control(&control), control_bytes);
        assert_eq!(decode_worker_control(control_bytes).unwrap(), control);
    }

    #[test]
    fn producer_fixtures_cover_saga_message_tags_and_usage() {
        let oracle = SagaMsg::OracleResult {
            saga_id: "s".into(),
            attempt: 2,
            outcome: Ok(vec![1, 2]),
            usage: Some(TokenUsage {
                input_tokens: 1,
                cached_input_tokens: 2,
                cache_write_input_tokens: 3,
                output_tokens: 4,
                reasoning_output_tokens: 5,
            }),
        };
        let oracle_bytes = br#"{"oracle_result":{"saga_id":"s","attempt":2,"outcome":{"Ok":[1,2]},"usage":{"input_tokens":1,"cached_input_tokens":2,"cache_write_input_tokens":3,"output_tokens":4,"reasoning_output_tokens":5}}}"#;
        assert_eq!(encode_saga_msg(&oracle), oracle_bytes);
        assert_eq!(decode_saga_msg(oracle_bytes).unwrap(), oracle);

        let failed = SagaMsg::OracleResult {
            saga_id: "s".into(),
            attempt: 2,
            outcome: Err("failed".into()),
            usage: None,
        };
        let failed_bytes =
            br#"{"oracle_result":{"saga_id":"s","attempt":2,"outcome":{"Err":"failed"}}}"#;
        assert_eq!(encode_saga_msg(&failed), failed_bytes);
        assert_eq!(decode_saga_msg(failed_bytes).unwrap(), failed);

        let renew = SagaMsg::RenewLease {
            saga_id: "s".into(),
            attempt: 2,
        };
        let renew_bytes = br#"{"renew_lease":{"saga_id":"s","attempt":2}}"#;
        assert_eq!(encode_saga_msg(&renew), renew_bytes);
        assert_eq!(decode_saga_msg(renew_bytes).unwrap(), renew);

        let accept = SagaMsg::Accept {
            saga_id: "s".into(),
            attempt: 2,
        };
        let accept_bytes = br#"{"accept":{"saga_id":"s","attempt":2}}"#;
        assert_eq!(encode_saga_msg(&accept), accept_bytes);
        assert_eq!(decode_saga_msg(accept_bytes).unwrap(), accept);
    }

    #[test]
    fn malformed_boundaries_keep_consumer_refusals() {
        assert!(decode_work_spec(
            br#"{"kind":"foreign","dispatch_id":"d","capability":"c","payload":[],"demands":{}}"#
        )
        .is_err());
        assert!(decode_work_spec(
            br#"{"kind":"dispatch-work-v1","dispatch_id":"d","capability":"c","payload":[],"demands":{},"extra":true}"#
        )
        .is_err());
        assert!(decode_worker_request(br#"{"saga_id":"s","attempt":"bad"}"#).is_err());
        assert!(decode_worker_control(
            br#"{"kind":"not-a-control","command":{"cancel_attempt":{"saga_id":"s","attempt":2,"assignee":[]}}}"#
        )
        .is_err());
        assert!(
            decode_saga_msg(br#"{"renew_lease":{"saga_id":"s","attempt":2,"extra":true}}"#)
                .is_err()
        );
        assert!(decode_saga_msg(br#"{"trigger":{}}"#).is_err());
    }

    #[test]
    fn agent_response_fixture_preserves_lenient_response_decode() {
        let bytes = br#"{"reply_blocks":[],"actions":[],"commit_message":"fix: exact subject\n\nExact body.","future_field":true}"#;
        let response: AgentResponse = sdk::wire::decode(bytes).unwrap();
        assert_eq!(response.reply_blocks, Vec::new());
        assert_eq!(response.actions, Vec::new());
        assert_eq!(
            response.commit_message.as_deref(),
            Some("fix: exact subject\n\nExact body.")
        );
        assert!(
            sdk::wire::decode::<AgentResponse>(br#"{"reply_blocks":[],"actions":"invalid"}"#)
                .is_err()
        );
    }

    #[test]
    fn agent_response_fixture_covers_nonempty_reply_and_action() {
        let bytes = br#"{"reply_blocks":[{"kind":"paragraph","text":"done"}],"actions":[{"operation":"tasks.create","target":{"project":"ducktape"},"input":{"title":"Verify contract"}}],"commit_message":"fix: verify contract"}"#;
        let response: AgentResponse = sdk::wire::decode(bytes).unwrap();
        assert_eq!(
            response.reply_blocks,
            vec![ReplyBlock {
                kind: "paragraph".into(),
                text: "done".into(),
                lang: None,
            }]
        );
        assert_eq!(
            response.actions,
            vec![ActionEnvelope {
                operation: "tasks.create".into(),
                target: Some(serde_json::json!({"project": "ducktape"})),
                input: serde_json::json!({"title": "Verify contract"}),
            }]
        );
        assert_eq!(
            sdk::wire::encode(&response),
            bytes,
            "the local mirror must preserve the producer's field order and omissions"
        );
    }
}
