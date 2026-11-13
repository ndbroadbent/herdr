use super::*;
use crate::metadata_tokens::{MARK_INACTIVE_TOKEN, MARK_SET_VALUE, MARK_UNREAD_TOKEN};

/// Keeps a done agent blue, as an unseen completion would.
const UNREAD: &[(&str, &str)] = &[(MARK_UNREAD_TOKEN, MARK_SET_VALUE)];

const PINK: &str = "#f5c2e7";

fn config(recent_done_color: bool) -> String {
    let color = if recent_done_color {
        format!("recent_done_color = \"{PINK}\"\n")
    } else {
        String::new()
    };
    format!("[ui.sidebar.agents]\n{color}rows = [[\"state_icon\", \"tab\", \"changed\"]]\n")
}

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

fn agent(
    n: usize,
    status: AgentStatus,
    age_ms: Option<u64>,
    tokens: &[(&str, &str)],
) -> ClientShellAgent {
    ClientShellAgent {
        pane_id: format!("pane_{n}"),
        workspace_id: "ws_1".into(),
        tab_id: format!("tab_{n}"),
        name: None,
        display_agent: None,
        agent: Some("claude".into()),
        title: None,
        terminal_title: None,
        terminal_title_stripped: None,
        agent_status: status,
        state_change_seq: n as u64,
        state_changed_at_unix_ms: age_ms.map(|age| now_ms() - age),
        state_labels: Vec::new(),
        tokens: tokens
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect(),
        focused: false,
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

fn shell(config: &str, agents: Vec<ClientShellAgent>) -> (ClientShellState, FrameData) {
    let config: Config = toml::from_str(config).expect("sidebar config");
    let mut projected = snapshot();
    projected.tabs = (1..=agents.len())
        .map(|n| tab(&format!("tab_{n}"), n, &format!("tab{n}")))
        .collect();
    projected.agents = agents;
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&config));
    state.set_snapshot(Box::new(projected));
    state.set_pane_surface(surface());
    let frame = state.compose(106, 40).expect("sidebar frame").frame;
    (state, frame)
}

/// The text of an agent's entry and the colour of its dot.
fn entry(state: &ClientShellState, frame: &FrameData, n: usize) -> (String, u32) {
    let pane_id = format!("pane_{n}");
    let rect = state
        .hits
        .agents
        .iter()
        .find(|(_, pane)| *pane == pane_id)
        .expect("agent entry")
        .0;
    let text = frame_rows(frame)[rect.y as usize]
        .chars()
        .skip(rect.x as usize)
        .take(rect.width as usize)
        .collect::<String>();
    let dot = frame.cells[rect.y as usize * frame.width as usize + rect.x as usize + 1].fg;
    (
        text.split_whitespace()
            .filter(|word| *word != "·")
            .collect::<Vec<_>>()
            .join(" "),
        dot,
    )
}

#[test]
fn entries_show_how_long_ago_their_state_changed_and_a_fresh_done_dot_is_pink() {
    let minute = 60_000;
    let (state, frame) = shell(
        &config(true),
        vec![
            agent(1, AgentStatus::Done, Some(10_000), UNREAD),
            agent(2, AgentStatus::Done, Some(5 * minute + 1_000), UNREAD),
            agent(3, AgentStatus::Working, Some(10_000), &[]),
            agent(4, AgentStatus::Idle, Some(3 * 60 * minute), &[]),
        ],
    );
    let pink = crate::protocol::color_to_u32(crate::config::parse_color(PINK));

    let (fresh, fresh_dot) = entry(&state, &frame, 1);
    assert!(fresh.ends_with("tab1 0m"), "{fresh}");
    assert_eq!(fresh_dot, pink);

    let (older, older_dot) = entry(&state, &frame, 2);
    assert!(older.ends_with("tab2 5m"), "{older}");
    assert_ne!(older_dot, pink, "pink only for the first minute");

    let (working, working_dot) = entry(&state, &frame, 3);
    assert!(working.ends_with("tab3 0m"), "{working}");
    assert_ne!(working_dot, pink, "only a done dot turns pink");

    let (idle, _) = entry(&state, &frame, 4);
    assert!(idle.ends_with("tab4 3h"), "{idle}");

    // The fresh dot goes back to blue (and its label to 1m) in under 50 seconds.
    let repaint_in = state.hits.agent_age_repaint_at_unix_ms.expect("repaint") - now_ms();
    assert!(repaint_in <= 50_000, "{repaint_in}");
    assert!(!state.tick_agent_ages());
}

#[test]
fn grey_dots_show_no_time() {
    let (state, frame) = shell(
        &config(true),
        vec![
            agent(
                1,
                AgentStatus::Idle,
                Some(10_000),
                &[(MARK_INACTIVE_TOKEN, MARK_SET_VALUE)],
            ),
            agent(2, AgentStatus::Unknown, Some(10_000), &[]),
        ],
    );
    assert!(entry(&state, &frame, 1).0.ends_with("tab1"));
    assert!(entry(&state, &frame, 2).0.ends_with("tab2"));
    assert_eq!(state.hits.agent_age_repaint_at_unix_ms, None);
}

#[test]
fn without_a_recent_colour_or_a_time_nothing_changes() {
    let (state, frame) = shell(
        &config(false),
        vec![
            agent(1, AgentStatus::Done, Some(10_000), UNREAD),
            agent(2, AgentStatus::Done, None, UNREAD),
        ],
    );
    let (fresh, fresh_dot) = entry(&state, &frame, 1);
    assert!(fresh.ends_with("tab1 0m"), "{fresh}");
    assert_eq!(
        fresh_dot,
        entry(&state, &frame, 2).1,
        "the usual done colour"
    );
    assert!(entry(&state, &frame, 2).0.ends_with("tab2"), "older server");
}
