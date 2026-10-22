//! The **folder tile** — what a folder row in the Browse by Folders listing draws in place of a
//! poster: the absence ground (`theme::CARD_ABSENT`) and the folder mark, centred.
//!
//! A folder row carries no artwork (the server's row is only a `key` and a `title`), so without this
//! the tile is a bare skeleton that reads as "still loading" forever. The folder's NAME is not drawn
//! here: unlike a collection, a folder tile's name rides the ordinary focused caption every poster
//! wears, so the face carries only the mark that says what the press opens.
//!
//! One widget, reached by the folder listing's `CardSource` overlay; it names no application type.
//! The glyph is laid out on the tile's RESTING rect and drawn through the card's own zoom, so a
//! focus pop grows the same cached mask rather than re-rasterising it (`collection_tile`'s rule).
use crate::icons::{self, Icon};
use crate::{theme, Painter, Rect, Zoom};

/// The mark's box as a fraction of the tile's smaller side — the same visual weight the collection
/// tile's mark carries on a poster cell.
const GLYPH_FRAC: f32 = 0.34;

/// Draw the folder face at `r` (the rect it is drawn at, popped when focused) with corner radius
/// `rad`; `rest` is its RESTING rect, which the mark is laid out on and grown with the card.
pub fn draw(p: Painter, rest: Rect, r: Rect, rad: f32) {
    // A folder has no artwork of its own and none is ever sent: ABSENCE, never a wait.
    // placeholder-exempt: absence, a folder row carries no thumb; nothing will arrive
    p.rrect_sheened(r, rad, theme::CARD_ABSENT);
    let side = rest.w.min(rest.h) * GLYPH_FRAC;
    let glyph = Rect::new(rest.cx() - side * 0.5, rest.cy() - side * 0.5, side, side);
    let s = r.w / rest.w.max(1.0);
    let (dx, dy) = (r.cx() - rest.cx(), r.cy() - rest.cy());
    if s == 1.0 && dx == 0.0 && dy == 0.0 {
        icons::draw(p, Icon::Folder, glyph, theme::TEXT_TERTIARY);
        return;
    }
    icons::draw(p.translate(dx, dy).zoomed(Zoom::about(rest, (0.5, 0.5), s)), Icon::Folder, glyph, theme::TEXT_TERTIARY);
}
