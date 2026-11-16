//! The calibration's viewer: an X client that repaints a known time after each key.

use std::hint::black_box;
use std::io::Read;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{
    ChangeGCAux, ConnectionExt as _, CreateGCAux, CreateWindowAux, EventMask, Rectangle,
    WindowClass,
};

use crate::keys::{HYPER_R, SHIFT_L};
use crate::x::{Res, keymap};

const WIDTH: u16 = 640;
const HEIGHT: u16 = 480;

fn number(arg: Option<&String>) -> Res<u64> {
    Ok(arg
        .ok_or("paint needs DELAY_MS STEPS MIB CHILD_MIB")?
        .parse()?)
}

/// `paint DELAY_MS STEPS MIB CHILD_MIB`: holds MIB of memory, starts a child that holds
/// CHILD_MIB, and maps a window. On each key it waits DELAY_MS and fills the window; with two
/// steps it first fills a strip at the bottom at once, as a viewer draws its bar before the
/// image. `x` draws nothing and `q` quits.
pub fn run(args: &[String]) -> Res<()> {
    let delay = Duration::from_millis(number(args.first())?);
    let steps = number(args.get(1))?;
    let held = black_box(vec![1u8; usize::try_from(number(args.get(2))?)? << 20]);
    // the child ends when its stdin closes, with this process
    let _child = Command::new(std::env::current_exe()?)
        .args(["hold", &number(args.get(3))?.to_string()])
        .stdin(Stdio::piped())
        .spawn()?;

    let (conn, screen) = x11rb::connect(None)?;
    let screen = &conn.setup().roots[screen];
    let keymap = keymap(&conn)?;
    let window = conn.generate_id()?;
    let attributes = CreateWindowAux::new()
        .background_pixel(screen.white_pixel)
        .event_mask(EventMask::KEY_PRESS | EventMask::EXPOSURE);
    conn.create_window(
        x11rb::COPY_DEPTH_FROM_PARENT,
        window,
        screen.root,
        0,
        0,
        WIDTH,
        HEIGHT,
        0,
        WindowClass::INPUT_OUTPUT,
        x11rb::COPY_FROM_PARENT,
        &attributes,
    )?;
    let gc = conn.generate_id()?;
    conn.create_gc(gc, window, &CreateGCAux::new())?;
    conn.map_window(window)?;
    conn.flush()?;

    let mut color = 0x0033_6699;
    let fill = |color: u32, y: i16, height: u16| -> Res<()> {
        conn.change_gc(gc, &ChangeGCAux::new().foreground(color))?;
        let area = Rectangle {
            x: 0,
            y,
            width: WIDTH,
            height,
        };
        conn.poly_fill_rectangle(window, gc, &[area])?;
        conn.flush()?;
        Ok(())
    };
    loop {
        match conn.wait_for_event()? {
            Event::Expose(e) if e.count == 0 => fill(color, 0, HEIGHT)?,
            Event::KeyPress(key) => match keymap.keysym(key.detail) {
                0x71 => break, // q
                0x78 => {}     // x
                SHIFT_L..=HYPER_R => {}
                _ => {
                    if steps == 2 {
                        fill(0x00ee_eeee ^ color, (HEIGHT - 20) as i16, 20)?;
                    }
                    thread::sleep(delay);
                    color ^= 0x00ff_ffff;
                    fill(color, 0, HEIGHT)?;
                }
            },
            _ => {}
        }
    }
    drop(held);
    Ok(())
}

/// `hold MIB`: holds MIB of memory until stdin closes.
pub fn hold(args: &[String]) -> Res<()> {
    let mib: usize = args.first().ok_or("hold needs MIB")?.parse()?;
    let held = black_box(vec![1u8; mib << 20]);
    std::io::stdin().read_to_end(&mut Vec::new())?;
    drop(held);
    Ok(())
}
