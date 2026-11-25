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

use posthaste_core::model::{FolderId, HttpMethod, RequestId, RequestProtocol};

use crate::app::AppState;
use crate::views::tree::{self, Reveal, RowRef};

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

/// One entry of the history list.
#[derive(Clone, Debug)]
pub(crate) struct NavHistoryRow {
    /// Index into `HistoryState::entries` at the cached generation.
    pub index: usize,
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
#[derive(Default)]
pub(crate) struct NavigationModel {
    key: Option<NavigationKey>,
    collections: Vec<NavCollection>,
    /// Collections the section holds for the current filter, whether or not
    /// the section is open.
    collection_count: usize,
    history: Vec<NavHistoryRow>,
    /// Entries the history section holds for the current filter, whether or
    /// not the section is open.
    history_count: usize,
    /// Refreshes that walked the collections tree. Test-only, so a collapsed
    /// section can prove it does no tree work.
    #[cfg(test)]
    walks: u64,
}

impl NavigationModel {
    pub(crate) fn collections(&self) -> &[NavCollection] {
        &self.collections
    }

    pub(crate) fn collection_count(&self) -> usize {
        self.collection_count
    }

    pub(crate) fn history(&self) -> &[NavHistoryRow] {
        &self.history
    }

    pub(crate) fn history_count(&self) -> usize {
        self.history_count
    }

    /// Tree walks performed so far. Test-only.
    #[cfg(test)]
    pub(crate) fn walks(&self) -> u64 {
        self.walks
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

        self.history.clear();
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
                    self.history.push(NavHistoryRow { index, linked_name });
                }
            }
            self.history_count = self.history.len();
        } else {
            self.history_count = app.history().entries.len();
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
