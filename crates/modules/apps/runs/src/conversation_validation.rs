//! Snapshot admission validates point-key identity and the retained cursor graph.
use super::*;

pub(crate) fn validate_records(records: &crate::receipts::Records) -> Result<(), String> {
    super::schedule::validate_queue(records)?;
    for (record_key, bytes) in records {
        if !record_key.starts_with("conversation/state/") {
            continue;
        }
        let state: ConversationView = sdk::wire::decode(bytes)?;
        let exact_key = *record_key == key("state", &state.conversation_id);
        let cursors_ordered = state.completed_cursor <= state.admitted_cursor;
        let bounded_graph = state.admitted_cursor <= records.len() as u64;
        if !exact_key || !cursors_ordered || !bounded_graph || state.next_turn == 0 {
            return Err("invalid conversation snapshot identity or cursor".into());
        }
        let native = run_envelope::NativeConversation {
            conversation_id: state.conversation_id.clone(),
            turn_id: crate::conversation_turn_id(0, 0),
            revision: state.history.as_ref().map(|h| h.revision).unwrap_or(0),
            history_prefix: state.history_prefix.clone(),
            history_snapshot: state.history.as_ref().map(|h| h.snapshot.clone()),
            session_path: state.session_path.clone(),
            packages: state.packages.clone(),
            events: Vec::new(),
        };
        native.validate()?;
        super::schedule::validate_for_conversation(records, &state)?;
        if let ConversationSource::Channel { channel_id } = &state.source {
            for binding in [
                key("channel", channel_id),
                key("account", &state.account.to_string()),
            ] {
                let target: String = decode_required(records, &binding)?;
                if target != state.conversation_id {
                    return Err("conversation binding points to another identity".into());
                }
            }
        }
        for cursor in 1..=state.admitted_cursor {
            let event: ConversationEvent =
                decode_required(records, &numbered("event", &state.conversation_id, cursor))?;
            if event.sequence != cursor {
                return Err("conversation event cursor does not match key".into());
            }
            if let ConversationInput::Chat { message } = &event.input {
                let valid_source = matches!(&state.source, ConversationSource::Channel { channel_id } if channel_id == &message.channel_id)
                    && matches!(message.head.content_origin, Origin::External(_))
                    && event.actor == message.head.content_origin
                    && message.seq <= state.source_cursor;
                if !valid_source {
                    return Err("invalid retained conversation source snapshot".into());
                }
            }
        }
        let Some(turn) = &state.active_turn else {
            continue;
        };
        let live_turn = turn.phase != ConversationTurnPhase::Settled
            && turn.turn < state.next_turn
            && turn.from_cursor == state.completed_cursor
            && turn.through_cursor > turn.from_cursor
            && turn.through_cursor <= state.admitted_cursor
            && turn.drained_actions <= turn.actions.len() as u64;
        if !live_turn {
            return Err("invalid active conversation turn".into());
        }
        let stored: ConversationTurn = decode_required(
            records,
            &numbered("turn", &state.conversation_id, turn.turn),
        )?;
        if stored != *turn {
            return Err("active conversation turn disagrees with retained turn".into());
        }
        let run_binding: String = decode_required(records, &key("run", &turn.run_id))?;
        if run_binding != state.conversation_id {
            return Err("conversation run binding mismatch".into());
        }
        let unique_actions: BTreeSet<&String> = turn.actions.iter().collect();
        if unique_actions.len() != turn.actions.len() {
            return Err("duplicate conversation action receipt".into());
        }
        if let Some(checkpoint) = &turn.checkpoint {
            let bound = checkpoint.run_id == turn.run_id
                && checkpoint.history.revision
                    > state.history.as_ref().map(|h| h.revision).unwrap_or(0);
            if !bound {
                return Err("conversation checkpoint is not bound to its turn".into());
            }
        }
    }
    Ok(())
}
fn decode_required<T: DeserializeOwned>(
    records: &crate::receipts::Records,
    key: &str,
) -> Result<T, String> {
    let bytes = records
        .get(key)
        .ok_or_else(|| format!("missing conversation snapshot record: {key}"))?;
    sdk::wire::decode(bytes)
}
