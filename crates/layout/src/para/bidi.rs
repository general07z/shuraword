use super::ParaLayout;
use super::model::{ClKind, Line, VisCl};

/// The bidi levels a piece of text is shaped by.
#[derive(Clone, Copy)]
pub(super) enum Dir<'l> {
    /// Paragraph text: the paragraph's levels at the piece's offset.
    Para,
    /// Text laid out on its own (a field result): these levels, by byte of the text.
    Own(&'l [u8]),
    /// Each strong run in its own direction (list labels, ruby, left-to-right results).
    Guess,
}

/// The direction of `c` if it is a strong letter (UAX #9 P2): `Some(true)` right to left.
pub(super) fn strong_rtl(c: char) -> Option<bool> {
    use unicode_bidi::BidiClass::{AL, L, R};
    // In ASCII only the letters are strong (L).
    if c.is_ascii() {
        return c.is_ascii_alphabetic().then_some(false);
    }
    match unicode_bidi::bidi_class(c) {
        R | AL => Some(true),
        L => Some(false),
        _ => None,
    }
}

/// Bidi embedding levels per byte of `text` (UAX #9 rules up to I2), or empty when the
/// paragraph is left to right and has no right-to-left characters.
pub fn bidi_levels(text: &str, rtl_para: bool) -> Vec<u8> {
    if !rtl_para && !text.chars().any(wordcraft_fonts::is_rtl) {
        return Vec::new();
    }
    let level = if rtl_para { unicode_bidi::Level::rtl() } else { unicode_bidi::Level::ltr() };
    let info = unicode_bidi::ParagraphBidiInfo::new(text, Some(level));
    info.levels.iter().map(|l| l.number()).collect()
}

/// [`bidi_levels`] for a left-to-right paragraph with right-to-left runs (`w:rtl`, byte ranges of
/// `text`). Word lays such a run out right to left whatever its characters: its neutral and weak
/// characters (a colon or a bracket at the end of an Arabic label) take the run's direction
/// instead of the paragraph's (ECMA-376 §17.3.2.30). Each run is read as a right-to-left
/// embedding (RLE … PDF), and the levels are mapped back onto `text`.
pub fn bidi_levels_with_rtl_runs(text: &str, rtl_runs: &[std::ops::Range<usize>]) -> Vec<u8> {
    bidi_levels_with_runs(text, false, rtl_runs)
}

/// [`bidi_levels`] with runs read in the direction opposite to the paragraph's: right-to-left
/// runs of a left-to-right paragraph, or left-to-right runs (no `w:rtl`, Latin letters only) of a
/// right-to-left one. Their neutral characters take the run's direction, as in Word: the slash
/// closing `https://www.example.org/` in an Arabic paragraph stays at the address's right end
/// instead of jumping to its left. Each run is read as an embedding (RLE or LRE … PDF), and the
/// levels are mapped back onto `text`. Ranges that don't fit `text` are ignored.
pub fn bidi_levels_with_runs(text: &str, rtl_para: bool, runs: &[std::ops::Range<usize>]) -> Vec<u8> {
    let runs: Vec<&std::ops::Range<usize>> =
        runs.iter().filter(|r| r.start < r.end && text.is_char_boundary(r.start) && text.get(r.start..r.end).is_some()).collect();
    if runs.is_empty() {
        return bidi_levels(text, rtl_para);
    }
    // A run against a right-to-left paragraph is embedded left to right, and the other way round.
    let open_mark = if rtl_para { '\u{202A}' } else { '\u{202B}' };
    const PDF: char = '\u{202C}';
    // The marked-up text, and for each of its bytes the byte of `text` it stands for (None for
    // the inserted marks).
    let mut marked = String::with_capacity(text.len() + runs.len() * 6);
    let mut origin: Vec<Option<usize>> = Vec::with_capacity(marked.capacity());
    let mut next = runs.iter().peekable();
    let mut open: Option<usize> = None;
    for (i, c) in text.char_indices() {
        if open.is_some_and(|end| i >= end) {
            marked.push(PDF);
            origin.extend(std::iter::repeat_n(None, PDF.len_utf8()));
            open = None;
        }
        while next.peek().is_some_and(|r| r.end <= i) {
            next.next();
        }
        if open.is_none() && next.peek().is_some_and(|r| r.start == i) {
            marked.push(open_mark);
            origin.extend(std::iter::repeat_n(None, open_mark.len_utf8()));
            open = next.next().map(|r| r.end);
        }
        marked.push(c);
        origin.extend((0..c.len_utf8()).map(|k| Some(i + k)));
    }
    if open.is_some() {
        marked.push(PDF);
        origin.extend(std::iter::repeat_n(None, PDF.len_utf8()));
    }
    let base = if rtl_para { unicode_bidi::Level::rtl() } else { unicode_bidi::Level::ltr() };
    let info = unicode_bidi::ParagraphBidiInfo::new(&marked, Some(base));
    let mut levels = vec![base.number(); text.len()];
    for (level, at) in info.levels.iter().zip(&origin) {
        if let Some(slot) = at.and_then(|b| levels.get_mut(b)) {
            *slot = level.number();
        }
    }
    levels
}

/// Whether a run without `w:rtl` reads left to right inside a right-to-left paragraph: it has
/// Latin (or other left-to-right) letters and no right-to-left ones. A run of only digits and
/// punctuation, or one holding Arabic, is left to the paragraph.
pub fn reads_left_to_right(run: &str) -> bool {
    let mut any = false;
    for c in run.chars() {
        match strong_rtl(c) {
            Some(true) => return false,
            Some(false) => any = true,
            None => {}
        }
    }
    any
}

/// Rule L2 of UAX #9: the visual order (left to right) of items with these levels: from the
/// highest level down to the lowest odd one, reverse every run at that level or above.
pub fn visual_order(levels: &[u8]) -> Vec<usize> {
    let mut order: Vec<usize> = (0..levels.len()).collect();
    let Some(min_odd) = levels.iter().copied().filter(|l| l % 2 == 1).min() else { return order };
    let max = levels.iter().copied().max().unwrap_or(0);
    let level_at = |order: &[usize], i: usize| order.get(i).and_then(|&o| levels.get(o)).copied().unwrap_or(0);
    let mut lvl = max;
    while lvl >= min_odd {
        let mut i = 0;
        while i < order.len() {
            if level_at(&order, i) >= lvl {
                let mut j = i;
                while j < order.len() && level_at(&order, j) >= lvl {
                    j += 1;
                }
                if let Some(run) = order.get_mut(i..j) {
                    run.reverse();
                }
                i = j;
            } else {
                i += 1;
            }
        }
        if lvl == 0 {
            break;
        }
        lvl -= 1;
    }
    order
}

/// Make a row's lines as tall as its tallest, and return that height.
pub(super) fn even_row(row: &mut [Line]) -> f32 {
    let h = row.iter().map(|l| l.height).fold(0.0f32, f32::max);
    for l in row {
        l.baseline += h - l.height;
        l.height = h;
    }
    h
}

/// Finish a broken line of a bidirectional paragraph: resolve each cluster's level for the line
/// (UAX #9 rule L1), order the clusters visually (rule L2) and give each its box. `line.xs`
/// arrive in logical order measured from the paragraph's start edge (mirrored for a
/// right-to-left paragraph); afterwards `line.vis` holds the boxes and `xs` their left edges.
pub(super) fn reorder_line(pl: &ParaLayout, line: &mut Line, width: f32) {
    let rtl_para = line.rtl;
    if rtl_para {
        let (l, r) = (line.left, line.right);
        line.left = width - r;
        line.right = width - l;
    }
    let (c0, c1) = (line.c0, line.c1);
    let n = c1.saturating_sub(c0);
    if n == 0 {
        if rtl_para {
            line.xs = line.xs.iter().map(|x| width - x).collect();
        }
        return;
    }
    let para_level = u8::from(rtl_para);
    let mut levels: Vec<u8> = (c0..c1).map(|k| pl.clusters.get(k).and_then(|c| pl.bidi_levels.get(c.start)).copied().unwrap_or(para_level)).collect();
    // L1: tabs and breaks, and the whitespace before them or at the end of the line, take the
    // paragraph level (so trailing spaces stay at the line's end edge).
    let mut trailing = true;
    for (i, k) in (c0..c1).enumerate().rev() {
        let kind = pl.clusters.get(k).map(|c| c.kind);
        let reset = match kind {
            Some(ClKind::Tab | ClKind::LineBreak) => {
                trailing = true;
                true
            }
            Some(ClKind::Space | ClKind::PageBreak | ClKind::ColumnBreak | ClKind::Marker) => trailing,
            _ => {
                trailing = false;
                false
            }
        };
        if reset && let Some(v) = levels.get_mut(i) {
            *v = para_level;
        }
    }
    if !rtl_para && levels.iter().all(|l| *l == 0) {
        return; // A left-to-right line of a paragraph with right-to-left text elsewhere.
    }
    let order = visual_order(&levels);
    let hyph = line.hyphen.map_or(0.0, |h| h.2);
    let mut w: Vec<f32> = (0..n).map(|i| line.xs.get(i + 1).zip(line.xs.get(i)).map_or(0.0, |(b, a)| b - a)).collect();
    if let Some(last) = w.last_mut() {
        *last -= hyph;
    }
    let total: f32 = w.iter().sum();
    let x0 = line.xs.first().copied().unwrap_or(0.0);
    let mut x = if rtl_para { width - x0 - total } else { x0 };
    let mut vis = vec![VisCl { x: 0.0, w: 0.0, level: para_level }; n];
    for &i in &order {
        if let (Some(v), Some(wi), Some(l)) = (vis.get_mut(i), w.get(i), levels.get(i)) {
            *v = VisCl { x, w: *wi, level: *l };
            x += *wi;
        }
    }
    line.vis = vis;
    let end = line.end_x();
    line.xs = line.vis.iter().map(|v| v.x).chain(std::iter::once(end)).collect();
}
