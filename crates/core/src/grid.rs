//! Thumbnail grid geometry: one cell per file in a view with the origin at the
//! top left, scrolled down by `scroll_y`.

use std::ops::Range;

use crate::config::{Config, THUMB_SIZES, ascending};
use crate::{Direction, Point, Rect, Size};

#[derive(Clone, Debug, PartialEq)]
pub struct GridLayout {
    count: usize,
    view_size: Size,
    size_index: usize,
    scroll_y: f64,
    thumb_sizes: Vec<f64>,
    thumb_padding: f64,
}

impl Default for GridLayout {
    fn default() -> Self {
        GridLayout::with_config(&Config::default())
    }
}

impl GridLayout {
    /// A grid that steps through `thumb_sizes`, starting at `thumb_size_index`, with
    /// `thumb_padding` around each thumbnail.
    pub fn with_config(config: &Config) -> Self {
        // a list built in code may skip the parser's checks
        let thumb_sizes = if ascending(&config.thumb_sizes, 0.0) {
            config.thumb_sizes.clone()
        } else {
            THUMB_SIZES.to_vec()
        };
        GridLayout {
            count: 0,
            view_size: Size::default(),
            size_index: config.thumb_size_index.min(thumb_sizes.len() - 1),
            scroll_y: 0.0,
            thumb_sizes,
            thumb_padding: config.thumb_padding,
        }
    }

    pub fn count(&self) -> usize {
        self.count
    }

    pub fn set_count(&mut self, count: usize) {
        self.count = count;
        self.clamp_scroll();
    }

    pub fn view_size(&self) -> Size {
        self.view_size
    }

    pub fn set_view_size(&mut self, size: Size) {
        self.view_size = size;
        self.clamp_scroll();
    }

    pub fn scroll_y(&self) -> f64 {
        self.scroll_y
    }

    pub fn thumb_size(&self) -> f64 {
        self.thumb_sizes[self.size_index]
    }

    /// The largest thumbnail size the grid steps through, which thumbnails are made for.
    pub fn max_thumb_size(&self) -> f64 {
        self.thumb_sizes[self.thumb_sizes.len() - 1]
    }

    pub fn cell_size(&self) -> f64 {
        self.thumb_size() + self.thumb_padding * 2.0
    }

    pub fn columns(&self) -> usize {
        ((self.view_size.width / self.cell_size()) as usize).max(1)
    }

    fn x_inset(&self) -> f64 {
        (self.view_size.width - self.columns() as f64 * self.cell_size()) / 2.0
    }

    fn content_height(&self) -> f64 {
        self.count.div_ceil(self.columns()) as f64 * self.cell_size()
    }

    /// The cell of file `i` as drawn, scroll applied.
    pub fn cell_rect(&self, i: usize) -> Rect {
        let (columns, cell) = (self.columns(), self.cell_size());
        Rect {
            x: self.x_inset() + (i % columns) as f64 * cell,
            y: (i / columns) as f64 * cell - self.scroll_y,
            width: cell,
            height: cell,
        }
    }

    /// The files in view, plus one row below it, so those thumbnails load early.
    pub fn visible(&self) -> Range<usize> {
        let (columns, cell) = (self.columns(), self.cell_size());
        // float-to-usize casts saturate, and the arithmetic after them must too
        let first = self
            .count
            .min(((self.scroll_y / cell) as usize).saturating_mul(columns));
        // rows reaching into the view, counting one the bottom edge only touches as outside
        let rows = ((self.scroll_y + self.view_size.height) / cell).ceil() as usize;
        let end = self
            .count
            .min(rows.saturating_add(1).saturating_mul(columns));
        first..first.max(end)
    }

    pub fn index_at(&self, p: Point) -> Option<usize> {
        let (columns, cell) = (self.columns(), self.cell_size());
        let (x, y) = (p.x - self.x_inset(), p.y + self.scroll_y);
        // the positive form turns away NaN coordinates too
        let inside = x >= 0.0 && x < columns as f64 * cell && y >= 0.0;
        if !inside {
            return None;
        }
        let i = ((y / cell) as usize)
            .checked_mul(columns)?
            .checked_add((x / cell) as usize)?;
        (i < self.count).then_some(i)
    }

    /// The cell next to `i` toward `direction`, staying on the first or last file at the ends.
    pub fn neighbor(&self, i: usize, direction: Direction) -> usize {
        let columns = self.columns() as isize;
        let step = match direction {
            Direction::Left => -1,
            Direction::Right => 1,
            Direction::Up => -columns,
            Direction::Down => columns,
        };
        (i as isize + step).min(self.count as isize - 1).max(0) as usize
    }

    /// Scrolls just enough to show the whole cell of file `i`.
    pub fn reveal(&mut self, i: usize) {
        let r = self.cell_rect(i);
        if r.y < 0.0 {
            self.scroll_y += r.y;
        } else if r.max_y() > self.view_size.height {
            self.scroll_y += r.max_y() - self.view_size.height;
        }
        self.clamp_scroll();
    }

    pub fn scroll(&mut self, dy: f64) {
        self.scroll_y += dy;
        self.clamp_scroll();
    }

    pub fn scroll_page(&mut self, direction: Direction) {
        match direction {
            Direction::Up => self.scroll(-self.view_size.height),
            Direction::Down => self.scroll(self.view_size.height),
            Direction::Left | Direction::Right => {}
        }
    }

    /// Moves `dir` thumbnail sizes up, or down when negative; false past either end.
    pub fn zoom(&mut self, dir: i32) -> bool {
        let next = self.size_index.checked_add_signed(dir as isize);
        match next.filter(|&n| n < self.thumb_sizes.len()) {
            Some(n) => {
                self.size_index = n;
                self.clamp_scroll();
                true
            }
            None => false,
        }
    }

    fn clamp_scroll(&mut self) {
        self.scroll_y = self
            .scroll_y
            .min(self.content_height() - self.view_size.height)
            .max(0.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 640 wide fits four 148 cells (128 thumbnails plus padding), 24 to spare on each side
    fn grid(count: usize) -> GridLayout {
        let mut g = GridLayout::default();
        g.set_view_size(Size {
            width: 640.0,
            height: 480.0,
        });
        g.set_count(count);
        g
    }

    #[test]
    fn cells_lay_out_in_centered_rows() {
        let g = grid(30);
        assert_eq!((g.columns(), g.cell_size()), (4, 148.0));
        assert_eq!(
            g.cell_rect(5),
            Rect {
                x: 172.0,
                y: 148.0,
                width: 148.0,
                height: 148.0
            }
        );
        assert_eq!(
            g.visible(),
            0..20,
            "four rows show, the last in part, and one more loads"
        );
        let mut exact = grid(30);
        exact.set_view_size(Size {
            width: 640.0,
            height: 592.0,
        });
        assert_eq!(
            exact.visible(),
            0..20,
            "four whole rows show, and one more loads"
        );
        assert_eq!(g.index_at(Point { x: 182.0, y: 153.0 }), Some(5));
        assert_eq!(
            g.index_at(Point { x: 10.0, y: 10.0 }),
            None,
            "the margin is no cell"
        );
        assert_eq!(
            grid(3).index_at(Point { x: 480.0, y: 10.0 }),
            None,
            "past the last file"
        );
    }

    #[test]
    fn moving_stops_at_the_ends() {
        let g = grid(30);
        assert_eq!(g.neighbor(0, Direction::Up), 0);
        assert_eq!(g.neighbor(29, Direction::Down), 29);
        assert_eq!(g.neighbor(5, Direction::Down), 9);
        assert_eq!(g.neighbor(5, Direction::Left), 4);
        assert_eq!(grid(0).neighbor(0, Direction::Right), 0);
    }

    #[test]
    fn reveal_and_page_scroll_within_the_content() {
        let mut g = grid(30);
        g.reveal(29);
        assert_eq!(
            g.scroll_y(),
            704.0,
            "the last row's bottom meets the view's"
        );
        assert_eq!(g.cell_rect(29).max_y(), 480.0);
        g.scroll_page(Direction::Up);
        assert_eq!(g.scroll_y(), 224.0);
        g.reveal(0);
        assert_eq!(g.scroll_y(), 0.0);
        g.set_count(2);
        g.scroll(500.0);
        assert_eq!(
            g.scroll_y(),
            0.0,
            "content shorter than the view does not scroll"
        );
    }

    #[test]
    fn thumbnail_sizes_stop_at_the_ends() {
        let mut g = grid(30);
        assert!(g.zoom(1) && g.thumb_size() == 160.0);
        assert!(!g.zoom(1));
        assert!(g.zoom(-1) && g.zoom(-1) && g.zoom(-1) && g.zoom(-1) && g.thumb_size() == 32.0);
        assert!(!g.zoom(-1));
        let mut g = grid(30);
        assert!(
            g.zoom(-2) && g.thumb_size() == 64.0,
            "a delta moves that many sizes"
        );
        assert!(g.zoom(0) && g.thumb_size() == 64.0);
    }

    #[test]
    fn the_config_sets_the_thumbnail_sizes_and_padding() {
        let config = Config {
            thumb_sizes: vec![50.0, 100.0],
            thumb_size_index: 1,
            thumb_padding: 5.0,
            ..Config::default()
        };
        let mut g = GridLayout::with_config(&config);
        assert_eq!((g.thumb_size(), g.cell_size()), (100.0, 110.0));
        assert!(!g.zoom(1) && g.zoom(-1) && g.thumb_size() == 50.0);
        assert_eq!(g.max_thumb_size(), 100.0, "whatever the size shown");
        for sizes in [vec![], vec![100.0, 50.0], vec![0.0, 50.0]] {
            let odd = Config {
                thumb_sizes: sizes,
                thumb_size_index: 9,
                ..Config::default()
            };
            assert_eq!(
                GridLayout::with_config(&odd).thumb_size(),
                160.0,
                "{:?} takes the defaults, and the index stays inside them",
                odd.thumb_sizes
            );
        }
    }

    #[test]
    fn odd_coordinates_find_no_cell() {
        let mut g = grid(30);
        assert_eq!(
            g.index_at(Point {
                x: f64::NAN,
                y: 10.0
            }),
            None
        );
        assert_eq!(
            g.index_at(Point {
                x: 30.0,
                y: f64::NAN
            }),
            None
        );
        assert_eq!(
            g.index_at(Point {
                x: 30.0,
                y: f64::INFINITY
            }),
            None
        );
        g.set_view_size(Size {
            width: 640.0,
            height: f64::MAX,
        });
        assert_eq!(g.visible(), 0..30);
    }
}
