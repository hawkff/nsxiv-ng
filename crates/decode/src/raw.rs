//! Camera RAW by its embedded JPEG preview alone: no sensor data is decoded. The search walks
//! the TIFF directories of CR2, NEF, ARW, DNG, PEF, ORF, RW2, SRW and ERF files, MakerNotes
//! included, reads CR3's preview boxes and RAF's header, and takes the JPEG that serves best.
//!
//! The offsets come from untrusted files, so every read is checked against the file's length,
//! nothing is allocated by a field's size before that check, and the directories and boxes
//! visited are bounded, so a loop of offsets ends.

use std::collections::HashSet;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use image::metadata::Orientation;

use crate::DecodeError;

/// How a RAW file keeps its previews.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Container {
    /// TIFF directories, behind a TIFF header, or ORF's or RW2's own marks.
    Tiff,
    /// CR3's ISO boxes.
    Cr3,
    /// RAF's header.
    Raf,
}

/// The extensions of the RAW files built on TIFF, which a TIFF header alone does not tell.
const TIFF_EXTENSIONS: [&str; 12] = [
    "cr2", "nef", "nrw", "arw", "srf", "sr2", "dng", "pef", "srw", "erf", "orf", "rw2",
];

/// The RAW container the file's start shows, or its name where the start is a plain TIFF
/// header: CR3's brand, RAF's magic, and ORF's and RW2's byte-order marks tell by content, as
/// does CR2's mark after its TIFF header.
pub(crate) fn detect(start: &[u8], path: &Path) -> Option<Container> {
    if start.get(4..12) == Some(b"ftypcrx ") {
        return Some(Container::Cr3);
    }
    if start.starts_with(b"FUJIFILMCCD-RAW") {
        return Some(Container::Raf);
    }
    if [b"IIRO", b"IIRS", b"MMOR", b"IIU\0"]
        .iter()
        .any(|mark| start.starts_with(*mark))
    {
        return Some(Container::Tiff);
    }
    let tiff = start.starts_with(b"II*\0") || start.starts_with(b"MM\0*");
    // CR2's mark: CR and its version 2 after the header, the first directory past them
    let cr2 = start.starts_with(b"II*\0")
        && start.get(8..11) == Some(b"CR\x02")
        && start.get(4..8).is_some_and(|first| {
            u32::from_le_bytes([first[0], first[1], first[2], first[3]]) >= 16
        });
    let named = path.extension().and_then(|e| e.to_str()).is_some_and(|e| {
        TIFF_EXTENSIONS
            .iter()
            .any(|raw| e.eq_ignore_ascii_case(raw))
    });
    (cr2 || tiff && named).then_some(Container::Tiff)
}

/// A RAW file's preview: the JPEG's bytes, and the orientation the RAW gives, which wins over
/// any the JPEG carries.
pub(crate) struct Preview {
    pub jpeg: Vec<u8>,
    pub orientation: Option<Orientation>,
}

/// The preview of the RAW file `reader` holds: with `side` 0 the largest embedded JPEG, else
/// the one [`covering`] picks. Its bytes count against `max_bytes` as a JPEG file's do.
pub(crate) fn preview(
    reader: &mut (impl Read + Seek),
    container: Container,
    side: u32,
    max_bytes: u64,
) -> Result<Preview, DecodeError> {
    let len = reader
        .seek(SeekFrom::End(0))
        .map_err(|e| DecodeError::Unreadable(e.to_string()))?;
    let mut search = Search {
        file: Bounded { reader, len },
        visited: HashSet::new(),
        found: Vec::new(),
        orientation: None,
    };
    match container {
        Container::Tiff => search.tiff_file(),
        Container::Cr3 => search.boxes(0, len, 0),
        Container::Raf => search.raf(),
    }
    let Search {
        mut file,
        found,
        orientation,
        ..
    } = search;
    // the JPEGs that are baseline or progressive, with their sizes; a lossless one is raw data
    let mut previews: Vec<((u32, u32), u64, u64)> = found
        .into_iter()
        .filter_map(|(at, length)| Some((jpeg_size(&mut file, at, length)?, at, length)))
        .collect();
    previews.sort_by_key(|&((w, h), _, length)| (u64::from(w) * u64::from(h), length));
    let Some(&(largest, ..)) = previews.last() else {
        return Err(DecodeError::NoPreview);
    };
    let sizes: Vec<(u32, u32)> = previews.iter().map(|&(size, ..)| size).collect();
    let chosen = (side > 0)
        .then(|| covering(largest, side, &sizes))
        .flatten()
        .unwrap_or(previews.len() - 1);
    let (_, at, length) = previews[chosen];
    if length > max_bytes {
        return Err(DecodeError::TooLarge { size: None });
    }
    let mut jpeg = file
        .bytes(at, length)
        .ok_or_else(|| DecodeError::Unreadable("a preview past the file's end".to_owned()))?;
    // the start of image, which Epson writes as EE D8
    jpeg[0] = 0xff;
    Ok(Preview {
        jpeg,
        orientation: orientation.and_then(|o| Orientation::from_exif(o.min(255) as u8)),
    })
}

/// Which of the preview sizes is the smallest that covers `side` with its longer dimension and
/// has the shape of the largest, `image`, to within 1%, so a preview cropped or letterboxed
/// otherwise never stands in for it: a 4:3 one beside a 3:2 image is 12.5% off. The HEIC
/// helper's `smallest_covering` asks for the shape to within a pixel; cameras round their
/// previews to standard sizes, as the Nikon Z 6's 1620x1080 beside 6048x4024, a pixel and a
/// half off, which that rule would refuse.
fn covering((width, height): (u32, u32), side: u32, sizes: &[(u32, u32)]) -> Option<usize> {
    let shaped = |w: u32, h: u32| {
        let (across, down) = (
            u64::from(w) * u64::from(height),
            u64::from(h) * u64::from(width),
        );
        across.abs_diff(down) <= across / 100
    };
    sizes
        .iter()
        .enumerate()
        .filter(|&(_, &(w, h))| w.max(h) >= side && shaped(w, h))
        .min_by_key(|&(_, &(w, h))| u64::from(w) * u64::from(h))
        .map(|(i, _)| i)
}

/// The file, read only within its length.
struct Bounded<'a, R> {
    reader: &'a mut R,
    len: u64,
}

impl<R: Read + Seek> Bounded<'_, R> {
    /// `length` bytes at `at`, or None past the file's end.
    fn bytes(&mut self, at: u64, length: u64) -> Option<Vec<u8>> {
        if at.checked_add(length)? > self.len {
            return None;
        }
        self.reader.seek(SeekFrom::Start(at)).ok()?;
        let mut bytes = vec![0; usize::try_from(length).ok()?];
        self.reader.read_exact(&mut bytes).ok()?;
        Some(bytes)
    }
}

/// Directories at most, over all the file's TIFF structures, and how deep they nest.
const MAX_DIRECTORIES: usize = 64;
const MAX_DEPTH: usize = 6;
/// Entries a directory may have, and boxes a CR3 level, far past what cameras write.
const MAX_ENTRIES: u64 = 1000;
const MAX_BOXES: usize = 256;
/// Markers read in a JPEG before its frame header, and the most sub-directories an entry lists.
const MAX_MARKERS: usize = 64;
/// Places a JPEG may lie that the search keeps, over the whole file; real files have fewer
/// than ten, and each costs a few reads to check.
const MAX_CANDIDATES: usize = 32;
const MAX_SUB_DIRECTORIES: u32 = 16;

/// Where the offsets of one TIFF structure count from, and its byte order.
#[derive(Clone, Copy)]
struct Tiff {
    base: u64,
    big: bool,
}

impl Tiff {
    fn u16(self, bytes: &[u8]) -> u16 {
        let bytes = [bytes[0], bytes[1]];
        if self.big {
            u16::from_be_bytes(bytes)
        } else {
            u16::from_le_bytes(bytes)
        }
    }

    fn u32(self, bytes: &[u8]) -> u32 {
        let bytes = [bytes[0], bytes[1], bytes[2], bytes[3]];
        if self.big {
            u32::from_be_bytes(bytes)
        } else {
            u32::from_le_bytes(bytes)
        }
    }

    /// The structure's offset `offset` in the file.
    fn at(self, offset: u32) -> u64 {
        self.base + u64::from(offset)
    }
}

/// Whose MakerNote a directory is, for the tags only that maker uses.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Maker {
    None,
    Nikon,
    Olympus,
    Pentax,
}

/// One directory entry: its tag, type, count, and the four bytes of its value or offset.
struct Entry {
    tag: u16,
    kind: u16,
    count: u32,
    value: [u8; 4],
}

struct Search<'a, R> {
    file: Bounded<'a, R>,
    /// The directories and boxes read, by their place in the file.
    visited: HashSet<u64>,
    /// Where a JPEG may lie, and how long it is.
    found: Vec<(u64, u64)>,
    orientation: Option<u32>,
}

impl<R: Read + Seek> Search<'_, R> {
    /// Notes that a JPEG may lie at `at`, `length` bytes long, as long as there is room.
    fn candidate(&mut self, (at, length): (u64, u64)) {
        if self.found.len() < MAX_CANDIDATES {
            self.found.push((at, length));
        }
    }

    /// A file that is a TIFF structure from its start.
    fn tiff_file(&mut self) {
        if let Some((tiff, first)) = self.header(0) {
            self.directory(tiff, first, 0, Maker::None);
        }
    }

    /// The byte order and the first directory's offset of a TIFF header at `at`, as TIFF,
    /// ORF and RW2 write it: two order marks, two bytes of their own, and the offset.
    fn header(&mut self, at: u64) -> Option<(Tiff, u32)> {
        let header = self.file.bytes(at, 8)?;
        let big = match &header[..2] {
            b"II" => false,
            b"MM" => true,
            _ => return None,
        };
        let tiff = Tiff { base: at, big };
        Some((tiff, tiff.u32(&header[4..])))
    }

    /// Walks the directory at `offset` and the ones it leads to, noting JPEGs.
    fn directory(&mut self, tiff: Tiff, offset: u32, depth: usize, maker: Maker) {
        let mut offset = offset;
        // the chain of directories that follow one another
        while offset != 0 {
            let at = tiff.at(offset);
            if depth > MAX_DEPTH
                || self.visited.len() >= MAX_DIRECTORIES
                || !self.visited.insert(at)
            {
                return;
            }
            let Some(count) = self.file.bytes(at, 2).map(|b| u64::from(tiff.u16(&b))) else {
                return;
            };
            if count > MAX_ENTRIES {
                return;
            }
            let Some(table) = self.file.bytes(at + 2, count * 12 + 4) else {
                return;
            };
            let (rows, next) = table.split_at(table.len() - 4);
            let entries: Vec<Entry> = rows
                .as_chunks::<12>()
                .0
                .iter()
                .map(|row| Entry {
                    tag: tiff.u16(&row[0..]),
                    kind: tiff.u16(&row[2..]),
                    count: tiff.u32(&row[4..]),
                    value: [row[8], row[9], row[10], row[11]],
                })
                .collect();
            // the file's first directory holds its orientation
            if self.visited.len() == 1 {
                self.orientation = number(tiff, &entries, 0x0112);
            }
            self.entries(tiff, &entries, depth, maker);
            offset = tiff.u32(next);
        }
    }

    // The JPEGs and the directories a directory's entries name.
    fn entries(&mut self, tiff: Tiff, entries: &[Entry], depth: usize, maker: Maker) {
        let value = |tag| number(tiff, entries, tag);
        // JPEGInterchangeFormat and its length, in a directory or Nikon's preview directory
        if let (Some(start), Some(length)) = (value(0x0201), value(0x0202)) {
            self.candidate((tiff.at(start), u64::from(length)));
        }
        // one strip of old-style or new-style JPEG, as CR2's and DNG's previews are
        if let (Some(6 | 7), Some(start), Some(length)) =
            (value(0x0103), value(0x0111), value(0x0117))
        {
            self.candidate((tiff.at(start), u64::from(length)));
        }
        let pairs = match maker {
            Maker::Olympus => Some((0x0101, 0x0102)),
            Maker::Pentax => Some((0x0004, 0x0003)),
            _ => None,
        };
        if let Some((start, length)) = pairs
            && let (Some(start), Some(length)) = (value(start), value(length))
        {
            self.candidate((tiff.at(start), u64::from(length)));
        }
        for entry in entries {
            let offset = tiff.u32(&entry.value);
            match (entry.tag, maker) {
                // a JPEG kept whole as a value: RW2's JpgFromRaw, Sony's and Epson's previews
                (0x002e | 0x2001 | 0x0280, _) if matches!(entry.kind, 1 | 7) => {
                    self.candidate((tiff.at(offset), u64::from(entry.count)));
                }
                // SubIFDs
                (0x014a, _) => {
                    for sub in self.offsets(tiff, entry) {
                        self.directory(tiff, sub, depth + 1, maker);
                    }
                }
                // the EXIF directory
                (0x8769, _) => self.directory(tiff, offset, depth + 1, Maker::None),
                (0x927c, _) => self.maker_note(tiff, tiff.at(offset), entry.count, depth + 1),
                // Nikon's preview directory, and Olympus's camera settings
                (0x0011, Maker::Nikon) | (0x2020, Maker::Olympus) => {
                    self.directory(tiff, offset, depth + 1, maker);
                }
                _ => {}
            }
        }
    }

    /// The offsets a LONG or IFD entry lists, at most a few.
    fn offsets(&mut self, tiff: Tiff, entry: &Entry) -> Vec<u32> {
        if !matches!(entry.kind, 4 | 13) || entry.count == 0 || entry.count > MAX_SUB_DIRECTORIES {
            return Vec::new();
        }
        if entry.count == 1 {
            return vec![tiff.u32(&entry.value)];
        }
        let at = tiff.at(tiff.u32(&entry.value));
        let Some(bytes) = self.file.bytes(at, u64::from(entry.count) * 4) else {
            return Vec::new();
        };
        bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|b| tiff.u32(b))
            .collect()
    }

    /// A MakerNote at `at`, `length` bytes long, by the header its maker writes.
    fn maker_note(&mut self, tiff: Tiff, at: u64, length: u32, depth: usize) {
        let Some(head) = self.file.bytes(at, u64::from(length.min(16))) else {
            return;
        };
        let order = |at: usize| match head.get(at..at + 2) {
            Some(b"MM") => Some(true),
            Some(b"II") => Some(false),
            _ => None,
        };
        // the directory's place relative to the MakerNote, from where its offsets count
        let own = |big: bool, first: u32| (Tiff { base: at, big }, first);
        let note = |skip: u64| (tiff, u32::try_from(at + skip - tiff.base).unwrap_or(0));
        let (tiff, first, maker) = if head.starts_with(b"Nikon\0") {
            // a TIFF structure of its own after ten bytes
            let Some((tiff, first)) = self.header(at + 10) else {
                return;
            };
            (tiff, first, Maker::Nikon)
        } else if head.starts_with(b"OLYMPUS\0") || head.starts_with(b"OM SYSTEM\0") {
            let mark = if head.starts_with(b"OLYMPUS\0") {
                8
            } else {
                12
            };
            let Some(big) = order(mark) else {
                return;
            };
            let (tiff, first) = own(big, mark as u32 + 4);
            (tiff, first, Maker::Olympus)
        } else if head.starts_with(b"OLYMP\0") {
            let (tiff, first) = note(8);
            (tiff, first, Maker::Olympus)
        } else if head.starts_with(b"AOC\0") {
            let big = order(4).unwrap_or(tiff.big);
            let (_, first) = note(6);
            (Tiff { big, ..tiff }, first, Maker::Pentax)
        } else if head.starts_with(b"PENTAX \0") {
            let Some(big) = order(8) else {
                return;
            };
            let (tiff, first) = own(big, 10);
            (tiff, first, Maker::Pentax)
        } else if head.starts_with(b"SONY DSC \0") || head.starts_with(b"SONY CAM \0") {
            let (tiff, first) = note(12);
            (tiff, first, Maker::None)
        } else if head.starts_with(b"EPSON\0") {
            let (tiff, first) = note(8);
            (tiff, first, Maker::None)
        } else {
            // a bare directory, as Canon's and older Sony bodies write it
            let (tiff, first) = note(0);
            (tiff, first, Maker::None)
        };
        self.directory(tiff, first, depth, maker);
    }

    // shortcut: PRVW's 1620x1080 is the largest preview read; the full-size JPEG CR3 keeps as
    // its first track needs the track's sample tables read, worth it once CR3 users ask
    /// The boxes between `from` and `to`, `depth` deep: CR3's CMT1 gives the orientation,
    /// and its THMB and PRVW boxes the previews.
    fn boxes(&mut self, from: u64, to: u64, depth: usize) {
        if depth > 3 {
            return;
        }
        let mut at = from;
        for _ in 0..MAX_BOXES {
            let Some(head) = self.file.bytes(at, 8).filter(|_| at + 8 <= to) else {
                return;
            };
            let size = u64::from(u32::from_be_bytes([head[0], head[1], head[2], head[3]]));
            let kind = [head[4], head[5], head[6], head[7]];
            let (header, size) = match size {
                0 => (8, to - at),
                1 => match self.file.bytes(at + 8, 8) {
                    Some(large) => (16, u64::from_be_bytes(large.try_into().unwrap_or([0; 8]))),
                    None => return,
                },
                size => (8, size),
            };
            let Some(end) = at
                .checked_add(size)
                .filter(|&end| end <= to && size >= header)
            else {
                return;
            };
            let payload = at + header;
            match &kind {
                b"moov" => self.boxes(payload, end, depth + 1),
                b"uuid" => {
                    let Some(uuid) = self.file.bytes(payload, 16) else {
                        return;
                    };
                    if uuid == CANON {
                        self.boxes(payload + 16, end, depth + 1);
                    } else if uuid == PREVIEW {
                        // eight bytes of its own before the PRVW box
                        self.boxes(payload + 24, end, depth + 1);
                    }
                }
                b"CMT1" => {
                    if let Some((tiff, first)) = self.header(payload) {
                        self.directory(tiff, first, 0, Maker::None);
                    }
                }
                b"THMB" | b"PRVW" => self.jpeg_within(payload, end),
                _ => {}
            }
            at = end;
        }
    }

    /// A JPEG that starts within the first bytes of a box's payload and runs to its end.
    fn jpeg_within(&mut self, from: u64, to: u64) {
        let Some(head) = self.file.bytes(from, (to - from).min(32)) else {
            return;
        };
        if let Some(start) = head.windows(3).position(|w| w == [0xff, 0xd8, 0xff]) {
            let at = from + start as u64;
            self.candidate((at, to - at));
        }
    }

    /// RAF's header names its JPEG's offset and length.
    fn raf(&mut self) {
        if let Some(fields) = self.file.bytes(84, 8) {
            let at = u32::from_be_bytes([fields[0], fields[1], fields[2], fields[3]]);
            let length = u32::from_be_bytes([fields[4], fields[5], fields[6], fields[7]]);
            self.candidate((u64::from(at), u64::from(length)));
        }
    }
}

/// The box that holds CR3's CMT boxes and THMB, and the one that holds PRVW.
const CANON: [u8; 16] = [
    0x85, 0xc0, 0xb6, 0x87, 0x82, 0x0f, 0x11, 0xe0, 0x81, 0x11, 0xf4, 0xce, 0x46, 0x2b, 0x6a, 0x48,
];
const PREVIEW: [u8; 16] = [
    0xea, 0xf4, 0x2b, 0x5e, 0x1c, 0x98, 0x4b, 0x88, 0xb9, 0xfb, 0xb7, 0xdc, 0x40, 0x6e, 0x4d, 0x16,
];

/// The number an entry of one SHORT or LONG holds.
fn number(tiff: Tiff, entries: &[Entry], tag: u16) -> Option<u32> {
    let entry = entries.iter().find(|e| e.tag == tag && e.count == 1)?;
    match entry.kind {
        3 => Some(u32::from(tiff.u16(&entry.value))),
        4 | 13 => Some(tiff.u32(&entry.value)),
        _ => None,
    }
}

/// The width and height of the JPEG `length` bytes at `at`, when it is one whose frame is
/// baseline or progressive; None for anything else, as the lossless JPEG of raw data.
fn jpeg_size<R: Read + Seek>(file: &mut Bounded<R>, at: u64, length: u64) -> Option<(u32, u32)> {
    let end = at.checked_add(length)?.min(file.len);
    // Epson's R-D1 stores its preview with the first byte EE, which ExifTool also mends
    if !matches!(file.bytes(at, 2)?[..], [0xff | 0xee, 0xd8]) {
        return None;
    }
    let mut position = at + 2;
    for _ in 0..MAX_MARKERS {
        if position + 4 > end {
            return None;
        }
        let marker = file.bytes(position, 4)?;
        if marker[0] != 0xff {
            return None;
        }
        let segment = u64::from(u16::from_be_bytes([marker[2], marker[3]]));
        match marker[1] {
            0xc0..=0xc2 => {
                let frame = file
                    .bytes(position + 4, 5)
                    .filter(|_| position + 9 <= end)?;
                let height = u32::from(u16::from_be_bytes([frame[1], frame[2]]));
                let width = u32::from(u16::from_be_bytes([frame[3], frame[4]]));
                return (width > 0 && height > 0).then_some((width, height));
            }
            // other frames, as lossless, and the scan or the end before any frame
            0xc3 | 0xc5..=0xc7 | 0xc9..=0xcb | 0xcd..=0xcf | 0xd8..=0xda => return None,
            // a fill byte, any number of which may come before a marker
            0xff => position += 1,
            _ => position += 2 + segment,
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fill_bytes_before_a_marker_are_passed_over() {
        // the start of image, an APP0 of two bytes, two fill bytes, then a baseline frame
        // header 16 high and 32 wide
        let head: [&[u8]; 4] = [
            &[0xff, 0xd8],
            &[0xff, 0xe0, 0x00, 0x04, 0x00, 0x00],
            &[0xff, 0xff],
            &[
                0xff, 0xc0, 0x00, 0x0b, 0x08, 0x00, 0x10, 0x00, 0x20, 0x01, 0x01, 0x11, 0x00,
            ],
        ];
        let jpeg = head.concat();
        let len = jpeg.len() as u64;
        let mut cursor = std::io::Cursor::new(jpeg);
        let mut file = Bounded {
            reader: &mut cursor,
            len,
        };
        assert_eq!(jpeg_size(&mut file, 0, len), Some((32, 16)));
    }

    #[test]
    fn a_file_of_many_candidates_keeps_a_few() {
        // a directory of 1000 JPEGs as values, each pointing at the header
        let mut file = b"II*\0\x08\0\0\0".to_vec();
        file.extend_from_slice(&1000u16.to_le_bytes());
        for _ in 0..1000 {
            file.extend_from_slice(&0x002eu16.to_le_bytes());
            file.extend_from_slice(&7u16.to_le_bytes());
            file.extend_from_slice(&8u32.to_le_bytes());
            file.extend_from_slice(&0u32.to_le_bytes());
        }
        file.extend_from_slice(&[0; 4]);
        let len = file.len() as u64;
        let mut cursor = std::io::Cursor::new(file);
        let mut search = Search {
            file: Bounded {
                reader: &mut cursor,
                len,
            },
            visited: HashSet::new(),
            found: Vec::new(),
            orientation: None,
        };
        search.tiff_file();
        assert_eq!(search.found.len(), MAX_CANDIDATES);
    }

    #[test]
    fn the_smallest_preview_of_the_image_s_shape_that_covers_the_side_serves() {
        // a 3:2 image with a 3:2 preview of 1620 and a letterboxed 4:3 one of 160
        let sizes = [(160, 120), (1620, 1080), (6000, 4000)];
        let image = (6000, 4000);
        assert_eq!(
            covering(image, 100, &sizes),
            Some(1),
            "not the one of another shape"
        );
        assert_eq!(covering(image, 1620, &sizes), Some(1));
        assert_eq!(covering(image, 1621, &sizes), Some(2));
        assert_eq!(covering(image, 7000, &sizes), None, "none covers it");
        // previews rounded to standard sizes are taken: the Nikon Z 6's 1620x1080, 0.2% off,
        // and its 640x424, 0.4% off; the D850's 640x424, 0.6% off
        let z6 = [(640, 424), (1620, 1080), (6048, 4024)];
        assert_eq!(covering((6048, 4024), 700, &z6), Some(1));
        assert_eq!(covering((6048, 4024), 256, &z6), Some(0));
        let d850 = [(640, 424), (1620, 1080), (8256, 5504)];
        assert_eq!(covering((8256, 5504), 256, &d850), Some(0));
        // the letterboxed 160x120 beside a 3:2 image is not
        assert_eq!(
            covering((8256, 5504), 100, &[(160, 120), (8256, 5504)]),
            Some(1)
        );
    }
}
