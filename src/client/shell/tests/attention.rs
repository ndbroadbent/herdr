use super::*;
use crate::api::schema::Method;
use crate::input::{KeybindAction, KeybindMatch};
use crate::metadata_tokens::{MARK_INACTIVE_TOKEN, MARK_UNREAD_TOKEN};
use crate::raw_input::RawInputEvent;
use crossterm::event::{MouseButton, MouseEventKind};
use std::collections::HashMap;

fn agent(
    pane_id: &str,
    status: AgentStatus,
    seq: u64,
    tokens: &[(&str, &str)],
) -> ClientShellAgent {
    ClientShellAgent {
        pane_id: pane_id.into(),
        workspace_id: "ws_1".into(),
        tab_id: "tab_1".into(),
        name: Some("pi".into()),
        display_agent: None,
        agent: Some("pi".into()),
        title: None,
        terminal_title: None,
        terminal_title_stripped: None,
        agent_status: status,
        state_change_seq: seq,
        state_changed_at_unix_ms: None,
        state_labels: Vec::new(),
        tokens: tokens
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect(),
        focused: true,
    }
}

fn shell(config: &Config, tokens: &[(&str, &str)]) -> ClientShellState {
    let mut projected = snapshot();
    projected.agents = vec![agent("pane_1", AgentStatus::Idle, 1, tokens)];
    let mut state = ClientShellState::new(ClientShellConfig::from_config(config));
    state.set_snapshot(Box::new(projected));
    state.set_pane_surface(surface());
    state.compose(106, 30).expect("sidebar frame");
    state
}

fn press(state: &mut ClientShellState, button: MouseButton, x: u16, y: u16) -> ClientShellInput {
    state.handle_raw_events(vec![RawInputEvent::Mouse(MouseEvent {
        kind: MouseEventKind::Down(button),
        column: x,
        row: y,
        modifiers: KeyModifiers::empty(),
    })])
}

fn press_binding(state: &mut ClientShellState, action: KeybindAction) -> ClientShellInput {
    let mut outcome = ClientShellInput::default();
    state.record_binding(KeybindMatch::Action(action), &mut outcome);
    outcome
}

fn reported_tokens(outcome: &ClientShellInput) -> HashMap<String, Option<String>> {
    let [ClientShellAction::Endpoint { request, .. }] = &outcome.actions[..] else {
        panic!("expected one endpoint request, got {:?}", outcome.actions);
    };
    let Method::PaneReportMetadata(params) = &request.method else {
        panic!("expected pane.report_metadata, got {:?}", request.method);
    };
    assert_eq!(params.pane_id, "pane_1");
    assert_eq!(params.ttl_ms, None, "a mark has to outlive the server");
    params.tokens.clone()
}

fn set(token: &str) -> HashMap<String, Option<String>> {
    HashMap::from([(token.to_string(), Some("1".to_string()))])
}

#[test]
fn unread_key_sets_the_mark_and_a_second_press_clears_it() {
    let mut unmarked = shell(&Config::default(), &[]);
    assert_eq!(
        reported_tokens(&press_binding(
            &mut unmarked,
            KeybindAction::ToggleUnreadMark
        )),
        set(MARK_UNREAD_TOKEN)
    );

    let mut marked = shell(&Config::default(), &[(MARK_UNREAD_TOKEN, "1")]);
    assert_eq!(
        reported_tokens(&press_binding(&mut marked, KeybindAction::ToggleUnreadMark)),
        HashMap::from([(MARK_UNREAD_TOKEN.to_string(), None)])
    );
}

#[test]
fn inactive_key_sets_the_mark_and_drops_any_unread_mark() {
    let mut state = shell(&Config::default(), &[(MARK_UNREAD_TOKEN, "1")]);

    assert_eq!(
        reported_tokens(&press_binding(
            &mut state,
            KeybindAction::ToggleInactiveMark
        )),
        HashMap::from([
            (MARK_INACTIVE_TOKEN.to_string(), Some("1".to_string())),
            (MARK_UNREAD_TOKEN.to_string(), None),
        ])
    );
}

#[test]
fn clearing_the_inactive_mark_leaves_unread_alone() {
    let mut state = shell(
        &Config::default(),
        &[(MARK_INACTIVE_TOKEN, "1"), (MARK_UNREAD_TOKEN, "1")],
    );

    assert_eq!(
        reported_tokens(&press_binding(
            &mut state,
            KeybindAction::ToggleInactiveMark
        )),
        HashMap::from([(MARK_INACTIVE_TOKEN.to_string(), None)])
    );
}

#[test]
fn right_clicking_an_agent_row_offers_marks_that_match_its_state() {
    let mut state = shell(&Config::default(), &[(MARK_INACTIVE_TOKEN, "1")]);
    let row = state.hits.agents.first().expect("agent row").0;

    press(&mut state, MouseButton::Right, row.x + 1, row.y);
    state.compose(106, 30).expect("menu frame");

    let Some(ClientShellOverlay::ContextMenu(menu)) = state.overlay.as_ref() else {
        panic!("agent menu");
    };
    let items = menu.items();
    let labels = items.iter().map(|item| item.label).collect::<Vec<_>>();
    assert!(labels.contains(&"Mark as unread"), "{labels:?}");
    assert!(labels.contains(&"Mark as active"), "{labels:?}");
    let index = items
        .iter()
        .position(|item| item.action == ClientContextMenuAction::ToggleUnreadMark)
        .expect("unread item");

    let target = state.hits.context_menu_rows[index].0;
    let outcome = press(&mut state, MouseButton::Left, target.x, target.y);

    assert_eq!(reported_tokens(&outcome), set(MARK_UNREAD_TOKEN));
}

#[test]
fn clicking_a_linked_token_opens_it_and_the_rest_of_the_row_still_focuses() {
    let config: Config = toml::from_str(
        r#"
[ui.sidebar.agents]
rows = [["state_icon", { token = "$pr", link = "$pr_url" }, "agent"]]
"#,
    )
    .expect("sidebar config");
    let mut state = shell(
        &config,
        &[("pr", "#12"), ("pr_url", "https://example.test/pull/12")],
    );
    let frame = state.compose(106, 30).expect("sidebar frame");
    let row = state.hits.agents.first().expect("agent row").0;
    let (link, url) = state.hits.agent_links.first().expect("link hit").clone();
    assert_eq!(url, "https://example.test/pull/12");
    assert_eq!(cell_symbol_position(&frame, row, "#12"), (link.x, link.y));
    assert_eq!(link.width, 3);

    let outcome = press(&mut state, MouseButton::Left, link.x + 2, link.y);
    assert!(matches!(
        &outcome.actions[..],
        [ClientShellAction::OpenSafeWebUrl(opened)] if *opened == url
    ));

    let outcome = press(&mut state, MouseButton::Left, row.right() - 1, row.y);
    let [ClientShellAction::Endpoint { request, .. }] = &outcome.actions[..] else {
        panic!("expected the row click to focus, got {:?}", outcome.actions);
    };
    assert!(matches!(
        &request.method,
        Method::PaneFocus(target) if target.pane_id == "pane_1"
    ));
}

#[test]
fn a_token_with_no_url_is_not_a_link() {
    let config: Config = toml::from_str(
        r#"
[ui.sidebar.agents]
rows = [["state_icon", { token = "$pr", link = "$pr_url" }, "agent"]]
"#,
    )
    .expect("sidebar config");
    let state = shell(&config, &[("pr", "#12")]);

    assert!(state.hits.agent_links.is_empty());
}

#[test]
fn only_an_idle_session_shows_the_inactive_grey() {
    let palette = crate::app::state::Palette::catppuccin();
    assert_eq!(
        super::super::agent_status_color(AgentStatus::Idle, true, &palette),
        palette.overlay0
    );
    assert_ne!(
        super::super::agent_status_color(AgentStatus::Idle, false, &palette),
        palette.overlay0
    );
    for status in [
        AgentStatus::Working,
        AgentStatus::Blocked,
        AgentStatus::Done,
    ] {
        assert_eq!(
            super::super::agent_status_color(status, true, &palette),
            super::super::agent_status_color(status, false, &palette),
            "{status:?}"
        );
    }
}

#[test]
fn priority_sort_puts_inactive_sessions_below_idle_ones() {
    let mut projected = snapshot();
    projected.agents = vec![
        agent(
            "newest_inactive",
            AgentStatus::Idle,
            9,
            &[(MARK_INACTIVE_TOKEN, "1")],
        ),
        agent("older_idle", AgentStatus::Idle, 1, &[]),
        agent(
            "inactive_but_finished",
            AgentStatus::Done,
            2,
            &[(MARK_INACTIVE_TOKEN, "1")],
        ),
    ];

    assert_eq!(
        super::super::agent_sidebar::ordered_agent_pane_ids(
            &projected,
            crate::config::AgentPanelSortConfig::Priority,
        ),
        vec!["inactive_but_finished", "older_idle", "newest_inactive"]
    );
}
