//! The X side: starting a viewer and finding its window, pressing keys through XTEST, and
//! timing the damage DAMAGE reports on the window.

use std::collections::HashMap;
use std::error::Error;
use std::hash::{DefaultHasher, Hasher};
use std::process::{Child, Command};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, TryRecvError, channel};
use std::thread;
use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::damage::{ConnectionExt as _, ReportLevel};
use x11rb::protocol::xproto::{
    ChangeWindowAttributesAux, ConnectionExt as _, EventMask, ImageFormat, InputFocus,
    KEY_PRESS_EVENT, KEY_RELEASE_EVENT, Window,
};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;
use x11rb::{CURRENT_TIME, NONE};

use crate::keys::Keymap;
use crate::memory;
use crate::settle::{self, Frame, Outcome};

pub type Res<T> = Result<T, Box<dyn Error>>;

/// How long a window may take to appear, a start to settle, or a frame to stay quiet once its
/// damage began, before the run stops: long past any frame worth measuring.
const CAP_MS: f64 = 120_000.0;

/// The server's keyboard mapping.
pub fn keymap(conn: &impl Connection) -> Res<Keymap> {
    let setup = conn.setup();
    let count = setup.max_keycode - setup.min_keycode + 1;
    let reply = conn
        .get_keyboard_mapping(setup.min_keycode, count)?
        .reply()?;
    Ok(Keymap {
        min_keycode: setup.min_keycode,
        per: usize::from(reply.keysyms_per_keycode),
        keysyms: reply.keysyms,
    })
}

fn since(start: Instant, t: Instant) -> f64 {
    t.saturating_duration_since(start).as_secs_f64() * 1000.0
}

/// A connection to the display in `DISPLAY` that watches the root window for new windows. A
/// thread reads the events and stamps each with the time it came in.
pub struct Display {
    conn: Arc<RustConnection>,
    root: Window,
    events: Receiver<(Instant, Event)>,
    keymap: Keymap,
}

/// A viewer the display started, and the window it maps. Dropped, it is killed, so a run that
/// stops on an error leaves no viewer behind.
pub struct Viewer {
    pub child: Child,
    window: Window,
}

impl Viewer {
    /// The peak memory of the viewer and its live descendants, in MiB. A viewer that ended
    /// fails the case, with its status.
    pub fn peak_mib(&mut self) -> Res<f64> {
        if let Some(status) = self.child.try_wait()? {
            return Err(format!("the viewer ended during the case: {status}").into());
        }
        Ok(memory::peak_mib(self.child.id())?)
    }
}

impl Drop for Viewer {
    fn drop(&mut self) {
        // a viewer that quit already makes these fail, which is fine
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Display {
    pub fn open() -> Res<Self> {
        let (conn, screen) = x11rb::connect(None)?;
        let root = conn.setup().roots[screen].root;
        conn.damage_query_version(1, 1)?.reply()?;
        conn.xtest_get_version(2, 2)?.reply()?;
        let watch = ChangeWindowAttributesAux::new().event_mask(EventMask::SUBSTRUCTURE_NOTIFY);
        conn.change_window_attributes(root, &watch)?.check()?;
        let keymap = keymap(&conn)?;
        let conn = Arc::new(conn);
        let (sender, events) = channel();
        let reader = Arc::clone(&conn);
        thread::spawn(move || {
            while let Ok(event) = reader.wait_for_event() {
                if sender.send((Instant::now(), event)).is_err() {
                    break;
                }
            }
        });
        Ok(Display {
            conn,
            root,
            events,
            keymap,
        })
    }

    fn drain(&self) {
        while self.events.try_recv().is_ok() {}
    }

    /// Starts the viewer and waits for its first frame. Its window is the first top-level window
    /// that is created and mapped after the start, so nothing else may map one meanwhile. The
    /// window gets the keyboard's focus, which needs no window manager. Returns the viewer, the
    /// time from the start to the settled first frame, and the count of late damage after it.
    pub fn start(&self, command: &mut Command, settle: f64) -> Res<(Viewer, Frame, usize)> {
        self.drain();
        let start = Instant::now();
        let mut viewer = Viewer {
            child: command.spawn()?,
            window: NONE,
        };
        let mut created = HashMap::new();
        let mut events = Vec::new();
        let window = loop {
            if since(start, Instant::now()) > CAP_MS {
                return Err("the viewer mapped no window".into());
            }
            let (t, event) = match self.events.recv_timeout(Duration::from_millis(100)) {
                Ok(next) => next,
                Err(RecvTimeoutError::Timeout) => {
                    if let Some(status) = viewer.child.try_wait()? {
                        return Err(
                            format!("the viewer ended before its window showed: {status}").into(),
                        );
                    }
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => return Err("the X connection closed".into()),
            };
            match event {
                // shortcut: the damage object is made once the CreateNotify arrives, when the
                // viewer may have mapped and drawn already; a viewer that draws once, at once,
                // reads as drawing nothing, which stops the run. Redirect the root's substructure
                // and make it before the map is passed on, where a viewer does that.
                Event::CreateNotify(c) if c.parent == self.root && !c.override_redirect => {
                    let id = self.conn.generate_id()?;
                    self.conn
                        .damage_create(id, c.window, ReportLevel::RAW_RECTANGLES)?;
                    self.conn.flush()?;
                    created.insert(c.window, id);
                }
                Event::MapNotify(m) if created.contains_key(&m.window) => break m.window,
                event @ Event::DamageNotify(_) => events.push((t, event)),
                _ => {}
            }
        };
        self.conn
            .set_input_focus(InputFocus::PARENT, window, CURRENT_TIME)?;
        self.conn.flush()?;
        viewer.window = window;
        let mut damage = Vec::new();
        let late = sort(&events, window, start, &mut damage);
        let (first, after) = self.collect(&viewer, start, damage, settle, CAP_MS)?;
        Ok((
            viewer,
            first.ok_or("the viewer's window drew nothing")?,
            late + after,
        ))
    }

    /// Presses and releases a key and returns the time the press went out.
    fn send(&self, name: &str) -> Res<Instant> {
        let codes = self
            .keymap
            .press(name)
            .ok_or_else(|| format!("no key types {name}"))?;
        for &code in &codes {
            self.conn
                .xtest_fake_input(KEY_PRESS_EVENT, code, CURRENT_TIME, NONE, 0, 0, 0)?;
        }
        for &code in codes.iter().rev() {
            self.conn
                .xtest_fake_input(KEY_RELEASE_EVENT, code, CURRENT_TIME, NONE, 0, 0, 0)?;
        }
        let pressed = Instant::now();
        self.conn.flush()?;
        Ok(pressed)
    }

    /// Presses a key and waits for its frame: None when it drew nothing within `limit`. Then
    /// the window must stay quiet for one more `settle`, which is no part of the frame, so a
    /// step drawn later than the settle time shows instead of landing in the next key's frame.
    /// Also returns the count of late damage: from before the press, after the frame, and in
    /// that wait.
    pub fn key(
        &self,
        viewer: &Viewer,
        name: &str,
        settle: f64,
        limit: f64,
    ) -> Res<(Option<Frame>, usize)> {
        let pressed = self.send(name)?;
        let (frame, late) = self.collect(viewer, pressed, Vec::new(), settle, limit)?;
        Ok((frame, late + self.quiet(viewer, settle)?))
    }

    /// Waits until the window has drawn nothing for `settle`, and returns the count of damage
    /// it drew meanwhile, which is late.
    pub fn quiet(&self, viewer: &Viewer, settle: f64) -> Res<usize> {
        let start = Instant::now();
        let mut last = start;
        let mut late = 0;
        loop {
            let now = Instant::now();
            if since(start, now) > CAP_MS {
                return Err(format!("the window did not stay quiet for {settle} ms").into());
            }
            let wait = settle - since(last, now);
            if wait <= 0.0 {
                return Ok(late);
            }
            match self
                .events
                .recv_timeout(Duration::from_secs_f64(wait / 1000.0))
            {
                Ok((t, Event::DamageNotify(d))) if d.drawable == viewer.window => {
                    late += 1;
                    last = last.max(t);
                }
                Ok(_) | Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return Err("the X connection closed".into()),
            }
        }
    }

    fn collect(
        &self,
        viewer: &Viewer,
        start: Instant,
        mut damage: Vec<f64>,
        settle: f64,
        limit: f64,
    ) -> Res<(Option<Frame>, usize)> {
        let mut late = 0;
        let mut events = Vec::new();
        loop {
            // everything queued is taken in before deciding, so no damage waits behind it
            loop {
                match self.events.try_recv() {
                    Ok(event) => events.push(event),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => return Err("the X connection closed".into()),
                }
            }
            late += sort(&events, viewer.window, start, &mut damage);
            events.clear();
            let now = since(start, Instant::now());
            match settle::decide(&damage, now, settle, limit, CAP_MS) {
                Some((Outcome::Frame(frame), after)) => return Ok((Some(frame), late + after)),
                Some((Outcome::Nothing, after)) => return Ok((None, late + after)),
                Some((Outcome::Unsettled, _)) => {
                    return Err(format!(
                        "the window did not stay quiet for {settle} ms in {} s",
                        CAP_MS / 1000.0
                    )
                    .into());
                }
                None => {}
            }
            let wait = settle::next_check(&damage, settle, limit, CAP_MS) - now;
            match self
                .events
                .recv_timeout(Duration::from_secs_f64(wait.max(0.0) / 1000.0))
            {
                Ok(event) => events.push(event),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return Err("the X connection closed".into()),
            }
        }
    }

    /// A hash of the window's pixels.
    pub fn digest(&self, viewer: &Viewer) -> Res<u64> {
        let geometry = self.conn.get_geometry(viewer.window)?.reply()?;
        let image = self
            .conn
            .get_image(
                ImageFormat::Z_PIXMAP,
                viewer.window,
                0,
                0,
                geometry.width,
                geometry.height,
                !0,
            )?
            .reply()?;
        let mut hasher = DefaultHasher::new();
        hasher.write(&image.data);
        Ok(hasher.finish())
    }

    /// Asks the viewer to quit with `q`. A viewer that has not quit within 10 seconds is killed,
    /// and that, or any exit but a clean one, fails the run.
    pub fn quit(&self, mut viewer: Viewer) -> Res<()> {
        self.send("q")?;
        let asked = Instant::now();
        let status = loop {
            if let Some(status) = viewer.child.try_wait()? {
                break status;
            }
            if asked.elapsed() > Duration::from_secs(10) {
                return Err("the viewer did not quit on q".into());
            }
            thread::sleep(Duration::from_millis(20));
        };
        if !status.success() {
            return Err(format!("the viewer did not quit cleanly on q: {status}").into());
        }
        Ok(())
    }
}

/// Sorts events that came in for a frame that began at `start`: damage of `window` from then on
/// goes to `damage`, in milliseconds since `start`. Returns the count of its damage from before
/// `start`, which the frame before left and is late. Other events count for nothing.
fn sort(
    events: &[(Instant, Event)],
    window: Window,
    start: Instant,
    damage: &mut Vec<f64>,
) -> usize {
    let mut late = 0;
    for (t, event) in events {
        match event {
            Event::DamageNotify(d) if d.drawable == window && *t < start => late += 1,
            Event::DamageNotify(d) if d.drawable == window => damage.push(since(start, *t)),
            _ => {}
        }
    }
    late
}

#[cfg(test)]
mod tests {
    use super::*;
    use x11rb::protocol::damage::NotifyEvent;
    use x11rb::protocol::xproto::{MapNotifyEvent, Rectangle};

    fn damage(drawable: Window) -> Event {
        let area = Rectangle {
            x: 0,
            y: 0,
            width: 1,
            height: 1,
        };
        Event::DamageNotify(NotifyEvent {
            response_type: 0,
            level: ReportLevel::RAW_RECTANGLES,
            sequence: 0,
            drawable,
            damage: 0,
            timestamp: 0,
            area,
            geometry: area,
        })
    }

    #[test]
    fn damage_before_the_key_is_late_and_other_windows_or_events_count_for_nothing() {
        let start = Instant::now() + Duration::from_secs(1);
        let at = |ms: i64| match ms {
            0.. => start + Duration::from_millis(ms as u64),
            _ => start - Duration::from_millis(ms.unsigned_abs()),
        };
        let map = Event::MapNotify(MapNotifyEvent {
            response_type: 0,
            sequence: 0,
            event: 1,
            window: 7,
            override_redirect: false,
        });
        let events = [
            (at(-20), damage(7)),
            (at(-1), damage(7)),
            (at(-5), damage(8)),
            (at(3), damage(7)),
            (at(4), damage(9)),
            (at(5), map),
            (at(180), damage(7)),
        ];
        let mut times = vec![];
        assert_eq!(sort(&events, 7, start, &mut times), 2);
        assert_eq!(times.len(), 2);
        assert!(
            (times[0] - 3.0).abs() < 1e-9 && (times[1] - 180.0).abs() < 1e-9,
            "{times:?}"
        );
    }
}
