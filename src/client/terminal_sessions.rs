//! `herdr terminal session observe|control`: a pane's live screen as newline-delimited JSON on
//! stdout, with commands as newline-delimited JSON on stdin. The record shapes and close codes
//! are a documented contract (socket-api.mdx, "Pane streams"); keep them stable.

use std::io::{self, BufRead, Write as _};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use base64::Engine;
use interprocess::local_socket::traits::Stream as _;
use interprocess::TryClone as _;
use serde_json::{json, Value};
use tracing::info;

use crate::api::client::ApiClient;
use crate::ipc::LocalStream;
use crate::protocol::{
    self, AttachScrollDirection, AttachScrollSource, ClientMessage, ClientMouseButton,
    ClientMouseKind, ClientMousePosition, RenderEncoding, ServerMessage, MAX_GRAPHICS_FRAME_SIZE,
};
use crate::server::socket_paths::client_socket_path;

use super::{do_handshake, init_logging, write_to_server, ClientError, HandshakePixelCapabilities};

/// Default and maximum rows for `terminal.scrollback`.
const DEFAULT_SCROLLBACK_LINES: u32 = 200;
const MAX_SCROLLBACK_LINES: u32 = 10_000;

/// Runs a terminal session observer. It never resizes the pane and any number can watch at
/// once. With `input`, it can also type into the pane (keys and paste) through the API.
pub fn run_terminal_session_observe(
    target: String,
    cols: u16,
    rows: u16,
    input: bool,
) -> io::Result<i32> {
    let records = Records::new();
    let Some(pane) = StreamPane::resolve(&target) else {
        return Ok(records.closed_before_start(&target));
    };
    let mut stream =
        connect_terminal_session_stream(target.clone(), cols, rows, "observing terminal session")?;
    write_to_server(&mut stream, &ClientMessage::ObserveTerminal { target })?;
    records.opened(&pane, if input { "observe_input" } else { "observe" });

    let commands = StreamCommands {
        records: records.clone(),
        pane,
        mode: StreamMode::Observe { input },
    };
    // An observer may run with stdin closed; that ends commands, not the stream.
    std::thread::spawn(move || {
        for line in io::stdin().lock().lines() {
            let Ok(line) = line else {
                break;
            };
            commands.handle(&line);
        }
    });

    write_terminal_session_output(stream, &records)
}

/// Runs a writable terminal session controller: one at a time, sized to the controller.
pub fn run_terminal_session_control(
    target: String,
    takeover: bool,
    cols: u16,
    rows: u16,
) -> io::Result<i32> {
    let records = Records::new();
    let Some(pane) = StreamPane::resolve(&target) else {
        return Ok(records.closed_before_start(&target));
    };
    let mut stream = connect_terminal_session_stream(
        target.clone(),
        cols,
        rows,
        "controlling terminal session",
    )?;
    write_to_server(
        &mut stream,
        &ClientMessage::ControlTerminal { target, takeover },
    )?;
    records.opened(&pane, "control");

    let mut write_stream = stream.try_clone()?;
    let commands = StreamCommands {
        records: records.clone(),
        pane,
        mode: StreamMode::Control,
    };
    std::thread::spawn(move || {
        for line in io::stdin().lock().lines() {
            let Ok(line) = line else {
                break;
            };
            let Some(message) = commands.handle(&line) else {
                continue;
            };
            let release = matches!(message, ClientMessage::Detach);
            if write_to_server(&mut write_stream, &message).is_err() || release {
                return;
            }
        }
        let _ = write_to_server(&mut write_stream, &ClientMessage::Detach);
    });

    write_terminal_session_output(stream, &records)
}

/// How long commands wait for the first frame before being handled anyway.
const FIRST_FRAME_WAIT: Duration = Duration::from_secs(5);

/// Serialises records from the frame reader and the command thread onto stdout, and holds
/// commands back until the first (full) frame is out, so every stream starts with the screen.
#[derive(Clone)]
struct Records(Arc<RecordsInner>);

struct RecordsInner {
    write: Mutex<()>,
    started: Mutex<bool>,
    started_changed: Condvar,
}

impl Records {
    fn new() -> Self {
        Self(Arc::new(RecordsInner {
            write: Mutex::new(()),
            started: Mutex::new(false),
            started_changed: Condvar::new(),
        }))
    }

    fn mark_started(&self) {
        let mut started = self
            .0
            .started
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !*started {
            *started = true;
            self.0.started_changed.notify_all();
        }
    }

    fn wait_until_started(&self) {
        let started = self
            .0
            .started
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let _ = self
            .0
            .started_changed
            .wait_timeout_while(started, FIRST_FRAME_WAIT, |started| !*started);
    }

    fn emit(&self, record: &Value) -> io::Result<()> {
        let _guard = self
            .0
            .write
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut stdout = io::stdout().lock();
        serde_json::to_writer(&mut stdout, record)?;
        stdout.write_all(b"\n")?;
        stdout.flush()
    }

    fn opened(&self, pane: &StreamPane, mode: &str) {
        let _ = self.emit(&json!({
            "type": "terminal.opened",
            "mode": mode,
            "pane_id": pane.pane_id(),
            "terminal_id": pane.terminal_id,
        }));
    }

    fn closed(&self, code: &str, reason: &str) -> i32 {
        self.mark_started();
        let _ = self.emit(&json!({
            "type": "terminal.closed",
            "code": code,
            "reason": reason,
        }));
        close_exit_code(code)
    }

    fn closed_before_start(&self, target: &str) -> i32 {
        self.closed(
            "target_not_found",
            &format!("terminal target {target} not found"),
        )
    }

    fn error(&self, command: Option<&str>, id: Option<&Value>, code: &str, message: &str) {
        let mut record = json!({
            "type": "terminal.error",
            "code": code,
            "message": message,
        });
        if let Some(command) = command {
            record["command"] = json!(command);
        }
        if let Some(id) = id {
            record["id"] = id.clone();
        }
        let _ = self.emit(&record);
    }
}

/// The stable close code for a server's stream-ending reason.
fn close_code(reason: &str) -> &'static str {
    if reason.contains("not found") && reason.contains("target") {
        "target_not_found"
    } else if reason.contains("exited") || reason.contains("not found") {
        "pane_closed"
    } else if reason.contains("already has an attached client") {
        "controller_exists"
    } else if reason.contains("taken over") {
        "taken_over"
    } else if reason.contains("read in progress") {
        "busy"
    } else if reason == "detached" {
        "released"
    } else if reason.contains("live update in progress") {
        "server_restarting"
    } else if reason.contains("shutting down") {
        "server_stopping"
    } else {
        "error"
    }
}

/// 0 when the stream ended normally; 1 when it couldn't start or was cut off.
fn close_exit_code(code: &str) -> i32 {
    match code {
        "pane_closed" | "released" | "server_restarting" | "server_stopping" | "taken_over" => 0,
        _ => 1,
    }
}

/// The pane behind a stream, for the commands that go through the API.
struct StreamPane {
    pane_id: Mutex<String>,
    terminal_id: String,
}

impl StreamPane {
    /// Resolves a pane, terminal, or agent target, as the stream itself does.
    fn resolve(target: &str) -> Option<Self> {
        let api = ApiClient::local();
        let from = |pane: &Value| {
            Some(Self {
                pane_id: Mutex::new(pane["pane_id"].as_str()?.to_owned()),
                terminal_id: pane["terminal_id"].as_str()?.to_owned(),
            })
        };
        if let Ok(result) = api_call(&api, "pane.get", json!({ "pane_id": target })) {
            return from(&result["pane"]);
        }
        if let Ok(result) = api_call(&api, "agent.get", json!({ "target": target })) {
            return from(&result["agent"]);
        }
        let panes = api_call(&api, "pane.list", json!({})).ok()?;
        panes["panes"]
            .as_array()?
            .iter()
            .find(|pane| pane["terminal_id"].as_str() == Some(target))
            .and_then(from)
    }

    fn pane_id(&self) -> String {
        self.pane_id
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Calls a pane method; if the pane moved (its public ID changed), finds it again by its
    /// terminal and retries once.
    fn call(&self, method: &str, mut params: Value) -> Result<Value, ApiError> {
        let api = ApiClient::local();
        params["pane_id"] = json!(self.pane_id());
        match api_call(&api, method, params.clone()) {
            Err(err) if err.code == "pane_not_found" => {
                let panes = api_call(&api, "pane.list", json!({}))?;
                let Some(pane_id) = panes["panes"].as_array().and_then(|panes| {
                    panes
                        .iter()
                        .find(|pane| pane["terminal_id"].as_str() == Some(&self.terminal_id))
                        .and_then(|pane| pane["pane_id"].as_str())
                }) else {
                    return Err(err);
                };
                *self
                    .pane_id
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = pane_id.to_owned();
                params["pane_id"] = json!(pane_id);
                api_call(&api, method, params)
            }
            result => result,
        }
    }
}

struct ApiError {
    code: String,
    message: String,
}

fn api_call(api: &ApiClient, method: &str, params: Value) -> Result<Value, ApiError> {
    let request = serde_json::from_value::<crate::api::schema::Request>(json!({
        "id": "cli:terminal:session",
        "method": method,
        "params": params,
    }))
    .map_err(|err| ApiError {
        code: "invalid_command".into(),
        message: err.to_string(),
    })?;
    let response = api.request_value(&request).map_err(|err| ApiError {
        code: "api_unavailable".into(),
        message: err.to_string(),
    })?;
    if let Some(error) = response.get("error") {
        return Err(ApiError {
            code: error["code"].as_str().unwrap_or("error").to_owned(),
            message: error["message"].as_str().unwrap_or_default().to_owned(),
        });
    }
    Ok(response["result"].clone())
}

enum StreamMode {
    Observe { input: bool },
    Control,
}

struct StreamCommands {
    records: Records,
    pane: StreamPane,
    mode: StreamMode,
}

/// Commands both modes take; the controller's own commands are `TerminalControlCommand`.
#[derive(serde::Deserialize)]
#[serde(tag = "type")]
enum TerminalStreamCommand {
    #[serde(rename = "terminal.keys")]
    Keys { keys: Vec<String> },
    #[serde(rename = "terminal.paste")]
    Paste { text: String },
    #[serde(rename = "terminal.scrollback")]
    Scrollback {
        #[serde(default)]
        lines: Option<u32>,
        #[serde(default)]
        format: Option<String>,
    },
}

impl StreamCommands {
    /// Handles one stdin line. Returns a message for the controller's stream to send, if any;
    /// everything else is answered on stdout.
    fn handle(&self, line: &str) -> Option<ClientMessage> {
        if line.trim().is_empty() {
            return None;
        }
        self.records.wait_until_started();
        let raw: Value = match serde_json::from_str(line) {
            Ok(raw) => raw,
            Err(err) => {
                self.records.error(
                    None,
                    None,
                    "invalid_command",
                    &format!("invalid json command: {err}"),
                );
                return None;
            }
        };
        let kind = raw["type"].as_str().map(str::to_owned);
        let id = raw.get("id").cloned();
        let kind_ref = kind.as_deref();
        let fail = |code: &str, message: &str| {
            self.records.error(kind_ref, id.as_ref(), code, message);
            None
        };

        if let Ok(command) = serde_json::from_value::<TerminalStreamCommand>(raw.clone()) {
            return match command {
                TerminalStreamCommand::Scrollback { lines, format } => {
                    self.scrollback(lines, format, id.as_ref());
                    None
                }
                TerminalStreamCommand::Keys { .. } | TerminalStreamCommand::Paste { .. }
                    if matches!(self.mode, StreamMode::Observe { input: false }) =>
                {
                    fail(
                        "input_not_enabled",
                        "this observer is read-only; start it with --input to type",
                    )
                }
                TerminalStreamCommand::Keys { keys } => {
                    if let Err(err) = self.pane.call("pane.send_keys", json!({ "keys": keys })) {
                        return fail(&err.code, &err.message);
                    }
                    None
                }
                // A complete bracketed paste on the controller's stream reaches the pane as
                // one paste, in order with its other input.
                TerminalStreamCommand::Paste { text }
                    if matches!(self.mode, StreamMode::Control) =>
                {
                    Some(ClientMessage::Input {
                        data: format!("\x1b[200~{text}\x1b[201~").into_bytes(),
                    })
                }
                TerminalStreamCommand::Paste { text } => {
                    if let Err(err) = self.pane.call("pane.send_text", json!({ "text": text })) {
                        return fail(&err.code, &err.message);
                    }
                    None
                }
            };
        }

        match self.mode {
            StreamMode::Control => match terminal_control_command_from_json(line) {
                Ok(message) => Some(message),
                Err(err) => fail("invalid_command", &err),
            },
            StreamMode::Observe { .. } => match kind_ref {
                Some(
                    "terminal.input" | "terminal.resize" | "terminal.scroll" | "terminal.mouse"
                    | "terminal.release",
                ) => fail(
                    "control_only",
                    "only a controller can send this; observers use terminal.keys and terminal.paste",
                ),
                _ => fail("invalid_command", "unknown command type"),
            },
        }
    }

    fn scrollback(&self, lines: Option<u32>, format: Option<String>, id: Option<&Value>) {
        let lines = lines
            .unwrap_or(DEFAULT_SCROLLBACK_LINES)
            .clamp(1, MAX_SCROLLBACK_LINES);
        let format = format.unwrap_or_else(|| "text".to_owned());
        if format != "text" && format != "ansi" {
            self.records.error(
                Some("terminal.scrollback"),
                id,
                "invalid_command",
                "format must be text or ansi",
            );
            return;
        }
        match self.pane.call(
            "pane.read",
            json!({ "source": "recent", "lines": lines, "format": format, "strip_ansi": format == "text" }),
        ) {
            Ok(result) => {
                let read = &result["read"];
                let mut record = json!({
                    "type": "terminal.scrollback",
                    "format": format,
                    "lines": lines,
                    "truncated": read["truncated"].as_bool().unwrap_or(false),
                    "text": read["text"].as_str().unwrap_or_default(),
                });
                if let Some(id) = id {
                    record["id"] = id.clone();
                }
                let _ = self.records.emit(&record);
            }
            Err(err) => {
                self.records
                    .error(Some("terminal.scrollback"), id, &err.code, &err.message)
            }
        }
    }
}

fn connect_terminal_session_stream(
    target: String,
    cols: u16,
    rows: u16,
    log_message: &'static str,
) -> io::Result<LocalStream> {
    init_logging();

    let socket_path = client_socket_path();
    crate::logging::startup("client");
    info!(path = %socket_path.display(), target = %target, cols, rows, "{log_message}");

    let mut stream = match crate::ipc::connect_local_stream(&socket_path) {
        Ok(stream) => stream,
        Err(err) => {
            eprintln!("herdr: {}", ClientError::ConnectionFailed(err));
            std::process::exit(1);
        }
    };

    match do_handshake(
        &mut stream,
        cols,
        rows,
        0,
        0,
        HandshakePixelCapabilities {
            exact_geometry: false,
            sgr_mouse: false,
        },
        None,
        false,
        false,
        true,
        true,
    ) {
        Ok(handshake) if handshake.encoding == RenderEncoding::TerminalAnsi => {}
        Ok(handshake) => {
            eprintln!(
                "herdr: terminal session observe negotiated unsupported encoding {:?}",
                handshake.encoding
            );
            std::process::exit(1);
        }
        Err(err) => {
            eprintln!("herdr: {err}");
            std::process::exit(1);
        }
    }

    stream.set_nonblocking(false)?;
    Ok(stream)
}

fn write_terminal_session_output(mut stream: LocalStream, records: &Records) -> io::Result<i32> {
    loop {
        match protocol::read_message(&mut stream, MAX_GRAPHICS_FRAME_SIZE) {
            Ok(ServerMessage::Terminal(frame)) => {
                let encoded = base64::engine::general_purpose::STANDARD.encode(&frame.bytes);
                records.emit(&json!({
                    "type": "terminal.frame",
                    "seq": frame.seq,
                    "encoding": "ansi",
                    "width": frame.width,
                    "height": frame.height,
                    "full": frame.full,
                    "bytes": encoded,
                }))?;
                records.mark_started();
            }
            Ok(ServerMessage::ServerShutdown { reason }) => {
                let reason = reason.unwrap_or_else(|| "server closed the stream".to_owned());
                return Ok(records.closed(close_code(&reason), &reason));
            }
            Ok(_) => {}
            Err(protocol::FramingError::UnexpectedEof) => {
                return Ok(records.closed("disconnected", "the connection to herdr closed"));
            }
            Err(err) => {
                let reason = err.to_string();
                return Ok(records.closed("disconnected", &reason));
            }
        }
    }
}

#[derive(serde::Deserialize)]
#[serde(tag = "type")]
enum TerminalControlCommand {
    #[serde(rename = "terminal.input")]
    Input {
        text: Option<String>,
        bytes: Option<String>,
    },
    #[serde(rename = "terminal.resize")]
    Resize {
        cols: u16,
        rows: u16,
        #[serde(default)]
        cell_width_px: u32,
        #[serde(default)]
        cell_height_px: u32,
    },
    #[serde(rename = "terminal.scroll")]
    Scroll {
        direction: TerminalControlScrollDirection,
        lines: u16,
        #[serde(default)]
        source: TerminalControlScrollSource,
        #[serde(default)]
        column: Option<u16>,
        #[serde(default)]
        row: Option<u16>,
        #[serde(default)]
        modifiers: u8,
    },
    #[serde(rename = "terminal.mouse")]
    Mouse {
        action: TerminalControlMouseAction,
        #[serde(default)]
        button: TerminalControlMouseButton,
        column: u16,
        row: u16,
        #[serde(default)]
        modifiers: u8,
    },
    #[serde(rename = "terminal.release")]
    Release {},
}

#[derive(Clone, Copy, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum TerminalControlMouseAction {
    Down,
    Up,
    Drag,
    Move,
}

#[derive(Clone, Copy, Default, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum TerminalControlMouseButton {
    #[default]
    Left,
    Right,
    Middle,
}

#[derive(Clone, Copy, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum TerminalControlScrollDirection {
    Up,
    Down,
}

#[derive(Clone, Copy, Default, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum TerminalControlScrollSource {
    #[default]
    Wheel,
    PageKey,
}

pub(super) fn terminal_control_command_from_json(raw: &str) -> Result<ClientMessage, String> {
    let command = serde_json::from_str::<TerminalControlCommand>(raw)
        .map_err(|err| format!("invalid json command: {err}"))?;
    match command {
        TerminalControlCommand::Input { text, bytes } => {
            let data = match (text, bytes) {
                (Some(_), Some(_)) => {
                    return Err("terminal.input accepts text or bytes, not both".into())
                }
                (Some(text), None) => text.into_bytes(),
                (None, Some(bytes)) => base64::engine::general_purpose::STANDARD
                    .decode(bytes)
                    .map_err(|err| format!("invalid terminal.input bytes: {err}"))?,
                (None, None) => Vec::new(),
            };
            Ok(ClientMessage::Input { data })
        }
        TerminalControlCommand::Resize {
            cols,
            rows,
            cell_width_px,
            cell_height_px,
        } => {
            if cols == 0 || rows == 0 {
                return Err("terminal.resize cols and rows must be greater than 0".into());
            }
            Ok(ClientMessage::Resize {
                cols,
                rows,
                cell_width_px,
                cell_height_px,
                pixel_mouse: false,
            })
        }
        TerminalControlCommand::Scroll {
            direction,
            lines,
            source,
            column,
            row,
            modifiers,
        } => {
            if lines == 0 {
                return Err("terminal.scroll lines must be greater than 0".into());
            }
            let direction = match direction {
                TerminalControlScrollDirection::Up => AttachScrollDirection::Up,
                TerminalControlScrollDirection::Down => AttachScrollDirection::Down,
            };
            let source = match source {
                TerminalControlScrollSource::Wheel => AttachScrollSource::Wheel,
                TerminalControlScrollSource::PageKey => AttachScrollSource::PageKey {
                    input: match direction {
                        AttachScrollDirection::Up => b"\x1b[5~".to_vec(),
                        AttachScrollDirection::Down => b"\x1b[6~".to_vec(),
                    },
                },
            };
            Ok(ClientMessage::AttachScroll {
                source,
                direction,
                lines,
                column,
                row,
                modifiers,
            })
        }
        TerminalControlCommand::Mouse {
            action,
            button,
            column,
            row,
            modifiers,
        } => {
            let button = match button {
                TerminalControlMouseButton::Left => ClientMouseButton::Left,
                TerminalControlMouseButton::Right => ClientMouseButton::Right,
                TerminalControlMouseButton::Middle => ClientMouseButton::Middle,
            };
            let kind = match action {
                TerminalControlMouseAction::Down => ClientMouseKind::Down(button),
                TerminalControlMouseAction::Up => ClientMouseKind::Up(button),
                TerminalControlMouseAction::Drag => ClientMouseKind::Drag(button),
                TerminalControlMouseAction::Move => ClientMouseKind::Moved,
            };
            // The server encodes the event for the pane's mouse mode and drops it
            // when the application has not enabled mouse reporting.
            Ok(ClientMessage::AttachMouse {
                kind,
                position: ClientMousePosition::Cell { column, row },
                geometry: None,
                modifiers,
                lines: 1,
            })
        }
        TerminalControlCommand::Release {} => Ok(ClientMessage::Detach),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commands(mode: StreamMode) -> StreamCommands {
        let records = Records::new();
        records.mark_started();
        StreamCommands {
            records,
            pane: StreamPane {
                pane_id: Mutex::new("w1:p1".into()),
                terminal_id: "term_1".into(),
            },
            mode,
        }
    }

    #[test]
    fn server_reasons_map_to_stable_close_codes() {
        for (reason, code, exit) in [
            (
                "terminal session observe failed: terminal target w1:p9 not found",
                "target_not_found",
                1,
            ),
            ("terminal attach ended: terminal term_1 not found", "pane_closed", 0),
            ("terminal term_1 exited", "pane_closed", 0),
            (
                "terminal attach failed: terminal term_1 already has an attached client; retry with --takeover",
                "controller_exists",
                1,
            ),
            ("terminal attach taken over", "taken_over", 0),
            (
                "terminal attach failed: terminal term_1 has a read in progress; retry",
                "busy",
                1,
            ),
            ("detached", "released", 0),
            (
                "live update in progress; reconnect after handoff completes",
                "server_restarting",
                0,
            ),
            ("server is shutting down", "server_stopping", 0),
            ("something new", "error", 1),
        ] {
            assert_eq!(close_code(reason), code, "{reason}");
            assert_eq!(close_exit_code(code), exit, "{code}");
        }
        assert_eq!(close_exit_code("disconnected"), 1);
    }

    #[test]
    fn a_read_only_observer_refuses_input_without_calling_the_api() {
        let observer = commands(StreamMode::Observe { input: false });
        for line in [
            r#"{"type":"terminal.keys","keys":["enter"]}"#,
            r#"{"type":"terminal.paste","text":"hi"}"#,
            r#"{"type":"terminal.input","text":"hi"}"#,
            r#"{"type":"terminal.resize","cols":10,"rows":5}"#,
            r#"{"type":"nope"}"#,
            "not json",
            "",
        ] {
            assert!(observer.handle(line).is_none(), "{line}");
        }
    }

    #[test]
    fn a_controller_pastes_on_its_own_stream_and_keeps_its_raw_commands() {
        let controller = commands(StreamMode::Control);
        assert_eq!(
            controller.handle(r#"{"type":"terminal.paste","text":"two\nlines"}"#),
            Some(ClientMessage::Input {
                data: b"\x1b[200~two\nlines\x1b[201~".to_vec()
            })
        );
        assert_eq!(
            controller.handle(r#"{"type":"terminal.input","text":"ls"}"#),
            Some(ClientMessage::Input {
                data: b"ls".to_vec()
            })
        );
        assert_eq!(
            controller.handle(r#"{"type":"terminal.release"}"#),
            Some(ClientMessage::Detach)
        );
        assert!(controller
            .handle(r#"{"type":"terminal.resize","cols":0,"rows":5}"#)
            .is_none());
    }
}
