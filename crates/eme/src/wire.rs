//! [`EmeWire`]: EME's transport as plain queues, so the transport proper (chat, offline, a future
//! MonkeyNet adapter) is a host system that never borrows EME's state.
//!
//! - **Hot path:** [`EmeTransport::send_edit`] queues the edit in `edits_out`; the dispatch system
//!   submits each through the voxel seam ([`benilla_world::voxel::MonkeyVoxelApi::submit`]) as a
//!   local prediction, and the voxel layer's own uplink carries it (`.mvox 02…` EDIT3). The
//!   server's answer comes back as a [`benilla_world::voxel::MonkeyVoxelEvent`], which the dispatch
//!   system turns into [`EmeInbound::EditConfirmed`] / [`EmeInbound::EditRejected`].
//! - **Cold path:** [`EmeTransport::send_service`] queues the request's envelope JSON; the host
//!   transport drains it with [`EmeWire::take_services`] and pushes replies / pushes into
//!   [`EmeWire::inbound`].
//! - **Link:** the host transport keeps [`EmeWire::link`] current (and queues an
//!   [`EmeInbound::Link`] on every change through [`EmeWire::set_link`]).

use std::collections::HashSet;

use bevy::prelude::*;
use eme_core::{
    EmeInbound, EmeLink, EmeRequestId, EmeRequestIdAlloc, EmeServiceEnvelope, EmeServiceRequest,
    EmeTransport, EmeVoxelEdit,
};

/// One queued service request.
#[derive(Debug, Clone, PartialEq)]
pub struct EmeOutgoingService {
    /// The request id (the frame id on the wire).
    pub id: EmeRequestId,
    /// The request (an emulated server reads it directly).
    pub request: EmeServiceRequest,
    /// The envelope JSON (`{"v":1,"id":…,"op":…}`), what a wire transport sends.
    pub json: String,
}

/// The queue-backed transport (see the module docs).
#[derive(Resource, Debug, Default)]
pub struct EmeWire {
    /// Current link state.
    pub link: EmeLink,
    /// No server: [`crate::offline`] answers services and confirms edits locally.
    pub offline: bool,
    /// Edits sent this frame, for the dispatch system to submit to the voxel layer.
    pub edits_out: Vec<EmeVoxelEdit>,
    services_out: Vec<EmeOutgoingService>,
    /// Inbound items for the next pump, in arrival order.
    pub inbound: Vec<EmeInbound>,
    ids: EmeRequestIdAlloc,
    /// Nonces of EME's edits awaiting the server (filters the voxel layer's events).
    pub(crate) awaiting: HashSet<u16>,
    /// Edits submitted while offline, for the emulated server to confirm.
    pub(crate) offline_sent: Vec<EmeVoxelEdit>,
}

impl EmeWire {
    /// Change the link state; queues an [`EmeInbound::Link`] when it differs.
    pub fn set_link(&mut self, link: EmeLink) {
        if self.link != link {
            self.link = link;
            self.inbound.push(EmeInbound::Link(link));
        }
    }

    /// Take the queued service requests (oldest first).
    pub fn take_services(&mut self) -> Vec<EmeOutgoingService> {
        std::mem::take(&mut self.services_out)
    }

    /// Queued service requests not yet taken.
    pub fn pending_services(&self) -> usize {
        self.services_out.len()
    }
}

impl EmeTransport for EmeWire {
    fn send_edit(&mut self, edit: &EmeVoxelEdit) {
        self.edits_out.push(*edit);
    }

    fn send_service(&mut self, request: &EmeServiceRequest) -> EmeRequestId {
        let id = self.ids.next_id();
        let json = EmeServiceEnvelope::new(id, request.clone()).to_json();
        self.services_out.push(EmeOutgoingService {
            id,
            request: request.clone(),
            json,
        });
        id
    }

    fn poll(&mut self) -> Vec<EmeInbound> {
        std::mem::take(&mut self.inbound)
    }

    fn state(&self) -> EmeLink {
        self.link
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_queues_both_paths_and_reports_link_changes_once() {
        let mut w = EmeWire::default();
        w.set_link(EmeLink::Up { server_version: 3 });
        w.set_link(EmeLink::Up { server_version: 3 });
        assert_eq!(w.poll(), vec![EmeInbound::Link(EmeLink::Up { server_version: 3 })]);
        let id = w.send_service(&EmeServiceRequest::Caps);
        let s = w.take_services();
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].id, id);
        assert_eq!(s[0].json, format!(r#"{{"v":1,"id":{},"op":"caps"}}"#, id.0));
        assert!(w.take_services().is_empty());
    }
}
