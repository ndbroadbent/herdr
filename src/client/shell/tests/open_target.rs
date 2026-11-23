use super::*;
use crate::api::schema::Method;

fn state_with_tabs() -> ClientShellState {
    let mut projected = snapshot();
    let mut triage = projected.tabs[0].clone();
    triage.tab_id = "tab_2".into();
    triage.number = 2;
    triage.label = "email triage".into();
    triage.focused = false;
    projected.tabs.push(triage);
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(projected));
    state.set_pane_surface(surface());
    state.compose(106, 24).unwrap();
    state
}

fn focused_tabs(outcome: &ClientShellInput) -> Vec<String> {
    outcome
        .actions
        .iter()
        .filter_map(|action| match action {
            ClientShellAction::Endpoint { request, .. } => match &request.method {
                Method::TabFocus(target) => Some(target.tab_id.clone()),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

#[test]
fn a_link_focuses_a_tab_on_this_computer_by_label_or_id() {
    let mut state = state_with_tabs();
    assert_eq!(
        focused_tabs(&state.open_target(None, "email triage")),
        ["tab_2"]
    );
    assert_eq!(
        focused_tabs(&state.open_target(Some("Local"), "tab_2")),
        ["tab_2"]
    );
}

#[test]
fn a_link_to_an_unknown_tab_or_machine_shows_a_notice_and_does_nothing() {
    let mut state = state_with_tabs();
    for (machine, tab) in [(None, "missing"), (Some("agents"), "email triage")] {
        let outcome = state.open_target(machine, tab);
        assert!(outcome.repaint, "{machine:?} {tab}");
        assert!(outcome.actions.is_empty(), "{machine:?} {tab}");
        assert!(state.visible_endpoint_notice.is_some(), "{machine:?} {tab}");
    }
}
