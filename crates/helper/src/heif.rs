//! HEIC and AVIF through libheif, which only a helper loads, with dlopen, before it locks
//! down: the viewer starts without libheif installed, and never maps it.
//!
//! libheif turns the image as its HEIF transformations (irot, imir) say, the one orientation
//! path; EXIF orientation inside a HEIF file goes unread.

use std::ffi::{CStr, c_char, c_int, c_void};
use std::fs::File;
use std::io::Read;
use std::ptr;
use std::sync::OnceLock;

use nsxiv_ng_decode::{DecodeError, Image};

// heif_colorspace_RGB, heif_chroma_interleaved_RGBA and heif_channel_interleaved
const RGB: c_int = 1;
const RGBA: c_int = 11;
const INTERLEAVED: c_int = 10;
// heif_color_profile_type: 'nclx', 'rICC' and 'prof' as fourcc numbers
const NCLX: c_int = 0x6e63_6c78;
const RICC: c_int = 0x7249_4343;
const PROF: c_int = 0x7072_6f66;
// heif_error_Memory_allocation_error, heif_error_Plugin_loading_error, and
// heif_suberror_Security_limit_exceeded
const MEMORY: c_int = 6;
const PLUGIN_LOADING: c_int = 11;
const SECURITY_LIMIT: c_int = 1000;

#[repr(C)]
struct HeifError {
    code: c_int,
    subcode: c_int,
    message: *const c_char,
}

// heif_color_profile_nclx, its version 1 fields
#[repr(C)]
struct Nclx {
    _version: u8,
    color_primaries: c_int,
    transfer_characteristics: c_int,
    _matrix_coefficients: c_int,
    _full_range_flag: u8,
    _decoded: [f32; 8],
}

type Opaque = *mut c_void;

struct Libheif {
    version: unsafe extern "C" fn() -> *const c_char,
    context_alloc: unsafe extern "C" fn() -> Opaque,
    context_free: unsafe extern "C" fn(Opaque),
    set_maximum_size: unsafe extern "C" fn(Opaque, c_int),
    read_from_memory:
        unsafe extern "C" fn(Opaque, *const c_void, usize, *const c_void) -> HeifError,
    primary_image: unsafe extern "C" fn(Opaque, *mut Opaque) -> HeifError,
    handle_width: unsafe extern "C" fn(Opaque) -> c_int,
    handle_height: unsafe extern "C" fn(Opaque) -> c_int,
    premultiplied: unsafe extern "C" fn(Opaque) -> c_int,
    profile_type: unsafe extern "C" fn(Opaque) -> c_int,
    raw_profile_size: unsafe extern "C" fn(Opaque) -> usize,
    raw_profile: unsafe extern "C" fn(Opaque, *mut c_void) -> HeifError,
    nclx_profile: unsafe extern "C" fn(Opaque, *mut *mut Nclx) -> HeifError,
    nclx_free: unsafe extern "C" fn(*mut Nclx),
    decode: unsafe extern "C" fn(Opaque, *mut Opaque, c_int, c_int, *const c_void) -> HeifError,
    image_width: unsafe extern "C" fn(Opaque, c_int) -> c_int,
    image_height: unsafe extern "C" fn(Opaque, c_int) -> c_int,
    plane: unsafe extern "C" fn(Opaque, c_int, *mut c_int) -> *const u8,
    image_release: unsafe extern "C" fn(Opaque),
    handle_release: unsafe extern "C" fn(Opaque),
    thumbnail_count: unsafe extern "C" fn(Opaque) -> c_int,
    thumbnail_ids: unsafe extern "C" fn(Opaque, *mut u32, c_int) -> c_int,
    thumbnail: unsafe extern "C" fn(Opaque, u32, *mut Opaque) -> HeifError,
    /// A codec plugin libheif found could not be loaded.
    plugin_failed: bool,
}

static LIBHEIF: OnceLock<Option<Libheif>> = OnceLock::new();

/// Loads libheif and its codec plugins, which a locked-down helper cannot open, and gives
/// its version, with a note where a plugin did not load, or None where libheif is not
/// installed.
pub(crate) fn load() -> Option<String> {
    let lib = LIBHEIF.get_or_init(open).as_ref()?;
    // SAFETY: heif_get_version returns a static string that libheif owns
    let version = unsafe { CStr::from_ptr((lib.version)()) };
    let version = version.to_string_lossy();
    Some(match lib.plugin_failed {
        true => format!("{version}, a plugin did not load"),
        false => version.into_owned(),
    })
}

fn open() -> Option<Libheif> {
    // SAFETY: dlopen of a library by its soname; its constructors run as they would at link time
    let library =
        unsafe { libc::dlopen(c"libheif.so.1".as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
    if library.is_null() {
        return None;
    }
    // Each symbol becomes the function type libheif's header declares for it.
    macro_rules! symbol {
        ($name:literal, $type:ty) => {{
            // SAFETY: library is a live handle from dlopen, which is never closed
            let address = unsafe { libc::dlsym(library, concat!($name, "\0").as_ptr().cast()) };
            if address.is_null() {
                return None;
            }
            // SAFETY: a function's non-null address, as $type matches its C declaration
            unsafe { std::mem::transmute::<*mut c_void, $type>(address) }
        }};
    }
    let init = symbol!("heif_init", unsafe extern "C" fn(*mut c_void) -> HeifError);
    let lib = Libheif {
        version: symbol!("heif_get_version", unsafe extern "C" fn() -> *const c_char),
        context_alloc: symbol!("heif_context_alloc", unsafe extern "C" fn() -> Opaque),
        context_free: symbol!("heif_context_free", unsafe extern "C" fn(Opaque)),
        set_maximum_size: symbol!(
            "heif_context_set_maximum_image_size_limit",
            unsafe extern "C" fn(Opaque, c_int)
        ),
        read_from_memory: symbol!(
            "heif_context_read_from_memory_without_copy",
            unsafe extern "C" fn(Opaque, *const c_void, usize, *const c_void) -> HeifError
        ),
        primary_image: symbol!(
            "heif_context_get_primary_image_handle",
            unsafe extern "C" fn(Opaque, *mut Opaque) -> HeifError
        ),
        handle_width: symbol!(
            "heif_image_handle_get_width",
            unsafe extern "C" fn(Opaque) -> c_int
        ),
        handle_height: symbol!(
            "heif_image_handle_get_height",
            unsafe extern "C" fn(Opaque) -> c_int
        ),
        premultiplied: symbol!(
            "heif_image_handle_is_premultiplied_alpha",
            unsafe extern "C" fn(Opaque) -> c_int
        ),
        profile_type: symbol!(
            "heif_image_handle_get_color_profile_type",
            unsafe extern "C" fn(Opaque) -> c_int
        ),
        raw_profile_size: symbol!(
            "heif_image_handle_get_raw_color_profile_size",
            unsafe extern "C" fn(Opaque) -> usize
        ),
        raw_profile: symbol!(
            "heif_image_handle_get_raw_color_profile",
            unsafe extern "C" fn(Opaque, *mut c_void) -> HeifError
        ),
        nclx_profile: symbol!(
            "heif_image_handle_get_nclx_color_profile",
            unsafe extern "C" fn(Opaque, *mut *mut Nclx) -> HeifError
        ),
        nclx_free: symbol!(
            "heif_nclx_color_profile_free",
            unsafe extern "C" fn(*mut Nclx)
        ),
        decode: symbol!(
            "heif_decode_image",
            unsafe extern "C" fn(Opaque, *mut Opaque, c_int, c_int, *const c_void) -> HeifError
        ),
        image_width: symbol!(
            "heif_image_get_width",
            unsafe extern "C" fn(Opaque, c_int) -> c_int
        ),
        image_height: symbol!(
            "heif_image_get_height",
            unsafe extern "C" fn(Opaque, c_int) -> c_int
        ),
        plane: symbol!(
            "heif_image_get_plane_readonly",
            unsafe extern "C" fn(Opaque, c_int, *mut c_int) -> *const u8
        ),
        image_release: symbol!("heif_image_release", unsafe extern "C" fn(Opaque)),
        handle_release: symbol!("heif_image_handle_release", unsafe extern "C" fn(Opaque)),
        thumbnail_count: symbol!(
            "heif_image_handle_get_number_of_thumbnails",
            unsafe extern "C" fn(Opaque) -> c_int
        ),
        thumbnail_ids: symbol!(
            "heif_image_handle_get_list_of_thumbnail_IDs",
            unsafe extern "C" fn(Opaque, *mut u32, c_int) -> c_int
        ),
        thumbnail: symbol!(
            "heif_image_handle_get_thumbnail",
            unsafe extern "C" fn(Opaque, u32, *mut Opaque) -> HeifError
        ),
        plugin_failed: false,
    };
    // SAFETY: heif_init takes null for its default parameters; it loads the codec plugins
    let error = unsafe { init(ptr::null_mut()) };
    // A plugin that cannot load, as one of Arch's whose library is an optional dependency,
    // fails heif_init only after the built-in codecs and every plugin that loads are in
    // (libheif 1.23, init.cc: heif_init returns at the first plugin directory that gave an
    // error, and heif_load_plugins tries every file in it). libheif's own calls then run
    // heif_init again (load_plugins_if_not_initialized_yet), which in the sandbox lists no
    // plugin directory and succeeds. Any other error leaves libheif unusable.
    // shortcut: plugin directories after the one with the failed plugin stay unread, which
    // only a LIBHEIF_PLUGIN_PATH of several directories has; load each with
    // heif_load_plugins if one ever matters
    match error.code {
        0 => Some(lib),
        PLUGIN_LOADING => Some(Libheif {
            plugin_failed: true,
            ..lib
        }),
        _ => None,
    }
}

// What libheif hands out, released once whatever happens.
struct Owned {
    pointer: Opaque,
    release: unsafe extern "C" fn(Opaque),
}

impl Drop for Owned {
    fn drop(&mut self) {
        if !self.pointer.is_null() {
            // SAFETY: the pointer came from libheif with this release function, and goes once
            unsafe { (self.release)(self.pointer) };
        }
    }
}

fn check(error: HeifError) -> Result<(), DecodeError> {
    match (error.code, error.subcode) {
        (0, _) => return Ok(()),
        // libheif's limits, which the cap set, or its own on memory
        (_, SECURITY_LIMIT) | (MEMORY, _) => return Err(DecodeError::TooLarge { size: None }),
        _ => {}
    }
    // SAFETY: libheif's message is always a valid C string, read before the next call
    let message = unsafe { CStr::from_ptr(error.message) };
    Err(DecodeError::Unreadable(
        message.to_string_lossy().into_owned(),
    ))
}

/// Decodes a HEIC or AVIF file's primary image as straight-alpha RGBA in sRGB, refusing one
/// whose decode would pass `max_bytes`. With a `side`, the smallest thumbnail embedded in the
/// file that covers it and has the image's aspect ratio decodes instead, where there is one.
pub(crate) fn decode(file: File, max_bytes: u64, side: u32) -> Result<Image, DecodeError> {
    let lib = LIBHEIF
        .get()
        .and_then(Option::as_ref)
        .ok_or_else(|| DecodeError::Unreadable("no libheif".to_owned()))?;
    let mut data = Vec::new();
    file.take(max_bytes.saturating_add(1))
        .read_to_end(&mut data)
        .map_err(|e| DecodeError::Unreadable(e.to_string()))?;
    if data.len() as u64 > max_bytes {
        return Err(DecodeError::TooLarge { size: None });
    }
    // SAFETY: every pointer below comes from libheif and is used while its owner lives; the
    // context outlives the handle, the handle the image, and data the context
    unsafe {
        let context = Owned {
            pointer: (lib.context_alloc)(),
            release: lib.context_free,
        };
        if context.pointer.is_null() {
            return Err(DecodeError::Unreadable("no memory for libheif".to_owned()));
        }
        // libheif's own limit, on the pixels the cap holds; 0 would mean none
        let widest = ((max_bytes / PEAK) as f64)
            .sqrt()
            .clamp(1.0, f64::from(c_int::MAX)) as c_int;
        (lib.set_maximum_size)(context.pointer, widest);
        check((lib.read_from_memory)(
            context.pointer,
            data.as_ptr().cast(),
            data.len(),
            ptr::null(),
        ))?;
        let mut primary = Owned {
            pointer: ptr::null_mut(),
            release: lib.handle_release,
        };
        check((lib.primary_image)(context.pointer, &mut primary.pointer))?;
        let thumbnail = (side > 0)
            .then(|| thumbnail(lib, primary.pointer, side))
            .flatten();
        let handle = thumbnail.as_ref().unwrap_or(&primary).pointer;
        let (width, height) = dimensions(lib, handle);
        if peak(width, height, data.len()) > max_bytes {
            return Err(DecodeError::TooLarge {
                size: Some((width, height)),
            });
        }
        // the handle's colr box describes the pixels: libheif's RGB conversion tags its output
        // sRGB but keeps the file's primaries and curve, as no step of it converts those
        // shortcut: a file without a colr box shows as sRGB whatever its bitstream signals;
        // read the decoded image's under libheif's nclx passthrough once such files turn up
        let color = match profile(lib, handle) {
            // a thumbnail with no profile of its own shows in the image's
            Color::Srgb if thumbnail.is_some() => profile(lib, primary.pointer),
            color => color,
        };
        let mut image = Owned {
            pointer: ptr::null_mut(),
            release: lib.image_release,
        };
        check((lib.decode)(
            handle,
            &mut image.pointer,
            RGB,
            RGBA,
            ptr::null(),
        ))?;
        // the decoded plane decides the size, not the size the file signals
        let width = (lib.image_width)(image.pointer, INTERLEAVED);
        let height = (lib.image_height)(image.pointer, INTERLEAVED);
        let mut stride: c_int = 0;
        let plane = (lib.plane)(image.pointer, INTERLEAVED, &mut stride);
        let (Ok(width), Ok(height), Ok(stride)) = (
            usize::try_from(width),
            usize::try_from(height),
            usize::try_from(stride),
        ) else {
            return Err(DecodeError::Unreadable("a plane of no size".to_owned()));
        };
        let row = width * 4;
        if plane.is_null() || width == 0 || height == 0 || stride < row {
            return Err(DecodeError::Unreadable("no interleaved plane".to_owned()));
        }
        if peak(width as u32, height as u32, data.len()) > max_bytes {
            return Err(DecodeError::TooLarge {
                size: Some((width as u32, height as u32)),
            });
        }
        let mut rgba = Vec::with_capacity(row * height);
        for y in 0..height {
            rgba.extend_from_slice(std::slice::from_raw_parts(plane.add(y * stride), row));
        }
        if (lib.premultiplied)(handle) != 0 {
            nsxiv_ng_decode::unpremultiply(&mut rgba);
        }
        match color {
            Color::Icc(icc) => nsxiv_ng_decode::icc_to_srgb(&mut rgba, width, &icc),
            Color::Cicp(primaries, transfer) => {
                nsxiv_ng_decode::cicp_to_srgb(&mut rgba, width, primaries, transfer)
            }
            Color::Srgb => {}
        }
        Ok(Image {
            width: width as u32,
            height: height as u32,
            rgba,
            may_animate: false,
        })
    }
}

// Bytes a pixel takes at the peak: libheif keeps its decoded planes, up to 8 bytes at 16 bits
// with alpha, through a conversion whose steps each hold their input and output, up to 8 and 4
// bytes more; then its RGBA plane sits beside the copy here.
const PEAK: u64 = 20;

// The memory a decode takes at its peak: the pixels, and the file, which libheif and its codec
// each copy once more.
fn peak(width: u32, height: u32, file: usize) -> u64 {
    (u64::from(width) * u64::from(height))
        .saturating_mul(PEAK)
        .saturating_add((file as u64).saturating_mul(3))
}

// The size of the image a handle stands for, after its transformations.
//
// SAFETY: the caller passes a live image handle from libheif
unsafe fn dimensions(lib: &Libheif, handle: Opaque) -> (u32, u32) {
    let signalled = |n: c_int| u32::try_from(n).unwrap_or(0);
    // SAFETY: handle is live
    unsafe {
        (
            signalled((lib.handle_width)(handle)),
            signalled((lib.handle_height)(handle)),
        )
    }
}

// The smallest thumbnail of the image that covers `side` and has its aspect ratio
// ([`smallest_covering`]).
//
// SAFETY: the caller passes a live image handle from libheif
unsafe fn thumbnail(lib: &Libheif, primary: Opaque, side: u32) -> Option<Owned> {
    // SAFETY: primary is live, the list is as long as libheif is told, and each thumbnail
    // handle is released by its Owned
    unsafe {
        let count = (lib.thumbnail_count)(primary);
        let mut ids = vec![0u32; usize::try_from(count).ok()?];
        let listed = (lib.thumbnail_ids)(primary, ids.as_mut_ptr(), count);
        ids.truncate(usize::try_from(listed).unwrap_or(0));
        let mut thumbnails = Vec::with_capacity(ids.len());
        for id in ids {
            let mut handle = Owned {
                pointer: ptr::null_mut(),
                release: lib.handle_release,
            };
            if (lib.thumbnail)(primary, id, &mut handle.pointer).code == 0
                && !handle.pointer.is_null()
            {
                thumbnails.push(handle);
            }
        }
        let sizes: Vec<_> = thumbnails
            .iter()
            .map(|handle| dimensions(lib, handle.pointer))
            .collect();
        let best = smallest_covering(dimensions(lib, primary), side, &sizes)?;
        Some(thumbnails.swap_remove(best))
    }
}

// Which of the thumbnail sizes is the smallest that covers `side` and has the image's aspect
// ratio, to within a pixel, so a thumbnail turned or cropped otherwise than the image never
// stands in for it.
fn smallest_covering(
    (width, height): (u32, u32),
    side: u32,
    sizes: &[(u32, u32)],
) -> Option<usize> {
    let skew = |w: u32, h: u32| {
        (u64::from(w) * u64::from(height)).abs_diff(u64::from(h) * u64::from(width))
    };
    sizes
        .iter()
        .enumerate()
        .filter(|&(_, &(w, h))| w.max(h) >= side && skew(w, h) <= u64::from(width.max(height)))
        .min_by_key(|&(_, &(w, h))| u64::from(w) * u64::from(h))
        .map(|(i, _)| i)
}

enum Color {
    Icc(Vec<u8>),
    Cicp(u8, u8),
    Srgb,
}

// The color space the handle declares, as an ICC profile or as nclx code points.
//
// SAFETY: the caller passes a live image handle from libheif
unsafe fn profile(lib: &Libheif, handle: Opaque) -> Color {
    // SAFETY: handle is live, and the buffers below are as large as libheif says they must be
    unsafe {
        match (lib.profile_type)(handle) {
            RICC | PROF => {
                let size = (lib.raw_profile_size)(handle);
                let mut icc = vec![0u8; size];
                if size > 0 && (lib.raw_profile)(handle, icc.as_mut_ptr().cast()).code == 0 {
                    return Color::Icc(icc);
                }
                Color::Srgb
            }
            NCLX => {
                let mut nclx: *mut Nclx = ptr::null_mut();
                if (lib.nclx_profile)(handle, &mut nclx).code != 0 || nclx.is_null() {
                    return Color::Srgb;
                }
                let code = |n: c_int| u8::try_from(n).unwrap_or(2); // 2: unspecified
                let color = Color::Cicp(
                    code((*nclx).color_primaries),
                    code((*nclx).transfer_characteristics),
                );
                (lib.nclx_free)(nclx);
                color
            }
            _ => Color::Srgb,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::smallest_covering;

    #[test]
    fn the_smallest_thumbnail_of_the_image_s_shape_that_covers_the_side_serves() {
        let photo = (4032, 3024);
        // out of size order: two of the photo's shape, one turned, one cropped, a large one
        let sizes = [(320, 240), (240, 320), (160, 120), (300, 300), (1024, 768)];
        assert_eq!(smallest_covering(photo, 160, &sizes), Some(2));
        assert_eq!(smallest_covering(photo, 161, &sizes), Some(0));
        assert_eq!(smallest_covering(photo, 321, &sizes), Some(4));
        assert_eq!(smallest_covering(photo, 1025, &sizes), None);
        // a pixel off the shape passes, three do not
        assert_eq!(smallest_covering((4000, 2250), 100, &[(320, 181)]), Some(0));
        assert_eq!(smallest_covering((4000, 2250), 100, &[(320, 183)]), None);
    }
}
