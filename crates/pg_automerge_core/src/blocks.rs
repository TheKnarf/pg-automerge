//! Whether a document has blocks: maps inside text objects (rich-text
//! paragraphs, headings, embeds; see docs/DESIGN.md, "Deep blocks").
//!
//! Automerge renders a block's value with `hydrate`, which recurses once
//! per level of nesting inside the block, about 13 kB of stack per level
//! in a release build. Its span iterator (`ReadDoc::spans`) and its
//! document iterator (`ReadDoc::iter_at`, whose text items are spans) do
//! that for every block they pass, so a block nested a few hundred levels
//! deep (a document of a few kB) overflows the backend's stack: a crash
//! no guard catches. `text()`, `map_range` and `list_range` never hydrate.
//!
//! [`has_blocks`] answers from the document's save: the objects its
//! map-making ops are in (`budget::any_map_parent`, run by run over the
//! object and action columns), each looked up in the loaded document for
//! its type, until the first text. Every op of the save counts, deleted ones
//! too, so the answer holds for every historical state. When the save
//! cannot be read that way (not one document chunk), the answer is `true`:
//! callers then take the path that never hydrates, which gives the same
//! result.

use automerge::{ActorId, Automerge, ObjId, ObjType, ReadDoc};

use crate::budget;

/// Whether `doc` has (or ever had) a map inside a text object, from
/// `save`, a save of `doc` (its stored bytes, `save_nocompress()`), or
/// from a save made here when `save` is `None`. `true` when the save is
/// not one readable document chunk, or names an object `doc` lacks (it
/// is then not a save of `doc`): the safe answer. Unguarded; never
/// panics on its own.
pub fn has_blocks(doc: &Automerge, save: Option<&[u8]>) -> bool {
    match save {
        Some(bytes) => blocks_in(doc, bytes),
        None => blocks_in(doc, &doc.save_nocompress()),
    }
}

fn blocks_in(doc: &Automerge, save: &[u8]) -> bool {
    budget::any_map_parent(save, |actor, index, ctr| {
        // Not an object of `doc` (so not its save): assume the worst.
        let obj = ObjId::Id(ctr, ActorId::from(actor), index);
        !matches!(
            doc.object_type(&obj),
            Ok(ObjType::Map | ObjType::List | ObjType::Table)
        )
    })
    .unwrap_or(true)
}
