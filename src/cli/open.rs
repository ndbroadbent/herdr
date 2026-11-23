//! `herdr open`: opens a tab in the Herdr window used most recently, from a
//! `herdr://open?machine=<label>&tab=<label>` link or from flags.

use crate::api::schema::{ClientOpenParams, Method, Request};

const USAGE: &str = "usage: herdr open <herdr://open?machine=<label>&tab=<label>>\n       herdr open --tab <label> [--on <machine>]\n       herdr open --install-handler [--herdr <path>] | --uninstall-handler";

pub(super) fn run_open_command(args: &[String]) -> std::io::Result<i32> {
    let params = match args.first().map(String::as_str) {
        None | Some("help" | "--help" | "-h") => {
            eprintln!("{USAGE}");
            return Ok(if args.is_empty() { 2 } else { 0 });
        }
        Some("--install-handler") => {
            return match args.get(1..).unwrap_or_default() {
                [] => install_handler(std::env::current_exe()?),
                [flag, path] if flag == "--herdr" => install_handler(path.into()),
                _ => {
                    eprintln!("{USAGE}");
                    Ok(2)
                }
            };
        }
        Some("--uninstall-handler") => {
            let removed = crate::platform::uninstall_link_handler()?;
            println!(
                "{}",
                serde_json::json!({ "type": "link_handler_uninstalled", "removed": removed })
            );
            return Ok(0);
        }
        Some(link) if link.contains("://") => match parse_link(link) {
            Ok(params) => params,
            Err(message) => {
                eprintln!("herdr open: {message}");
                return Ok(2);
            }
        },
        Some(_) => match parse_flags(args) {
            Some(params) => params,
            None => {
                eprintln!("{USAGE}");
                return Ok(2);
            }
        },
    };
    // Always this computer's server: that's where its Herdr windows are attached.
    let response = super::send_request(&Request {
        id: "cli:open".into(),
        method: Method::ClientOpen(params),
    })?;
    super::print_response(&response)
}

/// `herdr`: the program each link is passed to, normally this one; `--herdr` names another,
/// such as a wrapper that picks a session.
fn install_handler(herdr: std::path::PathBuf) -> std::io::Result<i32> {
    let app = crate::platform::install_link_handler(&herdr)?;
    println!(
        "{}",
        serde_json::json!({
            "type": "link_handler_installed",
            "path": app,
            "herdr": herdr,
        })
    );
    Ok(0)
}

fn parse_flags(args: &[String]) -> Option<ClientOpenParams> {
    let mut tab = None;
    let mut machine = None;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--tab" => tab = Some(args.next()?.clone()),
            "--on" => machine = Some(args.next()?.clone()),
            _ => return None,
        }
    }
    Some(ClientOpenParams {
        machine,
        tab: tab.filter(|tab| !tab.is_empty())?,
    })
}

/// Parses `herdr://open?machine=<label>&tab=<label>`. Without `machine` (or with `local`), the
/// tab is on this computer.
pub(super) fn parse_link(link: &str) -> Result<ClientOpenParams, String> {
    let rest = link
        .strip_prefix("herdr://")
        .ok_or_else(|| format!("not a herdr:// link: {link}"))?;
    let (action, query) = rest.split_once('?').unwrap_or((rest, ""));
    if action.trim_end_matches('/') != "open" {
        return Err(format!("unknown herdr:// link action: {action}"));
    }
    let mut machine = None;
    let mut tab = None;
    for pair in query.split('&').filter(|pair| !pair.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let value = decode_query_component(value)
            .ok_or_else(|| format!("invalid percent-encoding in {key}"))?;
        match key {
            "machine" => machine = Some(value).filter(|value| !value.is_empty()),
            "tab" => tab = Some(value),
            _ => {}
        }
    }
    let tab = tab
        .filter(|tab| !tab.is_empty())
        .ok_or_else(|| "the link has no tab".to_owned())?;
    Ok(ClientOpenParams { machine, tab })
}

fn decode_query_component(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => decoded.push(b' '),
            b'%' => {
                let hex = std::str::from_utf8(bytes.get(i + 1..i + 3)?).ok()?;
                decoded.push(u8::from_str_radix(hex, 16).ok()?);
                i += 2;
            }
            byte => decoded.push(byte),
        }
        i += 1;
    }
    String::from_utf8(decoded).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_name_a_tab_and_optionally_a_machine() {
        assert_eq!(
            parse_link("herdr://open?machine=agents&tab=email%20triage"),
            Ok(ClientOpenParams {
                machine: Some("agents".into()),
                tab: "email triage".into(),
            })
        );
        assert_eq!(
            parse_link("herdr://open/?tab=email+triage&machine="),
            Ok(ClientOpenParams {
                machine: None,
                tab: "email triage".into(),
            })
        );
        assert_eq!(
            parse_link("herdr://open?tab=caf%C3%A9&extra=1"),
            Ok(ClientOpenParams {
                machine: None,
                tab: "café".into(),
            })
        );
        assert!(parse_link("herdr://open?machine=agents").is_err());
        assert!(parse_link("herdr://close?tab=x").is_err());
        assert!(parse_link("https://open?tab=x").is_err());
        assert!(parse_link("herdr://open?tab=%zz").is_err());
    }

    #[test]
    fn flags_need_a_tab() {
        let args = |values: &[&str]| {
            values
                .iter()
                .map(|value| value.to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            parse_flags(&args(&["--tab", "email triage", "--on", "agents"])),
            Some(ClientOpenParams {
                machine: Some("agents".into()),
                tab: "email triage".into(),
            })
        );
        assert_eq!(parse_flags(&args(&["--on", "agents"])), None);
        assert_eq!(parse_flags(&args(&["--tab"])), None);
    }
}
