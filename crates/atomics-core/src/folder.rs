//! Bounded pages of worktree listings and host folders. No filesystem or UI dependencies.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::wire::{self, ErrorCode, Seq, WireError};

pub const PAGE_ENTRIES: usize = 128;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// Worktree-relative path, using `/` on every client.
    pub path: String,
    pub directory: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Page {
    pub worktree: String,
    pub path: String,
    pub revision: Seq,
    pub offset: u32,
    pub entries: Vec<Entry>,
    pub next_offset: Option<u32>,
}

/// Empty names the root; every other path stays inside that worktree.
pub fn valid_path(path: &str) -> bool {
    path.is_empty()
        || (path.len() <= wire::MAX_ID_BYTES
            && !path.contains(['\\', '\0'])
            && path.split('/').all(|part| !matches!(part, "" | "." | "..")))
}

/// List immediate children only, sorted and deduplicated. Output is bounded by both entries and JSON
/// bytes; a revision accompanies offsets so a caller cannot join pages from different file trees.
pub fn page<'a>(
    paths: impl IntoIterator<Item = &'a str>,
    worktree: &str,
    path: &str,
    revision: u64,
    offset: u32,
    budget: usize,
) -> Result<Page, WireError> {
    if !valid_path(path) {
        return Err(WireError::new(ErrorCode::Malformed, "folder path must be worktree-relative"));
    }
    let prefix = if path.is_empty() { String::new() } else { format!("{path}/") };
    let entries = paths.into_iter().filter(|file| valid_path(file)).filter_map(|file| {
        let rest = file.strip_prefix(&prefix).filter(|rest| !rest.is_empty())?;
        let (name, directory) = rest.split_once('/').map_or((rest, false), |(name, _)| (name, true));
        Some(Entry { path: format!("{prefix}{name}"), directory })
    });
    entries_page(entries, worktree, path, revision, offset, budget)
}

/// Page an immediate-child listing from either a published worktree or the host's folder picker.
/// Sorting, deduplication, offsets and the JSON byte/entry limits are shared by both callers.
pub fn entries_page(
    entries: impl IntoIterator<Item = Entry>,
    worktree: &str,
    path: &str,
    revision: u64,
    offset: u32,
    budget: usize,
) -> Result<Page, WireError> {
    if !valid_path(path) {
        return Err(WireError::new(ErrorCode::Malformed, "folder path must be worktree-relative"));
    }
    let prefix = if path.is_empty() { String::new() } else { format!("{path}/") };
    let mut children = BTreeMap::new();
    for entry in entries {
        if !valid_path(&entry.path) {
            continue;
        }
        let Some(name) = entry.path.strip_prefix(&prefix).filter(|name| !name.is_empty() && !name.contains('/')) else {
            continue;
        };
        children
            .entry(name.to_string())
            .and_modify(|directory| *directory |= entry.directory)
            .or_insert(entry.directory);
    }
    let mut page = Page {
        worktree: worktree.into(),
        path: path.into(),
        revision: Seq(revision),
        offset,
        entries: Vec::new(),
        next_offset: None,
    };
    let mut bytes = wire::json_len(&page) + 32;
    if bytes > budget {
        return Err(WireError::new(ErrorCode::Oversize, "folder page metadata is over the frame limit"));
    }
    for (name, directory) in children.into_iter().skip(offset as usize) {
        let entry = Entry { path: format!("{prefix}{name}"), directory };
        let size = wire::json_len(&entry) + 1;
        if page.entries.len() == PAGE_ENTRIES || bytes + size > budget {
            if page.entries.is_empty() {
                return Err(WireError::new(ErrorCode::Oversize, "one folder entry is over the frame limit"));
            }
            page.next_offset = Some(offset + page.entries.len() as u32);
            break;
        }
        bytes += size;
        page.entries.push(entry);
    }
    Ok(page)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folder_pages_are_immediate_sorted_deduplicated_and_bounded() {
        let paths: Vec<String> = (0..300)
            .map(|i| format!("src/file{i:03}.rs"))
            .chain(["src/nested/a.rs".into(), "src/nested/b.rs".into(), "other/a.rs".into()])
            .collect();
        let first = page(paths.iter().map(String::as_str), "/work/tree", "src", 7, 0, 4096).unwrap();
        assert!(first.entries.len() <= PAGE_ENTRIES && wire::json_len(&first) <= 4096);
        assert!(first.entries.iter().all(|e| e.path.starts_with("src/") && !e.path[4..].contains('/')));
        let mut entries = first.entries.clone();
        let mut next = first.next_offset;
        while let Some(offset) = next {
            let p = page(paths.iter().map(String::as_str), "/work/tree", "src", 7, offset, 4096).unwrap();
            assert_eq!(p.revision, Seq(7));
            assert!(wire::json_len(&p) <= 4096);
            entries.extend(p.entries);
            next = p.next_offset;
        }
        assert_eq!(entries.len(), 301);
        assert_eq!(entries.last(), Some(&Entry { path: "src/nested".into(), directory: true }));
        assert!(entries.windows(2).all(|pair| pair[0].path < pair[1].path));
        let root = page(paths.iter().map(String::as_str), "/work/tree", "", 7, 0, 4096).unwrap();
        assert_eq!(root.entries.iter().map(|e| e.path.as_str()).collect::<Vec<_>>(), ["other", "src"]);
    }

    #[test]
    fn folders_refuse_escaping_paths_and_entries_that_cannot_fit() {
        for path in ["../private", "/etc", "a/../b", "a//b", "a\\b", "a/", "a\0b"] {
            assert!(!valid_path(path), "{path:?}");
            assert_eq!(page(["a.rs"], "/work/tree", path, 1, 0, 4096).unwrap_err().code, ErrorCode::Malformed);
        }
        assert_eq!(page(["a.rs"], "/work/tree", "", 1, 0, 1).unwrap_err().code, ErrorCode::Oversize);
    }

    #[test]
    fn folder_topics_and_pages_round_trip_on_the_wire() {
        let page = page(["src/a.rs", "README.md"], "/work/tree", "", 1, 0, 4096).unwrap();
        for message in [
            wire::Message::Subscribe {
                topics: vec![wire::Topic::Folder { worktree: "/work/tree".into(), path: String::new() }],
            },
            wire::Message::FolderGet {
                worktree: "/work/tree".into(),
                path: "src".into(),
                revision: Seq(1),
                offset: 128,
            },
            wire::Message::FolderPage(page),
        ] {
            let envelope = wire::Envelope::new(message);
            assert_eq!(wire::decode(&wire::encode(&envelope).unwrap()).unwrap(), envelope);
        }
    }
}
