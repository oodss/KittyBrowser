//! During a wheel gesture Chrome can capture fewer pixels without reflowing
//! the page. Kitty can scale a cropped image to a whole number of cells.
//! Looking for matches at those boundaries lets Rex pan an existing image
//! even when the actual scroll distance is a fraction of a terminal row.
use super::graphics::{decode_png, large_image_file, transmit, FrameFiles, Pixels};
use std::collections::HashMap;
use std::io::Write;
use std::ops::Range;

const BASE_Z: i32 = -1_073_741_826;
const PATCH_Z: i32 = BASE_Z + 1;

struct Base {
    id: u32,
    pixels: Pixels,
    unique: HashMap<u64, Option<usize>>,
}

fn stride(p: &Pixels) -> usize {
    p.width as usize * if p.alpha { 4 } else { 3 }
}

// A sparse fingerprint only suggests a source row. The byte comparison below
// includes every pixel in the band, including overlays and sticky headers.
fn fingerprint(row: &[u8]) -> u64 {
    row.iter().step_by((row.len() / 32).max(1)).fold(1u64, |h, b| h.rotate_left(7) ^ u64::from(*b))
}

impl Base {
    fn new(id: u32, pixels: Pixels) -> Self {
        let mut unique = HashMap::new();
        for (y, row) in pixels.data.chunks_exact(stride(&pixels)).enumerate() {
            unique.entry(fingerprint(row)).and_modify(|v| *v = None).or_insert(Some(y));
        }
        Self { id, pixels, unique }
    }

    fn source(&self, new: &Pixels, range: Range<usize>, previous: Option<usize>) -> Option<usize> {
        let width = stride(new);
        let band = &new.data[range.start * width..range.end * width];
        let matches = |y: &usize| self.pixels.data.get(y * width..y * width + band.len()) == Some(band);
        [previous, Some(range.start)].into_iter().flatten().find(matches).or_else(|| {
            for offset in [0, range.len() / 2, range.len() - 1] {
                let row = &band[offset * width..][..width];
                if let Some(y) =
                    self.unique.get(&fingerprint(row)).copied().flatten().and_then(|y| y.checked_sub(offset))
                {
                    if matches(&y) {
                        return Some(y);
                    }
                }
            }
            None
        })
    }
}

struct Band {
    cells: Range<u16>,
    pixels: Range<usize>,
    source: Option<(u32, usize)>,
}

fn bands(images: &[&Base], new: &Pixels, rows: u16) -> Vec<Band> {
    let edge = |r: u16| (u64::from(r) * u64::from(new.height) / u64::from(rows)) as usize;
    let mut bands: Vec<Band> = Vec::new();
    for r in 0..rows {
        let pixels = edge(r)..edge(r + 1);
        let next = bands.last().and_then(|b| b.source.map(|(id, y)| (id, y + b.pixels.len())));
        let source = images.iter().find_map(|image| {
            let previous = next.filter(|(id, _)| *id == image.id).map(|(_, y)| y);
            image.source(new, pixels.clone(), previous).map(|y| (image.id, y))
        });
        if let Some(last) = bands.last_mut().filter(|_| source == next) {
            last.cells.end += 1;
            last.pixels.end = pixels.end;
        } else {
            bands.push(Band { cells: r..r + 1, pixels, source });
        }
    }
    bands
}

#[derive(Default)]
pub(super) struct Preview {
    // Split a single terminal image into independently matched columns here:
    // an animation on the left should not invalidate text on the right.
    columns: Vec<Base>,
    grid: (u16, u16, u32, u32, bool),
    // Only strips still visible in the latest frame can be reused next time.
    // The key is the terminal column. Each strip has its own source origin.
    patches: Vec<(u32, Base)>,
    // Visible overlays as (image, placement); the base is placed separately.
    overlays: Vec<(u32, u32)>,
    serial: u32,
    refresh: bool,
}

impl Preview {
    pub fn holds(&self, id: u32) -> bool {
        self.columns.first().is_some_and(|base| base.id == id) || self.patches.iter().any(|(_, p)| p.id == id)
    }

    pub fn retire(&self, out: &mut Vec<u8>) {
        for image in self.columns.first().into_iter().chain(self.patches.iter().map(|(_, image)| image)) {
            write!(out, "\x1b_Ga=d,d=I,i={},q=2\x1b\\", image.id).unwrap();
        }
    }

    pub fn refresh(&mut self) {
        self.refresh = true;
    }

    fn id(&mut self) -> u32 {
        loop {
            self.serial = self.serial.wrapping_add(1).max(2_000_000);
            if !self.holds(self.serial) {
                return self.serial;
            }
        }
    }

    pub fn draw(
        &mut self,
        out: &mut Vec<u8>,
        png: &str,
        cols: u16,
        rows: u16,
        top: u16,
        files: &mut FrameFiles,
    ) -> Option<bool> {
        let encoded = crate::base64::decode(png)?;
        let new = decode_png(&encoded, Vec::new())?;
        let grid = (cols, rows, new.width, new.height, new.alpha);
        if cols == 0 || rows == 0 {
            return None;
        }
        let count = if new.width >= u32::from(cols) { cols.min(4) } else { 1 };
        let edge = |column: u16| u32::from(column) * u32::from(cols) / u32::from(count);
        let mut reused = 0;
        let mut commands = 0;
        let mut columns = Vec::new();
        for column in 0..count {
            let (left, right) = (edge(column), edge(column + 1));
            let pixel = |cell: u32| (u64::from(cell) * u64::from(new.width) / u64::from(cols)) as u32;
            let x = pixel(left);
            let pixels = super::graphics::cut(&new, x, 0, pixel(right) - x, new.height);
            let mut plan = Vec::new();
            if !self.refresh && self.grid == grid && new.height >= u32::from(rows) {
                let base = &self.columns[column as usize];
                let sources: Vec<_> = std::iter::once(base)
                    .chain(self.patches.iter().filter(|(x, _)| *x == left + 1).map(|(_, patch)| patch))
                    .collect();
                plan = bands(&sources, &pixels, rows);
                reused += plan
                    .iter()
                    .filter(|b| b.source.is_some())
                    .map(|b| b.pixels.len() * pixels.width as usize)
                    .sum::<usize>();
                commands += plan.iter().filter(|b| b.source != Some((base.id, b.pixels.start))).count();
            }
            columns.push((left + 1, right - left, x, pixels, plan));
        }
        // Refresh when keeping a fragmented or largely replaced reference
        // would cost more work or memory than forwarding Chrome's PNG.
        let area = new.width as usize * new.height as usize;
        let retained: usize = self.patches.iter().map(|(_, p)| p.pixels.data.len()).sum();
        let whole = reused * 2 < area || commands > 32 || retained > 2 * new.data.len();
        let mut retire = std::mem::take(&mut self.overlays);
        if whole {
            let id = self.id();
            let keys = format!("a=T,f=100,i={id},p=1,c={cols},r={rows},z={BASE_Z},C=1");
            write!(out, "\x1b[{top};1H").unwrap();
            if !large_image_file(out, Some(files), id, &keys, &encoded) {
                transmit(out, &format!("{keys},q=2"), png);
            }
            if let Some(previous) = self.columns.first() {
                retire.push((previous.id, 0));
            }
            self.columns = columns.into_iter().map(|(_, _, _, pixels, _)| Base::new(id, pixels)).collect();
            self.grid = grid;

            self.refresh = false;
        } else {
            let base_id = self.columns[0].id;
            for (column, cols, x, pixels, plan) in columns {
                for b in plan {
                    if b.source == Some((base_id, b.pixels.start)) {
                        continue;
                    }
                    let id = self.id();
                    let row = top + b.cells.start;
                    let count = b.cells.len();
                    let height = b.pixels.len();
                    let width = pixels.width;
                    write!(out, "\x1b[{row};{column}H").unwrap();
                    if let Some((image, y)) = b.source {
                        let x = if image == base_id { x } else { 0 };
                        write!(
                            out,
                            "\x1b_Ga=p,i={image},p={id},x={x},y={y},w={width},h={height},c={cols},r={count},z={PATCH_Z},C=1,q=1\x1b\\"
                        ).unwrap();
                        self.overlays.push((image, id));
                    } else {
                        let row_bytes = stride(&pixels);
                        let raw = &pixels.data[b.pixels.start * row_bytes..b.pixels.end * row_bytes];
                        let data = miniz_oxide::deflate::compress_to_vec_zlib(raw, 1);
                        let format = if new.alpha { 32 } else { 24 };
                        let keys = format!(
                            "a=T,f={format},s={width},v={height},o=z,i={id},p=1,c={cols},r={count},z={PATCH_Z},C=1"
                        );
                        if !large_image_file(out, Some(files), id, &keys, &data) {
                            transmit(out, &format!("{keys},q=2"), &crate::base64::encode(&data));
                        }
                        self.patches.push((
                            column,
                            Base::new(
                                id,
                                Pixels { width, height: height as u32, alpha: new.alpha, data: raw.to_vec() },
                            ),
                        ));
                        self.overlays.push((id, 1));
                    }
                }
            }
        }
        self.patches.retain(|(_, patch)| {
            let visible = self.overlays.iter().any(|&(image, _)| image == patch.id);
            if !visible {
                retire.push((patch.id, 0));
            }
            visible
        });
        for (id, placement) in retire {
            if placement == 0 {
                write!(out, "\x1b_Ga=d,d=I,i={id},q=2\x1b\\").unwrap();
            } else {
                write!(out, "\x1b_Ga=d,d=i,i={id},p={placement},q=2\x1b\\").unwrap();
            }
        }
        Some(whole)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::term::layers::tests::{encode, frame, Screen};

    #[test]
    fn scroll_reversal_sticky_header_and_refresh_reconstruct_the_preview() {
        let mut preview = Preview::default();
        let mut screen = Screen::default();
        let mut files = FrameFiles::default();
        let cell = (8, 4);
        for shift in [0, 3, 9, 3, 0, 33, 6] {
            let pixels = frame(40, 40, |x, y| ((if y < 4 { y } else { y + shift }) * 7 + x) as u8);
            let mut out = Vec::new();
            let whole = preview.draw(&mut out, &encode(&pixels), 5, 10, 3, &mut files).unwrap();
            if shift == 9 {
                assert!(!whole);
                assert!(String::from_utf8_lossy(&out).contains("a=p,"));
                preview.refresh();
            }
            // In preview coordinates each cell covers eight by four samples.
            // The same crop is scaled to physical cells by Rex.
            screen.feed(&out, cell);
            assert_eq!(screen.page(3, cell, 40, 40), pixels.data);
        }
    }

    #[test]
    fn hash_collisions_and_uneven_cell_edges_preserve_every_row() {
        let base = Base::new(1, frame(77, 43, |x, y| (x + y * 7) as u8));
        let mut new = frame(77, 43, |x, y| (x + y * 7) as u8);
        // A changed byte between fingerprint samples must still travel.
        new.data[11 * stride(&base.pixels) + 1] ^= 1;
        let plan = bands(&[&base], &new, 10);
        assert_eq!(plan.first().unwrap().pixels.start, 0);
        assert_eq!(plan.last().unwrap().pixels.end, 43);
        assert!(plan.iter().any(|b| b.pixels.contains(&11) && b.source.is_none()));
        assert!(plan.windows(2).all(|b| b[0].pixels.end == b[1].pixels.start && b[0].cells.end == b[1].cells.start));
    }
}
