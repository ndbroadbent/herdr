use super::*;
use crate::api::schema::Method;
use crate::raw_input::RawInputEvent;
use crossterm::event::{MouseButton, MouseEventKind};

const SIDEBAR: &str = r##"
[ui.sidebar.agents]
show_tabs_without_agents = true
rows = [
  ["state_icon", { token = "tab", fg = "#cdd6f4" }, "workspace"],
  ["agent", "$pr"],
]
"##;

fn tab(tab_id: &str, number: usize, label: &str) -> ClientShellTab {
    ClientShellTab {
        tab_id: tab_id.into(),
        workspace_id: "ws_1".into(),
        number,
        label: label.into(),
        custom_label: true,
        zoomed: false,
        focused: false,
        agent_status: AgentStatus::Unknown,
    }
}

fn pane(pane_id: &str, tab_id: &str) -> ClientShellPane {
    ClientShellPane {
        pane_id: pane_id.into(),
        workspace_id: "ws_1".into(),
        tab_id: tab_id.into(),
        label: None,
        cwd: None,
        foreground_cwd: None,
        focused: false,
        right_click_passthrough: false,
    }
}

/// Tabs "blog", "api" and "notes", in that order. Only "api" runs an agent.
fn shell(config: &str) -> (ClientShellState, FrameData) {
    let config: Config = toml::from_str(config).expect("sidebar config");
    let mut projected = snapshot();
    projected.tabs = vec![
        tab("tab_1", 1, "blog"),
        ClientShellTab {
            focused: true,
            ..tab("tab_2", 2, "api")
        },
        tab("tab_3", 3, "notes"),
    ];
    projected.panes = vec![
        pane("pane_1", "tab_1"),
        ClientShellPane {
            focused: true,
            ..pane("pane_2", "tab_2")
        },
        pane("pane_3", "tab_3"),
    ];
    projected.focused_tab_id = Some("tab_2".into());
    projected.focused_pane_id = Some("pane_2".into());
    projected.agents = vec![ClientShellAgent {
        pane_id: "pane_2".into(),
        workspace_id: "ws_1".into(),
        tab_id: "tab_2".into(),
        name: None,
        display_agent: None,
        agent: Some("claude".into()),
        title: None,
        terminal_title: None,
        terminal_title_stripped: None,
        agent_status: AgentStatus::Idle,
        state_change_seq: 1,
        state_changed_at_unix_ms: None,
        state_labels: Vec::new(),
        tokens: vec![("pr".into(), "#12".into())],
        focused: true,
    }];
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&config));
    state.set_snapshot(Box::new(projected));
    state.set_pane_surface(surface());
    let frame = state.compose(106, 40).expect("sidebar frame").frame;
    (state, frame)
}

fn press(state: &mut ClientShellState, button: MouseButton, x: u16, y: u16) -> ClientShellInput {
    state.handle_raw_events(vec![RawInputEvent::Mouse(MouseEvent {
        kind: MouseEventKind::Down(button),
        column: x,
        row: y,
        modifiers: KeyModifiers::empty(),
    })])
}

fn line(frame: &FrameData, area: Rect, y: u16) -> String {
    frame_rows(frame)[y as usize]
        .chars()
        .skip(area.x as usize)
        .take(area.width as usize)
        .collect::<String>()
}

fn tab_entries(state: &ClientShellState) -> Vec<&str> {
    state
        .hits
        .agent_tabs
        .iter()
        .map(|(_, tab_id)| tab_id.as_str())
        .collect()
}

#[test]
fn grouped_tabs_without_agents_sit_in_tab_bar_order_with_a_name_and_a_blank_line() {
    let (state, frame) = shell(SIDEBAR);
    let agent_row = state.hits.agents.first().expect("agent entry").0;
    assert_eq!(tab_entries(&state), ["tab_1", "tab_3"]);

    let (blog, notes) = (state.hits.agent_tabs[0].0, state.hits.agent_tabs[1].0);
    assert_eq!(blog.height, 2, "as tall as an agent entry");
    assert_eq!(agent_row.y, blog.bottom(), "blog, then api");
    assert_eq!(notes.y, agent_row.bottom(), "api, then notes");

    let (agent_name_x, _) = cell_symbol_position(&frame, agent_row, "api");
    let (blog_name_x, blog_y) = cell_symbol_position(&frame, blog, "blog");
    assert_eq!(blog_name_x, agent_name_x, "names line up");
    assert_eq!(
        line(&frame, blog, blog_y).trim(),
        "blog",
        "no dot, no workspace"
    );
    assert_eq!(line(&frame, blog, blog_y + 1).trim(), "");
    assert_eq!(
        frame.cells[blog_y as usize * frame.width as usize + blog_name_x as usize].fg,
        frame.cells[agent_row.y as usize * frame.width as usize + agent_name_x as usize].fg,
        "styled like an agent's tab name"
    );
}

#[test]
fn by_priority_tabs_without_agents_follow_the_agents() {
    let (state, _) = shell(&SIDEBAR.replace(
        "[ui.sidebar.agents]",
        "[ui]\nagent_panel_sort = \"priority\"\n\n[ui.sidebar.agents]",
    ));
    let agent_row = state.hits.agents.first().expect("agent entry").0;
    assert_eq!(tab_entries(&state), ["tab_1", "tab_3"]);
    assert_eq!(state.hits.agent_tabs[0].0.y, agent_row.bottom());
}

#[test]
fn clicking_a_tab_entry_switches_to_it_and_right_clicking_opens_its_menu() {
    let (mut state, _) = shell(SIDEBAR);
    let blog = state.hits.agent_tabs[0].0;

    let outcome = press(&mut state, MouseButton::Left, blog.x + 3, blog.y + 1);
    let [ClientShellAction::Endpoint { request, .. }] = &outcome.actions[..] else {
        panic!("expected one request, got {:?}", outcome.actions);
    };
    assert!(matches!(
        &request.method,
        Method::TabFocus(target) if target.tab_id == "tab_1"
    ));

    press(&mut state, MouseButton::Right, blog.x + 3, blog.y);
    assert!(matches!(
        state.overlay.as_ref(),
        Some(ClientShellOverlay::ContextMenu(ClientContextMenuOverlay {
            target: ClientContextMenuTarget::Tab { tab_id, .. },
            ..
        })) if tab_id == "tab_1"
    ));
}

#[test]
fn tabs_without_agents_are_left_out_unless_asked_for() {
    let (state, frame) = shell(&SIDEBAR.replace("show_tabs_without_agents = true", ""));
    assert!(state.hits.agent_tabs.is_empty());
    let body = state.hits.agent_body;
    assert!((body.y..body.bottom()).all(|y| !line(&frame, body, y).contains("blog")));
}
