//! sway's placement of the viewer's floating window, through its IPC. sway keeps a floating
//! window's top-left corner when the window resizes itself and sends no bounds, so a growing
//! window ran off the screen. Its IPC does what the macOS side does: the window keeps its
//! center, stays inside its workspace, and grows no larger than the workspace leaves room for.

use std::process::{Command, Stdio};

use nsxiv_ng_core::{Rect, Size};
use serde_json::Value;

/// What sway must do for the viewer's window after it resized itself.
#[derive(Debug, PartialEq)]
pub(super) struct Placement {
    /// The command that moves the window's frame, where it must move.
    pub command: Option<String>,
    /// The room the window's content has in its workspace, sway's title bar and borders left
    /// out, in logical units.
    pub room: Size,
}

/// Moves the floating window of process `pid`, which went from `old` to `new` in logical
/// units, through `swaymsg`, where it `moves`; the room its content has in its workspace, None
/// where the window is not a floating one or is fullscreen, or sway did not answer.
pub(super) fn place(pid: u32, old: (u32, u32), new: (u32, u32), moves: bool) -> Option<Size> {
    let tree = swaymsg(&["-t", "get_tree"])?;
    let placement = placement(&tree, pid, old, new)?;
    if moves && let Some(command) = &placement.command {
        swaymsg(&[command]);
    }
    Some(placement.room)
}

/// The placement of the window of process `pid` in sway's `tree`, which went from `old` to
/// `new` in logical units: its center kept, inside its workspace.
pub(super) fn placement(
    tree: &Value,
    pid: u32,
    old: (u32, u32),
    new: (u32, u32),
) -> Option<Placement> {
    let (node, space) = find(tree, pid, None)?;
    if node["type"] != "floating_con" || node["fullscreen_mode"].as_i64().unwrap_or(0) != 0 {
        return None;
    }
    let id = node["id"].as_i64()?;
    let below = rect(&node["rect"])?;
    let content = rect(&node["window_rect"])?;
    // like i3, sway's rect leaves out the title bar, which deco_rect describes
    let title = rect(&node["deco_rect"]).map_or(0.0, |deco| deco.height);
    let frame = Rect {
        y: below.y - title,
        height: below.height + title,
        ..below
    };
    // sway's title bar and borders; the frame's corner is still where it was before the resize
    let deco = (frame.width - content.width, frame.height - content.height);
    let outer = (f64::from(new.0) + deco.0, f64::from(new.1) + deco.1);
    let center = (
        frame.x + (f64::from(old.0) + deco.0) / 2.0,
        frame.y + (f64::from(old.1) + deco.1) / 2.0,
    );
    let x = space
        .x
        .max((center.0 - outer.0 / 2.0).min(space.max_x() - outer.0)) as i64;
    let y = space
        .y
        .max((center.1 - outer.1 / 2.0).min(space.max_y() - outer.1)) as i64;
    let moved = x != frame.x as i64 || y != frame.y as i64;
    Some(Placement {
        command: moved.then(|| format!("[con_id={id}] move absolute position {x} {y}")),
        room: Size {
            width: space.width - deco.0,
            height: space.height - deco.1,
        },
    })
}

// The node of process `pid`, with the rect of the workspace it is on.
fn find(node: &Value, pid: u32, workspace: Option<Rect>) -> Option<(&Value, Rect)> {
    let is_workspace = node["type"] == "workspace";
    // hidden scratchpad windows sit on a stand-in workspace that spans every output
    if is_workspace
        && node["name"]
            .as_str()
            .is_some_and(|name| name.starts_with("__i3"))
    {
        return None;
    }
    let workspace = if is_workspace {
        rect(&node["rect"])
    } else {
        workspace
    };
    if node["pid"].as_u64() == Some(u64::from(pid))
        && let Some(workspace) = workspace
    {
        return Some((node, workspace));
    }
    let children = |key: &str| node[key].as_array().into_iter().flatten();
    children("nodes")
        .chain(children("floating_nodes"))
        .find_map(|child| find(child, pid, workspace))
}

fn rect(value: &Value) -> Option<Rect> {
    let side = |key: &str| value[key].as_i64().map(|n| n as f64);
    Some(Rect {
        x: side("x")?,
        y: side("y")?,
        width: side("width")?,
        height: side("height")?,
    })
}

// sway's own client, which speaks its IPC at the socket SWAYSOCK names; None when it does not
// start or exits with an error.
fn swaymsg(args: &[&str]) -> Option<Value> {
    let output = Command::new("swaymsg")
        .arg("-r")
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| serde_json::from_slice(&output.stdout).ok())
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    // a workspace of 1280x800 with the viewer's floating window at 600,300 below a title bar
    // of 20 and in borders of 2, a tiled window, and the scratchpad's hidden window
    const TREE: &str = r#"{
        "type": "root", "rect": {"x": 0, "y": 0, "width": 1280, "height": 800},
        "nodes": [{
            "type": "output", "name": "HEADLESS-1",
            "rect": {"x": 0, "y": 0, "width": 1280, "height": 800},
            "nodes": [{
                "type": "workspace", "name": "1",
                "rect": {"x": 0, "y": 0, "width": 1280, "height": 800},
                "nodes": [{
                    "type": "con", "id": 5, "pid": 41, "fullscreen_mode": 0,
                    "rect": {"x": 0, "y": 0, "width": 640, "height": 800},
                    "window_rect": {"x": 0, "y": 0, "width": 640, "height": 800},
                    "deco_rect": {"x": 0, "y": 0, "width": 0, "height": 0}
                }],
                "floating_nodes": [{
                    "type": "floating_con", "id": 7, "pid": 42, "fullscreen_mode": 0,
                    "rect": {"x": 600, "y": 300, "width": 404, "height": 324},
                    "window_rect": {"x": 2, "y": 0, "width": 400, "height": 322},
                    "deco_rect": {"x": 0, "y": 0, "width": 404, "height": 20}
                }]
            }]
        }, {
            "type": "output", "name": "__i3",
            "nodes": [{
                "type": "workspace", "name": "__i3_scratch",
                "rect": {"x": 0, "y": 0, "width": 2560, "height": 800},
                "floating_nodes": [{
                    "type": "floating_con", "id": 9, "pid": 43, "fullscreen_mode": 0,
                    "rect": {"x": 0, "y": 0, "width": 100, "height": 100},
                    "window_rect": {"x": 0, "y": 0, "width": 100, "height": 100}
                }]
            }]
        }]
    }"#;

    fn tree() -> Value {
        serde_json::from_str(TREE).unwrap()
    }

    const ROOM: Size = Size {
        width: 1276.0,
        height: 778.0,
    };

    #[test]
    fn a_growing_window_keeps_its_center_inside_its_workspace() {
        // the frame is 404x344 at 600,280 with its title bar, so its center is at 802,452
        let placed = placement(&tree(), 42, (400, 322), (800, 600)).unwrap();
        assert_eq!(
            placed,
            Placement {
                command: Some("[con_id=7] move absolute position 400 141".to_owned()),
                room: ROOM,
            }
        );
        // grown past the bottom-right edge from its center, it moves back inside
        let placed = placement(&tree(), 42, (400, 322), (1000, 700)).unwrap();
        assert_eq!(
            placed.command.as_deref(),
            Some("[con_id=7] move absolute position 276 78")
        );
        // larger than the workspace, its corner stays at the workspace's
        let placed = placement(&tree(), 42, (400, 322), (2000, 1000)).unwrap();
        assert_eq!(
            placed.command.as_deref(),
            Some("[con_id=7] move absolute position 0 0")
        );
    }

    #[test]
    fn a_window_whose_frame_stays_is_not_moved() {
        let placed = placement(&tree(), 42, (400, 322), (400, 322)).unwrap();
        assert_eq!(placed.command, None);
        assert_eq!(placed.room, ROOM, "the room all the same");
    }

    #[test]
    fn only_a_floating_window_of_the_viewer_that_is_not_fullscreen_is_placed() {
        assert_eq!(
            placement(&tree(), 41, (640, 800), (700, 800)),
            None,
            "tiled"
        );
        assert_eq!(
            placement(&tree(), 43, (100, 100), (200, 200)),
            None,
            "hidden in the scratchpad"
        );
        assert_eq!(
            placement(&tree(), 44, (100, 100), (200, 200)),
            None,
            "not in the tree"
        );
        let fullscreen = TREE.replacen(
            r#""id": 7, "pid": 42, "fullscreen_mode": 0"#,
            r#""id": 7, "pid": 42, "fullscreen_mode": 1"#,
            1,
        );
        let fullscreen: Value = serde_json::from_str(&fullscreen).unwrap();
        assert_eq!(placement(&fullscreen, 42, (400, 322), (800, 600)), None);
    }
}
