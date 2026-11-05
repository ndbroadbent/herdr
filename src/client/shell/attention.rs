//! Attention marks a person sets on a session: unread (blue until they come back to it) and
//! inactive (grey until the session next does something). The server stores them as pane tokens.

use std::collections::HashMap;

use crate::api::schema::{Method, PaneReportMetadataParams};
use crate::metadata_tokens::{MARK_INACTIVE_TOKEN, MARK_SET_VALUE, MARK_UNREAD_TOKEN};
use crate::protocol::ClientShellAgent;

const MARK_SOURCE: &str = "herdr:marks";

fn has_token(agent: &ClientShellAgent, token: &str) -> bool {
    agent.tokens.iter().any(|(key, _)| key == token)
}

pub(super) fn is_marked_unread(agent: &ClientShellAgent) -> bool {
    has_token(agent, MARK_UNREAD_TOKEN)
}

pub(super) fn is_marked_inactive(agent: &ClientShellAgent) -> bool {
    has_token(agent, MARK_INACTIVE_TOKEN)
}

pub(super) fn toggle_unread(agent: &ClientShellAgent) -> Method {
    let value = (!is_marked_unread(agent)).then(|| MARK_SET_VALUE.to_string());
    report_tokens(
        agent,
        HashMap::from([(MARK_UNREAD_TOKEN.to_string(), value)]),
    )
}

pub(super) fn toggle_inactive(agent: &ClientShellAgent) -> Method {
    let marking = !is_marked_inactive(agent);
    let mut tokens = HashMap::from([(
        MARK_INACTIVE_TOKEN.to_string(),
        marking.then(|| MARK_SET_VALUE.to_string()),
    )]);
    // Wrapping a session up means there's nothing left to come back and read.
    if marking {
        tokens.insert(MARK_UNREAD_TOKEN.to_string(), None);
    }
    report_tokens(agent, tokens)
}

fn report_tokens(agent: &ClientShellAgent, tokens: HashMap<String, Option<String>>) -> Method {
    Method::PaneReportMetadata(PaneReportMetadataParams {
        pane_id: agent.pane_id.clone(),
        source: MARK_SOURCE.to_string(),
        agent: None,
        applies_to_source: None,
        title: None,
        display_agent: None,
        state_labels: HashMap::new(),
        tokens,
        clear_title: false,
        clear_display_agent: false,
        clear_state_labels: false,
        seq: None,
        ttl_ms: None,
    })
}
