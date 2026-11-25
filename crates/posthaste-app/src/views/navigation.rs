//! Cached sidebar navigation rows.
//!
//! `sidebar::render` runs on every frame, but the collections tree, the
//! filtered history, and each entry's linked request change only when the
//! workspace mutates, the search or an expansion moves, or a history entry
//! arrives. This module owns that work behind one cache key, so a repaint
//! rebuilds nothing.
//!
//! The rows are owned display data. The sidebar builds elements from them
//! while holding the cache, so painting needs no `Tree` and no
//! `&LoadedCollection`. A history row keeps its index into
//! `HistoryState::entries`, valid for the generation the cache was built at,
//! so the view never searches for an entry by id.
//!
//! `SidebarRows` is the flat index the collection tree's uniform list walks.
//! It is derived here, beside the collections it comes from, so a repaint only
//! clones an `Rc`: neither the tree nor the index is rebuilt. The model also
//! owns both scroll handles (the tree's uniform list and history's
//! variable-height list) so the cache, the rows, and the scroll positions live
//! in one place.

use std::cell::Cell;
use std::rc::Rc;

use gpui::{ListAlignment, ListState, Pixels, UniformListScrollHandle, px};
use posthaste_core::history::HistoryId;
use posthaste_core::model::{FolderId, HttpMethod, RequestId, RequestProtocol};

use crate::app::AppState;
use crate::views::tree::{self, Reveal, RevealedItem, RowRef};

/// Extra space above and below the history viewport that the list may measure.
const HISTORY_LIST_OVERDRAW: Pixels = px(240.);

/// One request row, owned so it outlives the tree that produced it.
#[derive(Clone, Debug)]
pub(crate) struct NavRequest {
    pub id: RequestId,
    pub name: String,
    pub protocol: RequestProtocol,
    pub method: HttpMethod,
    /// Distance from the collection row: `1` for a direct child.
    pub depth: usize,
}

/// One folder row, owned so it outlives the tree that produced it.
#[derive(Clone, Debug)]
pub(crate) struct NavFolder {
    pub id: FolderId,
    pub name: String,
    pub depth: usize,
    pub expanded: bool,
    /// Whether expanding the folder would reveal anything.
    pub has_children: bool,
    /// Recursive request count, shown only on a collapsed folder that has any.
    pub request_count: Option<usize>,
}

/// One row under a collection.
#[derive(Clone, Debug)]
pub(crate) enum NavRow {
    Folder(NavFolder),
    Request(NavRequest),
}

/// One collection as the sidebar shows it for the current search.
#[derive(Clone, Debug)]
pub(crate) struct NavCollection {
    pub slug: String,
    pub name: String,
    /// Every request in the collection, open or not.
    pub request_count: usize,
    /// Whether the collection branch is open in this frame.
    pub expanded: bool,
    /// The rows under an open branch. A collapsed branch is never painted, so
    /// its rows are not stored.
    pub rows: Vec<NavRow>,
}

/// One indexed row of the collection tree.
///
/// The variant carries every display field the row builder needs, so the list
/// closure resolves only selection, rename, menu, move, and save state from
/// `AppState`. The empty-request and empty-folder rows are items; the
/// empty-collections and no-match blocks are not and stay outside the list.
#[derive(Clone, Debug)]
pub(crate) enum SidebarItem {
    Collection {
        slug: String,
        name: String,
        request_count: usize,
        expanded: bool,
    },
    Folder {
        slug: String,
        folder: NavFolder,
    },
    Request {
        slug: String,
        request: NavRequest,
    },
    EmptyRequests {
        slug: String,
    },
    EmptyFolder {
        slug: String,
        id: FolderId,
        depth: usize,
    },
}

impl SidebarItem {
    /// Whether `reveal` names this row.
    fn is_revealed_by(&self, reveal: &Reveal) -> bool {
        match self {
            SidebarItem::Collection { slug, .. } => {
                reveal.slug == *slug && reveal.item == RevealedItem::Collection
            }
            SidebarItem::Folder { slug, folder } => {
                reveal.slug == *slug && reveal.item == RevealedItem::Folder(folder.id)
            }
            SidebarItem::Request { slug, request } => {
                reveal.slug == *slug && reveal.item == RevealedItem::Request(request.id)
            }
            SidebarItem::EmptyRequests { .. } | SidebarItem::EmptyFolder { .. } => false,
        }
    }
}

/// The collection tree as one flat, ordered index.
#[derive(Default, Debug)]
pub(crate) struct SidebarRows {
    pub(crate) items: Vec<SidebarItem>,
}

impl SidebarRows {
    pub(crate) fn len(&self) -> usize {
        self.items.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub(crate) fn item(&self, index: usize) -> Option<&SidebarItem> {
        self.items.get(index)
    }

    /// The index of the row `reveal` names, if the index holds it.
    pub(crate) fn index_of(&self, reveal: &Reveal) -> Option<usize> {
        self.items
            .iter()
            .position(|item| item.is_revealed_by(reveal))
    }

    /// Appends one collection and, when open, its child rows in paint order.
    fn push_collection(&mut self, collection: &NavCollection, show_empty_folders: bool) {
        self.items.push(SidebarItem::Collection {
            slug: collection.slug.clone(),
            name: collection.name.clone(),
            request_count: collection.request_count,
            expanded: collection.expanded,
        });
        if !collection.expanded {
            return;
        }
        if collection.rows.is_empty() {
            self.items.push(SidebarItem::EmptyRequests {
                slug: collection.slug.clone(),
            });
            return;
        }
        for row in &collection.rows {
            match row {
                NavRow::Folder(folder) => {
                    self.items.push(SidebarItem::Folder {
                        slug: collection.slug.clone(),
                        folder: folder.clone(),
                    });
                    // An expanded folder with nothing beneath it gets a quiet
                    // child row; it is suppressed while a search is active,
                    // where only the match itself should show.
                    if show_empty_folders && folder.expanded && !folder.has_children {
                        self.items.push(SidebarItem::EmptyFolder {
                            slug: collection.slug.clone(),
                            id: folder.id,
                            depth: folder.depth + 1,
                        });
                    }
                }
                NavRow::Request(request) => self.items.push(SidebarItem::Request {
                    slug: collection.slug.clone(),
                    request: request.clone(),
                }),
            }
        }
    }
}

/// One entry of the history list.
#[derive(Clone, Debug)]
pub(crate) struct NavHistoryRow {
    /// Index into `HistoryState::entries` at the cached generation.
    pub index: usize,
    /// The entry's identifier, so the list can reveal a selected row by index.
    pub id: HistoryId,
    /// The linked saved request's current name, when the entry links to one.
    pub linked_name: Option<String>,
}

/// Everything the cached model depends on. Equality of this key means the
/// rows are current.
#[derive(Clone, PartialEq, Eq)]
struct NavigationKey {
    /// Every workspace, history, environment, and variable change bumps it.
    generation: u64,
    active_environment: Option<String>,
    filter: String,
    /// Hash of the stored collection and folder disclosure.
    expansion: u64,
    reveal: Option<Reveal>,
    show_collections: bool,
    show_history: bool,
}

/// The sidebar's cached rows, rebuilt only when [`NavigationKey`] moves.
pub(crate) struct NavigationModel {
    key: Option<NavigationKey>,
    collections: Vec<NavCollection>,
    /// Collections the section holds for the current filter, whether or not
    /// the section is open.
    collection_count: usize,
    /// The flat collection-tree index the uniform list walks.
    rows: Rc<SidebarRows>,
    /// The uniform list's scroll handle. Reveal scrolls it by row index.
    tree_scroll: UniformListScrollHandle,
    /// The matching history entries, shared with the history list closure.
    history: Rc<Vec<NavHistoryRow>>,
    /// The variable-height history list.
    history_list: ListState,
    /// Entries the history section holds for the current filter, whether or
    /// not the section is open.
    history_count: usize,
    /// The reveal value the tree last scrolled to, so a repaint does not move
    /// the list again.
    revealed_tree: Cell<Option<Reveal>>,
    /// The selected history entry the list last scrolled to.
    revealed_history: Cell<Option<HistoryId>>,
    /// Rows the sidebar's list builders constructed. Test-only readers compare
    /// deltas; construction itself stays free of index-walk work.
    rows_built: Rc<Cell<u64>>,
    /// Refreshes that walked the collections tree. Test-only, so a collapsed
    /// section can prove it does no tree work.
    #[cfg(test)]
    walks: u64,
    /// Refreshes that rebuilt the flat index. Test-only, so a repaint can
    /// prove it reads the cached index instead of walking the tree again.
    #[cfg(test)]
    index_builds: u64,
}

impl Default for NavigationModel {
    fn default() -> Self {
        Self {
            key: None,
            collections: Vec::new(),
            collection_count: 0,
            rows: Rc::new(SidebarRows::default()),
            tree_scroll: UniformListScrollHandle::new(),
            history: Rc::from(Vec::new()),
            // Measure every row once when the list is (re)built. Rows are
            // variable height, and a selected offscreen entry can only be
            // revealed exactly when every row's height is known. The one-time
            // measurement is bounded by history retention (default 500, max
            // 5000) and happens only on list (re)build; later repaints
            // construct only the visible window and its overdraw.
            history_list: ListState::new(0, ListAlignment::Top, HISTORY_LIST_OVERDRAW)
                .measure_all(),
            history_count: 0,
            revealed_tree: Cell::new(None),
            revealed_history: Cell::new(None),
            rows_built: Rc::new(Cell::new(0)),
            #[cfg(test)]
            walks: 0,
            #[cfg(test)]
            index_builds: 0,
        }
    }
}

impl NavigationModel {
    pub(crate) fn collection_count(&self) -> usize {
        self.collection_count
    }

    /// The flat collection-tree index. Cloning the handle is O(1), so a
    /// repaint never copies the rows.
    pub(crate) fn rows(&self) -> &Rc<SidebarRows> {
        &self.rows
    }

    pub(crate) fn tree_scroll(&self) -> UniformListScrollHandle {
        self.tree_scroll.clone()
    }

    pub(crate) fn history(&self) -> &[NavHistoryRow] {
        self.history.as_slice()
    }

    /// The matching history rows, shared with the list closure.
    pub(crate) fn history_rows(&self) -> Rc<Vec<NavHistoryRow>> {
        Rc::clone(&self.history)
    }

    pub(crate) fn history_scroll(&self) -> ListState {
        self.history_list.clone()
    }

    pub(crate) fn history_count(&self) -> usize {
        self.history_count
    }

    /// The counter the sidebar's row builders increment. Test-only readers
    /// take deltas around a frame.
    pub(crate) fn rows_built_counter(&self) -> Rc<Cell<u64>> {
        Rc::clone(&self.rows_built)
    }

    /// The tree index to reveal, returned once per reveal value.
    ///
    /// The handles and the last revealed value live here, so a later repaint
    /// with the same reveal does not move the list. Resetting or revealing a
    /// different item makes the next call scroll again.
    pub(crate) fn take_tree_reveal(&self, reveal: Option<&Reveal>) -> Option<usize> {
        let previous = self.revealed_tree.replace(reveal.cloned());
        if previous.as_ref() == reveal {
            return None;
        }
        reveal.and_then(|reveal| self.rows.index_of(reveal))
    }

    /// The history index to reveal, returned once per selected entry.
    ///
    /// A selection the current filter hides is not consumed: the stored value
    /// stays put, so clearing the filter can still reveal it later.
    pub(crate) fn take_history_reveal(&self, selected: Option<HistoryId>) -> Option<usize> {
        let id = selected?;
        let index = self.history.iter().position(|row| row.id == id)?;
        if self.revealed_history.get() == Some(id) {
            return None;
        }
        self.revealed_history.set(Some(id));
        Some(index)
    }

    /// Tree walks performed so far. Test-only.
    #[cfg(test)]
    pub(crate) fn walks(&self) -> u64 {
        self.walks
    }

    /// Index builds performed so far. Test-only.
    #[cfg(test)]
    pub(crate) fn index_builds(&self) -> u64 {
        self.index_builds
    }

    /// Rows both lists have constructed so far. Test-only.
    #[cfg(test)]
    pub(crate) fn rows_built(&self) -> u64 {
        self.rows_built.get()
    }

    /// The tree list's current vertical scroll offset. Test-only.
    #[cfg(test)]
    pub(crate) fn tree_offset(&self) -> Pixels {
        self.tree_scroll.0.borrow().base_handle.offset().y
    }

    /// The history list's current vertical scroll offset. Test-only.
    #[cfg(test)]
    pub(crate) fn history_offset(&self) -> Pixels {
        self.history_list.scroll_px_offset_for_scrollbar().y
    }

    /// The history list's largest vertical scroll offset. Test-only.
    #[cfg(test)]
    pub(crate) fn history_max_offset(&self) -> Pixels {
        self.history_list.max_offset_for_scrollbar().height
    }

    /// Clears the row-build counter. Test-only, so a repaint can be measured.
    #[cfg(test)]
    pub(crate) fn reset_rows_built(&self) {
        self.rows_built.set(0);
    }

    /// Rebuilds the rows when the key no longer matches.
    ///
    /// A closed section stores only its count: neither the collections tree
    /// nor the history list is walked while it cannot be painted. A closed
    /// collections section only happens with an empty filter, where the count
    /// is the loaded collections themselves.
    pub(crate) fn refresh(
        &mut self,
        app: &AppState,
        show_collections: bool,
        show_history: bool,
        expansion: u64,
    ) {
        let filter = app.sidebar_filter().trim().to_lowercase();
        let reveal = app.sidebar_reveal();
        let key = NavigationKey {
            generation: app.workspace().navigation_generation(),
            active_environment: app.active_environment().map(str::to_owned),
            filter: filter.clone(),
            expansion,
            reveal: reveal.clone(),
            show_collections,
            show_history,
        };
        if self.key.as_ref() == Some(&key) {
            return;
        }

        self.collections.clear();
        if show_collections {
            #[cfg(test)]
            {
                self.walks += 1;
            }
            let visible = tree::visible_collections(
                &app.workspace().collections,
                &filter,
                |slug| app.collection_expanded(slug),
                |id| app.folder_expanded(id),
                reveal.as_ref(),
            );
            self.collection_count = visible.len();
            self.collections = visible
                .into_iter()
                .map(|visible| NavCollection {
                    slug: visible.entry.collection.id.clone(),
                    name: visible.entry.collection.name.clone(),
                    request_count: visible.entry.requests.len(),
                    expanded: visible.expanded,
                    rows: if visible.expanded {
                        visible.rows.into_iter().map(nav_row).collect()
                    } else {
                        Vec::new()
                    },
                })
                .collect();
        } else {
            self.collection_count = app.workspace().collections.len();
        }

        // The flat index follows the same visible rows the tree built, in the
        // same order: a collection, then its folders and requests, with the
        // empty child rows in the positions they paint.
        let mut rows = SidebarRows::default();
        if show_collections {
            let show_empty_folders = filter.is_empty();
            for collection in &self.collections {
                rows.push_collection(collection, show_empty_folders);
            }
        }
        self.rows = Rc::new(rows);

        let mut history = Vec::new();
        if show_history {
            // The linked request is resolved once per entry, not once per
            // rendered row: the resolution compares a whole spec, and the
            // name it yields is what the row shows and what the search reads.
            for (index, entry) in app.history().entries.iter().enumerate() {
                let linked_name = app
                    .linked_history_request(entry)
                    .map(|request| request.name.clone());
                let matches = filter.is_empty()
                    || entry.request.url.to_lowercase().contains(&filter)
                    || entry
                        .request
                        .method
                        .as_str()
                        .to_lowercase()
                        .contains(&filter)
                    || linked_name
                        .as_deref()
                        .is_some_and(|name| name.to_lowercase().contains(&filter));
                if matches {
                    history.push(NavHistoryRow {
                        index,
                        id: entry.id,
                        linked_name,
                    });
                }
            }
            self.history_count = history.len();
        } else {
            self.history_count = app.history().entries.len();
        }

        // The list's measured rows must follow the count, or a deleted entry
        // leaves a phantom row and a new one is never measured. The old and new
        // id sequences share a prefix and a suffix around whatever changed, so
        // an append, a prepend, a filter hiding a middle entry, or a middle
        // deletion splices only the difference. Unchanged rows keep their
        // measurements.
        //
        // A closed section empties the list, and reopening it splices in rows
        // with no measured height. That transition resets the list instead, so
        // the next paint measures every row and the scrollbar and an offscreen
        // reveal stay exact.
        let old_history = self.history.as_slice();
        let old_len = old_history.len();
        let previous = self.history_list.item_count();
        let next = history.len();
        if previous != old_len {
            // The list diverged from the rows (an earlier transition reset one
            // but not the other); rebuild the count rather than splice.
            self.history_list.reset(next);
        } else if next == 0 {
            self.history_list.reset(0);
        } else if previous == 0 {
            self.history_list.reset(next);
        } else {
            let prefix = old_history
                .iter()
                .zip(&history)
                .take_while(|(old, new)| old.id == new.id)
                .count();
            let shared = old_len.min(next) - prefix;
            let suffix = old_history
                .iter()
                .rev()
                .zip(history.iter().rev())
                .take(shared)
                .take_while(|(old, new)| old.id == new.id)
                .count();
            let removed = old_len - prefix - suffix;
            let added = next - prefix - suffix;
            if removed != 0 || added != 0 {
                self.history_list.splice(prefix..old_len - suffix, added);
            }
        }
        self.history = Rc::new(history);

        #[cfg(test)]
        {
            self.index_builds += 1;
        }
        self.key = Some(key);
    }
}

fn nav_row(row: tree::TreeRow<'_>) -> NavRow {
    let depth = row.depth;
    match row.row {
        RowRef::Folder(folder) => NavRow::Folder(NavFolder {
            id: folder.id,
            name: folder.name.clone(),
            depth,
            expanded: row.expanded,
            has_children: row.has_children,
            request_count: row.request_count,
        }),
        RowRef::Request(request) => NavRow::Request(NavRequest {
            id: request.id(),
            name: request.name().to_owned(),
            protocol: request.protocol(),
            method: request.method(),
            depth,
        }),
    }
}
