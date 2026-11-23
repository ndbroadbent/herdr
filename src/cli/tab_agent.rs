//! `herdr tab create --agent <kind>`: create a tab, start an agent in it, and answer once the
//! agent is ready for input.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::agent::{launch_agent, AgentLaunch};
use crate::api::schema::{
    AgentReadParams, AgentTarget, EmptyParams, Method, PaneReadParams, ReadFormat, ReadSource,
    Request, TabCreateParams, TabTarget,
};

const REQUEST_ID: &str = "cli:tab:create";
const DEFAULT_TIMEOUT_MS: u64 = 30_000;
const POLL_INTERVAL: Duration = Duration::from_millis(150);
const OUTPUT_TAIL_LINES: usize = 20;
const MAX_AGENT_NAME_LEN: usize = 32;

/// A screen an agent can stop on while starting, which a person has to answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum StartupScreen {
    /// "Quick safety check: is this a project you created or one you trust?"
    Trust,
    /// "WARNING: Claude Code running in Bypass Permissions mode"
    BypassPermissions,
    /// Resuming a large or old session: "Resume from summary" or "Resume full session as-is".
    ResumeFromSummary,
    /// Any other screen that blocks start-up.
    Unknown,
}

impl StartupScreen {
    pub(super) fn id(self) -> &'static str {
        match self {
            Self::Trust => "trust",
            Self::BypassPermissions => "bypass_permissions",
            Self::ResumeFromSummary => "resume_from_summary",
            Self::Unknown => "unknown",
        }
    }

    fn description(self) -> &'static str {
        match self {
            Self::Trust => "the folder trust prompt",
            Self::BypassPermissions => "the bypass permissions warning",
            Self::ResumeFromSummary => "the resume prompt (resume from a summary or in full)",
            Self::Unknown => "a prompt",
        }
    }
}

/// Names the start-up screen in an agent's detection text. Each heading is the screen's own
/// title, which appears nowhere else in Claude's start-up.
pub(super) fn startup_screen(detection_text: &str) -> StartupScreen {
    let text = detection_text.to_lowercase();
    if text.contains("is this a project you created or one you trust") {
        StartupScreen::Trust
    } else if text.contains("running in bypass permissions mode") {
        StartupScreen::BypassPermissions
    } else if text.contains("resume from summary") || text.contains("resume full session") {
        StartupScreen::ResumeFromSummary
    } else {
        StartupScreen::Unknown
    }
}

/// An agent name for a tab, since agent names can't hold spaces or capitals: the label in
/// lowercase with every other run of characters as one `-`, made unique with `-2`, `-3`…
pub(super) fn agent_name_for_label(
    label: Option<&str>,
    kind: &str,
    taken: &HashSet<String>,
) -> String {
    let mut base = String::new();
    for ch in label.unwrap_or(kind).chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_lowercase() || ch.is_ascii_digit() {
            base.push(ch);
        } else if !base.is_empty() && !base.ends_with('-') {
            base.push('-');
        }
    }
    let mut base = base.trim_end_matches('-').to_string();
    if !base.starts_with(|ch: char| ch.is_ascii_lowercase()) {
        base = format!("{kind}-{base}").trim_end_matches('-').to_string();
    }
    base.truncate(MAX_AGENT_NAME_LEN);
    let base = base.trim_end_matches('-').to_string();
    if !taken.contains(&base) {
        return base;
    }
    (2..)
        .map(|n| {
            let suffix = format!("-{n}");
            let mut stem = base.clone();
            stem.truncate(MAX_AGENT_NAME_LEN - suffix.len());
            format!("{}{suffix}", stem.trim_end_matches('-'))
        })
        .find(|name| !taken.contains(name))
        .unwrap_or(base)
}

/// Whether the screen itself shows the agent idle. Agents set their window title before they
/// draw, so a title alone doesn't mean the agent takes input yet.
fn screen_shows_idle(explain: &Value) -> Option<bool> {
    let rule = &explain["matched_rule"];
    let region = rule["region"].as_str()?;
    Some(rule["state"].as_str() == Some("idle") && !region.starts_with("osc_"))
}

pub(super) struct TabAgent {
    pub(super) kind: String,
    pub(super) args: Vec<String>,
    pub(super) timeout_ms: Option<u64>,
}

pub(super) fn create_tab_with_agent(
    params: TabCreateParams,
    agent: TabAgent,
) -> std::io::Result<i32> {
    let label = params.label.clone();
    let created = super::send_request(&Request {
        id: REQUEST_ID.into(),
        method: Method::TabCreate(params),
    })?;
    if created.get("error").is_some() {
        return super::print_response(&created);
    }
    let (Some(tab_id), Some(pane_id)) = (
        created["result"]["tab"]["tab_id"]
            .as_str()
            .map(str::to_string),
        created["result"]["root_pane"]["pane_id"]
            .as_str()
            .map(str::to_string),
    ) else {
        return super::print_response(&created);
    };

    let started = Instant::now();
    let timeout = Duration::from_millis(agent.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS));
    let launched = start_named_agent(label.as_deref(), &pane_id, &agent)?;

    if let Some(error) = launched.get("error") {
        let code = error["code"].as_str().unwrap_or_default();
        if code == "agent_not_ready" {
            return report_startup_screen(&tab_id, &pane_id, label.as_deref());
        }
        // A timed-out agent may still be starting, so its tab stays for the person to see.
        let close_tab = code != "timeout";
        return report_failure(&tab_id, &pane_id, error, close_tab);
    }

    let deadline = started + timeout;
    loop {
        let explain = super::send_request(&Request {
            id: REQUEST_ID.into(),
            method: Method::AgentExplain(AgentTarget {
                target: pane_id.clone(),
            }),
        })?;
        let explain = &explain["result"]["explain"];
        if explain["state"].as_str() == Some("blocked") {
            return report_startup_screen(&tab_id, &pane_id, label.as_deref());
        }
        // Agents without a screen rule for idle (Codex reports `unknown`) are taken as ready.
        if screen_shows_idle(explain).unwrap_or(true)
            || explain["state"].as_str() == Some("unknown")
        {
            break;
        }
        if Instant::now() >= deadline {
            let error = json!({
                "code": "timeout",
                "message": "the agent started, but its screen never showed it ready for input",
            });
            return report_failure(&tab_id, &pane_id, &error, false);
        }
        std::thread::sleep(POLL_INTERVAL);
    }

    let agent_info = super::send_request(&Request {
        id: REQUEST_ID.into(),
        method: Method::AgentGet(AgentTarget {
            target: pane_id.clone(),
        }),
    })?;
    let mut response = created;
    response["result"]["agent"] = if agent_info.get("error").is_none() {
        agent_info["result"]["agent"].clone()
    } else {
        launched["result"]["agent"].clone()
    };
    super::print_response(&response)
}

/// Starts the agent under a name generated from the tab label, trying the next name if another
/// agent takes it first.
fn start_named_agent(
    label: Option<&str>,
    pane_id: &str,
    agent: &TabAgent,
) -> std::io::Result<Value> {
    let mut taken = agent_names()?;
    loop {
        let name = agent_name_for_label(label, &agent.kind, &taken);
        let response = launch_agent(&AgentLaunch {
            name: name.clone(),
            kind: agent.kind.clone(),
            pane_id: pane_id.to_string(),
            args: agent.args.clone(),
            timeout_ms: agent.timeout_ms,
        })?;
        if response["error"]["code"].as_str() != Some("agent_name_taken") || !taken.insert(name) {
            return Ok(response);
        }
    }
}

fn agent_names() -> std::io::Result<HashSet<String>> {
    let response = super::send_request(&Request {
        id: REQUEST_ID.into(),
        method: Method::AgentList(EmptyParams::default()),
    })?;
    Ok(response["result"]["agents"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|agent| agent["name"].as_str().map(str::to_string))
        .collect())
}

fn detection_text(pane_id: &str) -> std::io::Result<String> {
    let response = super::send_request(&Request {
        id: REQUEST_ID.into(),
        method: Method::AgentRead(AgentReadParams {
            target: pane_id.to_string(),
            source: ReadSource::Detection,
            lines: None,
            format: ReadFormat::Text,
            strip_ansi: true,
        }),
    })?;
    Ok(response["result"]["read"]["text"]
        .as_str()
        .unwrap_or_default()
        .to_string())
}

fn output_tail(text: &str) -> Vec<String> {
    let lines: Vec<String> = text
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.trim().is_empty())
        .map(str::to_string)
        .collect();
    lines[lines.len().saturating_sub(OUTPUT_TAIL_LINES)..].to_vec()
}

fn report_startup_screen(tab_id: &str, pane_id: &str, label: Option<&str>) -> std::io::Result<i32> {
    let screen = startup_screen(&detection_text(pane_id)?);
    let tab = label.unwrap_or(tab_id);
    super::print_response(&json!({
        "id": REQUEST_ID,
        "error": {
            "code": "agent_startup_screen",
            "message": format!(
                "{tab} needs you in herdr: the agent stopped at {} while starting. The tab is left open.",
                screen.description()
            ),
            "screen": screen.id(),
            "tab_id": tab_id,
            "pane_id": pane_id,
            "tab_closed": false,
        }
    }))
}

fn recent_text(pane_id: &str) -> std::io::Result<String> {
    let response = super::send_request(&Request {
        id: REQUEST_ID.into(),
        method: Method::PaneRead(PaneReadParams {
            pane_id: pane_id.to_string(),
            source: ReadSource::Recent,
            lines: Some(200),
            format: ReadFormat::Text,
            strip_ansi: true,
            intent: Default::default(),
        }),
    })?;
    Ok(response["result"]["read"]["text"]
        .as_str()
        .unwrap_or_default()
        .to_string())
}

fn report_failure(
    tab_id: &str,
    pane_id: &str,
    error: &Value,
    close_tab: bool,
) -> std::io::Result<i32> {
    let output = output_tail(&recent_text(pane_id).unwrap_or_default());
    let tab_closed = close_tab
        && super::send_request(&Request {
            id: REQUEST_ID.into(),
            method: Method::TabClose(TabTarget {
                tab_id: tab_id.to_string(),
            }),
        })
        .is_ok_and(|response| response.get("error").is_none());
    super::print_response(&json!({
        "id": REQUEST_ID,
        "error": {
            "code": error["code"].clone(),
            "message": error["message"].clone(),
            "tab_id": tab_id,
            "pane_id": pane_id,
            "tab_closed": tab_closed,
            "output": output,
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(items: &[&str]) -> HashSet<String> {
        items.iter().map(|item| (*item).to_string()).collect()
    }

    #[test]
    fn a_label_with_spaces_becomes_a_valid_agent_name() {
        let none = HashSet::new();
        assert_eq!(
            agent_name_for_label(Some("esign cert"), "claude", &none),
            "esign-cert"
        );
        assert_eq!(
            agent_name_for_label(Some("Email Triage!"), "claude", &none),
            "email-triage"
        );
        assert_eq!(
            agent_name_for_label(Some("  ci / deploy  "), "claude", &none),
            "ci-deploy"
        );
        assert_eq!(
            agent_name_for_label(Some("2026 taxes"), "claude", &none),
            "claude-2026-taxes"
        );
        assert_eq!(
            agent_name_for_label(Some("日本"), "claude", &none),
            "claude"
        );
        assert_eq!(agent_name_for_label(None, "claude", &none), "claude");
        let long = agent_name_for_label(Some(&"a very long tab label ".repeat(4)), "claude", &none);
        assert!(
            long.len() <= MAX_AGENT_NAME_LEN && !long.ends_with('-'),
            "{long}"
        );
    }

    #[test]
    fn a_taken_agent_name_gets_the_next_free_number() {
        let taken = names(&["esign-cert", "esign-cert-2"]);
        assert_eq!(
            agent_name_for_label(Some("esign cert"), "claude", &taken),
            "esign-cert-3"
        );

        let long_label = "x".repeat(40);
        let full = "x".repeat(MAX_AGENT_NAME_LEN);
        let next = agent_name_for_label(Some(&long_label), "claude", &names(&[&full]));
        assert_eq!(next.len(), MAX_AGENT_NAME_LEN);
        assert!(next.ends_with("-2"));
    }

    #[test]
    fn startup_screens_are_named_by_their_headings() {
        assert_eq!(
            startup_screen("Quick safety check: Is this a project you created or one you trust?\n❯ 1. Yes, I trust this folder"),
            StartupScreen::Trust
        );
        assert_eq!(
            startup_screen("WARNING: Claude Code running in Bypass Permissions mode\n❯ 1. No, exit\n  2. Yes, I accept"),
            StartupScreen::BypassPermissions
        );
        assert_eq!(
            startup_screen(
                "❯ 1. Resume from summary (instant, recommended)\n  2. Resume full session as-is"
            ),
            StartupScreen::ResumeFromSummary
        );
        assert_eq!(
            startup_screen("Do you want to proceed?\n❯ 1. Yes"),
            StartupScreen::Unknown
        );
    }

    #[test]
    fn only_a_screen_rule_counts_as_ready() {
        let rule =
            |state: &str, region: &str| json!({"matched_rule": {"state": state, "region": region}});
        assert_eq!(
            screen_shows_idle(&rule("idle", "prompt_box_body")),
            Some(true)
        );
        assert_eq!(screen_shows_idle(&rule("idle", "osc_title")), Some(false));
        assert_eq!(
            screen_shows_idle(&rule("working", "bottom_non_empty_lines(12)")),
            Some(false)
        );
        assert_eq!(screen_shows_idle(&json!({"matched_rule": null})), None);
    }

    #[test]
    fn the_output_tail_keeps_the_last_non_empty_lines() {
        let text = (1..=30)
            .map(|n| format!("line {n}\n\n"))
            .collect::<String>();
        let tail = output_tail(&text);
        assert_eq!(tail.len(), OUTPUT_TAIL_LINES);
        assert_eq!(tail.last().map(String::as_str), Some("line 30"));
    }
}
