//! A small, root-confined filesystem surface for the workspace UI.
//!
//! Paths crossing this boundary are deliberately not operating-system paths:
//! they are slash-separated, workspace-relative wire paths.  Keeping that
//! distinction here makes the eventual Tauri commands thin and prevents a
//! webview request from becoming authority to access an arbitrary local path.

use std::error::Error;
use std::fmt;
use std::fs::{self, Metadata, OpenOptions};
use std::path::{Path, PathBuf};

/// The largest text prefix returned to the webview in one read.
pub const TEXT_READ_LIMIT: usize = 1024 * 1024;

/// The largest file returned to the webview whole, as bytes.
///
/// A prefix of an image is not a smaller image, it is a broken one, so this
/// read has no truncation to fall back on — it either fits or it is refused.
/// The bound is what keeps that from being unbounded: the bytes are base64'd
/// into a `data:` URL and land in the webview's memory in one piece.
pub const BINARY_READ_LIMIT: u64 = 8 * 1024 * 1024;

/// A root-confined workspace filesystem.
#[derive(Debug, Clone)]
pub struct Workspace {
    root: PathBuf,
}

/// What a direct child in a listed directory is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Directory,
    /// A symbolic link or Windows reparse point. Its target is never followed.
    Link,
}

/// One direct child returned by [`Workspace::list`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceEntry {
    /// The one-component child name, not a path supplied by the operating
    /// system for use as authority.
    pub name: String,
    /// Slash-separated path relative to [`Workspace::root`].
    pub path: String,
    pub kind: EntryKind,
}

/// A bounded UTF-8 text file response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextFile {
    pub path: String,
    pub text: String,
    /// Revision of the whole file, including bytes beyond a returned prefix.
    pub revision: String,
    /// Cheap identity of the file as it was at this read/write: length plus
    /// last-modified time. Not content-based, so a poll can recompute it from
    /// metadata alone; the editor compares it to notice the disk moved.
    pub fingerprint: String,
    /// Size of the whole file in bytes.
    pub bytes: u64,
    /// Whether `text` is a prefix rather than the entire file.
    pub truncated: bool,
}

/// The metadata answer for change detection: everything the editor needs to
/// tell whether a file moved since it last read it, without paying for the
/// file's bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceStat {
    pub path: String,
    pub fingerprint: String,
    pub bytes: u64,
}

/// A regular file returned as bytes rather than as text.
///
/// It carries no revision and no `truncated`, and that is the point: this is
/// what the viewer reads to *draw* a file it cannot edit, so there is nothing
/// to write back and nothing that a prefix would still be useful for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BinaryFile {
    pub path: String,
    pub bytes: u64,
    pub data: Vec<u8>,
}

/// Errors exposed by the workspace boundary.
#[derive(Debug)]
pub enum WorkspaceError {
    InvalidPath(String),
    InvalidName(String),
    NotFound(String),
    NotFile(String),
    NotDirectory(String),
    UnsupportedEntry(String),
    LinkPath(String),
    OutsideRoot(String),
    BinaryFile(String),
    InvalidUtf8(String),
    AlreadyExists(String),
    TooLarge {
        path: String,
        bytes: u64,
    },
    /// The caller must re-read and explicitly decide how to merge its edit.
    RevisionConflict {
        current: String,
    },
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
}

impl fmt::Display for WorkspaceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidPath(path) => write!(f, "invalid workspace path '{path}'"),
            Self::InvalidName(name) => write!(f, "invalid file name '{name}'"),
            Self::NotFound(path) => write!(f, "workspace path '{path}' does not exist"),
            Self::NotFile(path) => write!(f, "workspace path '{path}' is not a file"),
            Self::NotDirectory(path) => write!(f, "workspace path '{path}' is not a directory"),
            Self::UnsupportedEntry(path) => write!(f, "workspace path '{path}' is not a regular file or directory"),
            Self::LinkPath(path) => write!(f, "workspace path '{path}' passes through a link"),
            Self::OutsideRoot(path) => write!(f, "workspace path '{path}' resolves outside the workspace root"),
            Self::BinaryFile(path) => write!(f, "workspace file '{path}' contains a NUL byte and is not text"),
            Self::InvalidUtf8(path) => write!(f, "workspace file '{path}' is not valid UTF-8"),
            Self::AlreadyExists(path) => write!(f, "workspace path '{path}' already exists"),
            Self::TooLarge { path, bytes } => write!(
                f,
                "workspace file '{path}' is {bytes} bytes — too large to display whole"
            ),
            Self::RevisionConflict { current } => write!(
                f,
                "revision conflict: the file changed; re-read it before writing (current revision {current})"
            ),
            Self::Io { path, source } => write!(f, "cannot access {}: {source}", path.display()),
        }
    }
}

impl Error for WorkspaceError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl Workspace {
    /// Open an existing directory as a workspace root.
    ///
    /// This deliberately goes through `paths::canonical_dir`: paths which key
    /// app state must not retain Windows' short-lived verbatim spelling.
    pub fn open(root: &Path) -> Result<Self, WorkspaceError> {
        let root = crate::paths::canonical_dir(root).map_err(|source| WorkspaceError::Io {
            path: root.to_path_buf(),
            source,
        })?;
        let metadata = fs::metadata(&root).map_err(|source| WorkspaceError::Io {
            path: root.clone(),
            source,
        })?;
        if !metadata.is_dir() {
            return Err(WorkspaceError::NotDirectory(root.display().to_string()));
        }
        Ok(Self { root })
    }

    /// The canonical workspace root. This is display information, never a
    /// replacement for the relative wire paths accepted by the other methods.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The host path of an existing entry, for handing to something outside this
    /// process (see `openers`).
    ///
    /// This is the one method that returns an operating-system path, and it is
    /// still not an escape hatch: it resolves through exactly the same checks as
    /// every read and write — no component may be a link, and every one of them
    /// must canonicalize inside the root — so the caller receives a path only for
    /// entries this boundary already agreed to.
    pub fn host_path(&self, path: &str) -> Result<PathBuf, WorkspaceError> {
        self.resolve_existing_entry(path)
    }

    /// List one directory without recursing. `None` means the workspace root;
    /// all `Some` values must be non-empty relative wire paths.
    pub fn list(&self, path: Option<&str>) -> Result<Vec<WorkspaceEntry>, WorkspaceError> {
        let (directory, wire_directory) = match path {
            Some(path) => (self.resolve_existing_dir(path)?, path.to_owned()),
            None => (self.root.clone(), String::new()),
        };
        let mut entries = Vec::new();
        let reader = fs::read_dir(&directory).map_err(|source| WorkspaceError::Io {
            path: directory.clone(),
            source,
        })?;

        for item in reader {
            let item = item.map_err(|source| WorkspaceError::Io {
                path: directory.clone(),
                source,
            })?;
            let name = item
                .file_name()
                .into_string()
                .map_err(|name| WorkspaceError::InvalidName(name.to_string_lossy().into_owned()))?;
            // Names returned by the OS are never reused directly. Refusing an
            // unrepresentable/reserved component keeps every listed path safe
            // to send back on the wire, including when a directory was created
            // outside this service.
            validate_name(&name)?;
            let full = item.path();
            let metadata = fs::symlink_metadata(&full).map_err(|source| WorkspaceError::Io {
                path: full.clone(),
                source,
            })?;
            let kind = if is_link(&metadata) {
                EntryKind::Link
            } else if metadata.is_file() {
                self.assert_canonical_inside(&full)?;
                EntryKind::File
            } else if metadata.is_dir() {
                self.assert_canonical_inside(&full)?;
                EntryKind::Directory
            } else {
                return Err(WorkspaceError::UnsupportedEntry(full.display().to_string()));
            };
            let path = if wire_directory.is_empty() {
                name.clone()
            } else {
                format!("{wire_directory}/{name}")
            };
            entries.push(WorkspaceEntry { name, path, kind });
        }
        entries.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(entries)
    }

    /// Entries that could finish a partly-typed workspace path.
    ///
    /// `prefix` is what the person has typed after an `@`: everything up to the
    /// last `/` names the directory to look in, and what follows is matched
    /// against the names in it. So `src/wo` lists `src` and keeps `workspace.rs`
    /// and `workspaceTree.ts`; `src/` lists `src` whole; `` lists the root.
    ///
    /// Matching is case-insensitive because a completion that refuses to match
    /// `readme` against `README.md` is a completion the typist has to work
    /// around. A directory that does not exist is not an error — it is a path
    /// still being typed — so it comes back empty.
    ///
    /// [`Self::list`] does the walking, which is what keeps every name here
    /// inside the root and representable on the wire; this only chooses among
    /// what it returned.
    pub fn complete(&self, prefix: &str, limit: usize) -> Vec<WorkspaceEntry> {
        let (directory, fragment) = match prefix.rsplit_once('/') {
            Some((directory, fragment)) => (directory, fragment),
            None => ("", prefix),
        };
        let directory = (!directory.is_empty()).then_some(directory);
        let Ok(mut entries) = self.list(directory) else {
            return Vec::new();
        };
        let wanted = fragment.to_lowercase();
        entries.retain(|entry| entry.name.to_lowercase().starts_with(&wanted));
        // Names that start with a dot are real answers but rarely the one being
        // typed, so they sort after the rest instead of filling the list. An
        // explicit leading dot in the fragment puts them back at the front by
        // being the only thing that matches at all.
        entries.sort_by(|left, right| {
            let hidden = |entry: &WorkspaceEntry| entry.name.starts_with('.');
            hidden(left)
                .cmp(&hidden(right))
                .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
        });
        entries.truncate(limit);
        entries
    }

    /// Whether this wire path names something that is really here.
    ///
    /// It answers through the same resolution as every read, so "exists" means
    /// what it means everywhere else on this boundary: no component is a link,
    /// and the whole path canonicalizes inside the root. A path that is merely
    /// unreadable, unrepresentable, or outside is simply not here.
    pub fn exists(&self, path: &str) -> bool {
        self.resolve_existing_entry(path).is_ok()
    }

    /// Read a regular UTF-8 file. Text beyond [`TEXT_READ_LIMIT`] is omitted
    /// from the response but still contributes to its revision.
    pub fn read(&self, path: &str) -> Result<TextFile, WorkspaceError> {
        let full = self.resolve_existing_file(path)?;
        let bytes = fs::read(&full).map_err(|source| WorkspaceError::Io {
            path: full.clone(),
            source,
        })?;
        self.text_response(&full, path, bytes)
    }

    /// Read a regular file as bytes, for the views that draw a file rather than
    /// read it — an image, an icon.
    ///
    /// It goes through the same `resolve_existing_file` as [`Workspace::read`]
    /// on purpose: the reason images could not be opened at all was that this
    /// boundary had one door and it only admitted UTF-8. Adding a second door
    /// beside it (the `show` viewer's path, which checks containment its own
    /// way) would have been two definitions of "inside the workspace".
    pub fn read_binary(&self, path: &str) -> Result<BinaryFile, WorkspaceError> {
        let full = self.resolve_existing_file(path)?;
        let size = fs::metadata(&full)
            .map_err(|source| WorkspaceError::Io {
                path: full.clone(),
                source,
            })?
            .len();
        if size > BINARY_READ_LIMIT {
            return Err(WorkspaceError::TooLarge {
                path: path.to_owned(),
                bytes: size,
            });
        }
        let data = fs::read(&full).map_err(|source| WorkspaceError::Io { path: full, source })?;
        Ok(BinaryFile {
            path: path.to_owned(),
            bytes: data.len() as u64,
            data,
        })
    }

    /// Whether the file named by a wire path has changed since it was last
    /// read or written.
    ///
    /// The fingerprint is metadata, not content — that is the whole point of
    /// the separate answer: the editor polls it every couple of seconds, and a
    /// content hash would make each poll a full read of files it may not even
    /// display.
    pub fn stat(&self, path: &str) -> Result<WorkspaceStat, WorkspaceError> {
        let full = self.resolve_existing_file(path)?;
        let metadata = fs::metadata(&full).map_err(|source| WorkspaceError::Io {
            path: full.clone(),
            source,
        })?;
        Ok(WorkspaceStat {
            path: path.to_owned(),
            fingerprint: fingerprint_of(&metadata),
            bytes: metadata.len(),
        })
    }

    /// Replace a regular text file only if `revision` still identifies its
    /// current complete contents.
    pub fn write(
        &self,
        path: &str,
        text: &str,
        revision: &str,
    ) -> Result<TextFile, WorkspaceError> {
        let (full, current) = self.prepare_write(path, text)?;
        let current_revision = revision_of(&current);
        if revision != current_revision {
            return Err(WorkspaceError::RevisionConflict {
                current: current_revision,
            });
        }
        self.write_bytes(&full, path, text)
    }

    /// Replace a regular text file regardless of what is on disk now.
    ///
    /// This is the explicit "overwrite" answer to a file that changed outside
    /// the editor: the reader has seen the revision check's warning and chose
    /// to lose the other side's changes anyway. Every other validation —
    /// containment, NUL, UTF-8 — still runs.
    pub fn write_force(&self, path: &str, text: &str) -> Result<TextFile, WorkspaceError> {
        let (full, _current) = self.prepare_write(path, text)?;
        self.write_bytes(&full, path, text)
    }

    /// The shared start of both write paths: the text must be writable, and
    /// the current bytes are needed either to compare revisions or to know the
    /// write is even valid before anything is truncated.
    fn prepare_write(&self, path: &str, text: &str) -> Result<(PathBuf, Vec<u8>), WorkspaceError> {
        if text.as_bytes().contains(&0) {
            return Err(WorkspaceError::BinaryFile(path.to_owned()));
        }
        let full = self.resolve_existing_file(path)?;
        let current = fs::read(&full).map_err(|source| WorkspaceError::Io {
            path: full.clone(),
            source,
        })?;
        self.validate_text(path, &current)?;
        Ok((full, current))
    }

    fn write_bytes(&self, full: &Path, path: &str, text: &str) -> Result<TextFile, WorkspaceError> {
        OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(full)
            .and_then(|mut file| std::io::Write::write_all(&mut file, text.as_bytes()))
            .map_err(|source| WorkspaceError::Io {
                path: full.to_path_buf(),
                source,
            })?;
        self.text_response(full, path, text.as_bytes().to_vec())
    }

    /// Create a UTF-8 text file under `parent` (`None` is the root).
    pub fn create_file(
        &self,
        parent: Option<&str>,
        name: &str,
        text: &str,
    ) -> Result<TextFile, WorkspaceError> {
        validate_name(name)?;
        if text.as_bytes().contains(&0) {
            return Err(WorkspaceError::BinaryFile(name.to_owned()));
        }
        let (parent_path, parent_wire) = self.resolve_parent(parent)?;
        let full = parent_path.join(name);
        ensure_absent(&full)?;
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&full)
            .and_then(|mut file| std::io::Write::write_all(&mut file, text.as_bytes()))
            .map_err(|source| WorkspaceError::Io {
                path: full.clone(),
                source,
            })?;
        let path = join_wire(&parent_wire, name);
        self.text_response(&full, &path, text.as_bytes().to_vec())
    }

    /// Create a directory under `parent` (`None` is the root).
    pub fn create_dir(
        &self,
        parent: Option<&str>,
        name: &str,
    ) -> Result<WorkspaceEntry, WorkspaceError> {
        validate_name(name)?;
        let (parent_path, parent_wire) = self.resolve_parent(parent)?;
        let full = parent_path.join(name);
        ensure_absent(&full)?;
        fs::create_dir(&full).map_err(|source| WorkspaceError::Io { path: full, source })?;
        Ok(WorkspaceEntry {
            name: name.to_owned(),
            path: join_wire(&parent_wire, name),
            kind: EntryKind::Directory,
        })
    }

    /// Rename a regular file or directory without moving it out of its current
    /// directory. Links and paths which pass through links are refused.
    pub fn rename(&self, path: &str, new_name: &str) -> Result<WorkspaceEntry, WorkspaceError> {
        validate_name(new_name)?;
        let source = self.resolve_existing_entry(path)?;
        let parent = source.parent().expect("workspace child has a parent");
        let target = parent.join(new_name);
        ensure_absent(&target)?;
        let metadata =
            fs::symlink_metadata(&source).map_err(|source_error| WorkspaceError::Io {
                path: source.clone(),
                source: source_error,
            })?;
        let kind = entry_kind(&metadata, path)?;
        fs::rename(&source, &target).map_err(|source_error| WorkspaceError::Io {
            path: source,
            source: source_error,
        })?;
        let parent_wire = wire_parent(path);
        Ok(WorkspaceEntry {
            name: new_name.to_owned(),
            path: join_wire(parent_wire, new_name),
            kind,
        })
    }

    /// Delete a regular file or an empty directory. Deletion never recurses.
    /// A link is removed as a link without following its target.
    pub fn delete(&self, path: &str) -> Result<(), WorkspaceError> {
        let components = parse_wire_path(path)?;
        let name = components.last().expect("non-empty wire path");
        let parent = self.resolve_components_dir(&components[..components.len() - 1], path)?;
        let full = parent.join(name);
        let metadata = fs::symlink_metadata(&full).map_err(|source| match source.kind() {
            std::io::ErrorKind::NotFound => WorkspaceError::NotFound(path.to_owned()),
            _ => WorkspaceError::Io {
                path: full.clone(),
                source,
            },
        })?;

        if is_link(&metadata) {
            // On Windows directory links/reparse points require RemoveDirectory,
            // while file links require DeleteFile. Neither operation follows the
            // link; trying both is safer than inspecting its target.
            remove_link(&full)?;
        } else if metadata.is_file() {
            self.assert_canonical_inside(&full)?;
            fs::remove_file(&full).map_err(|source| WorkspaceError::Io { path: full, source })?;
        } else if metadata.is_dir() {
            self.assert_canonical_inside(&full)?;
            fs::remove_dir(&full).map_err(|source| WorkspaceError::Io { path: full, source })?;
        } else {
            return Err(WorkspaceError::UnsupportedEntry(path.to_owned()));
        }
        Ok(())
    }

    /// Move a regular file, directory (empty or not) or link to the platform
    /// trash instead of removing it. Recoverable, so unlike [`Self::delete`]
    /// this is the action a file manager would call "Move to trash".
    ///
    /// It resolves through the same checks as every other operation — no
    /// component may be a link, and the entry must canonicalize inside the
    /// root — and the entry itself is moved whole: a link is trashed as a
    /// link, a directory is trashed with its contents.
    pub fn trash(&self, path: &str) -> Result<(), WorkspaceError> {
        let components = parse_wire_path(path)?;
        let name = components.last().expect("non-empty wire path");
        let parent = self.resolve_components_dir(&components[..components.len() - 1], path)?;
        let full = parent.join(name);
        let metadata = fs::symlink_metadata(&full).map_err(|source| match source.kind() {
            std::io::ErrorKind::NotFound => WorkspaceError::NotFound(path.to_owned()),
            _ => WorkspaceError::Io {
                path: full.clone(),
                source,
            },
        })?;
        if !is_link(&metadata) {
            // A link's target may live anywhere; only a real entry has to be
            // inside the root.
            self.assert_canonical_inside(&full)?;
        }
        trash::delete(&full).map_err(|source| WorkspaceError::Io {
            path: full.clone(),
            source: std::io::Error::other(source.to_string()),
        })
    }

    fn resolve_parent(&self, parent: Option<&str>) -> Result<(PathBuf, String), WorkspaceError> {
        match parent {
            Some(path) => Ok((self.resolve_existing_dir(path)?, path.to_owned())),
            None => Ok((self.root.clone(), String::new())),
        }
    }

    fn resolve_existing_file(&self, path: &str) -> Result<PathBuf, WorkspaceError> {
        let full = self.resolve_existing_entry(path)?;
        let metadata = fs::symlink_metadata(&full).map_err(|source| WorkspaceError::Io {
            path: full.clone(),
            source,
        })?;
        if metadata.is_file() {
            Ok(full)
        } else if metadata.is_dir() {
            Err(WorkspaceError::NotFile(path.to_owned()))
        } else {
            Err(WorkspaceError::UnsupportedEntry(path.to_owned()))
        }
    }

    fn resolve_existing_dir(&self, path: &str) -> Result<PathBuf, WorkspaceError> {
        let full = self.resolve_existing_entry(path)?;
        let metadata = fs::symlink_metadata(&full).map_err(|source| WorkspaceError::Io {
            path: full.clone(),
            source,
        })?;
        if metadata.is_dir() {
            Ok(full)
        } else if metadata.is_file() {
            Err(WorkspaceError::NotDirectory(path.to_owned()))
        } else {
            Err(WorkspaceError::UnsupportedEntry(path.to_owned()))
        }
    }

    /// Resolve each component with `symlink_metadata`, so a link is caught
    /// before any operation can traverse it. Every non-link component is also
    /// canonicalized and contained in the root as a defense against mounts,
    /// junctions, and unusual filesystem topology.
    fn resolve_existing_entry(&self, path: &str) -> Result<PathBuf, WorkspaceError> {
        let components = parse_wire_path(path)?;
        let mut full = self.root.clone();
        for component in components {
            full.push(component);
            let metadata = fs::symlink_metadata(&full).map_err(|source| match source.kind() {
                std::io::ErrorKind::NotFound => WorkspaceError::NotFound(path.to_owned()),
                _ => WorkspaceError::Io {
                    path: full.clone(),
                    source,
                },
            })?;
            if is_link(&metadata) {
                return Err(WorkspaceError::LinkPath(path.to_owned()));
            }
            self.assert_canonical_inside(&full)?;
        }
        Ok(full)
    }

    fn resolve_components_dir(
        &self,
        components: &[String],
        requested: &str,
    ) -> Result<PathBuf, WorkspaceError> {
        let mut full = self.root.clone();
        for component in components {
            full.push(component);
            let metadata = fs::symlink_metadata(&full).map_err(|source| match source.kind() {
                std::io::ErrorKind::NotFound => WorkspaceError::NotFound(requested.to_owned()),
                _ => WorkspaceError::Io {
                    path: full.clone(),
                    source,
                },
            })?;
            if is_link(&metadata) {
                return Err(WorkspaceError::LinkPath(requested.to_owned()));
            }
            if !metadata.is_dir() {
                return Err(WorkspaceError::NotDirectory(requested.to_owned()));
            }
            self.assert_canonical_inside(&full)?;
        }
        Ok(full)
    }

    fn assert_canonical_inside(&self, path: &Path) -> Result<(), WorkspaceError> {
        let canonical = crate::paths::canonical_dir(path).map_err(|source| WorkspaceError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        if canonical.starts_with(&self.root) {
            Ok(())
        } else {
            Err(WorkspaceError::OutsideRoot(path.display().to_string()))
        }
    }

    fn validate_text<'a>(&self, path: &str, bytes: &'a [u8]) -> Result<&'a str, WorkspaceError> {
        if bytes.contains(&0) {
            return Err(WorkspaceError::BinaryFile(path.to_owned()));
        }
        std::str::from_utf8(bytes).map_err(|_| WorkspaceError::InvalidUtf8(path.to_owned()))
    }

    fn text_response(
        &self,
        full: &Path,
        path: &str,
        bytes: Vec<u8>,
    ) -> Result<TextFile, WorkspaceError> {
        let text = self.validate_text(path, &bytes)?;
        let mut end = text.len().min(TEXT_READ_LIMIT);
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        let metadata = fs::metadata(full).map_err(|source| WorkspaceError::Io {
            path: full.to_path_buf(),
            source,
        })?;
        Ok(TextFile {
            path: path.to_owned(),
            text: text[..end].to_owned(),
            revision: revision_of(&bytes),
            fingerprint: fingerprint_of(&metadata),
            bytes: bytes.len() as u64,
            truncated: bytes.len() > TEXT_READ_LIMIT,
        })
    }
}

fn parse_wire_path(path: &str) -> Result<Vec<String>, WorkspaceError> {
    if path.is_empty()
        || path.starts_with('/')
        || path.starts_with('\\')
        || path.contains('\\')
        || is_drive_prefixed(path)
        || path.contains('\0')
    {
        return Err(WorkspaceError::InvalidPath(path.to_owned()));
    }
    let components: Vec<_> = path.split(['/', '\\']).map(str::to_owned).collect();
    if components
        .iter()
        .any(|component| validate_name(component).is_err())
    {
        return Err(WorkspaceError::InvalidPath(path.to_owned()));
    }
    Ok(components)
}

fn validate_name(name: &str) -> Result<(), WorkspaceError> {
    if name.is_empty()
        || matches!(name, "." | "..")
        || name.contains(['/', '\\', '\0'])
        || name.chars().any(char::is_control)
    {
        return Err(WorkspaceError::InvalidName(name.to_owned()));
    }

    // The wire path must accept names the current filesystem can already hold.
    // Windows-only reserved spellings remain invalid there, while Unix keeps
    // legitimate names such as a timestamp containing colons usable.
    #[cfg(windows)]
    if name
        .chars()
        .any(|character| matches!(character, '<' | '>' | ':' | '"' | '|' | '?' | '*'))
        || name.ends_with(['.', ' '])
        || is_windows_device_name(name)
    {
        return Err(WorkspaceError::InvalidName(name.to_owned()));
    }

    Ok(())
}

fn is_drive_prefixed(path: &str) -> bool {
    path.as_bytes()
        .get(0..2)
        .is_some_and(|prefix| prefix[0].is_ascii_alphabetic() && prefix[1] == b':')
}

#[cfg(windows)]
fn is_windows_device_name(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or(name).to_ascii_uppercase();
    matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || stem
            .strip_prefix("COM")
            .or_else(|| stem.strip_prefix("LPT"))
            .is_some_and(|number| {
                matches!(number, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9")
            })
}

fn ensure_absent(path: &Path) -> Result<(), WorkspaceError> {
    match fs::symlink_metadata(path) {
        Ok(_) => Err(WorkspaceError::AlreadyExists(path.display().to_string())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(WorkspaceError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn entry_kind(metadata: &Metadata, path: &str) -> Result<EntryKind, WorkspaceError> {
    if is_link(metadata) {
        Err(WorkspaceError::LinkPath(path.to_owned()))
    } else if metadata.is_file() {
        Ok(EntryKind::File)
    } else if metadata.is_dir() {
        Ok(EntryKind::Directory)
    } else {
        Err(WorkspaceError::UnsupportedEntry(path.to_owned()))
    }
}

fn is_link(metadata: &Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
    #[cfg(not(windows))]
    {
        false
    }
}

fn remove_link(path: &Path) -> Result<(), WorkspaceError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(file_error) => fs::remove_dir(path).map_err(|dir_error| WorkspaceError::Io {
            path: path.to_path_buf(),
            source: if dir_error.kind() == std::io::ErrorKind::NotADirectory {
                file_error
            } else {
                dir_error
            },
        }),
    }
}

fn join_wire(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.to_owned()
    } else {
        format!("{parent}/{name}")
    }
}

fn wire_parent(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(parent, _)| parent)
}

/// A stable, std-only fingerprint. It is an optimistic-concurrency token, not
/// a cryptographic integrity primitive; the length makes ordinary prefix cases
/// distinct as well.
fn revision_of(bytes: &[u8]) -> String {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}-{}", bytes.len())
}

/// A cheap identity of a file's current contents for change detection: length
/// plus last-modified time. Unlike [`revision_of`], it needs only metadata, so
/// a poll can compare it without reading the file.
fn fingerprint_of(metadata: &Metadata) -> String {
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos().to_string())
        .unwrap_or_else(|| "?".to_owned());
    format!("{}-{}", metadata.len(), modified)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace() -> (tempfile::TempDir, Workspace) {
        let root = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(root.path()).unwrap();
        (root, workspace)
    }

    #[test]
    fn completion_matches_the_fragment_after_the_last_slash() {
        let (root, workspace) = workspace();
        fs::create_dir(root.path().join("src")).unwrap();
        fs::write(root.path().join("src/workspace.rs"), "").unwrap();
        fs::write(root.path().join("src/workspaceTree.ts"), "").unwrap();
        fs::write(root.path().join("src/main.rs"), "").unwrap();
        fs::write(root.path().join("README.md"), "").unwrap();
        fs::write(root.path().join(".env"), "").unwrap();

        let names = |prefix: &str| -> Vec<String> {
            workspace
                .complete(prefix, 12)
                .into_iter()
                .map(|entry| entry.path)
                .collect()
        };

        assert_eq!(
            names("src/wo"),
            ["src/workspace.rs", "src/workspaceTree.ts"],
            "the directory is what precedes the last slash"
        );
        // A partly-typed name is not a missing directory, and it is not an error.
        assert!(names("nope/th").is_empty());
        // Case-insensitive, or `readme` never finds `README.md`.
        assert_eq!(names("read"), ["README.md"]);
        // Dotfiles are real answers that are rarely the one being typed, so they
        // sort last — unless the dot is what was typed.
        assert_eq!(names(""), ["README.md", "src", ".env"]);
        assert_eq!(names("."), [".env"]);
    }

    #[test]
    fn wire_paths_cannot_escape_or_use_windows_spellings() {
        let (_root, workspace) = workspace();
        for path in [
            "../secret",
            "/secret",
            r"C:\\secret",
            r"\\server\\share",
            r"a\b",
            "a//b",
            "a/./b",
            "",
        ] {
            assert!(
                matches!(workspace.read(path), Err(WorkspaceError::InvalidPath(_))),
                "{path}"
            );
        }
        for name in [".", "..", "a/b", r"a\\b"] {
            assert!(
                matches!(
                    workspace.create_dir(None, name),
                    Err(WorkspaceError::InvalidName(_))
                ),
                "{name}"
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_reserved_names_are_rejected() {
        let (_root, workspace) = workspace();
        for name in ["NUL.txt", "bad:", "trailing.", "trailing "] {
            assert!(
                matches!(
                    workspace.create_dir(None, name),
                    Err(WorkspaceError::InvalidName(_))
                ),
                "{name}"
            );
        }
    }

    #[cfg(not(windows))]
    #[test]
    fn unix_names_with_colons_and_unicode_are_listed_and_reachable() {
        let (root, workspace) = workspace();
        let name = "ChatGPT Image 2026年7月31日 09:44:44.png";
        fs::write(root.path().join(name), "image placeholder").unwrap();

        assert!(matches!(
            workspace.list(None).unwrap().as_slice(),
            [WorkspaceEntry { name: listed, path, kind: EntryKind::File }] if listed == name && path == name
        ));
        assert_eq!(workspace.read(name).unwrap().text, "image placeholder");
    }

    #[test]
    fn list_is_shallow_and_sorted_by_name() {
        let (_root, workspace) = workspace();
        workspace.create_file(None, "z.txt", "z").unwrap();
        workspace.create_dir(None, "alpha").unwrap();
        workspace.create_file(None, "middle.txt", "m").unwrap();
        workspace
            .create_file(Some("alpha"), "nested.txt", "n")
            .unwrap();

        let entries = workspace.list(None).unwrap();
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.path.as_str())
                .collect::<Vec<_>>(),
            ["alpha", "middle.txt", "z.txt"]
        );
        assert_eq!(workspace.list(Some("alpha")).unwrap().len(), 1);
    }

    #[test]
    fn crud_and_revision_conflicts_are_explicit() {
        let (_root, workspace) = workspace();
        workspace.create_dir(None, "src").unwrap();
        let first = workspace
            .create_file(Some("src"), "main.rs", "one")
            .unwrap();
        let second = workspace
            .write("src/main.rs", "two", &first.revision)
            .unwrap();
        assert_eq!(second.text, "two");
        assert!(matches!(
            workspace.write("src/main.rs", "three", &first.revision),
            Err(WorkspaceError::RevisionConflict { .. })
        ));
        let renamed = workspace.rename("src/main.rs", "lib.rs").unwrap();
        assert_eq!(renamed.path, "src/lib.rs");
        workspace.delete("src/lib.rs").unwrap();
        workspace.delete("src").unwrap();
    }

    #[test]
    fn stat_fingerprint_changes_when_the_file_does_and_not_otherwise() {
        let (root, workspace) = workspace();
        workspace.create_file(None, "watch.txt", "one").unwrap();

        let first = workspace.stat("watch.txt").unwrap();
        assert_eq!(first.path, "watch.txt");
        assert_eq!(first.bytes, 3);
        assert_eq!(
            workspace.stat("watch.txt").unwrap().fingerprint,
            first.fingerprint
        );

        fs::write(root.path().join("watch.txt"), "two words").unwrap();
        let second = workspace.stat("watch.txt").unwrap();
        // Different length too, so the assertion holds even on filesystems
        // whose mtime granularity is coarser than the gap between the writes.
        assert_ne!(second.fingerprint, first.fingerprint);

        assert!(matches!(
            workspace.stat("missing.txt"),
            Err(WorkspaceError::NotFound(_))
        ));
    }

    #[test]
    fn write_force_overwrites_a_revision_the_plain_write_refuses() {
        let (root, workspace) = workspace();
        workspace.create_file(None, "shared.txt", "disk").unwrap();
        let stale = workspace.read("shared.txt").unwrap();

        // The disk moves behind the reader's back, exactly like an agent edit.
        fs::write(root.path().join("shared.txt"), "agent change").unwrap();
        assert!(matches!(
            workspace.write("shared.txt", "my change", &stale.revision),
            Err(WorkspaceError::RevisionConflict { .. })
        ));

        let overwritten = workspace.write_force("shared.txt", "my change").unwrap();
        assert_eq!(overwritten.text, "my change");
        assert_eq!(
            fs::read_to_string(root.path().join("shared.txt")).unwrap(),
            "my change"
        );

        // The force path still refuses what the plain path refuses.
        fs::write(root.path().join("bin.dat"), b"text\0data").unwrap();
        assert!(matches!(
            workspace.write_force("bin.dat", "nope"),
            Err(WorkspaceError::BinaryFile(_))
        ));
    }

    #[test]
    fn delete_does_not_recurse() {
        let (_root, workspace) = workspace();
        workspace.create_dir(None, "nonempty").unwrap();
        workspace
            .create_file(Some("nonempty"), "file", "x")
            .unwrap();
        assert!(workspace.delete("nonempty").is_err());
    }

    /// The freedesktop trash layout is what the Linux implementation writes:
    /// the entry lands under `files/` and its `.trashinfo` beside it. Only
    /// asserted on Linux — Windows and macOS have their own trash, and this
    /// test is here for the platform the report named.
    #[cfg(target_os = "linux")]
    #[test]
    fn trash_moves_a_file_and_a_nonempty_directory_to_the_platform_trash() {
        // Point the trash at a scratch location so the test never touches the
        // machine's real trash. `trash` reads `XDG_DATA_HOME` per call; no
        // other test in this binary reads it, and this one restores it.
        let trash_home = tempfile::tempdir().unwrap();
        let previous = std::env::var_os("XDG_DATA_HOME");
        std::env::set_var("XDG_DATA_HOME", trash_home.path());
        let restore = || match previous {
            Some(value) => std::env::set_var("XDG_DATA_HOME", value),
            None => std::env::remove_var("XDG_DATA_HOME"),
        };

        let (root, workspace) = workspace();
        workspace.create_file(None, "draft.md", "hello").unwrap();
        workspace.create_dir(None, "project").unwrap();
        workspace
            .create_file(Some("project"), "notes.txt", "keep me")
            .unwrap();

        workspace.trash("draft.md").unwrap();
        workspace.trash("project").unwrap();

        restore();
        assert!(!root.path().join("draft.md").exists());
        assert!(!root.path().join("project").exists());
        let files = trash_home.path().join("Trash/files");
        assert_eq!(
            std::fs::read_to_string(files.join("draft.md")).unwrap(),
            "hello"
        );
        assert_eq!(
            std::fs::read_to_string(files.join("project/notes.txt")).unwrap(),
            "keep me"
        );
        assert!(trash_home.path().join("Trash/info/draft.md.trashinfo").exists());
    }

    #[test]
    fn trash_keeps_the_same_boundary_as_delete() {
        let (_root, workspace) = workspace();
        assert!(matches!(
            workspace.trash("../secret"),
            Err(WorkspaceError::InvalidPath(_))
        ));
        assert!(matches!(
            workspace.trash("missing.txt"),
            Err(WorkspaceError::NotFound(_))
        ));
        assert!(matches!(
            workspace.trash(""),
            Err(WorkspaceError::InvalidPath(_))
        ));
    }

    #[test]
    fn text_reads_truncate_and_reject_binary_data() {
        let (_root, workspace) = workspace();
        let body = "é".repeat(TEXT_READ_LIMIT / 2 + 10);
        workspace.create_file(None, "large.txt", &body).unwrap();
        let read = workspace.read("large.txt").unwrap();
        assert!(read.truncated);
        assert!(read.text.len() <= TEXT_READ_LIMIT);
        assert!(std::str::from_utf8(read.text.as_bytes()).is_ok());

        fs::write(workspace.root().join("binary.dat"), b"text\0data").unwrap();
        assert!(matches!(
            workspace.read("binary.dat"),
            Err(WorkspaceError::BinaryFile(_))
        ));
    }

    /// The bytes door, and the two things that make it a door rather than a
    /// hole: it admits exactly what the text door refuses, and it stays inside
    /// the same root — a link out of the workspace is not readable as an image
    /// either.
    #[test]
    fn binary_reads_return_whole_files_and_keep_the_root() {
        let (root, workspace) = workspace();
        fs::write(root.path().join("icon.ico"), b"\0\x01icon bytes").unwrap();

        let read = workspace.read_binary("icon.ico").unwrap();
        assert_eq!(read.data, b"\0\x01icon bytes");
        assert_eq!(read.bytes, 12);

        assert!(matches!(
            workspace.read_binary("../outside.png"),
            Err(WorkspaceError::InvalidPath(_) | WorkspaceError::OutsideRoot(_))
        ));
        assert!(matches!(
            workspace.read_binary("missing.png"),
            Err(WorkspaceError::NotFound(_))
        ));
    }

    #[test]
    fn links_are_listed_not_followed_and_are_deleted_as_links() {
        let (root, workspace) = workspace();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret.txt"), "secret").unwrap();
        let link = root.path().join("escape");
        if !make_dir_link(outside.path(), &link) {
            return; // Windows may deny symlink creation without Developer Mode.
        }

        assert!(matches!(
            workspace.list(None).unwrap().as_slice(),
            [WorkspaceEntry { name, kind: EntryKind::Link, .. }] if name == "escape"
        ));
        assert!(matches!(
            workspace.read("escape/secret.txt"),
            Err(WorkspaceError::LinkPath(_))
        ));
        assert!(matches!(
            workspace.write("escape/secret.txt", "changed", "irrelevant"),
            Err(WorkspaceError::LinkPath(_))
        ));
        assert!(matches!(
            workspace.rename("escape/secret.txt", "renamed.txt"),
            Err(WorkspaceError::LinkPath(_))
        ));
        workspace.delete("escape").unwrap();
        assert!(!link.exists());
        assert!(outside.path().join("secret.txt").exists());
    }

    #[cfg(unix)]
    fn make_dir_link(target: &Path, link: &Path) -> bool {
        std::os::unix::fs::symlink(target, link).is_ok()
    }

    #[cfg(windows)]
    fn make_dir_link(target: &Path, link: &Path) -> bool {
        std::os::windows::fs::symlink_dir(target, link).is_ok()
    }
}
