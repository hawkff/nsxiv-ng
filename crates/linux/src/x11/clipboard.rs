//! CLIPBOARD, which the window owns for an edit's copy: a PNG as `image/png`, or text as
//! `UTF8_STRING` and `text/plain;charset=utf-8`, given to each client that asks, as ICCCM says.
//! Data larger than one request takes goes INCR, a piece each time the client deletes the last.

use std::rc::Rc;

use x11rb::connection::Connection;
use x11rb::errors::{ConnectionError, ReplyError};
use x11rb::protocol::xproto::{
    AtomEnum, ChangeWindowAttributesAux, ConnectionExt as _, EventMask, PropMode, Property,
    PropertyNotifyEvent, SELECTION_NOTIFY_EVENT, SelectionNotifyEvent, SelectionRequestEvent,
    Timestamp,
};
use x11rb::wrapper::ConnectionExt as _;

use super::Atoms;

/// What the window offers while it owns CLIPBOARD.
struct Offer {
    data: Rc<[u8]>,
    image: bool,
}

/// Data going INCR to a client's property, a piece at a time.
struct Transfer {
    requestor: u32,
    property: u32,
    target: u32,
    data: Rc<[u8]>,
    sent: usize,
    /// The last, empty, piece went, which ends the transfer.
    done: bool,
}

impl Transfer {
    /// The next piece of at most `max` bytes, then an empty one, then None.
    fn next(&mut self, max: usize) -> Option<&[u8]> {
        if self.done {
            return None;
        }
        let end = (self.sent + max).min(self.data.len());
        let piece = &self.data[self.sent..end];
        self.done = piece.is_empty();
        self.sent = end;
        Some(piece)
    }
}

/// CLIPBOARD as the window holds it.
#[derive(Default)]
pub(super) struct Clipboard {
    offer: Option<Offer>,
    transfers: Vec<Transfer>,
    /// The window that owns CLIPBOARD, and the time it owns it from.
    window: u32,
    since: Timestamp,
}

/// The most bytes of data one ChangeProperty takes: the server's largest request less the
/// request's own 24 and the 4 of a big request's length.
fn most(conn: &impl Connection) -> usize {
    conn.maximum_request_bytes().saturating_sub(28).max(1)
}

impl Clipboard {
    /// The window owns CLIPBOARD from `time` on, offering `data`, a PNG where `image` says so,
    /// else UTF-8 text. False where the server gave it another, as one that took it at a later
    /// time, and the window offers nothing then.
    pub(super) fn own(
        &mut self,
        conn: &impl Connection,
        atoms: &Atoms,
        window: u32,
        time: Timestamp,
        data: Vec<u8>,
        image: bool,
    ) -> Result<bool, ReplyError> {
        self.offer = None;
        conn.set_selection_owner(window, atoms.CLIPBOARD, time)?;
        if conn.get_selection_owner(atoms.CLIPBOARD)?.reply()?.owner != window {
            return Ok(false);
        }
        (self.window, self.since) = (window, time);
        self.offer = Some(Offer {
            data: data.into(),
            image,
        });
        Ok(true)
    }

    /// A request the window answers with no data: one of its own, or one of a time before it
    /// owned CLIPBOARD, which is for the owner then; times wrap round, so before is within half
    /// their range.
    fn refused(&self, request: &SelectionRequestEvent) -> bool {
        request.requestor == self.window || self.before_owned(request.time)
    }

    /// Whether `time` is before the window owned CLIPBOARD, where both are known.
    fn before_owned(&self, time: Timestamp) -> bool {
        time != x11rb::CURRENT_TIME
            && self.since != x11rb::CURRENT_TIME
            && (time.wrapping_sub(self.since) as i32) < 0
    }

    /// Another client took CLIPBOARD at `time`; transfers under way go on. A clear of before
    /// the window owned it, queued while it took it again, is of an ownership gone.
    pub(super) fn lost(&mut self, time: Timestamp) {
        if !self.before_owned(time) {
            self.offer = None;
        }
    }

    /// A client asks for CLIPBOARD as `target`: TARGETS, or one of the types offered, which
    /// goes INCR where it is larger than a request; anything else is refused.
    pub(super) fn request(
        &mut self,
        conn: &impl Connection,
        atoms: &Atoms,
        request: &SelectionRequestEvent,
    ) -> Result<(), ConnectionError> {
        let (requestor, target) = (request.requestor, request.target);
        // a client of before ICCCM names no property, and gets the target's
        let property = match request.property {
            0 => target,
            property => property,
        };
        // a refusal where the data could not be written, so the client waits for nothing
        let given = !self.refused(request)
            && (self.give(conn, atoms, requestor, property, target)).unwrap_or(false);
        let given = if given {
            property
        } else {
            AtomEnum::NONE.into()
        };
        let notify = SelectionNotifyEvent {
            response_type: SELECTION_NOTIFY_EVENT,
            sequence: 0,
            time: request.time,
            requestor,
            selection: request.selection,
            target,
            property: given,
        };
        conn.send_event(false, requestor, EventMask::NO_EVENT, notify)?;
        Ok(())
    }

    /// Writes what `target` asks for to the client's `property`: TARGETS, or the data, at once
    /// or INCR. False for a target not offered.
    fn give(
        &mut self,
        conn: &impl Connection,
        atoms: &Atoms,
        requestor: u32,
        property: u32,
        target: u32,
    ) -> Result<bool, ConnectionError> {
        let types = match &self.offer {
            Some(offer) if offer.image => vec![atoms.IMAGE_PNG],
            Some(_) => vec![atoms.UTF8_STRING, atoms.TEXT_PLAIN_UTF8],
            None => Vec::new(),
        };
        let given = match &self.offer {
            Some(_) if target == atoms.TARGETS => {
                let mut targets = vec![atoms.TARGETS];
                targets.extend(&types);
                conn.change_property32(
                    PropMode::REPLACE,
                    requestor,
                    property,
                    AtomEnum::ATOM,
                    &targets,
                )?;
                true
            }
            Some(offer) if types.contains(&target) => {
                let data = Rc::clone(&offer.data);
                if data.len() <= most(conn) {
                    conn.change_property8(PropMode::REPLACE, requestor, property, target, &data)?;
                } else {
                    // the client's deletes of the property ask for each piece, and its window's
                    // end ends the transfer
                    let mask = EventMask::PROPERTY_CHANGE | EventMask::STRUCTURE_NOTIFY;
                    let aux = ChangeWindowAttributesAux::new().event_mask(mask);
                    conn.change_window_attributes(requestor, &aux)?;
                    let length = [u32::try_from(data.len()).unwrap_or(u32::MAX)];
                    conn.change_property32(
                        PropMode::REPLACE,
                        requestor,
                        property,
                        atoms.INCR,
                        &length,
                    )?;
                    self.transfers
                        .retain(|t| (t.requestor, t.property) != (requestor, property));
                    self.transfers.push(Transfer {
                        requestor,
                        property,
                        target,
                        data,
                        sent: 0,
                        done: false,
                    });
                }
                true
            }
            _ => false,
        };
        Ok(given)
    }

    /// A property changed on a window the clipboard watches: a transfer's property deleted
    /// asks for its next piece. False where the change is none of its own.
    // shortcut: a client whose window lives on and that never reads the rest keeps its
    // transfer, and the data, until the viewer quits; drop a transfer the client left for some
    // seconds if that shows
    pub(super) fn property(
        &mut self,
        conn: &impl Connection,
        event: &PropertyNotifyEvent,
    ) -> Result<bool, ConnectionError> {
        let Some(at) = (self.transfers.iter())
            .position(|t| (t.requestor, t.property) == (event.window, event.atom))
        else {
            return Ok(false);
        };
        if event.state != Property::DELETE {
            return Ok(true);
        }
        let max = most(conn);
        let transfer = &mut self.transfers[at];
        let (requestor, property, target) =
            (transfer.requestor, transfer.property, transfer.target);
        let written = match transfer.next(max) {
            Some(piece) => conn
                .change_property8(PropMode::REPLACE, requestor, property, target, piece)
                .map(drop),
            None => Ok(()),
        };
        // a piece that could not be written ends the transfer
        if (transfer.done || written.is_err()) && self.end(at).is_some() {
            let aux = ChangeWindowAttributesAux::new().event_mask(EventMask::NO_EVENT);
            conn.change_window_attributes(requestor, &aux)?;
        }
        Ok(true)
    }

    /// A client's window went, as its DestroyNotify or an error naming it says: its transfers
    /// end. False where it had none.
    pub(super) fn destroyed(&mut self, window: u32) -> bool {
        let before = self.transfers.len();
        self.transfers.retain(|t| t.requestor != window);
        self.transfers.len() < before
    }

    /// Ends transfer `at`, and gives its client's window where no other transfer to it runs,
    /// which needs its events no more: the mask is the window's, not a property's.
    fn end(&mut self, at: usize) -> Option<u32> {
        let window = self.transfers.remove(at).requestor;
        (!self.transfers.iter().any(|t| t.requestor == window)).then_some(window)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_transfer_goes_in_pieces_then_an_empty_one() {
        let mut transfer = Transfer {
            requestor: 1,
            property: 2,
            target: 3,
            data: (0..10).collect::<Vec<u8>>().into(),
            sent: 0,
            done: false,
        };
        let mut pieces = Vec::new();
        while let Some(piece) = transfer.next(4) {
            pieces.push(piece.to_vec());
        }
        assert_eq!(
            pieces,
            [vec![0, 1, 2, 3], vec![4, 5, 6, 7], vec![8, 9], vec![]]
        );
        assert!(transfer.done);
    }

    fn transfer(requestor: u32, property: u32) -> Transfer {
        Transfer {
            requestor,
            property,
            target: 3,
            data: vec![0; 4].into(),
            sent: 0,
            done: false,
        }
    }

    #[test]
    fn a_request_of_the_window_itself_or_of_before_it_owned_is_refused() {
        let clipboard = Clipboard {
            window: 7,
            since: 1000,
            ..Clipboard::default()
        };
        let asked = |requestor, time| SelectionRequestEvent {
            requestor,
            time,
            ..SelectionRequestEvent::default()
        };
        assert!(!clipboard.refused(&asked(8, 1000)));
        assert!(!clipboard.refused(&asked(8, 5000)));
        assert!(!clipboard.refused(&asked(8, x11rb::CURRENT_TIME)));
        assert!(clipboard.refused(&asked(8, 999)), "before");
        assert!(clipboard.refused(&asked(7, 5000)), "its own");
        // past the wrap, a time after one near the end of the range is small
        let late = Clipboard {
            window: 7,
            since: u32::MAX - 10,
            ..Clipboard::default()
        };
        assert!(!late.refused(&asked(8, 5)));
        assert!(late.refused(&asked(8, u32::MAX - 11)));
    }

    #[test]
    fn a_clear_of_before_the_window_owned_leaves_the_offer() {
        let mut clipboard = Clipboard {
            offer: Some(Offer {
                data: vec![1].into(),
                image: true,
            }),
            window: 7,
            since: 1000,
            ..Clipboard::default()
        };
        clipboard.lost(999);
        assert!(clipboard.offer.is_some(), "an older clear");
        clipboard.lost(1000);
        assert!(clipboard.offer.is_none());
    }

    #[test]
    fn a_window_gone_ends_its_transfers_alone() {
        let mut clipboard = Clipboard {
            transfers: vec![transfer(1, 10), transfer(2, 10), transfer(1, 11)],
            ..Clipboard::default()
        };
        assert!(clipboard.destroyed(1));
        let left: Vec<u32> = clipboard.transfers.iter().map(|t| t.requestor).collect();
        assert_eq!(left, [2]);
        assert!(!clipboard.destroyed(1), "none left");
    }

    #[test]
    fn a_window_is_watched_until_its_last_transfer_ends() {
        let mut clipboard = Clipboard {
            transfers: vec![transfer(1, 10), transfer(2, 10), transfer(1, 11)],
            ..Clipboard::default()
        };
        assert_eq!(clipboard.end(0), None, "another to window 1 runs");
        assert_eq!(clipboard.end(1), Some(1));
        assert_eq!(clipboard.end(0), Some(2));
    }
}
