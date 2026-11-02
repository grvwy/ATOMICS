//! What a client draws a diff from: the diff with its colors, and a whole file with its colors.
//! Plain data, built on the host (it owns the highlighter) and shared with any client.

use std::sync::Arc;

use crate::diff::WorktreeDiff;

/// A colored byte range of a line. Small, since every line of a diff keeps a few.
#[derive(Clone, Copy)]
pub struct Span {
    pub start: u32,
    pub end: u32,
    pub color: [u8; 4],
}

pub type Spans = Vec<Span>;

/// One file's colors, per hunk, per line.
pub type FileHighlights = Vec<Vec<Spans>>;

pub struct Prepared {
    pub diff: WorktreeDiff,
    /// Per file, per hunk, per line.
    pub highlights: Vec<Arc<FileHighlights>>,
    pub longest_line: usize,
    /// Lines added and removed, counted once rather than every frame.
    pub additions: usize,
    pub deletions: usize,
    /// Every file git shows in the worktree, for the file picker.
    pub all_files: Arc<Vec<Arc<str>>>,
}

/// A whole file opened from the file picker.
pub struct FileView {
    pub lines: Vec<String>,
    pub highlights: Vec<Spans>,
    pub longest_line: usize,
    /// Why the content is not shown (binary, too large).
    pub note: Option<String>,
}
