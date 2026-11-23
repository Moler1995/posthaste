//! A selected rectangle of cookie-table cells, shared by the jar's dialog and
//! the response's Cookies section so both tables select and copy alike.

/// An inclusive rectangle of table cells, so a drag can select a range and
/// Ctrl+C can copy it as tab-separated rows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CookieSelection {
    anchor: (usize, usize),
    head: (usize, usize),
}

impl CookieSelection {
    /// A selection that starts and ends on one cell.
    pub(crate) fn new(row: usize, column: usize) -> Self {
        let point = (row, column);
        Self {
            anchor: point,
            head: point,
        }
    }

    pub(crate) fn rows(self) -> (usize, usize) {
        (
            self.anchor.0.min(self.head.0),
            self.anchor.0.max(self.head.0),
        )
    }

    pub(crate) fn columns(self) -> (usize, usize) {
        (
            self.anchor.1.min(self.head.1),
            self.anchor.1.max(self.head.1),
        )
    }

    pub(crate) fn contains(self, row: usize, column: usize) -> bool {
        let (first_row, last_row) = self.rows();
        let (first_column, last_column) = self.columns();
        (first_row..=last_row).contains(&row) && (first_column..=last_column).contains(&column)
    }

    /// Moves the far end of the rectangle, keeping the anchor in place.
    pub(crate) fn extend_to(&mut self, row: usize, column: usize) {
        self.head = (row, column);
    }

    pub(crate) fn head(self) -> (usize, usize) {
        self.head
    }
}
