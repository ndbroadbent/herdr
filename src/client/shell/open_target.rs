//! Opens a tab on a machine when a `herdr://open` link asks this client to (`client.open`).

use super::*;

impl ClientShellState {
    /// Switches to `tab` (a tab label or ID) on `machine` (a saved machine's label or profile
    /// ID; none or `local` means this computer). Shows a notice when it can't.
    pub(crate) fn open_target(&mut self, machine: Option<&str>, tab: &str) -> ClientShellInput {
        let mut outcome = ClientShellInput {
            repaint: true,
            ..ClientShellInput::default()
        };
        match self.resolve_open_target(machine, tab) {
            Ok((endpoint_id, tab_id)) => {
                self.focus_or_activate(
                    endpoint_id,
                    ClientEndpointFocusTarget::Tab(tab_id),
                    &mut outcome,
                );
            }
            Err(message) => {
                self.push_endpoint_notice(
                    ClientEndpointNoticeKind::Rejected,
                    message.clone(),
                    "Can't open link",
                    message,
                );
            }
        }
        outcome
    }

    fn resolve_open_target(
        &self,
        machine: Option<&str>,
        tab: &str,
    ) -> Result<(ClientEndpointId, String), String> {
        let endpoint_id = match machine.filter(|machine| !machine.eq_ignore_ascii_case("local")) {
            None => ClientEndpointId::Local,
            Some(machine) => self
                .endpoints
                .iter()
                .find(|endpoint| endpoint.label == machine)
                .or_else(|| {
                    self.endpoints.iter().find(|endpoint| {
                        matches!(&endpoint.endpoint_id, ClientEndpointId::Ssh(profile) if profile.as_str() == machine)
                    })
                })
                .map(|endpoint| endpoint.endpoint_id.clone())
                .ok_or_else(|| format!("No saved machine is named {machine}."))?,
        };
        let label = self.endpoint_label(&endpoint_id).to_owned();
        let snapshot = if self.endpoint_is_active(&endpoint_id) {
            self.snapshot.as_deref()
        } else {
            self.endpoints
                .iter()
                .find(|endpoint| endpoint.endpoint_id == endpoint_id)
                .and_then(|endpoint| endpoint.snapshot.as_deref())
        }
        .ok_or_else(|| format!("{label} isn't connected."))?;
        let tab_id = snapshot
            .tabs
            .iter()
            .find(|candidate| candidate.tab_id == tab)
            .or_else(|| {
                snapshot
                    .tabs
                    .iter()
                    .find(|candidate| candidate.label == tab)
            })
            .map(|candidate| candidate.tab_id.clone())
            .ok_or_else(|| format!("{label} has no tab named {tab}."))?;
        Ok((endpoint_id, tab_id))
    }
}
