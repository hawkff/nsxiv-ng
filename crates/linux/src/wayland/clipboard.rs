//! The clipboard as the Wayland window offers it: a data source on the seat's data device for
//! an edit's copy, and the data written into each reader's pipe as the pipe takes it.

use std::io::{self, Write};

/// What a PNG is offered as.
pub(super) const IMAGE: [&str; 1] = ["image/png"];
/// What text is offered as, all of them UTF-8.
pub(super) const TEXT: [&str; 3] = ["text/plain;charset=utf-8", "UTF8_STRING", "text/plain"];

/// The most bytes one wakeup writes into a reader's pipe: a MiB, some 16 of a pipe's 64 KiB, a
/// millisecond or so of copying, so a reader as fast as the viewer still leaves room between for
/// the frames and the input; the pipe, writable still, wakes the loop again for the rest.
pub(super) const PER_WAKEUP: usize = 1 << 20;

/// What a write into a reader's pipe left.
#[derive(Debug, PartialEq)]
pub(super) enum Fed {
    /// All the data is in the pipe.
    Done,
    /// The pipe is full, or this wakeup wrote its share; the rest goes in the next.
    More,
    /// The reader closed its end, or the pipe failed: the rest goes nowhere.
    Gone,
}

/// Writes `data` from `sent` on into `pipe` until it would block or `share` bytes went, moving
/// `sent` on.
pub(super) fn feed(pipe: &mut impl Write, data: &[u8], sent: &mut usize, share: usize) -> Fed {
    let end = sent.saturating_add(share).min(data.len());
    while *sent < data.len() {
        if *sent >= end {
            return Fed::More;
        }
        match pipe.write(&data[*sent..end]) {
            Ok(0) => return Fed::Gone,
            Ok(written) => *sent += written,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Fed::More,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return Fed::Gone,
        }
    }
    Fed::Done
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn a_pipe_takes_what_it_can_then_the_rest_and_a_closed_one_ends_the_write() {
        let (mut reader, mut writer) = io::pipe().unwrap();
        rustix::fs::fcntl_setfl(&writer, rustix::fs::OFlags::NONBLOCK).unwrap();
        // more than a pipe holds: it takes some, and would block
        let data: Vec<u8> = (0..1 << 20).map(|i| i as u8).collect();
        let mut sent = 0;
        assert_eq!(feed(&mut writer, &data, &mut sent, usize::MAX), Fed::More);
        assert!(sent > 0 && sent < data.len());
        // read as it is written, it all goes
        let mut read = Vec::new();
        let mut buffer = vec![0; 1 << 16];
        loop {
            let n = reader.read(&mut buffer).unwrap();
            read.extend_from_slice(&buffer[..n]);
            if feed(&mut writer, &data, &mut sent, usize::MAX) == Fed::Done {
                break;
            }
        }
        drop(writer);
        reader.read_to_end(&mut read).unwrap();
        assert!(read == data);
        // a reader gone: the write ends, with no signal to the viewer
        let (reader, mut writer) = io::pipe().unwrap();
        rustix::fs::fcntl_setfl(&writer, rustix::fs::OFlags::NONBLOCK).unwrap();
        drop(reader);
        let mut sent = 0;
        assert_eq!(feed(&mut writer, &data, &mut sent, usize::MAX), Fed::Gone);
    }

    #[test]
    fn a_wakeup_writes_its_share_and_leaves_the_rest_for_the_next() {
        // a reader that takes everything at once
        let mut taken = Vec::new();
        let data = vec![7; 2 * PER_WAKEUP + 10];
        let mut sent = 0;
        let fed: Vec<_> = (0..3)
            .map(|_| feed(&mut taken, &data, &mut sent, PER_WAKEUP))
            .collect();
        assert_eq!(fed, [Fed::More, Fed::More, Fed::Done]);
        assert_eq!(taken.len(), data.len());
    }
}
