//! CLI output primitives. Commands compose their stdout from [`primitives`]
//! using a [`palette`]-resolved color set and [`glyphs`] for status symbols;
//! long output is routed through [`pager`] on a TTY.
//!
//! Ported from norn's proven output layer and trimmed to what `vard` needs.

pub mod format;
pub mod glyphs;
pub mod pager;
pub mod palette;
pub mod primitives;
