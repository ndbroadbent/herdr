//! Sends an agent the auto-continue message once it's back and settled: after a restart that
//! interrupted its turn, or after Codex's usage limit resets.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::auto_continue::{
    auto_continue_message, auto_continue_step, codex_usage_limit_retry_at, duration_until_retry,
    AutoContinueReason, AutoContinueStep, PendingAutoContinue,
};
use crate::detect::{Agent, AgentState};
use crate::terminal::TerminalId;

use super::App;

const CHECK_INTERVAL: Duration = Duration::from_secs(1);

pub(crate) struct AutoContinueState {
    pub config: crate::config::AutoContinueConfig,
    pending: HashMap<TerminalId, PendingAutoContinue>,
    /// Codex panes whose turn just ended, to look for a usage limit notice.
    limit_checks: Vec<TerminalId>,
    pub next_check: Option<Instant>,
}

impl AutoContinueState {
    /// `restored_mid_turn`: the restored session has agents to ask, so look at start-up.
    pub(crate) fn new(config: crate::config::AutoContinueConfig, restored_mid_turn: bool) -> Self {
        Self {
            config,
            pending: HashMap::new(),
            limit_checks: Vec::new(),
            next_check: restored_mid_turn.then(Instant::now),
        }
    }

    fn delay(&self) -> Duration {
        Duration::from_secs(self.config.delay_seconds)
    }
}

impl App {
    /// The server is stopping, handing off, or the machine is shutting down. Remembers which
    /// agents were in the middle of a turn before their processes can die, and makes sure the
    /// final save records it.
    pub(crate) fn begin_stopping(&mut self) {
        if self.stopping {
            return;
        }
        self.stopping = true;
        let mut interrupted = false;
        for terminal in self.state.terminals.values_mut() {
            terminal.state_at_stop = Some(terminal.state);
            interrupted |= terminal.turn_interrupted_by_stop();
        }
        if interrupted {
            self.state.session_dirty = true;
        }
    }

    pub(crate) fn note_agent_state_change_for_auto_continue(
        &mut self,
        update: &crate::app::actions::PaneStateUpdate,
    ) {
        if !self.auto_continue.config.enabled
            || update.known_agent != Some(Agent::Codex)
            || update.previous_state != AgentState::Working
            || update.state != AgentState::Idle
        {
            return;
        }
        let Some(terminal_id) = self
            .state
            .workspaces
            .get(update.ws_idx)
            .and_then(|workspace| workspace.terminal_id(update.pane_id))
        else {
            return;
        };
        self.auto_continue.limit_checks.push(terminal_id.clone());
        self.auto_continue.next_check = Some(Instant::now());
    }

    pub(crate) fn run_auto_continue(&mut self, now: Instant) {
        self.auto_continue.next_check = None;
        let enabled = self.auto_continue.config.enabled;

        // Agents restored mid-turn.
        for terminal in self.state.terminals.values_mut() {
            if let Some(reason) = terminal.auto_continue_reason.take() {
                if enabled {
                    self.auto_continue
                        .pending
                        .insert(terminal.id.clone(), PendingAutoContinue::new(reason, now));
                }
            }
        }

        for terminal_id in std::mem::take(&mut self.auto_continue.limit_checks) {
            if enabled {
                self.schedule_usage_limit_continue(terminal_id, now);
            }
        }

        if !enabled {
            self.auto_continue.pending.clear();
            return;
        }

        let delay = self.auto_continue.delay();
        let terminal_ids: Vec<TerminalId> = self.auto_continue.pending.keys().cloned().collect();
        for terminal_id in terminal_ids {
            let Some(target) = self.public_pane_id_for_terminal(&terminal_id) else {
                self.auto_continue.pending.remove(&terminal_id);
                continue;
            };
            // Restored agents relaunch only once a client attaches; the wait starts then.
            let relaunch_pending = self
                .state
                .terminals
                .get(&terminal_id)
                .is_some_and(|terminal| terminal.pending_agent_resume_plan.is_some());
            if relaunch_pending {
                if let Some(pending) = self.auto_continue.pending.get_mut(&terminal_id) {
                    pending.created_at = now;
                }
                continue;
            }
            let state = self
                .state
                .terminals
                .get(&terminal_id)
                .and_then(|terminal| terminal.effective_known_agent().map(|_| terminal.state));
            let Some(pending) = self.auto_continue.pending.get_mut(&terminal_id) else {
                continue;
            };
            match auto_continue_step(pending, state, delay, now) {
                AutoContinueStep::Wait => {}
                AutoContinueStep::Cancel(why) => {
                    tracing::info!(pane = %target, reason = why, "auto-continue cancelled");
                    self.auto_continue.pending.remove(&terminal_id);
                }
                AutoContinueStep::Send => {
                    let reason = pending.reason;
                    let message = auto_continue_message(&self.auto_continue.config.message, reason);
                    match self.send_agent_prompt(target.clone(), message) {
                        Ok(()) => {
                            self.auto_continue.pending.remove(&terminal_id);
                            tracing::info!(
                                pane = %target,
                                reason = reason.text(),
                                "auto-continue sent"
                            );
                        }
                        // Not ready after all (still starting, or a question appeared): wait
                        // for it to settle again, within the same time limit.
                        Err(err)
                            if err.contains("agent_not_ready") || err.contains("agent_blocked") =>
                        {
                            if let Some(pending) = self.auto_continue.pending.get_mut(&terminal_id)
                            {
                                pending.ready_since = None;
                            }
                            tracing::debug!(pane = %target, error = %err, "auto-continue waiting");
                        }
                        Err(err) => {
                            self.auto_continue.pending.remove(&terminal_id);
                            tracing::warn!(
                                pane = %target,
                                error = %err,
                                "auto-continue could not be sent"
                            );
                        }
                    }
                }
            }
        }

        if !self.auto_continue.pending.is_empty() {
            self.auto_continue.next_check = Some(now + CHECK_INTERVAL);
        }
    }

    fn schedule_usage_limit_continue(&mut self, terminal_id: TerminalId, now: Instant) {
        let Some(runtime) = self.terminal_runtimes.get(&terminal_id) else {
            return;
        };
        let Some(retry) = codex_usage_limit_retry_at(&runtime.detection_text()) else {
            return;
        };
        let Some(wait) = crate::platform::local_datetime()
            .and_then(|local_now| duration_until_retry(local_now, retry))
        else {
            return;
        };
        tracing::info!(
            terminal = %terminal_id,
            wait_secs = wait.as_secs(),
            "usage limit hit; auto-continue scheduled for the reset"
        );
        self.auto_continue.pending.insert(
            terminal_id,
            PendingAutoContinue::new(AutoContinueReason::UsageLimitReset, now).after(now + wait),
        );
    }

    fn public_pane_id_for_terminal(&self, terminal_id: &TerminalId) -> Option<String> {
        self.state
            .workspaces
            .iter()
            .enumerate()
            .find_map(|(ws_idx, workspace)| {
                workspace
                    .tabs
                    .iter()
                    .flat_map(|tab| {
                        tab.layout
                            .pane_ids()
                            .into_iter()
                            .filter(|pane_id| tab.terminal_id(*pane_id) == Some(terminal_id))
                            .collect::<Vec<_>>()
                    })
                    .next()
                    .map(|pane_id| (ws_idx, pane_id))
            })
            .and_then(|(ws_idx, pane_id)| self.public_pane_id(ws_idx, pane_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app_with_agents(enabled: bool) -> (App, TerminalId, TerminalId) {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut config = crate::config::Config::default();
        config.session.auto_continue.enabled = enabled;
        let mut app = App::new(
            &config,
            crate::app::AppPolicy::TEST,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        let working = crate::workspace::Workspace::test_new("working");
        let idle = crate::workspace::Workspace::test_new("idle");
        let terminal_of = |workspace: &crate::workspace::Workspace| {
            workspace
                .terminal_id(workspace.tabs[0].root_pane)
                .cloned()
                .expect("terminal")
        };
        let (working_id, idle_id) = (terminal_of(&working), terminal_of(&idle));
        app.state.workspaces = vec![working, idle];
        app.state.ensure_test_terminals();
        for (id, state) in [
            (&working_id, AgentState::Working),
            (&idle_id, AgentState::Idle),
        ] {
            app.state
                .terminals
                .get_mut(id)
                .expect("terminal")
                .set_detected_state(Some(Agent::Claude), state);
        }
        (app, working_id, idle_id)
    }

    fn interrupted_panes(app: &App) -> Vec<bool> {
        let snapshot = crate::persist::capture(
            &app.state.workspaces,
            &app.state.terminals,
            &app.terminal_runtimes,
            app.state.active,
            app.state.selected,
        );
        snapshot
            .workspaces
            .iter()
            .map(|workspace| {
                workspace.tabs[0]
                    .panes
                    .values()
                    .all(|pane| pane.interrupted_turn.is_some())
            })
            .collect()
    }

    #[test]
    fn stopping_records_which_agents_were_mid_turn() {
        let (mut app, working_id, _) = app_with_agents(false);
        assert_eq!(
            interrupted_panes(&app),
            [false, false],
            "nothing stopped yet"
        );

        app.state.session_dirty = false;
        app.begin_stopping();
        assert!(app.state.session_dirty, "the final save must record it");
        // The agent's process dies while the server shuts down.
        app.state
            .terminals
            .get_mut(&working_id)
            .expect("terminal")
            .set_detected_state(Some(Agent::Claude), AgentState::Unknown);

        assert_eq!(interrupted_panes(&app), [true, false]);
    }

    #[test]
    fn restored_agents_wait_for_the_delay_and_are_dropped_when_disabled() {
        let (mut app, working_id, _) = app_with_agents(true);
        let now = Instant::now();
        app.state
            .terminals
            .get_mut(&working_id)
            .expect("terminal")
            .auto_continue_reason = Some(AutoContinueReason::HerdrRestarted);

        app.run_auto_continue(now);
        assert!(app.auto_continue.pending.contains_key(&working_id));
        assert_eq!(app.auto_continue.next_check, Some(now + CHECK_INTERVAL));

        // Turning the setting off drops what's waiting.
        app.auto_continue.config.enabled = false;
        app.run_auto_continue(now + CHECK_INTERVAL);
        assert!(app.auto_continue.pending.is_empty());
        assert_eq!(app.auto_continue.next_check, None);

        // A reason restored while it's off is dropped, not kept for later.
        app.state
            .terminals
            .get_mut(&working_id)
            .expect("terminal")
            .auto_continue_reason = Some(AutoContinueReason::HerdrRestarted);
        app.run_auto_continue(now);
        app.auto_continue.config.enabled = true;
        app.run_auto_continue(now);
        assert!(app.auto_continue.pending.is_empty());
    }

    #[test]
    fn a_closed_pane_drops_its_message() {
        let (mut app, working_id, _) = app_with_agents(true);
        let now = Instant::now();
        app.state
            .terminals
            .get_mut(&working_id)
            .expect("terminal")
            .auto_continue_reason = Some(AutoContinueReason::HerdrRestarted);
        app.run_auto_continue(now);
        app.state.workspaces.remove(0);

        app.run_auto_continue(now + CHECK_INTERVAL);
        assert!(app.auto_continue.pending.is_empty());
    }
}
