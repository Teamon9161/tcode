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
    /// Size of the whole file in bytes.
    pub bytes: u64,
    /// Whether `text` is a prefix rather than the entire file.
    pub truncated: bool,
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

    /// Read a regular UTF-8 file. Text beyond [`TEXT_READ_LIMIT`] is omitted
    /// from the response but still contributes to its revision.
    pub fn read(&self, path: &str) -> Result<TextFile, WorkspaceError> {
        let full = self.resolve_existing_file(path)?;
        let bytes = fs::read(&full).map_err(|source| WorkspaceError::Io { path: full, source })?;
        self.text_response(path, bytes)
    }

    /// Replace a regular text file only if `revision` still identifies its
    /// current complete contents.
    pub fn write(
        &self,
        path: &str,
        text: &str,
        revision: &str,
    ) -> Result<TextFile, WorkspaceError> {
        if text.as_bytes().contains(&0) {
            return Err(WorkspaceError::BinaryFile(path.to_owned()));
        }
        let full = self.resolve_existing_file(path)?;
        let current = fs::read(&full).map_err(|source| WorkspaceError::Io {
            path: full.clone(),
            source,
        })?;
        self.validate_text(path, &current)?;
        let current_revision = revision_of(&current);
        if revision != current_revision {
            return Err(WorkspaceError::RevisionConflict {
                current: current_revision,
            });
        }

        OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&full)
            .and_then(|mut file| std::io::Write::write_all(&mut file, text.as_bytes()))
            .map_err(|source| WorkspaceError::Io { path: full, source })?;
        self.text_response(path, text.as_bytes().to_vec())
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
            .map_err(|source| WorkspaceError::Io { path: full, source })?;
        let path = join_wire(&parent_wire, name);
        self.text_response(&path, text.as_bytes().to_vec())
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

    fn text_response(&self, path: &str, bytes: Vec<u8>) -> Result<TextFile, WorkspaceError> {
        let text = self.validate_text(path, &bytes)?;
        let mut end = text.len().min(TEXT_READ_LIMIT);
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        Ok(TextFile {
            path: path.to_owned(),
            text: text[..end].to_owned(),
            revision: revision_of(&bytes),
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
        || name.chars().any(|character| {
            character.is_control() || matches!(character, '<' | '>' | ':' | '"' | '|' | '?' | '*')
        })
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

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace() -> (tempfile::TempDir, Workspace) {
        let root = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(root.path()).unwrap();
        (root, workspace)
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
        for name in [".", "..", "a/b", r"a\\b", "NUL.txt", "bad:"] {
            assert!(
                matches!(
                    workspace.create_dir(None, name),
                    Err(WorkspaceError::InvalidName(_))
                ),
                "{name}"
            );
        }
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
    fn delete_does_not_recurse() {
        let (_root, workspace) = workspace();
        workspace.create_dir(None, "nonempty").unwrap();
        workspace
            .create_file(Some("nonempty"), "file", "x")
            .unwrap();
        assert!(workspace.delete("nonempty").is_err());
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
