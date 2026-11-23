//! Pure tree assembly and filtering for the collections sidebar.
//!
//! The sidebar renders a flat list of rows; nesting only changes a row's
//! indent. This module owns sibling ordering, depth, subtree counts, filtering,
//! and reveal rules so the view code maps rows to elements without walking the
//! tree itself. It has no gpui dependency and is tested without a window.

use std::collections::{HashMap, HashSet};

use posthaste_core::model::{Folder, FolderId, RequestId};
use posthaste_core::storage::{LoadedCollection, RequestEntry};

/// Whether any value contains the filter. The filter is expected to be already
/// lowercased and trimmed; an empty filter matches everything.
pub(crate) fn matches_filter(filter: &str, values: &[&str]) -> bool {
    filter.is_empty()
        || values
            .iter()
            .any(|value| value.to_lowercase().contains(filter))
}

/// The item a tree row renders.
#[derive(Clone, Copy, Debug)]
pub(crate) enum RowRef<'a> {
    Folder(&'a Folder),
    Request(&'a RequestEntry),
}

/// The item a search or a just-completed action keeps visible even when it
/// does not match the current filter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RevealedItem {
    Collection,
    Folder(FolderId),
    Request(RequestId),
}

/// One revealed item and the collection that contains it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Reveal {
    pub slug: String,
    pub item: RevealedItem,
}

impl Reveal {
    pub fn collection(slug: impl Into<String>) -> Self {
        Self {
            slug: slug.into(),
            item: RevealedItem::Collection,
        }
    }

    pub fn folder(slug: impl Into<String>, id: FolderId) -> Self {
        Self {
            slug: slug.into(),
            item: RevealedItem::Folder(id),
        }
    }

    pub fn request(slug: impl Into<String>, id: RequestId) -> Self {
        Self {
            slug: slug.into(),
            item: RevealedItem::Request(id),
        }
    }
}

/// One row of the flattened tree.
#[derive(Clone, Debug)]
pub(crate) struct TreeRow<'a> {
    pub row: RowRef<'a>,
    /// Distance from the collection row: `1` for a direct child.
    pub depth: usize,
    /// Whether expanding the folder would reveal anything. The sidebar reads
    /// this to add a "No requests" child row for an expanded empty folder.
    pub has_children: bool,
    pub expanded: bool,
    /// Recursive request count, shown only on a collapsed folder that has any.
    pub request_count: Option<usize>,
}

/// One collection as the sidebar shows it for the current search.
pub(crate) struct VisibleCollection<'a> {
    pub entry: &'a LoadedCollection,
    pub rows: Vec<TreeRow<'a>>,
    /// Whether the collection branch is open in this frame.
    pub expanded: bool,
}

/// A loaded collection indexed for tree rendering.
///
/// Child lists are sorted once when the index is built, and recursive request
/// counts are cached in a post-order walk, so rendering a row never walks its
/// subtree.
pub(crate) struct Tree<'a> {
    folder_children: HashMap<Option<FolderId>, Vec<&'a Folder>>,
    folder_parents: HashMap<FolderId, Option<FolderId>>,
    folder_requests: HashMap<Option<FolderId>, Vec<&'a RequestEntry>>,
    counts: HashMap<FolderId, usize>,
}

/// Which rows a walk shows. Filtering never mutates the stored expansion: the
/// filtered variant carries the sets computed for this frame only.
enum Visibility<'s> {
    All,
    Filtered {
        folders: &'s HashSet<FolderId>,
        requests: &'s HashSet<RequestId>,
        expanded: &'s HashSet<FolderId>,
    },
}

impl Visibility<'_> {
    fn shows_folder(&self, id: FolderId) -> bool {
        match self {
            Self::All => true,
            Self::Filtered { folders, .. } => folders.contains(&id),
        }
    }

    fn shows_request(&self, id: RequestId) -> bool {
        match self {
            Self::All => true,
            Self::Filtered { requests, .. } => requests.contains(&id),
        }
    }
}

impl<'a> Tree<'a> {
    pub fn new(entry: &'a LoadedCollection) -> Self {
        let mut folder_children: HashMap<Option<FolderId>, Vec<&'a Folder>> = HashMap::new();
        let mut folder_parents = HashMap::new();
        for folder in entry.folders() {
            folder_children
                .entry(folder.parent_id)
                .or_default()
                .push(folder);
            folder_parents.insert(folder.id, folder.parent_id);
        }
        for children in folder_children.values_mut() {
            children.sort_by_cached_key(|folder| (folder.name.to_lowercase(), folder.id));
        }

        let mut folder_requests: HashMap<Option<FolderId>, Vec<&'a RequestEntry>> = HashMap::new();
        for request in &entry.requests {
            folder_requests
                .entry(request.folder_id())
                .or_default()
                .push(request);
        }
        for requests in folder_requests.values_mut() {
            requests.sort_by_cached_key(|request| (request.name().to_lowercase(), request.id()));
        }

        let mut counts = HashMap::new();
        let mut visiting = HashSet::new();
        for folder in entry.folders() {
            count_folder(
                folder.id,
                &folder_children,
                &folder_requests,
                &mut counts,
                &mut visiting,
            );
        }

        Self {
            folder_children,
            folder_parents,
            folder_requests,
            counts,
        }
    }

    /// Child folders of `parent_id` in sibling order. `None` is the collection
    /// root.
    pub fn children(&self, parent_id: Option<FolderId>) -> &[&'a Folder] {
        self.folder_children
            .get(&parent_id)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    /// Requests directly inside `parent_id` in sibling order.
    pub fn requests(&self, parent_id: Option<FolderId>) -> &[&'a RequestEntry] {
        self.folder_requests
            .get(&parent_id)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    /// The parent of `id`, or `None` for a root folder or an unknown identifier.
    pub fn parent(&self, id: FolderId) -> Option<FolderId> {
        self.folder_parents.get(&id).copied().flatten()
    }

    /// Number of requests in `id` and every folder below it.
    pub fn folder_count(&self, id: FolderId) -> usize {
        self.counts.get(&id).copied().unwrap_or(0)
    }

    /// The full tree, flattened in render order.
    ///
    /// Folders precede requests at every level, and a collapsed folder hides
    /// its descendants without losing their expansion state: `expanded` is
    /// consulted only for rows that are visible.
    pub fn rows(&self, expanded: impl Fn(FolderId) -> bool) -> Vec<TreeRow<'a>> {
        let mut rows = Vec::new();
        let mut visited = HashSet::new();
        self.push_rows(
            None,
            1,
            &Visibility::All,
            &expanded,
            &mut visited,
            &mut rows,
        );
        rows
    }

    /// The subset of the tree selected for an active search.
    ///
    /// Only folders and requests present in the sets render; only folders in
    /// `expanded` open, which keeps ancestor rows expanded for structure while
    /// leaving everything else collapsed.
    pub fn filtered_rows(
        &self,
        folders: &HashSet<FolderId>,
        requests: &HashSet<RequestId>,
        expanded: &HashSet<FolderId>,
    ) -> Vec<TreeRow<'a>> {
        let mut rows = Vec::new();
        let mut visited = HashSet::new();
        let visibility = Visibility::Filtered {
            folders,
            requests,
            expanded,
        };
        self.push_rows(None, 1, &visibility, &|_| false, &mut visited, &mut rows);
        rows
    }

    fn push_rows<F: Fn(FolderId) -> bool>(
        &self,
        parent_id: Option<FolderId>,
        depth: usize,
        visibility: &Visibility<'_>,
        stored_expanded: &F,
        visited: &mut HashSet<FolderId>,
        rows: &mut Vec<TreeRow<'a>>,
    ) {
        for folder in self.children(parent_id) {
            if !visibility.shows_folder(folder.id) {
                continue;
            }
            // Load validation removes cycles, but the walk must stay finite
            // even for hand-built data.
            if !visited.insert(folder.id) {
                continue;
            }
            let has_children = self.has_children(folder.id);
            let is_expanded = match visibility {
                Visibility::All => stored_expanded(folder.id),
                Visibility::Filtered { expanded, .. } => expanded.contains(&folder.id),
            };
            let count = self.folder_count(folder.id);
            rows.push(TreeRow {
                row: RowRef::Folder(folder),
                depth,
                has_children,
                expanded: is_expanded,
                request_count: (!is_expanded && count > 0).then_some(count),
            });
            if is_expanded {
                self.push_rows(
                    Some(folder.id),
                    depth + 1,
                    visibility,
                    stored_expanded,
                    visited,
                    rows,
                );
            }
        }
        for request in self.requests(parent_id) {
            if !visibility.shows_request(request.id()) {
                continue;
            }
            rows.push(TreeRow {
                row: RowRef::Request(request),
                depth,
                has_children: false,
                expanded: false,
                request_count: None,
            });
        }
    }

    fn has_children(&self, id: FolderId) -> bool {
        !self.children(Some(id)).is_empty() || !self.requests(Some(id)).is_empty()
    }

    /// Includes `id`, every descendant folder, and every request in the
    /// subtree. Every included folder opens so the whole subtree renders.
    fn mark_subtree(
        &self,
        id: FolderId,
        folders: &mut HashSet<FolderId>,
        requests: &mut HashSet<RequestId>,
        expanded: &mut HashSet<FolderId>,
    ) {
        self.mark_ancestors(id, folders, expanded);
        self.mark_descendants(id, folders, requests, expanded);
    }

    fn mark_descendants(
        &self,
        id: FolderId,
        folders: &mut HashSet<FolderId>,
        requests: &mut HashSet<RequestId>,
        expanded: &mut HashSet<FolderId>,
    ) {
        folders.insert(id);
        expanded.insert(id);
        for child in self.children(Some(id)) {
            self.mark_descendants(child.id, folders, requests, expanded);
        }
        for request in self.requests(Some(id)) {
            requests.insert(request.id());
        }
    }

    /// Includes every folder above `id` and forces it open. Stops on a repeat
    /// so a hand-built cycle cannot loop forever.
    fn mark_ancestors(
        &self,
        id: FolderId,
        folders: &mut HashSet<FolderId>,
        expanded: &mut HashSet<FolderId>,
    ) {
        let mut current = self.parent(id);
        while let Some(folder_id) = current {
            if !folders.insert(folder_id) {
                break;
            }
            expanded.insert(folder_id);
            current = self.parent(folder_id);
        }
    }
}

fn count_folder(
    id: FolderId,
    children: &HashMap<Option<FolderId>, Vec<&Folder>>,
    requests: &HashMap<Option<FolderId>, Vec<&RequestEntry>>,
    counts: &mut HashMap<FolderId, usize>,
    visiting: &mut HashSet<FolderId>,
) -> usize {
    if let Some(count) = counts.get(&id) {
        return *count;
    }
    if !visiting.insert(id) {
        return 0;
    }
    let mut total = requests.get(&Some(id)).map_or(0, Vec::len);
    if let Some(child_folders) = children.get(&Some(id)) {
        for child in child_folders {
            total += count_folder(child.id, children, requests, counts, visiting);
        }
    }
    visiting.remove(&id);
    counts.insert(id, total);
    total
}

/// Resolves which collections and rows the sidebar renders.
///
/// Without a search, everything is visible and stored expansion decides each
/// branch. A collection name or identifier match shows the collection's whole
/// tree with every folder open; otherwise a folder match shows that folder, its
/// ancestors, and its entire subtree, and a request match shows the request and
/// its ancestor chain. A folder included only for structure is forced open. The
/// collection is hidden when nothing in it matches unless the collection itself
/// is revealed. Stored expansion is only read, never changed, so clearing the
/// search restores the tree exactly.
pub(crate) fn visible_collections<'a>(
    collections: &'a [LoadedCollection],
    filter: &str,
    expanded_collection: impl Fn(&str) -> bool,
    expanded_folder: impl Fn(FolderId) -> bool,
    reveal: Option<&Reveal>,
) -> Vec<VisibleCollection<'a>> {
    collections
        .iter()
        .filter_map(|entry| {
            let slug = entry.collection.id.as_str();
            if filter.is_empty() {
                return Some(VisibleCollection {
                    entry,
                    rows: Tree::new(entry).rows(&expanded_folder),
                    expanded: expanded_collection(slug),
                });
            }

            let collection_match = matches_filter(
                filter,
                &[entry.collection.name.as_str(), entry.collection.id.as_str()],
            );
            if collection_match {
                return Some(VisibleCollection {
                    entry,
                    rows: Tree::new(entry).rows(|_| true),
                    expanded: true,
                });
            }

            let tree = Tree::new(entry);
            let (folders, requests, forced) = filtered_selection(entry, &tree, filter, reveal);
            let rows = tree.filtered_rows(&folders, &requests, &forced);
            let revealed_collection = reveal.is_some_and(|reveal| {
                reveal.slug == entry.collection.id
                    && matches!(reveal.item, RevealedItem::Collection)
            });
            if rows.is_empty() && !revealed_collection {
                return None;
            }
            Some(VisibleCollection {
                entry,
                expanded: !rows.is_empty(),
                rows,
            })
        })
        .collect()
}

/// The folder and request sets an active search keeps, plus the folders forced
/// open so a matched descendant stays reachable.
fn filtered_selection(
    entry: &LoadedCollection,
    tree: &Tree<'_>,
    filter: &str,
    reveal: Option<&Reveal>,
) -> (HashSet<FolderId>, HashSet<RequestId>, HashSet<FolderId>) {
    let mut folders = HashSet::new();
    let mut requests = HashSet::new();
    let mut expanded = HashSet::new();

    // A folder match keeps the folder, its ancestors, and its whole subtree.
    for folder in entry.folders() {
        if matches_filter(filter, &[folder.name.as_str(), &folder.id.to_string()]) {
            tree.mark_subtree(folder.id, &mut folders, &mut requests, &mut expanded);
        }
    }

    // A request match keeps the request and its ancestor chain only.
    for request in &entry.requests {
        // Loaded and cached requests both report their query parameters, so a
        // search for one finds the request either way.
        let url = request.search_url();
        if matches_filter(
            filter,
            &[request.name(), url.as_ref(), request.method().as_str()],
        ) {
            requests.insert(request.id());
            if let Some(folder_id) = request.folder_id() {
                folders.insert(folder_id);
                expanded.insert(folder_id);
                tree.mark_ancestors(folder_id, &mut folders, &mut expanded);
            }
        }
    }

    // A revealed item stays visible with its ancestor chain, whatever the
    // filter says.
    if let Some(reveal) = reveal.filter(|reveal| reveal.slug == entry.collection.id) {
        match reveal.item {
            RevealedItem::Folder(id) => {
                if entry.folder(id).is_some() {
                    folders.insert(id);
                    tree.mark_ancestors(id, &mut folders, &mut expanded);
                }
            }
            RevealedItem::Request(id) => {
                if let Some(request) = entry.requests.iter().find(|request| request.id() == id) {
                    requests.insert(id);
                    if let Some(folder_id) = request.folder_id() {
                        folders.insert(folder_id);
                        expanded.insert(folder_id);
                        tree.mark_ancestors(folder_id, &mut folders, &mut expanded);
                    }
                }
            }
            RevealedItem::Collection => {}
        }
    }

    (folders, requests, expanded)
}

#[cfg(test)]
mod tests {
    use super::*;
    use posthaste_core::model::{Collection, RequestSpec};

    fn request(name: &str, folder_id: Option<FolderId>) -> RequestEntry {
        let mut spec = RequestSpec::new(name);
        spec.folder_id = folder_id;
        RequestEntry::Loaded(Box::new(spec))
    }

    fn loaded(
        slug: &str,
        name: &str,
        folders: Vec<Folder>,
        requests: Vec<RequestEntry>,
    ) -> LoadedCollection {
        let mut collection = Collection::new(slug, name);
        collection.folders = folders;
        LoadedCollection {
            collection,
            requests,
        }
    }

    fn row_name<'a>(row: RowRef<'a>) -> &'a str {
        match row {
            RowRef::Folder(folder) => &folder.name,
            RowRef::Request(request) => request.name(),
        }
    }

    fn row_names<'a>(rows: &[TreeRow<'a>]) -> Vec<&'a str> {
        rows.iter().map(|row| row_name(row.row)).collect()
    }

    /// API:
    ///   Auth/
    ///     Tokens/
    ///       Refresh
    ///     Login
    ///   Billing/
    ///     Pay
    ///   Status
    struct Sample {
        api: LoadedCollection,
        other: LoadedCollection,
        auth: FolderId,
        tokens: FolderId,
        refresh: RequestId,
    }

    fn sample() -> Sample {
        let auth = Folder::new("Auth", None);
        let tokens = Folder::new("Tokens", Some(auth.id));
        let billing = Folder::new("Billing", None);
        let login = request("Login", Some(auth.id));
        let refresh = request("Refresh", Some(tokens.id));
        let pay = request("Pay", Some(billing.id));
        let status = request("Status", None);
        Sample {
            api: loaded(
                "api",
                "API",
                vec![auth.clone(), tokens.clone(), billing.clone()],
                vec![login, refresh.clone(), pay, status],
            ),
            other: loaded("other", "Other", Vec::new(), vec![request("Alpha", None)]),
            auth: auth.id,
            tokens: tokens.id,
            refresh: refresh.id(),
        }
    }

    fn summary<'a>(
        collections: &'a [LoadedCollection],
        filter: &str,
        expand_collection: &dyn Fn(&str) -> bool,
        expand_folder: &dyn Fn(FolderId) -> bool,
        reveal: Option<&Reveal>,
    ) -> Vec<(&'a str, Vec<&'a str>, bool)> {
        visible_collections(
            collections,
            filter,
            expand_collection,
            expand_folder,
            reveal,
        )
        .into_iter()
        .map(|visible| {
            (
                visible.entry.collection.name.as_str(),
                row_names(&visible.rows),
                visible.expanded,
            )
        })
        .collect()
    }

    #[test]
    fn folders_precede_requests_and_siblings_sort_by_lowercase_name() {
        let zebra = Folder::new("Zebra", None);
        let auth = Folder::new("auth", None);
        let entry = loaded(
            "api",
            "API",
            vec![zebra, auth],
            vec![request("beta", None), request("Alpha", None)],
        );
        let tree = Tree::new(&entry);

        let rows = tree.rows(|_| false);
        assert_eq!(
            row_names(&rows),
            vec!["auth", "Zebra", "Alpha", "beta"],
            "folders sort before requests and names compare case-insensitively"
        );
        assert!(matches!(rows[0].row, RowRef::Folder(_)));
        assert!(matches!(rows[2].row, RowRef::Request(_)));
    }

    #[test]
    fn equal_names_are_tied_by_identifier() {
        let first = Folder::new("Auth", None);
        let second = Folder::new("Auth", None);
        let first_id = first.id;
        let second_id = second.id;
        let mut expected = vec![first_id, second_id];
        expected.sort();

        let login_first = request("Login", None);
        let login_second = request("Login", None);
        let mut expected_requests = vec![login_first.id(), login_second.id()];
        expected_requests.sort();

        let entry = loaded(
            "api",
            "API",
            vec![first, second],
            vec![login_first, login_second],
        );
        let tree = Tree::new(&entry);
        let rows = tree.rows(|_| false);

        let folder_ids: Vec<FolderId> = rows
            .iter()
            .filter_map(|row| match row.row {
                RowRef::Folder(folder) => Some(folder.id),
                RowRef::Request(_) => None,
            })
            .collect();
        assert_eq!(folder_ids, expected);

        let request_ids: Vec<RequestId> = rows
            .iter()
            .filter_map(|row| match row.row {
                RowRef::Request(request) => Some(request.id()),
                RowRef::Folder(_) => None,
            })
            .collect();
        assert_eq!(request_ids, expected_requests);
    }

    #[test]
    fn nested_folders_report_their_depth() {
        let mut folders = Vec::new();
        let mut parent = None;
        for level in 1..=8 {
            let folder = Folder::new(format!("Level {level}"), parent);
            parent = Some(folder.id);
            folders.push(folder);
        }
        let entry = loaded("api", "API", folders, vec![request("Deep", parent)]);
        let tree = Tree::new(&entry);

        let rows = tree.rows(|_| true);
        assert_eq!(rows.len(), 9);
        for (index, row) in rows.iter().take(8).enumerate() {
            assert_eq!(row.depth, index + 1, "folder {index} sits at its level");
            assert!(row.has_children);
        }
        let request_row = rows.last().expect("deep request row");
        assert_eq!(request_row.depth, 9);
        assert_eq!(row_name(request_row.row), "Deep");
        assert!(!request_row.has_children);
    }

    #[test]
    fn collapsed_parents_hide_descendants() {
        let auth = Folder::new("Auth", None);
        let tokens = Folder::new("Tokens", Some(auth.id));
        let entry = loaded(
            "api",
            "API",
            vec![auth.clone(), tokens.clone()],
            vec![
                request("Root", None),
                request("Login", Some(auth.id)),
                request("Refresh", Some(tokens.id)),
            ],
        );
        let tree = Tree::new(&entry);

        let collapsed = tree.rows(|_| false);
        assert_eq!(row_names(&collapsed), vec!["Auth", "Root"]);
        let auth_row = &collapsed[0];
        assert_eq!(auth_row.request_count, Some(2));
        assert!(!auth_row.expanded);

        let expanded = tree.rows(|_| true);
        assert_eq!(
            row_names(&expanded),
            vec!["Auth", "Tokens", "Refresh", "Login", "Root"],
            "every level lists its folders before its requests"
        );
        assert_eq!(expanded[0].request_count, None, "expanded hides the count");
    }

    #[test]
    fn subtree_counts_include_every_nested_folder() {
        let auth = Folder::new("Auth", None);
        let tokens = Folder::new("Tokens", Some(auth.id));
        let refresh = Folder::new("Refresh", Some(tokens.id));
        let empty = Folder::new("Empty", None);
        let entry = loaded(
            "api",
            "API",
            vec![auth.clone(), tokens.clone(), refresh.clone(), empty.clone()],
            vec![
                request("Login", Some(auth.id)),
                request("Access", Some(tokens.id)),
                request("Rotate", Some(refresh.id)),
            ],
        );
        let tree = Tree::new(&entry);

        assert_eq!(tree.folder_count(auth.id), 3);
        assert_eq!(tree.folder_count(tokens.id), 2);
        assert_eq!(tree.folder_count(refresh.id), 1);
        assert_eq!(tree.folder_count(empty.id), 0);

        let rows = tree.rows(|_| false);
        let empty_row = rows
            .iter()
            .find(|row| row_name(row.row) == "Empty")
            .expect("empty folder row");
        assert_eq!(
            empty_row.request_count, None,
            "a folder without requests shows no count"
        );
        assert!(!empty_row.has_children);

        // Expanding one level re-reads the cached count for the child.
        let partially = tree.rows(|id| id == auth.id);
        let tokens_row = partially
            .iter()
            .find(|row| row_name(row.row) == "Tokens")
            .expect("tokens row");
        assert_eq!(tokens_row.request_count, Some(2));
        assert_eq!(tokens_row.depth, 2);
    }

    #[test]
    fn requests_count_as_children_of_their_folder() {
        let auth = Folder::new("Auth", None);
        let entry = loaded(
            "api",
            "API",
            vec![auth.clone()],
            vec![request("Login", Some(auth.id))],
        );
        let tree = Tree::new(&entry);

        let rows = tree.rows(|_| true);
        assert_eq!(rows.len(), 2);
        assert!(
            rows[0].has_children,
            "a folder holding only requests has children"
        );
        let login = &rows[1];
        assert_eq!(login.depth, 2);
        assert_eq!(login.request_count, None);
    }

    #[test]
    fn empty_filter_follows_stored_expansion() {
        let sample = sample();
        let collections = [sample.api, sample.other];
        let visible = summary(&collections, "", &|_| true, &|id| id == sample.auth, None);

        assert_eq!(
            visible,
            vec![
                (
                    "API",
                    vec!["Auth", "Tokens", "Login", "Billing", "Status"],
                    true
                ),
                ("Other", vec!["Alpha"], true),
            ]
        );

        // The same data with both folders collapsed keeps every stored decision.
        let visible = summary(&collections, "", &|_| false, &|_| false, None);
        assert_eq!(
            visible,
            vec![
                ("API", vec!["Auth", "Billing", "Status"], false),
                ("Other", vec!["Alpha"], false),
            ]
        );
    }

    #[test]
    fn collection_match_shows_the_whole_tree_expanded() {
        let sample = sample();
        let collections = [sample.api, sample.other];
        let visible = summary(&collections, "api", &|_| false, &|_| false, None);

        assert_eq!(
            visible,
            vec![(
                "API",
                vec![
                    "Auth", "Tokens", "Refresh", "Login", "Billing", "Pay", "Status"
                ],
                true
            )],
            "an active search opens every folder and hides the other collection"
        );
    }

    #[test]
    fn folder_match_shows_its_subtree_only() {
        let sample = sample();
        let collections = [sample.api, sample.other];
        let visible = summary(&collections, "auth", &|_| false, &|_| false, None);

        assert_eq!(
            visible,
            vec![("API", vec!["Auth", "Tokens", "Refresh", "Login"], true)],
            "non-matching sibling folders and root requests stay hidden"
        );

        // A folder identifier matches exactly like its name.
        let by_id = sample.auth.to_string();
        let visible = summary(&collections, &by_id, &|_| false, &|_| false, None);
        assert_eq!(
            visible,
            vec![("API", vec!["Auth", "Tokens", "Refresh", "Login"], true)]
        );
    }

    #[test]
    fn deep_request_match_shows_only_the_ancestor_chain() {
        let sample = sample();
        let collections = [sample.api, sample.other];
        let visible = summary(&collections, "refresh", &|_| false, &|_| false, None);

        assert_eq!(
            visible,
            vec![("API", vec!["Auth", "Tokens", "Refresh"], true)],
            "both ancestor folders are shown and forced open"
        );
        assert!(
            visible_collections(&collections, "refresh", |_| false, |_| false, None)[0].rows[0]
                .expanded
        );
    }

    #[test]
    fn folder_without_matches_is_hidden() {
        let sample = sample();
        let collections = [sample.api, sample.other];
        let visible = summary(&collections, "billing", &|_| false, &|_| false, None);
        assert_eq!(visible, vec![("API", vec!["Billing", "Pay"], true)]);

        assert!(summary(&collections, "missing", &|_| false, &|_| false, None).is_empty());
    }

    #[test]
    fn revealed_request_and_folder_stay_visible() {
        let sample = sample();
        let collections = [sample.api, sample.other];

        let reveal = Reveal::request("api", sample.refresh);
        let visible = summary(
            &collections,
            "missing",
            &|_| false,
            &|_| false,
            Some(&reveal),
        );
        assert_eq!(
            visible,
            vec![("API", vec!["Auth", "Tokens", "Refresh"], true)]
        );

        let reveal = Reveal::folder("api", sample.tokens);
        let visible = summary(
            &collections,
            "missing",
            &|_| false,
            &|_| false,
            Some(&reveal),
        );
        assert_eq!(
            visible,
            vec![("API", vec!["Auth", "Tokens"], true)],
            "ancestors are forced open, but the revealed folder itself does not open"
        );
        let rows =
            visible_collections(&collections, "missing", |_| false, |_| false, Some(&reveal))[0]
                .rows
                .clone();
        assert!(rows[0].expanded);
        assert!(!rows[1].expanded);
    }

    #[test]
    fn revealed_collection_stays_visible_without_matches() {
        let sample = sample();
        let collections = [sample.api, sample.other];
        let reveal = Reveal::collection("api");
        let visible = summary(
            &collections,
            "missing",
            &|_| false,
            &|_| false,
            Some(&reveal),
        );
        assert_eq!(
            visible,
            vec![("API", Vec::new(), false)],
            "a revealed collection with no matching row shows only its own row"
        );
    }

    #[test]
    fn clearing_the_filter_restores_stored_expansion() {
        let sample = sample();
        let collections = [sample.api, sample.other];
        let expand_folder = |id: FolderId| id == sample.auth;

        let filtered = summary(&collections, "tokens", &|_| false, &expand_folder, None);
        assert_eq!(
            filtered,
            vec![("API", vec!["Auth", "Tokens", "Refresh"], true)]
        );

        let restored = summary(&collections, "", &|_| true, &expand_folder, None);
        assert_eq!(
            restored,
            vec![
                (
                    "API",
                    vec!["Auth", "Tokens", "Login", "Billing", "Status"],
                    true
                ),
                ("Other", vec!["Alpha"], true),
            ],
            "the stored predicate is untouched by the search"
        );

        // A collapsed collection stays collapsed when the search clears.
        let restored = summary(&collections, "", &|_| false, &expand_folder, None);
        assert!(!restored[0].2);
        assert_eq!(
            restored[0].1,
            vec!["Auth", "Tokens", "Login", "Billing", "Status"],
            "only the search forces ancestor folders open, so the stored Auth expansion returns"
        );
    }
}
