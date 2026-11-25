//! Add as contact from a call (docs/design.md "Contacts", cast-40e.10): a
//! participant offers another one to become contacts, and the host passes
//! the offer on.
//!
//! - The offering participant's install made the contact code for the other
//!   participant's person ([`crate::contact::Contacts::new_code_for`]), so
//!   their request with it is accepted with nobody asked.
//! - The host passes it on ([`Event::ContactOffer`]) only between two joined
//!   participants of different persons, each named by a certificate the host
//!   verified (the host machine stands for this host's own person), and only
//!   a code that parses as one (or a link whose fragment does).

use castcorder_link::person::ContactCode;
use castcorder_protocol::{ErrorKind, Event, ParticipantId, Response};
use tracing::{info, warn};

use super::Call;
use crate::room::ConnId;
use castcorder_transport::GuestId;

impl Call {
    /// Participant `from` offers participant `to` to become contacts.
    pub(super) fn offer_contact(
        &mut self,
        from: ConnId,
        to: ParticipantId,
        code: &str,
    ) -> Response {
        let refuse = |why: &str| {
            crate::room::decision(&from.to_string(), "guest", "offer contact", false, why);
            Response::Error {
                kind: ErrorKind::NotAllowed,
                message: why.to_string(),
            }
        };
        let Some(mine) = self.person_in_call(from) else {
            return refuse("only a participant linked to a person can add contacts");
        };
        let Some(theirs) = self.person_in_call(to.0) else {
            return refuse("that participant is not linked to a person");
        };
        if mine == theirs {
            return refuse("that participant is the same person");
        }
        // A link to it (its fragment the code) or the code itself.
        let fragment = code.rsplit('#').next().unwrap_or_default();
        if ContactCode::parse(fragment).is_err() {
            return refuse("not a contact code");
        }
        let name = self.names.get(&GuestId(from)).cloned().unwrap_or_default();
        let event = Event::ContactOffer {
            from: ParticipantId(from),
            name,
            code: code.to_string(),
        };
        let sent = self
            .rpc
            .get_mut(&GuestId(to.0))
            .is_some_and(|rpc| rpc.emit(event).is_ok());
        if sent {
            info!(from, to = to.0, "contact offer passed on");
            Response::Done
        } else {
            warn!(to = to.0, "contact offer not passed on: channel closed");
            refuse("that participant is not reachable")
        }
    }
}
