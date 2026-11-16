//! Decoding helpers: nsxiv-ng itself, run with [`SERVE`], in a process the kernel sandboxes,
//! for the formats whose decoders a crafted file can crash or stall: SVG, and HEIC and AVIF
//! through libheif. The viewer opens each file and passes it over a socket, as a helper cannot
//! open files, and reads back pixels whose size it checks. A helper that crashes, or runs past
//! the timeout, ends, and the next file starts another. On macOS the sandbox is Seatbelt, and
//! HEIC and AVIF start no helper: ImageIO decodes them in the viewer's own process.

#[cfg(target_os = "linux")]
mod heif;
#[cfg(target_os = "macos")]
mod imageio;
mod serve;

pub use serve::{probe, serve};

use std::fs::File;
use std::io::{self, ErrorKind, IoSlice, Read, Write};
use std::mem::MaybeUninit;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use nsxiv_ng_decode::{DecodeError, DecodeShare, Image};
use rustix::io::Errno;
use rustix::net::{SendAncillaryBuffer, SendAncillaryMessage, SendFlags};

/// The flag that runs nsxiv-ng as a decoding helper, followed by the memory cap in bytes.
pub const SERVE: &str = "--decode-helper";
/// The flag that runs nsxiv-ng as a probe of the helpers' sandbox, for `-v`.
pub const PROBE: &str = "--decode-helper-probe";

// The first byte of an answer says which it is.
const IMAGE: u8 = 0;
const TOO_LARGE: u8 = 1;
const UNREADABLE: u8 = 2;
// no file name or error message runs longer
const MAX_TEXT: usize = 4096;
/// The flags a request goes with. Darwin has no MSG_NOSIGNAL; a Rust program ignores SIGPIPE
/// from its start, so a send to a helper whose socket closed gives EPIPE there all the same.
#[cfg(target_os = "linux")]
const SEND_FLAGS: SendFlags = SendFlags::NOSIGNAL;
#[cfg(target_os = "macos")]
const SEND_FLAGS: SendFlags = SendFlags::empty();

/// A pool of decoding helpers.
pub struct Helpers {
    program: PathBuf,
    count: usize,
    cap: u64,
    timeout: Duration,
    pool: Mutex<Pool>,
    free: Condvar,
}

#[derive(Default)]
struct Pool {
    idle: Vec<Helper>,
    /// Helpers alive, idle or decoding.
    started: usize,
}

struct Helper {
    child: Child,
    socket: UnixStream,
}

impl Helpers {
    /// Helpers that run as `program SERVE cap`, `count` of them at most, each started when a
    /// file first needs it. A decode that runs past `timeout` ends its helper.
    pub fn new(program: PathBuf, count: usize, cap: u64, timeout: Duration) -> Helpers {
        Helpers {
            program,
            count: count.max(1),
            cap,
            timeout,
            pool: Mutex::default(),
            free: Condvar::new(),
        }
    }

    /// Decodes the file in a helper when its format needs one, and in this process otherwise.
    pub fn decode(&self, path: &Path, max_bytes: u64) -> Result<Image, DecodeError> {
        self.run(path, max_bytes, 0, None)
    }

    /// [`decode`](Self::decode) for a thumbnail that must cover `side` pixels: on Linux a HEIC
    /// or AVIF file gives the smallest thumbnail embedded in it that covers the side and has
    /// the image's aspect ratio, where it has one; on macOS its image scaled by ImageIO so its
    /// longer side is the side, where that is smaller; any other file gives its whole image. A
    /// decode in this process, HEIC and AVIF on macOS among them, waits for room in the budget
    /// of `share` and takes it there; one in a helper does not.
    pub fn thumbnail(
        &self,
        path: &Path,
        side: u32,
        max_bytes: u64,
        share: Option<&DecodeShare>,
    ) -> Result<Image, DecodeError> {
        self.run(path, max_bytes, side.max(1), share)
    }

    fn run(
        &self,
        path: &Path,
        max_bytes: u64,
        side: u32,
        share: Option<&DecodeShare>,
    ) -> Result<Image, DecodeError> {
        let mut file = File::open(path).map_err(unreadable)?;
        let helped = nsxiv_ng_decode::needs_helper(&mut file, path).map_err(unreadable)?;
        if helped.is_none() {
            // refusing the helper formats, as a file may have turned into one since the check
            return nsxiv_ng_decode::decode_here(file, path, max_bytes, side, share);
        }
        #[cfg(target_os = "macos")]
        if helped == Some(nsxiv_ng_decode::Helped::Heif) {
            return imageio::decode(file, max_bytes, side, share);
        }
        let mut helper = self.take()?;
        match helper.decode(file, path, max_bytes.min(self.cap), side, self.timeout) {
            Ok(answer) => {
                self.give_back(helper);
                answer
            }
            // the helper crashed, ran out of time or broke the protocol, so it ends
            Err(why) => {
                drop(helper);
                self.lost();
                Err(DecodeError::Unreadable(why))
            }
        }
    }

    // An idle helper, a new one while fewer than `count` live, or else the next one freed.
    fn take(&self) -> Result<Helper, DecodeError> {
        let mut pool = self.pool.lock().unwrap();
        loop {
            while let Some(mut helper) = pool.idle.pop() {
                // one that ended while idle, as the kernel's CPU limit ends them, goes
                if matches!(helper.child.try_wait(), Ok(None)) {
                    return Ok(helper);
                }
                pool.started -= 1;
            }
            if pool.started < self.count {
                pool.started += 1;
                drop(pool);
                return self.start().map_err(|e| {
                    self.lost();
                    DecodeError::Unreadable(format!("no decoding process: {e}"))
                });
            }
            pool = self.free.wait(pool).unwrap();
        }
    }

    fn give_back(&self, helper: Helper) {
        self.pool.lock().unwrap().idle.push(helper);
        self.free.notify_one();
    }

    fn lost(&self) {
        self.pool.lock().unwrap().started -= 1;
        self.free.notify_one();
    }

    fn start(&self) -> io::Result<Helper> {
        let (socket, theirs) = UnixStream::pair()?;
        let child = Command::new(&self.program)
            .arg(SERVE)
            .arg(self.cap.to_string())
            .stdin(Stdio::from(OwnedFd::from(theirs)))
            .stdout(Stdio::null())
            // no terminal to type into, nor to fill with a crash's report
            .stderr(Stdio::null())
            .spawn()?;
        Ok(Helper { child, socket })
    }
}

impl Helper {
    // Sends the file and reads the answer; Err when the helper failed rather than the file.
    fn decode(
        &mut self,
        file: File,
        path: &Path,
        max_bytes: u64,
        side: u32,
        timeout: Duration,
    ) -> Result<Result<Image, DecodeError>, String> {
        let name = path.file_name().map_or(&[][..], |name| name.as_bytes());
        let name = &name[..name.len().min(MAX_TEXT)];
        let mut request = Vec::with_capacity(16 + name.len());
        request.extend_from_slice(&max_bytes.to_le_bytes());
        request.extend_from_slice(&side.to_le_bytes());
        request.extend_from_slice(&(name.len() as u32).to_le_bytes());
        request.extend_from_slice(name);
        // one deadline for the whole exchange, so an answer dripped in pieces runs out too
        // shortcut: on macOS nothing caps a helper's memory; polling its footprint with
        // proc_pid_rusage while it decodes, and ending it past the cap, would, once a file is
        // seen to make a helper there take the machine's memory before its timeout
        let deadline = Instant::now() + timeout;
        // macOS refuses a timeout on a socket whose other end closed with EINVAL: a helper that
        // is gone, as the send would find on Linux
        let gone = |e: &io::Error| {
            cfg!(target_os = "macos") && e.raw_os_error() == Some(Errno::INVAL.raw_os_error())
        };
        self.socket.set_write_timeout(Some(timeout)).map_err(|e| {
            failure(if gone(&e) {
                ErrorKind::BrokenPipe.into()
            } else {
                e
            })
        })?;
        let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(1))];
        let mut control = SendAncillaryBuffer::new(&mut space);
        let fds = [file.as_fd()];
        control.push(SendAncillaryMessage::ScmRights(&fds));
        let sent = rustix::net::sendmsg(
            &self.socket,
            &[IoSlice::new(&request)],
            &mut control,
            SEND_FLAGS,
        )
        .map_err(|e| failure(e.into()))?;
        self.socket.write_all(&request[sent..]).map_err(failure)?;
        drop(file);
        let mut answer = Deadline {
            socket: &mut self.socket,
            by: deadline,
        };
        read_answer(&mut answer, max_bytes).map_err(failure)
    }
}

// Reads from the socket that end, together, by a deadline.
struct Deadline<'a> {
    socket: &'a mut UnixStream,
    by: Instant,
}

impl Read for Deadline<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let left = self.by.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(ErrorKind::TimedOut.into());
        }
        self.socket.set_read_timeout(Some(left))?;
        self.socket.read(buf)
    }
}

impl Drop for Helper {
    fn drop(&mut self) {
        // ends a helper stuck in a decode as well as an idle one
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// The helper's answer. An Err is a helper that failed or broke the protocol, as one that
// sends more pixels than the cap holds.
fn read_answer(socket: &mut impl Read, max_bytes: u64) -> io::Result<Result<Image, DecodeError>> {
    let mut tag = [0; 1];
    socket.read_exact(&mut tag)?;
    let mut head = [0; 9];
    match tag[0] {
        IMAGE => {
            socket.read_exact(&mut head)?;
            let (width, height) = dimensions(&head[..8]);
            // in u128, so dimensions no image has cannot wrap past the check
            let bytes = u128::from(width) * u128::from(height) * 4;
            if bytes > u128::from(max_bytes) {
                return Err(io::Error::other("more pixels than the cap holds"));
            }
            let mut rgba = vec![0; usize::try_from(bytes).map_err(io::Error::other)?];
            socket.read_exact(&mut rgba)?;
            Ok(Ok(Image {
                width,
                height,
                rgba,
                may_animate: head[8] != 0,
            }))
        }
        TOO_LARGE => {
            socket.read_exact(&mut head)?;
            let size = (head[0] != 0).then(|| dimensions(&head[1..]));
            Ok(Err(DecodeError::TooLarge { size }))
        }
        UNREADABLE => {
            let mut length = [0; 4];
            socket.read_exact(&mut length)?;
            let length = u32::from_le_bytes(length) as usize;
            if length > MAX_TEXT {
                return Err(io::Error::other("a message too long"));
            }
            let mut text = vec![0; length];
            socket.read_exact(&mut text)?;
            Ok(Err(DecodeError::Unreadable(
                String::from_utf8_lossy(&text).into_owned(),
            )))
        }
        _ => Err(io::Error::other("an answer of no known kind")),
    }
}

fn dimensions(bytes: &[u8]) -> (u32, u32) {
    let word = |at: usize| u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
    (word(0), word(4))
}

fn failure(e: io::Error) -> String {
    match e.kind() {
        ErrorKind::WouldBlock | ErrorKind::TimedOut => "the decoder ran out of time".to_owned(),
        ErrorKind::UnexpectedEof | ErrorKind::BrokenPipe | ErrorKind::ConnectionReset => {
            "the decoder crashed".to_owned()
        }
        _ => format!("the decoder failed: {e}"),
    }
}

fn unreadable(e: io::Error) -> DecodeError {
    DecodeError::Unreadable(e.to_string())
}

/// The sandbox's status line, from a probe run as `program PROBE`.
pub fn sandbox_status(program: &Path) -> String {
    match Command::new(program).arg(PROBE).output() {
        Ok(output) => String::from_utf8_lossy(&output.stdout).trim().to_owned(),
        Err(e) => format!("sandbox: none (no probe ran: {e})"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // macOS has no SOCK_CLOEXEC, so a pair made while another test spawns a child can leave
    // its other end open in that child: the tests that make a pair and spawn take turns
    static PAIRS: Mutex<()> = Mutex::new(());

    #[test]
    fn a_request_to_a_helper_whose_socket_closed_answers_that_the_decoder_crashed() {
        let _turn = PAIRS.lock().unwrap_or_else(|e| e.into_inner());
        let (ours, theirs) = UnixStream::pair().unwrap();
        drop(theirs);
        let child = Command::new("sleep").arg("5").spawn().unwrap();
        let mut helper = Helper {
            child,
            socket: ours,
        };
        let file = File::open(env!("CARGO_MANIFEST_DIR")).unwrap();
        let answer = helper.decode(file, Path::new("a.svg"), 1 << 20, 0, Duration::from_secs(1));
        assert_eq!(answer.err().as_deref(), Some("the decoder crashed"));
    }

    #[test]
    fn a_request_sent_to_a_socket_whose_other_end_closed_is_epipe_not_a_signal() {
        // a signal would end this test's process, failing the run
        let _turn = PAIRS.lock().unwrap_or_else(|e| e.into_inner());
        let (ours, theirs) = UnixStream::pair().unwrap();
        drop(theirs);
        let file = File::open(env!("CARGO_MANIFEST_DIR")).unwrap();
        let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(1))];
        let mut control = SendAncillaryBuffer::new(&mut space);
        let fds = [file.as_fd()];
        control.push(SendAncillaryMessage::ScmRights(&fds));
        let sent = rustix::net::sendmsg(&ours, &[IoSlice::new(b"a")], &mut control, SEND_FLAGS);
        assert_eq!(sent, Err(Errno::PIPE));
    }

    #[test]
    fn an_answer_dripped_in_pieces_runs_out_of_time() {
        let _turn = PAIRS.lock().unwrap_or_else(|e| e.into_inner());
        let (mut ours, mut theirs) = UnixStream::pair().unwrap();
        // a 1x1 image, a byte at a time, slower than the deadline allows in all
        let mut answer = vec![IMAGE];
        answer.extend_from_slice(&1u32.to_le_bytes());
        answer.extend_from_slice(&1u32.to_le_bytes());
        answer.extend_from_slice(&[0, 255, 0, 0, 255]);
        let dripper = std::thread::spawn(move || {
            for byte in answer {
                if theirs.write_all(&[byte]).is_err() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(150));
            }
        });
        let mut reads = Deadline {
            socket: &mut ours,
            by: Instant::now() + Duration::from_millis(600),
        };
        let error = read_answer(&mut reads, 1 << 20).unwrap_err();
        assert_eq!(failure(error), "the decoder ran out of time");
        drop(ours);
        dripper.join().unwrap();
    }
}
