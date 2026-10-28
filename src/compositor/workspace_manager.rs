//! [`ext-workspace-v1`](https://wayland.app/protocols/ext-workspace-v1)
//! server support. Smithay 0.7 has no high-level module for this protocol, so
//! Wisp wires the generated `wayland-protocols` types directly: one global,
//! one workspace group (the single output set), and a dynamic set of
//! workspaces through the highest active/occupied index plus one spare.
use smithay::reexports::wayland_protocols::ext::workspace::v1::server::{
    ext_workspace_group_handle_v1::{self, ExtWorkspaceGroupHandleV1, GroupCapabilities},
    ext_workspace_handle_v1::{self, ExtWorkspaceHandleV1, State, WorkspaceCapabilities},
    ext_workspace_manager_v1::{self, ExtWorkspaceManagerV1},
};
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource, Weak,
    backend::{ClientId, GlobalId},
    protocol::wl_output::WlOutput,
};
use tracing::warn;

use crate::protocol::Action;

use super::Wisp;

/// Global data attached to the advertised `ext_workspace_manager_v1`.
#[derive(Clone, Copy)]
pub(crate) struct ExtWorkspaceManagerGlobalData;

/// User data for workspace group handles (the single group).
#[derive(Clone, Copy)]
pub(crate) struct ExtWorkspaceGroupData;

/// User data for workspace handles.
#[derive(Clone, Copy)]
pub(crate) struct ExtWorkspaceWorkspaceData {
    /// 1-based workspace index tracked by Wisp.
    pub(super) index: u8,
}

/// Capabilities advertised per workspace handle.
const WORKSPACE_CAPABILITIES: WorkspaceCapabilities = WorkspaceCapabilities::Activate
    .union(WorkspaceCapabilities::Deactivate)
    .union(WorkspaceCapabilities::Assign);

/// Capabilities advertised per workspace group handle.
const GROUP_CAPABILITIES: GroupCapabilities = GroupCapabilities::CreateWorkspace;

/// One bound `ext_workspace_manager_v1` per client.
pub(crate) struct ExtWorkspaceInstance {
    client: Client,
    manager: ExtWorkspaceManagerV1,
    group: ExtWorkspaceGroupHandleV1,
    workspaces: Vec<(u8, ExtWorkspaceHandleV1)>,
    /// Last state bits sent per workspace index, to avoid redundant events.
    sent: Vec<(u8, u32)>,
}

/// Wisp state for the ext-workspace protocol.
pub(crate) struct ExtWorkspaceManagerState {
    #[allow(dead_code)] // kept for future global removal support
    global: GlobalId,
    instances: Vec<ExtWorkspaceInstance>,
    /// Outputs bound by clients, linked into workspace groups.
    outputs: Vec<Weak<WlOutput>>,
}

impl ExtWorkspaceManagerState {
    pub(crate) fn new(dh: &DisplayHandle) -> Self {
        let global =
            dh.create_global::<Wisp, ExtWorkspaceManagerV1, _>(1, ExtWorkspaceManagerGlobalData);
        Self {
            global,
            instances: Vec::new(),
            outputs: Vec::new(),
        }
    }
}

/// State bits for a workspace handle.
fn workspace_state_flags(index: u8, active: u8, urgent: &[u8]) -> State {
    let mut flags = State::empty();
    if index == active {
        flags |= State::Active;
    } else {
        flags |= State::Hidden;
    }
    if urgent.contains(&index) {
        flags |= State::Urgent;
    }
    flags
}

impl Wisp {
    /// Called when a client binds the `ext_workspace_manager_v1` global.
    fn ext_workspace_register(
        &mut self,
        dh: &DisplayHandle,
        client: Client,
        manager: ExtWorkspaceManagerV1,
    ) {
        let group = match client
            .create_resource::<ExtWorkspaceGroupHandleV1, ExtWorkspaceGroupData, Wisp>(
                dh,
                manager.version(),
                ExtWorkspaceGroupData,
            ) {
            Ok(group) => group,
            Err(error) => {
                warn!(%error, "could not create ext-workspace group handle");
                manager.finished();
                return;
            }
        };
        manager.workspace_group(&group);
        group.capabilities(GROUP_CAPABILITIES);
        // Outputs this client already bound: link them to the group.
        for output in &self.ext_workspace.outputs {
            let Ok(output) = output.upgrade() else {
                continue;
            };
            if output.client().map(|bound| bound.id()) != Some(client.id()) {
                continue;
            }
            group.output_enter(&output);
        }
        self.ext_workspace.instances.push(ExtWorkspaceInstance {
            client,
            manager,
            group,
            workspaces: Vec::new(),
            sent: Vec::new(),
        });
        self.ext_workspace_notify();
    }

    /// Called when a client binds a `wl_output` global.
    pub(super) fn ext_workspace_output_bound(&mut self, wl_output: WlOutput) {
        self.ext_workspace
            .outputs
            .retain(|output| output.upgrade().is_ok());
        self.ext_workspace.outputs.push(wl_output.downgrade());
        let Some(bound_client) = wl_output.client() else {
            return;
        };
        for instance in &mut self.ext_workspace.instances {
            if instance.client.id() != bound_client.id() {
                continue;
            }
            instance.group.output_enter(&wl_output);
            // Wake bars that were waiting for this output to appear.
            instance.manager.done();
        }
    }

    /// Pushes current workspace state to all bound clients, creating handles
    /// as needed and removing unused trailing workspaces. A single spare
    /// allows the bar to create the next workspace by clicking an empty pill.
    pub(super) fn ext_workspace_notify(&mut self) {
        let highest_used = self
            .windows
            .iter()
            .filter(|window| window.mapped && !window.scratchpad_hidden)
            .map(|window| window.workspace)
            .max()
            .unwrap_or(1)
            .max(self.active_workspace);
        let last = highest_used
            .saturating_add(1)
            .max(self.created_workspace.unwrap_or(1))
            .min(self.config.general.workspaces);
        self.materialized_workspaces = (1..=last).collect();
        let dh = self.display_handle.clone();
        let materialized = self.materialized_workspaces.clone();
        let active = self.active_workspace;
        let urgent: Vec<u8> = self
            .windows
            .iter()
            .filter(|window| window.urgent && !window.scratchpad_hidden)
            .map(|window| window.workspace)
            .collect();
        let max = self.config.general.workspaces;
        for instance in &mut self.ext_workspace.instances {
            let mut changed = false;
            // 1. Create handles for newly materialized workspaces. The handle
            //    object must be announced before its details are sent, since
            //    clients attach listeners on the creation event.
            for &index in &materialized {
                if index > max || instance.workspaces.iter().any(|(seen, _)| *seen == index) {
                    continue;
                }
                let handle = match instance
                    .client
                    .create_resource::<ExtWorkspaceHandleV1, ExtWorkspaceWorkspaceData, Wisp>(
                        &dh,
                        instance.manager.version(),
                        ExtWorkspaceWorkspaceData { index },
                    ) {
                    Ok(handle) => handle,
                    Err(error) => {
                        warn!(%error, "could not create ext-workspace handle");
                        continue;
                    }
                };
                instance.manager.workspace(&handle);
                handle.id(format!("wisp-{index}"));
                handle.name(index.to_string());
                // Coordinates are an array of uints; noctalia reads `size / 4`,
                // so a `u8` index must be widened first or the array holds a
                // single byte and parses as zero coordinates.
                handle.coordinates(u32::from(index).to_le_bytes().to_vec());
                handle.capabilities(WORKSPACE_CAPABILITIES);
                instance.group.workspace_enter(&handle);
                let flags = workspace_state_flags(index, active, &urgent);
                handle.state(flags);
                instance.sent.push((index, flags.bits()));
                instance.workspaces.push((index, handle));
                changed = true;
            }
            // 2. Drop handles for workspaces no longer materialized.
            let mut removed = false;
            instance.workspaces.retain(|(index, handle)| {
                if materialized.contains(index) && *index <= max {
                    return true;
                }
                instance.group.workspace_leave(handle);
                handle.removed();
                removed = true;
                false
            });
            if removed {
                instance
                    .sent
                    .retain(|(index, _)| materialized.contains(index) && *index <= max);
                changed = true;
            }
            // 3. Refresh state bits of remaining handles.
            for (index, handle) in instance.workspaces.iter_mut() {
                let flags = workspace_state_flags(*index, active, &urgent);
                let bits = flags.bits();
                match instance.sent.iter_mut().find(|entry| entry.0 == *index) {
                    Some(entry) if entry.1 == bits => continue,
                    Some(entry) => entry.1 = bits,
                    None => instance.sent.push((*index, bits)),
                }
                handle.state(flags);
                changed = true;
            }
            if changed {
                instance.manager.done();
            }
        }
    }
}

impl GlobalDispatch<ExtWorkspaceManagerV1, ExtWorkspaceManagerGlobalData, Wisp> for Wisp {
    fn bind(
        state: &mut Wisp,
        dh: &DisplayHandle,
        client: &Client,
        resource: New<ExtWorkspaceManagerV1>,
        _global_data: &ExtWorkspaceManagerGlobalData,
        data_init: &mut DataInit<'_, Wisp>,
    ) {
        let manager = data_init.init(resource, ());
        state.ext_workspace_register(dh, client.clone(), manager);
    }
}

impl Dispatch<ExtWorkspaceManagerV1, (), Wisp> for Wisp {
    fn request(
        state: &mut Wisp,
        _client: &Client,
        resource: &ExtWorkspaceManagerV1,
        request: ext_workspace_manager_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Wisp>,
    ) {
        match request {
            // Requests are processed eagerly; commit just flushes clients.
            ext_workspace_manager_v1::Request::Commit => state.ext_workspace_notify(),
            ext_workspace_manager_v1::Request::Stop => {
                let dead = resource.id();
                state.ext_workspace.instances.retain(|instance| {
                    if instance.manager.id() == dead {
                        instance.manager.finished();
                        false // the client will destroy the manager object
                    } else {
                        true
                    }
                });
            }
            _ => {} // future revisions; unknown opcodes are rejected on the wire
        }
    }

    fn destroyed(
        state: &mut Wisp,
        _client_id: ClientId,
        resource: &ExtWorkspaceManagerV1,
        _data: &(),
    ) {
        let dead = resource.id();
        state
            .ext_workspace
            .instances
            .retain(|instance| instance.manager.id() != dead);
    }
}

impl Dispatch<ExtWorkspaceGroupHandleV1, ExtWorkspaceGroupData, Wisp> for Wisp {
    fn request(
        state: &mut Wisp,
        _client: &Client,
        _resource: &ExtWorkspaceGroupHandleV1,
        request: ext_workspace_group_handle_v1::Request,
        _data: &ExtWorkspaceGroupData,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Wisp>,
    ) {
        match request {
            ext_workspace_group_handle_v1::Request::CreateWorkspace { workspace: name } => {
                // Keep explicitly created (but still empty) workspaces until
                // the next switch; normal updates retain only one spare.
                if let Some(index) = state
                    .materialized_workspaces
                    .last()
                    .and_then(|index| index.checked_add(1))
                    .filter(|index| *index <= state.config.general.workspaces)
                {
                    state.created_workspace = Some(index);
                    state.ext_workspace_notify();
                } else {
                    warn!(
                        workspace = %name,
                        "client attempted to create workspace beyond configured maximum; ignored"
                    );
                }
            }
            ext_workspace_group_handle_v1::Request::Destroy => {}
            _ => {}
        }
    }
}

impl Dispatch<ExtWorkspaceHandleV1, ExtWorkspaceWorkspaceData, Wisp> for Wisp {
    fn request(
        state: &mut Wisp,
        _client: &Client,
        _resource: &ExtWorkspaceHandleV1,
        request: ext_workspace_handle_v1::Request,
        data: &ExtWorkspaceWorkspaceData,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Wisp>,
    ) {
        match request {
            ext_workspace_handle_v1::Request::Destroy => {}
            ext_workspace_handle_v1::Request::Activate => {
                let _ = state.dispatch(Action::Workspace { index: data.index });
            }
            // There is a single group and all workspaces are members of it, so
            // assign and deactivate have no effect beyond a flush.
            ext_workspace_handle_v1::Request::Deactivate => state.ext_workspace_notify(),
            ext_workspace_handle_v1::Request::Assign { .. } => {}
            ext_workspace_handle_v1::Request::Remove => {
                warn!(
                    workspace = data.index,
                    "client requested removal of a configured workspace; ignored"
                )
            }
            _ => {}
        }
    }
}
