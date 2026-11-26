//! Starting the herdr server through the user's systemd unit when one exists.
//!
//! A server VM runs `herdr server` as the `herdr.service` user unit, so systemd restarts it and
//! stops it cleanly. When that server is gone (an OOM kill, a crash, a stop), anything that
//! auto-starts a server (the TUI, or the bridge an SSH client attaches through) would otherwise
//! spawn its own `herdr server` inside the caller's session. That server holds the socket, the
//! unit then fails with "already running" on every restart, and every tab lives in a process with
//! no supervisor that dies with the SSH session. Asking systemd to start the unit instead keeps
//! the server where it belongs.

use std::process::{Command, Stdio};

/// The user unit name herdr's server guide uses for `herdr server`.
const SERVER_UNIT: &str = "herdr.service";

/// Starts the server through its systemd user unit. Returns `Ok(false)` when there is no such
/// unit (or no reachable user manager), so the caller spawns the server itself as before.
pub(crate) fn start_supervised_server() -> std::io::Result<bool> {
    let Some(load_state) =
        systemctl_output(&["show", "--property=LoadState", "--value", SERVER_UNIT])
    else {
        return Ok(false);
    };
    if !unit_is_loaded(&load_state) {
        return Ok(false);
    }
    // A unit that hit its start limit (it kept failing while another server held the socket)
    // refuses to start until its failure count is cleared.
    let _ = systemctl_output(&["reset-failed", SERVER_UNIT]);
    let status = Command::new("systemctl")
        .args(["--user", "start", "--no-block", SERVER_UNIT])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|err| {
            std::io::Error::new(err.kind(), format!("failed to run systemctl: {err}"))
        })?;
    if !status.success() {
        return Err(std::io::Error::other(format!(
            "systemctl --user start {SERVER_UNIT} failed ({status}); see `journalctl --user -u {SERVER_UNIT}`"
        )));
    }
    tracing::info!(
        unit = SERVER_UNIT,
        "asked systemd to start the herdr server"
    );
    Ok(true)
}

fn systemctl_output(args: &[&str]) -> Option<String> {
    let output = Command::new("systemctl")
        .arg("--user")
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

/// `systemctl show --property=LoadState --value` prints `loaded` for an installed unit and
/// `not-found` (or `masked`, `error`, `bad-setting`) otherwise.
fn unit_is_loaded(load_state: &str) -> bool {
    load_state.trim() == "loaded"
}

#[cfg(test)]
mod tests {
    use super::unit_is_loaded;

    #[test]
    fn only_a_loaded_unit_counts() {
        assert!(unit_is_loaded("loaded\n"));
        for state in ["not-found\n", "masked\n", "error\n", "bad-setting\n", ""] {
            assert!(!unit_is_loaded(state), "{state:?}");
        }
    }
}
