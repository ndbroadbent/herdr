//! `herdr restart`: replace the running server with the `herdr` binary that runs this command.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::api::schema::{Method, Request, ServerLiveHandoffParams};

const USAGE: &str = "usage: herdr restart [--fresh]";

/// A fresh server restores every pane before its socket answers, which takes a while with
/// dozens of agents.
const FRESH_SERVER_READY_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RestartMode {
    /// Hand every pane to a new server, so their processes keep running.
    Handoff,
    /// Stop the server and its panes, then start a new server.
    Fresh,
}

pub(super) fn run_restart_command(args: &[String]) -> std::io::Result<i32> {
    let Some(mode) = parse_restart_args(args) else {
        eprintln!("{USAGE}");
        return Ok(2);
    };
    if super::target::is_remote() {
        eprintln!("herdr restart only restarts the server on this machine");
        return Ok(2);
    }
    let exe = std::env::current_exe()?;
    match mode {
        RestartMode::Handoff => restart_with_handoff(&exe),
        RestartMode::Fresh => restart_fresh(&exe),
    }
}

fn parse_restart_args(args: &[String]) -> Option<RestartMode> {
    match args {
        [] => Some(RestartMode::Handoff),
        [flag] if flag == "--fresh" => Some(RestartMode::Fresh),
        _ => None,
    }
}

fn handoff_params(exe: &Path) -> ServerLiveHandoffParams {
    ServerLiveHandoffParams {
        import_exe: Some(exe.display().to_string()),
        // The new server refuses the handoff unless it is this exact build.
        expected_protocol: Some(crate::protocol::PROTOCOL_VERSION),
        expected_version: Some(crate::build_info::version()),
    }
}

fn restart_with_handoff(exe: &Path) -> std::io::Result<i32> {
    // Like `herdr server live-handoff`, this must reach a server that speaks an older
    // protocol, so it skips the CLI's compatibility check.
    let response = super::send_request_unchecked(&Request {
        id: "cli:restart:live-handoff".into(),
        method: Method::ServerLiveHandoff(handoff_params(exe)),
    })?;
    if let Some(error) = response.get("error") {
        let message = error
            .get("message")
            .and_then(|message| message.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| error.to_string());
        eprintln!("live handoff failed: {message}");
        eprintln!("the old server is still running with your panes.");
        return Ok(1);
    }

    println!(
        "restarted the herdr server from {}; every pane kept running.",
        exe.display()
    );
    println!("run `herdr` to reattach.");
    Ok(0)
}

fn restart_fresh(exe: &Path) -> std::io::Result<i32> {
    let api_socket = crate::session::active_api_socket_path();
    if let Some(message) = fresh_restart_refusal(
        &api_socket,
        std::env::var(crate::HERDR_ENV_VAR).ok().as_deref(),
        std::env::var_os(crate::api::SOCKET_PATH_ENV_VAR).map(PathBuf::from),
    ) {
        eprintln!("{message}");
        return Ok(2);
    }

    if crate::server::autodetect::is_server_listening()? {
        eprintln!("stopping the herdr server and every pane...");
        if let Err(err) = crate::session::stop_active_server() {
            eprintln!("{err}");
            return Ok(1);
        }
    }

    let pid = crate::server::autodetect::spawn_server_daemon()?;
    crate::server::autodetect::wait_for_server_socket(
        &crate::server::socket_paths::client_socket_path(),
        FRESH_SERVER_READY_TIMEOUT,
    )?;
    println!(
        "started a new herdr server from {} (pid {pid}).",
        exe.display()
    );
    println!("run `herdr` to attach.");
    Ok(0)
}

/// Stopping the server that hosts this shell would kill the shell before it could start the
/// new server.
fn fresh_restart_refusal(
    target_api_socket: &Path,
    herdr_env: Option<&str>,
    host_api_socket: Option<PathBuf>,
) -> Option<&'static str> {
    let inside_herdr = herdr_env == Some(crate::HERDR_ENV_VALUE);
    let hosts_this_shell = host_api_socket.is_some_and(|host| host == target_api_socket);
    (inside_herdr && hosts_this_shell).then_some(
        "herdr restart --fresh stops every pane, including this one, so it can't start the \
         new server from here. Run `herdr restart` to keep the panes running, or run \
         `herdr restart --fresh` from a terminal outside herdr.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_string()).collect()
    }

    #[test]
    fn restart_hands_off_by_default_and_fresh_stops_everything() {
        assert_eq!(parse_restart_args(&[]), Some(RestartMode::Handoff));
        assert_eq!(
            parse_restart_args(&args(&["--fresh"])),
            Some(RestartMode::Fresh)
        );
        assert_eq!(parse_restart_args(&args(&["--handoff"])), None);
        assert_eq!(parse_restart_args(&args(&["--fresh", "now"])), None);
    }

    #[test]
    fn handoff_asks_for_this_exact_build() {
        let params = handoff_params(Path::new("/opt/herdr/bin/herdr"));

        assert_eq!(params.import_exe.as_deref(), Some("/opt/herdr/bin/herdr"));
        assert_eq!(
            params.expected_protocol,
            Some(crate::protocol::PROTOCOL_VERSION)
        );
        assert_eq!(params.expected_version, Some(crate::build_info::version()));
    }

    #[test]
    fn fresh_restart_refuses_only_from_a_pane_of_the_same_server() {
        let target = Path::new("/home/me/.config/herdr/herdr.sock");
        let same = Some(target.to_path_buf());
        let other = Some(PathBuf::from(
            "/home/me/.config/herdr/sessions/work/herdr.sock",
        ));

        assert!(fresh_restart_refusal(target, Some("1"), same.clone()).is_some());
        assert!(fresh_restart_refusal(target, Some("1"), other).is_none());
        assert!(fresh_restart_refusal(target, None, same.clone()).is_none());
        assert!(fresh_restart_refusal(target, Some("0"), same).is_none());
        assert!(fresh_restart_refusal(target, Some("1"), None).is_none());
    }
}
