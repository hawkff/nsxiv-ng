//! Build-time defaults, in the spirit of nsxiv's config.h, and the config file that overrides
//! them.

use std::path::{Path, PathBuf};

use toml::de::{DeTable, DeValue};

/// The program's name, which the binary, the window class and the config directory use.
pub const APP_NAME: &str = "nsxiv-ng";

/// The version `-v` prints.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Seconds between slideshow images when `-S` gets no number.
pub const SLIDESHOW_DELAY: f64 = 5.0;

/// Gamma and contrast steps each way; the top step reaches GAMMA_MAX or CONTRAST_MAX.
pub const CC_STEPS: i32 = 32;
pub const GAMMA_MAX: f64 = 10.0;
pub const CONTRAST_MAX: f64 = 4.0;

/// The zoom levels `-` and `+` step through; the first and last bound zooming.
pub const ZOOM_LEVELS: [f64; 9] = [0.125, 0.25, 0.5, 0.75, 1.0, 1.5, 2.0, 4.0, 8.0];

/// A keyboard pan moves the view by 1/PAN_FRACTION of its size.
pub const PAN_FRACTION: f64 = 5.0;

/// Thumbnail sizes `-` and `+` step through in the grid, and the one it starts at.
pub const THUMB_SIZES: [f64; 5] = [32.0, 64.0, 96.0, 128.0, 160.0];
pub const THUMB_SIZE_INDEX: usize = 3;

/// Space around each thumbnail in its grid cell.
pub const THUMB_PADDING: f64 = 10.0;

/// The values a user may change in `config.toml`, one key each, named as the field.
#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    /// The window's size before the first image fits it, in logical units.
    pub window_width: f64,
    pub window_height: f64,
    /// Seconds between slideshow images when the command line names none.
    pub slideshow_delay: f64,
    /// Gamma and contrast steps each way; the top step reaches `gamma_max` or `contrast_max`.
    pub cc_steps: i32,
    pub gamma_max: f64,
    pub contrast_max: f64,
    /// Ascending by more than 0.002 each; the first and last bound zooming.
    pub zoom_levels: Vec<f64>,
    /// A keyboard pan moves the view by 1/`pan_fraction` of its size.
    pub pan_fraction: f64,
    /// Ascending, with the grid starting at `thumb_sizes[thumb_size_index]`.
    pub thumb_sizes: Vec<f64>,
    pub thumb_size_index: usize,
    pub thumb_padding: f64,
    /// The share of the window's width at each side that a click navigates in.
    pub nav_width_fraction: f64,
    pub background: [u8; 3],
    pub foreground: [u8; 3],
    pub bar_background: [u8; 3],
    pub selection_color: [u8; 3],
    pub mark_color: [u8; 3],
    /// Which of the five colors the file set, each with a value it took; one it left out is
    /// its default here, which a frontend may take from the system instead.
    pub colors_set: ColorsSet,
    /// A font family; `monospace` names the system's monospace font.
    pub bar_font: String,
    /// In logical pixels.
    pub bar_font_size: f64,
    /// The decoded-image cache gets this percent of physical memory, and at most
    /// `image_cache_max_mib` MiB.
    pub image_cache_percent: f64,
    pub image_cache_max_mib: u64,
    /// An image whose pixels need more than this percent of physical memory is refused.
    pub max_image_percent: f64,
    /// Bytes an animation's decoded frames may hold, in MiB.
    pub animation_budget_mib: u64,
    /// The status bar shows at start; `b` shows and hides it, and `-b` starts it hidden.
    pub show_bar: bool,
}

/// Which of [`Config`]'s colors its file set, a flag each, named as the color.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ColorsSet {
    pub background: bool,
    pub foreground: bool,
    pub bar_background: bool,
    pub selection_color: bool,
    pub mark_color: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            window_width: 800.0,
            window_height: 600.0,
            slideshow_delay: SLIDESHOW_DELAY,
            cc_steps: CC_STEPS,
            gamma_max: GAMMA_MAX,
            contrast_max: CONTRAST_MAX,
            zoom_levels: ZOOM_LEVELS.to_vec(),
            pan_fraction: PAN_FRACTION,
            thumb_sizes: THUMB_SIZES.to_vec(),
            thumb_size_index: THUMB_SIZE_INDEX,
            thumb_padding: THUMB_PADDING,
            nav_width_fraction: 0.33,
            background: [0xff, 0xff, 0xff],
            foreground: [0x00, 0x00, 0x00],
            bar_background: [0xee, 0xee, 0xee],
            selection_color: [0x34, 0x78, 0xf6],
            mark_color: [0xff, 0xcc, 0x00],
            colors_set: ColorsSet::default(),
            bar_font: "monospace".to_owned(),
            bar_font_size: 13.0,
            image_cache_percent: 3.0,
            image_cache_max_mib: 256,
            max_image_percent: 25.0,
            animation_budget_mib: 256,
            show_bar: false,
        }
    }
}

impl Config {
    /// The defaults with the keys of a `config.toml` applied, and a warning for each key or
    /// value it could not take, naming its line. A key that gets a warning keeps its default.
    /// Text with a syntax error applies no key: each error gets a warning, and a last one says
    /// the text was ignored.
    pub fn parse(text: &str) -> (Config, Vec<String>) {
        let line = |offset: usize| {
            text.as_bytes()[..offset.min(text.len())]
                .iter()
                .filter(|&&b| b == b'\n')
                .count()
                + 1
        };
        let lines = |mut warnings: Vec<(usize, String)>| -> Vec<String> {
            warnings.sort_by_key(|w| w.0);
            warnings
                .into_iter()
                .map(|(at, text)| format!("line {at}: {text}"))
                .collect()
        };
        let (table, errors) = DeTable::parse_recoverable(text);
        if !errors.is_empty() {
            // the parser's recovery drops keys and stands in for values, so it serves only to
            // find every error
            let errors = errors
                .iter()
                // an error with no place is one the end of the text left open
                .map(|e| {
                    (
                        line(e.span().map_or(text.len(), |s| s.start)),
                        e.message().to_owned(),
                    )
                })
                .collect();
            let mut warnings = lines(errors);
            warnings
                .push("ignored for its syntax errors; every value keeps its default".to_owned());
            return (Config::default(), warnings);
        }
        let mut config = Config::default();
        let mut warnings = Vec::new();
        let mut index_line = None;
        for (key, value) in table.get_ref() {
            let at = line(key.span().start);
            let key: &str = key.get_ref();
            if key == "thumb_size_index" {
                index_line = Some(at);
            }
            match config.set(key, value.get_ref()) {
                Ok(true) => {}
                Ok(false) => warnings.push((at, format!("unknown key: {key}"))),
                Err(wanted) => warnings.push((at, format!("{key}: expected {wanted}"))),
            }
        }
        if config.thumb_size_index >= config.thumb_sizes.len() {
            if let Some(at) = index_line {
                let wanted = "thumb_size_index: expected an index into thumb_sizes";
                warnings.push((at, wanted.to_owned()));
            }
            config.thumb_size_index = THUMB_SIZE_INDEX.min(config.thumb_sizes.len() - 1);
        }
        (config, lines(warnings))
    }

    /// `$XDG_CONFIG_HOME/nsxiv-ng/config.toml`, else `~/.config/nsxiv-ng/config.toml`; none
    /// without a home directory.
    pub fn path() -> Option<PathBuf> {
        config_path(std::env::var_os("XDG_CONFIG_HOME"), std::env::home_dir())
    }

    /// The config file at [`Config::path`] parsed, its warnings led by the path. A missing file
    /// gives the defaults and no warning.
    pub fn load() -> (Config, Vec<String>) {
        match Config::path() {
            Some(path) => load_file(&path),
            None => (Config::default(), Vec::new()),
        }
    }

    // false for a key that names no value
    fn set(&mut self, key: &str, v: &DeValue<'_>) -> Result<bool, &'static str> {
        const POSITIVE: &str = "a number above 0";
        let positive = |x: f64| x > 0.0;
        match key {
            "window_width" => self.window_width = number(v, positive).ok_or(POSITIVE)?,
            "window_height" => self.window_height = number(v, positive).ok_or(POSITIVE)?,
            "slideshow_delay" => self.slideshow_delay = number(v, positive).ok_or(POSITIVE)?,
            "cc_steps" => {
                self.cc_steps =
                    integer(v, 2, i32::MAX.into()).ok_or("a whole number of 2 or more")? as i32
            }
            "gamma_max" => {
                self.gamma_max = number(v, |x| x >= 1.0).ok_or("a number of 1 or more")?
            }
            "contrast_max" => {
                self.contrast_max = number(v, |x| x >= 1.0).ok_or("a number of 1 or more")?
            }
            "zoom_levels" => {
                self.zoom_levels = levels(v, ZOOM_GAP)
                    .ok_or("a list of numbers above 0, each more than 0.002 above the one before")?
            }
            "pan_fraction" => {
                self.pan_fraction = number(v, |x| x >= 1.0).ok_or("a number of 1 or more")?
            }
            "thumb_sizes" => {
                self.thumb_sizes = levels(v, 0.0).ok_or("a list of ascending numbers above 0")?
            }
            "thumb_size_index" => {
                self.thumb_size_index =
                    integer(v, 0, i64::MAX).ok_or("an index into thumb_sizes")? as usize
            }
            "thumb_padding" => {
                self.thumb_padding = number(v, |x| x >= 0.0).ok_or("a number of 0 or more")?
            }
            "nav_width_fraction" => {
                self.nav_width_fraction =
                    number(v, |x| x > 0.0 && x <= 0.5).ok_or("a number above 0 and at most 0.5")?
            }
            "background" => {
                self.background = color(v).ok_or(COLOR)?;
                self.colors_set.background = true;
            }
            "foreground" => {
                self.foreground = color(v).ok_or(COLOR)?;
                self.colors_set.foreground = true;
            }
            "bar_background" => {
                self.bar_background = color(v).ok_or(COLOR)?;
                self.colors_set.bar_background = true;
            }
            "selection_color" => {
                self.selection_color = color(v).ok_or(COLOR)?;
                self.colors_set.selection_color = true;
            }
            "mark_color" => {
                self.mark_color = color(v).ok_or(COLOR)?;
                self.colors_set.mark_color = true;
            }
            "bar_font" => {
                self.bar_font = v
                    .as_str()
                    .filter(|s| !s.trim().is_empty())
                    .ok_or("a font family")?
                    .to_owned()
            }
            "bar_font_size" => self.bar_font_size = number(v, positive).ok_or(POSITIVE)?,
            "image_cache_percent" => {
                self.image_cache_percent = number(v, |x| x > 0.0 && x <= 100.0)
                    .ok_or("a number above 0 and at most 100")?
            }
            "image_cache_max_mib" => self.image_cache_max_mib = mib(v).ok_or(MIB)?,
            "max_image_percent" => {
                self.max_image_percent = number(v, |x| x > 0.0 && x <= 100.0)
                    .ok_or("a number above 0 and at most 100")?
            }
            "animation_budget_mib" => self.animation_budget_mib = mib(v).ok_or(MIB)?,
            "show_bar" => self.show_bar = v.as_bool().ok_or("true or false")?,
            _ => return Ok(false),
        }
        Ok(true)
    }
}

/// How far apart zoom levels must be: zooming steps to the first level more than 0.001 past
/// the zoom, which would skip a closer one.
pub(crate) const ZOOM_GAP: f64 = 0.002;

/// Whether `list` serves as zoom levels or thumbnail sizes: not empty, finite and above 0, and
/// each more than `gap` above the one before.
pub(crate) fn ascending(list: &[f64], gap: f64) -> bool {
    !list.is_empty()
        && list.iter().all(|x| x.is_finite() && *x > 0.0)
        && list.windows(2).all(|w| w[1] - w[0] > gap)
}

const COLOR: &str = "a color as \"#rrggbb\"";
const MIB: &str = "a whole number of MiB above 0";

fn config_path(xdg: Option<std::ffi::OsString>, home: Option<PathBuf>) -> Option<PathBuf> {
    // the XDG spec has a relative or empty XDG_CONFIG_HOME ignored
    let base = xdg
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| home.map(|h| h.join(".config")))?;
    Some(base.join(APP_NAME).join("config.toml"))
}

fn load_file(path: &Path) -> (Config, Vec<String>) {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            let (config, warnings) = Config::parse(&text);
            let warnings = warnings
                .into_iter()
                .map(|w| format!("{}: {w}", path.display()))
                .collect();
            (config, warnings)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (Config::default(), Vec::new()),
        Err(e) => (Config::default(), vec![format!("{}: {e}", path.display())]),
    }
}

// a whole number counts as a number, as `delay = 5` should
fn number(v: &DeValue<'_>, ok: impl Fn(f64) -> bool) -> Option<f64> {
    let x = match v {
        DeValue::Float(f) => f.as_str().parse().ok()?,
        DeValue::Integer(_) => integer(v, i64::MIN, i64::MAX)? as f64,
        _ => return None,
    };
    Some(x).filter(|x: &f64| x.is_finite() && ok(*x))
}

fn integer(v: &DeValue<'_>, min: i64, max: i64) -> Option<i64> {
    let n = v.as_integer()?;
    i64::from_str_radix(n.as_str(), n.radix())
        .ok()
        .filter(|n| (min..=max).contains(n))
}

// a cap the later layers multiply by 2^20 in a u64
fn mib(v: &DeValue<'_>) -> Option<u64> {
    integer(v, 1, (u64::MAX >> 20) as i64).map(|n| n as u64)
}

fn levels(v: &DeValue<'_>, gap: f64) -> Option<Vec<f64>> {
    let list = v
        .as_array()?
        .iter()
        .map(|x| number(x.get_ref(), |_| true))
        .collect::<Option<Vec<f64>>>()?;
    ascending(&list, gap).then_some(list)
}

fn color(v: &DeValue<'_>) -> Option<[u8; 3]> {
    let hex = v.as_str()?.strip_prefix('#')?;
    if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let byte = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
    Some([byte(0)?, byte(2)?, byte(4)?])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_file_gives_the_defaults() {
        assert!(!Config::default().show_bar, "the bar starts hidden");
        assert_eq!(Config::parse(""), (Config::default(), vec![]));
        assert_eq!(
            Config::parse("# only a comment\n"),
            (Config::default(), vec![])
        );
    }

    #[test]
    fn keys_override_their_defaults() {
        let text = r##"
window_width = 1024
window_height = 768.5
slideshow_delay = 2
cc_steps = 16
gamma_max = 5.0
contrast_max = 2
zoom_levels = [0.5, 1, 3]
pan_fraction = 10
thumb_sizes = [64, 128]
thumb_size_index = 0
thumb_padding = 0
nav_width_fraction = 0.25
background = "#000000"
foreground = "#FFFFFF"
bar_background = "#123456"
selection_color = "#abcdef"
mark_color = "#00ff00"
bar_font = "DejaVu Sans Mono"
bar_font_size = 15
image_cache_percent = 1.5
image_cache_max_mib = 0x200
max_image_percent = 50
animation_budget_mib = 1_024
show_bar = true
"##;
        let expected = Config {
            window_width: 1024.0,
            window_height: 768.5,
            slideshow_delay: 2.0,
            cc_steps: 16,
            gamma_max: 5.0,
            contrast_max: 2.0,
            zoom_levels: vec![0.5, 1.0, 3.0],
            pan_fraction: 10.0,
            thumb_sizes: vec![64.0, 128.0],
            thumb_size_index: 0,
            thumb_padding: 0.0,
            nav_width_fraction: 0.25,
            background: [0, 0, 0],
            foreground: [255, 255, 255],
            bar_background: [0x12, 0x34, 0x56],
            selection_color: [0xab, 0xcd, 0xef],
            mark_color: [0, 255, 0],
            colors_set: ColorsSet {
                background: true,
                foreground: true,
                bar_background: true,
                selection_color: true,
                mark_color: true,
            },
            bar_font: "DejaVu Sans Mono".to_owned(),
            bar_font_size: 15.0,
            image_cache_percent: 1.5,
            image_cache_max_mib: 512,
            max_image_percent: 50.0,
            animation_budget_mib: 1024,
            show_bar: true,
        };
        assert_eq!(Config::parse(text), (expected, vec![]));
    }

    #[test]
    fn the_colors_the_file_set_are_told_also_at_their_defaults() {
        let (config, warnings) =
            Config::parse("background = \"#ffffff\"\nmark_color = \"#00ff00\"\n");
        assert!(warnings.is_empty());
        assert_eq!(config.background, Config::default().background);
        assert_eq!(
            config.colors_set,
            ColorsSet {
                background: true,
                mark_color: true,
                ..ColorsSet::default()
            }
        );
        assert_eq!(Config::default().colors_set, ColorsSet::default());
    }

    #[test]
    fn bad_keys_and_values_warn_with_their_line_and_keep_the_default() {
        let text = r##"window_width = 900
windo_height = 700
background = "#fff"
cc_steps = 1
zoom_levels = [1, 0.5]
thumb_sizes = []
nav_width_fraction = 0.9
slideshow_delay = nan
image_cache_max_mib = 0
bar_font = ""
mark_color = 0xffcc00
max_image_percent = 101
show_bar = 1

[colors]
background = "#000000"
"##;
        let (config, warnings) = Config::parse(text);
        assert_eq!(
            config,
            Config {
                window_width: 900.0,
                ..Config::default()
            }
        );
        assert_eq!(
            warnings,
            [
                "line 2: unknown key: windo_height",
                "line 3: background: expected a color as \"#rrggbb\"",
                "line 4: cc_steps: expected a whole number of 2 or more",
                "line 5: zoom_levels: expected a list of numbers above 0, each more than 0.002 \
                 above the one before",
                "line 6: thumb_sizes: expected a list of ascending numbers above 0",
                "line 7: nav_width_fraction: expected a number above 0 and at most 0.5",
                "line 8: slideshow_delay: expected a number above 0",
                "line 9: image_cache_max_mib: expected a whole number of MiB above 0",
                "line 10: bar_font: expected a font family",
                "line 11: mark_color: expected a color as \"#rrggbb\"",
                "line 12: max_image_percent: expected a number above 0 and at most 100",
                "line 13: show_bar: expected true or false",
                "line 15: unknown key: colors",
            ]
        );
        let (config, warnings) = Config::parse("zoom_levels = [1.0, 1.0005]\n");
        assert!(
            config == Config::default() && warnings.len() == 1,
            "levels closer than a zoom step can tell apart: {warnings:?}"
        );
    }

    // the lines a file with syntax errors was ignored for, once its config is the default
    fn ignored_for(text: &str) -> Vec<usize> {
        let (config, mut warnings) = Config::parse(text);
        assert_eq!(config, Config::default(), "{text:?}");
        assert_eq!(
            warnings.pop().as_deref(),
            Some("ignored for its syntax errors; every value keeps its default"),
            "{text:?}"
        );
        assert!(!warnings.is_empty(), "{text:?}");
        let mut lines: Vec<usize> = warnings
            .iter()
            .map(|w| {
                let rest = w.strip_prefix("line ").expect(w);
                rest[..rest.find(':').expect(w)].parse().expect(w)
            })
            .collect();
        lines.dedup();
        lines
    }

    #[test]
    fn a_syntax_error_makes_the_whole_file_count_for_nothing() {
        assert_eq!(
            ignored_for("pan_fraction = 4\nbroken\nbar_font_size = 20\n"),
            [2],
            "a line with no value ends the parse"
        );
        assert_eq!(
            ignored_for("pan_fraction = 4\nthumb_padding ==8\nbar_font_size = 20\n"),
            [2],
            "a value the parser stands in for"
        );
        assert_eq!(
            ignored_for("pan_fraction = 4\r\nbroken\r\nbar_font_size = 20\r\n"),
            [2],
            "CRLF lines"
        );
        assert_eq!(
            ignored_for("pan_fraction = 4\nzoom_levels = [1, 2"),
            [2],
            "an array the end of the file leaves open"
        );
        assert_eq!(
            ignored_for("pan_fraction = 4\npan_fraction = 5\n"),
            [2],
            "a duplicate key"
        );
    }

    #[test]
    fn the_thumbnail_index_stays_inside_the_sizes() {
        let (config, warnings) = Config::parse("thumb_sizes = [64, 128]\n");
        assert_eq!((config.thumb_size_index, warnings), (1, vec![]));
        let (config, warnings) = Config::parse("thumb_size_index = 5\n");
        assert_eq!(
            (config.thumb_size_index, warnings),
            (
                THUMB_SIZE_INDEX,
                vec!["line 1: thumb_size_index: expected an index into thumb_sizes".to_owned()]
            )
        );
    }

    #[test]
    fn the_path_follows_xdg() {
        let file = |base: &str| Some(PathBuf::from(base).join("nsxiv-ng/config.toml"));
        let home = Some(PathBuf::from("/home/u"));
        assert_eq!(config_path(Some("/xdg".into()), home.clone()), file("/xdg"));
        assert_eq!(config_path(None, home.clone()), file("/home/u/.config"));
        assert_eq!(
            config_path(Some("".into()), home.clone()),
            file("/home/u/.config"),
            "an empty XDG_CONFIG_HOME is ignored"
        );
        assert_eq!(
            config_path(Some("rel".into()), home),
            file("/home/u/.config"),
            "so is a relative one"
        );
        assert_eq!(config_path(None, None), None);
    }

    #[test]
    fn loading_reads_the_file_and_a_missing_one_means_defaults() {
        let dir = std::env::temp_dir().join(format!("nsxiv-ng-config-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        assert_eq!(load_file(&path), (Config::default(), vec![]));
        std::fs::write(&path, "bar_font_size = 20\nbar = 1\n").unwrap();
        let (config, warnings) = load_file(&path);
        assert_eq!(config.bar_font_size, 20.0);
        assert_eq!(
            warnings,
            [format!("{}: line 2: unknown key: bar", path.display())]
        );
        let (config, warnings) = load_file(&dir);
        assert!(
            config == Config::default() && warnings.len() == 1,
            "a file that cannot be read warns: {warnings:?}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
