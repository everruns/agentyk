//! File system access as a bundled capability.
//!
//! Everruns' `SessionFileSystem` is a service bag keyed by `SessionId`, with
//! a `WorkspaceScopedFileSystem` decorator that redirects a session's file
//! I/O to a shared workspace's keyspace. Agentyk's version is deliberately
//! smaller: [`FileSystem`] has no session parameter at all — one store is one
//! workspace, matching everruns' own `RealDiskFileStore` (which already
//! ignores the `session_id` it's handed). Multi-workspace hosts compose this
//! by attaching a different [`FileSystemCapability`] per agent/session
//! rather than routing through a shared, keyed store; first-classing
//! `workspace_id` is deferred until an adopter actually needs one store
//! shared across sessions.
//!
//! ```no_run
//! use agentyk::{Agent, FileSystemCapability, ModelSpec, RealDiskFileSystem};
//!
//! # fn demo() -> agentyk::Result<Agent> {
//! let store = RealDiskFileSystem::new("/tmp/workspace")?;
//! Agent::builder()
//!     .model(ModelSpec::llmsim())
//!     .capability(FileSystemCapability::new(store))
//!     .build()
//! # }
//! ```

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::Mutex;

use agentyk_core::capability::Capability;
use agentyk_core::error::{Error, Result};
use agentyk_core::tool::{Tool, ToolContext, ToolDefinition, ToolOutput};

/// One entry in a [`FileSystem::list_directory`] listing.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct FileEntry {
    /// The entry's own name, not a path.
    pub name: String,
    /// Whether it is a directory.
    pub is_dir: bool,
    /// Size in bytes; `0` for directories.
    pub size: u64,
}

impl FileEntry {
    /// A file entry.
    pub fn file(name: impl Into<String>, size: u64) -> Self {
        Self {
            name: name.into(),
            is_dir: false,
            size,
        }
    }

    /// A directory entry.
    pub fn dir(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            is_dir: true,
            size: 0,
        }
    }
}

/// What [`FileSystem::stat`] reports about one path.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct FileStat {
    /// The path as asked for.
    pub path: String,
    /// Whether anything exists there. `false` makes the other fields
    /// meaningless — a missing file is an answer, not an error, because
    /// "does this exist?" is the question `stat_file` is usually asked.
    pub exists: bool,
    /// Whether it is a directory.
    pub is_dir: bool,
    /// Size in bytes; `0` for directories and for anything missing.
    pub size: u64,
}

impl FileStat {
    /// A path that does not exist.
    pub fn missing(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            exists: false,
            is_dir: false,
            size: 0,
        }
    }

    /// A path that exists, described by its directory entry.
    pub fn found(path: impl Into<String>, entry: &FileEntry) -> Self {
        Self {
            path: path.into(),
            exists: true,
            is_dir: entry.is_dir,
            size: entry.size,
        }
    }
}

/// A workspace a [`FileSystemCapability`] reads and writes. Mirrors
/// everruns' `SessionFileSystem`, minus the `session_id` parameter — see the
/// module docs.
#[async_trait]
pub trait FileSystem: Send + Sync {
    /// Read a file's full contents. Paths are relative to the workspace
    /// root; an implementation must refuse to escape it.
    async fn read_file(&self, path: &str) -> Result<String>;
    /// Write a file's full contents, creating it and any parent directories.
    async fn write_file(&self, path: &str, content: &str) -> Result<()>;
    /// List the entries directly inside a directory, not recursively.
    async fn list_directory(&self, path: &str) -> Result<Vec<FileEntry>>;
    /// Delete a file, or a directory when `recursive` is set. Deleting a
    /// non-empty directory without it must fail.
    async fn delete_file(&self, path: &str, recursive: bool) -> Result<()>;

    /// Describe one path without reading it.
    ///
    /// Defaulted in terms of [`FileSystem::list_directory`], so every
    /// existing implementation gains it for free; override it when the
    /// backing store can answer directly (real disk does — see
    /// [`RealDiskFileSystem`]) rather than by listing a parent that may hold
    /// thousands of entries.
    async fn stat(&self, path: &str) -> Result<FileStat> {
        let trimmed = path.trim_matches('/');
        if trimmed.is_empty() {
            // The root always exists and is always a directory.
            return Ok(FileStat {
                path: path.to_string(),
                exists: true,
                is_dir: true,
                size: 0,
            });
        }
        let (parent, name) = match trimmed.rsplit_once('/') {
            Some((parent, name)) => (parent, name),
            None => ("", trimmed),
        };
        let entries = match self.list_directory(parent).await {
            Ok(entries) => entries,
            // A missing parent means a missing path, which is a `FileStat`,
            // not a failure.
            Err(_) => return Ok(FileStat::missing(path)),
        };
        Ok(entries
            .iter()
            .find(|entry| entry.name == name)
            .map(|entry| FileStat::found(path, entry))
            .unwrap_or_else(|| FileStat::missing(path)))
    }
}

/// A real-disk workspace rooted at a fixed directory. Every path is resolved
/// relative to the root; `..` components are rejected structurally (never
/// escape via component-by-component joining), so there is no path to break
/// out of the root regardless of what the model sends.
pub struct RealDiskFileSystem {
    root: PathBuf,
}

impl RealDiskFileSystem {
    /// Creates the root directory if it doesn't exist yet.
    pub fn new(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref();
        std::fs::create_dir_all(root)?;
        Ok(Self {
            root: root.canonicalize()?,
        })
    }

    fn resolve(&self, path: &str) -> Result<PathBuf> {
        let mut resolved = self.root.clone();
        for component in Path::new(path.trim_start_matches('/')).components() {
            match component {
                Component::Normal(part) => resolved.push(part),
                Component::CurDir | Component::RootDir | Component::Prefix(_) => {}
                Component::ParentDir => {
                    return Err(Error::Other(format!(
                        "path escapes the workspace root: `{path}`"
                    )));
                }
            }
        }
        Ok(resolved)
    }
}

#[async_trait]
impl FileSystem for RealDiskFileSystem {
    async fn read_file(&self, path: &str) -> Result<String> {
        Ok(tokio::fs::read_to_string(self.resolve(path)?).await?)
    }

    /// Answered from the file's own metadata rather than by listing its
    /// parent directory, which the default implementation would have to do.
    async fn stat(&self, path: &str) -> Result<FileStat> {
        let full = self.resolve(path)?;
        match tokio::fs::metadata(full).await {
            Ok(metadata) => Ok(FileStat {
                path: path.to_string(),
                exists: true,
                is_dir: metadata.is_dir(),
                size: if metadata.is_dir() { 0 } else { metadata.len() },
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok(FileStat::missing(path))
            }
            Err(error) => Err(error.into()),
        }
    }

    async fn write_file(&self, path: &str, content: &str) -> Result<()> {
        let full = self.resolve(path)?;
        if let Some(parent) = full.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(full, content).await?;
        Ok(())
    }

    async fn list_directory(&self, path: &str) -> Result<Vec<FileEntry>> {
        let full = self.resolve(path)?;
        let mut entries = Vec::new();
        let mut read_dir = tokio::fs::read_dir(full).await?;
        while let Some(entry) = read_dir.next_entry().await? {
            let metadata = entry.metadata().await?;
            let name = entry.file_name().to_string_lossy().into_owned();
            entries.push(if metadata.is_dir() {
                FileEntry::dir(name)
            } else {
                FileEntry::file(name, metadata.len())
            });
        }
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(entries)
    }

    async fn delete_file(&self, path: &str, recursive: bool) -> Result<()> {
        let full = self.resolve(path)?;
        let metadata = tokio::fs::metadata(&full).await?;
        if metadata.is_dir() {
            if recursive {
                tokio::fs::remove_dir_all(&full).await?;
            } else {
                tokio::fs::remove_dir(&full).await?;
            }
        } else {
            tokio::fs::remove_file(&full).await?;
        }
        Ok(())
    }
}

/// An in-memory workspace — no host disk touched. Useful for tests and for
/// hosts that don't want a real filesystem in the loop at all. Directories
/// are inferred from `/`-separated key prefixes, not stored explicitly.
#[derive(Default)]
pub struct InMemoryFileSystem {
    files: Mutex<HashMap<String, String>>,
}

impl InMemoryFileSystem {
    /// An empty workspace.
    pub fn new() -> Self {
        Self::default()
    }

    fn normalize(path: &str) -> String {
        path.trim_start_matches('/').to_string()
    }
}

#[async_trait]
impl FileSystem for InMemoryFileSystem {
    async fn read_file(&self, path: &str) -> Result<String> {
        let key = Self::normalize(path);
        self.files
            .lock()
            .await
            .get(&key)
            .cloned()
            .ok_or_else(|| Error::Other(format!("no such file: `{path}`")))
    }

    async fn write_file(&self, path: &str, content: &str) -> Result<()> {
        self.files
            .lock()
            .await
            .insert(Self::normalize(path), content.to_string());
        Ok(())
    }

    async fn list_directory(&self, path: &str) -> Result<Vec<FileEntry>> {
        let normalized = Self::normalize(path);
        let prefix = if normalized.is_empty() {
            normalized
        } else {
            format!("{normalized}/")
        };
        let files = self.files.lock().await;
        let mut seen: std::collections::BTreeMap<String, FileEntry> =
            std::collections::BTreeMap::new();
        for (key, content) in files.iter() {
            let Some(rest) = key.strip_prefix(prefix.as_str()) else {
                continue;
            };
            if rest.is_empty() {
                continue;
            }
            match rest.split_once('/') {
                Some((dir, _)) => {
                    seen.entry(dir.to_string()).or_insert(FileEntry::dir(dir));
                }
                None => {
                    seen.insert(
                        rest.to_string(),
                        FileEntry::file(rest, content.len() as u64),
                    );
                }
            }
        }
        Ok(seen.into_values().collect())
    }

    async fn delete_file(&self, path: &str, recursive: bool) -> Result<()> {
        let key = Self::normalize(path);
        let mut files = self.files.lock().await;
        if files.remove(&key).is_some() {
            return Ok(());
        }
        let prefix = format!("{key}/");
        let matching: Vec<String> = files
            .keys()
            .filter(|candidate| candidate.starts_with(&prefix))
            .cloned()
            .collect();
        if matching.is_empty() {
            return Err(Error::Other(format!("no such file: `{path}`")));
        }
        if !recursive {
            return Err(Error::Other(format!(
                "`{path}` is a directory; pass recursive=true"
            )));
        }
        for key in matching {
            files.remove(&key);
        }
        Ok(())
    }
}

const DEFAULT_WRITE_BLOCKLIST: &[&str] = &[".git", "node_modules", "target", "dist", "build"];

/// Decorator that rejects writes and deletes under any blocklisted path
/// component (vendored/build dirs by default). Reads pass through
/// untouched. Mirrors everruns' `WriteBlocklistFileStore`.
pub struct WriteBlocklistFileSystem {
    inner: Arc<dyn FileSystem>,
    blocklist: Vec<String>,
}

impl WriteBlocklistFileSystem {
    /// Guard a store with the default blocklist: `.git`, `node_modules`,
    /// `target`, `dist`, `build`.
    pub fn wrap(inner: Arc<dyn FileSystem>) -> Self {
        Self::with_blocklist(inner, DEFAULT_WRITE_BLOCKLIST.iter().copied())
    }

    /// Guard a store with your own list of protected path components.
    pub fn with_blocklist(
        inner: Arc<dyn FileSystem>,
        blocklist: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        Self {
            inner,
            blocklist: blocklist.into_iter().map(Into::into).collect(),
        }
    }

    fn is_blocked(&self, path: &str) -> bool {
        Path::new(path).components().any(|component| {
            matches!(component, Component::Normal(part) if self.blocklist.iter().any(|blocked| part == blocked.as_str()))
        })
    }
}

#[async_trait]
impl FileSystem for WriteBlocklistFileSystem {
    async fn read_file(&self, path: &str) -> Result<String> {
        self.inner.read_file(path).await
    }

    async fn write_file(&self, path: &str, content: &str) -> Result<()> {
        if self.is_blocked(path) {
            return Err(Error::Other(format!(
                "write blocked: `{path}` is under a protected directory"
            )));
        }
        self.inner.write_file(path, content).await
    }

    async fn list_directory(&self, path: &str) -> Result<Vec<FileEntry>> {
        self.inner.list_directory(path).await
    }

    async fn delete_file(&self, path: &str, recursive: bool) -> Result<()> {
        if self.is_blocked(path) {
            return Err(Error::Other(format!(
                "delete blocked: `{path}` is under a protected directory"
            )));
        }
        self.inner.delete_file(path, recursive).await
    }

    /// Reads are never blocked, so this is a straight delegation — and it
    /// must be one, or the decorator would silently downgrade the inner
    /// store's efficient implementation to the trait default.
    async fn stat(&self, path: &str) -> Result<FileStat> {
        self.inner.stat(path).await
    }
}

/// Risk hints for the host, carried in [`ToolDefinition::metadata`] under
/// `"hints"` — the convention `agentyk-everruns-poc` established and the one
/// an approval middleware reads to decide what needs a human. Never sent to
/// the model.
///
/// Bundled tools declare their own so a host does not have to keep a list of
/// tool names in sync with the library.
fn hints(readonly: bool, destructive: bool) -> Value {
    json!({"hints": {"readonly": readonly, "destructive": destructive}})
}

/// Ceiling on `grep_files` matches in one result, so a broad pattern cannot
/// fill the model's context window.
const MAX_GREP_MATCHES: usize = 200;

/// Ceiling on files walked in one `grep_files` call, so a huge tree cannot
/// hang a turn.
const MAX_GREP_FILES: usize = 5_000;

struct ReadFileTool {
    store: Arc<dyn FileSystem>,
}

#[async_trait]
impl Tool for ReadFileTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            "read_file",
            "Read a file's text content. Use `offset`/`limit` to read part \
             of a large file instead of all of it.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Path relative to the workspace root."},
                    "offset": {"type": "integer", "description": "1-based line to start at. Default 1."},
                    "limit": {"type": "integer", "description": "Maximum number of lines to return."},
                },
                "required": ["path"],
            }),
        )
        .with_metadata(hints(true, false))
    }

    async fn execute(&self, arguments: Value, _context: &ToolContext) -> ToolOutput {
        let Some(path) = arguments["path"].as_str() else {
            return ToolOutput::error("missing required argument `path`");
        };
        let content = match self.store.read_file(path).await {
            Ok(content) => content,
            Err(error) => return ToolOutput::error(error.to_string()),
        };
        let offset = arguments["offset"].as_u64().unwrap_or(1).max(1);
        let limit = arguments["limit"].as_u64();
        if offset == 1 && limit.is_none() {
            return ToolOutput::text(content);
        }

        let total = content.lines().count() as u64;
        let taken: Vec<&str> = content
            .lines()
            .skip((offset - 1) as usize)
            .take(limit.unwrap_or(u64::MAX).min(usize::MAX as u64) as usize)
            .collect();
        let last = offset + taken.len() as u64 - 1;
        // The window is stated in the result, not just implied by it: a model
        // that cannot tell a truncated read from a whole file will summarize
        // half a file as if it were the whole one.
        ToolOutput::text(taken.join("\n")).with_metadata(json!({
            "lines": {"offset": offset, "returned": taken.len(), "total": total, "last": last},
        }))
    }
}

struct WriteFileTool {
    store: Arc<dyn FileSystem>,
}

#[async_trait]
impl Tool for WriteFileTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            "write_file",
            "Write (creating or overwriting) a file's full text content.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Path relative to the workspace root."},
                    "content": {"type": "string", "description": "The file's new full content."},
                },
                "required": ["path", "content"],
            }),
        )
        .with_metadata(hints(false, true))
    }

    async fn execute(&self, arguments: Value, _context: &ToolContext) -> ToolOutput {
        let Some(path) = arguments["path"].as_str() else {
            return ToolOutput::error("missing required argument `path`");
        };
        let Some(content) = arguments["content"].as_str() else {
            return ToolOutput::error("missing required argument `content`");
        };
        match self.store.write_file(path, content).await {
            Ok(()) => ToolOutput::text(format!("wrote {}", path)),
            Err(error) => ToolOutput::error(error.to_string()),
        }
    }
}

struct ListDirectoryTool {
    store: Arc<dyn FileSystem>,
}

#[async_trait]
impl Tool for ListDirectoryTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            "list_directory",
            "List the files and directories directly inside a directory.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Path relative to the workspace root. Defaults to the root."},
                },
            }),
        )
        .with_metadata(hints(true, false))
    }

    async fn execute(&self, arguments: Value, _context: &ToolContext) -> ToolOutput {
        let path = arguments["path"].as_str().unwrap_or("");
        match self.store.list_directory(path).await {
            Ok(entries) => {
                if entries.is_empty() {
                    return ToolOutput::text("(empty)");
                }
                let listing = entries
                    .into_iter()
                    .map(|entry| {
                        if entry.is_dir {
                            format!("{}/", entry.name)
                        } else {
                            format!("{} ({} bytes)", entry.name, entry.size)
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                ToolOutput::text(listing)
            }
            Err(error) => ToolOutput::error(error.to_string()),
        }
    }
}

struct DeleteFileTool {
    store: Arc<dyn FileSystem>,
}

#[async_trait]
impl Tool for DeleteFileTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            "delete_file",
            "Delete a file or (with recursive=true) a directory.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Path relative to the workspace root."},
                    "recursive": {"type": "boolean", "description": "Required to delete a non-empty directory. Default false."},
                },
                "required": ["path"],
            }),
        )
        .with_metadata(hints(false, true))
    }

    async fn execute(&self, arguments: Value, _context: &ToolContext) -> ToolOutput {
        let Some(path) = arguments["path"].as_str() else {
            return ToolOutput::error("missing required argument `path`");
        };
        let recursive = arguments["recursive"].as_bool().unwrap_or(false);
        match self.store.delete_file(path, recursive).await {
            Ok(()) => ToolOutput::text(format!("deleted {}", path)),
            Err(error) => ToolOutput::error(error.to_string()),
        }
    }
}

struct EditFileTool {
    store: Arc<dyn FileSystem>,
}

#[async_trait]
impl Tool for EditFileTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            "edit_file",
            "Replace an exact string in a file. Prefer this over write_file \
             for changing part of an existing file: it never rewrites what it \
             did not mean to touch.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Path relative to the workspace root."},
                    "old_string": {"type": "string", "description": "Exact text to replace, including indentation."},
                    "new_string": {"type": "string", "description": "Replacement text."},
                    "replace_all": {"type": "boolean", "description": "Replace every occurrence instead of requiring a unique one. Default false."},
                },
                "required": ["path", "old_string", "new_string"],
            }),
        )
        .with_metadata(hints(false, true))
    }

    async fn execute(&self, arguments: Value, _context: &ToolContext) -> ToolOutput {
        let (Some(path), Some(old), Some(new)) = (
            arguments["path"].as_str(),
            arguments["old_string"].as_str(),
            arguments["new_string"].as_str(),
        ) else {
            return ToolOutput::error(
                "missing required arguments: `path`, `old_string`, `new_string`",
            );
        };
        if old.is_empty() {
            return ToolOutput::error(
                "`old_string` must not be empty — use write_file to create a file",
            );
        }
        let content = match self.store.read_file(path).await {
            Ok(content) => content,
            Err(error) => return ToolOutput::error(error.to_string()),
        };

        // Uniqueness is the safety property, and it is why this is not just
        // "read, string-replace, write" done by the model itself: an ambiguous
        // match means the model does not know which occurrence it is editing,
        // so the edit is refused rather than applied to a coin flip. It is
        // the same guarantee everruns gets from a content hash, expressed in
        // terms the model can act on — it can add surrounding context and
        // retry, which it cannot do with a stale-hash error.
        let occurrences = content.matches(old).count();
        let replace_all = arguments["replace_all"].as_bool().unwrap_or(false);
        if occurrences == 0 {
            return ToolOutput::error(format!(
                "`old_string` does not appear in {path} — read the file and match its exact text"
            ));
        }
        if occurrences > 1 && !replace_all {
            return ToolOutput::error(format!(
                "`old_string` appears {occurrences} times in {path} — include surrounding \
                 context to make it unique, or pass replace_all"
            ));
        }

        let updated = match replace_all {
            true => content.replace(old, new),
            false => content.replacen(old, new, 1),
        };
        match self.store.write_file(path, &updated).await {
            Ok(()) => ToolOutput::text(format!("edited {path} ({occurrences} replacement(s))"))
                .with_metadata(json!({"path": path, "replacements": occurrences})),
            Err(error) => ToolOutput::error(error.to_string()),
        }
    }
}

struct GrepFilesTool {
    store: Arc<dyn FileSystem>,
}

impl GrepFilesTool {
    /// Walk the tree under `root`, newest-first order not guaranteed, and
    /// collect matching lines.
    ///
    /// Implemented against the [`FileSystem`] trait alone — `list_directory`
    /// plus `read_file` — so it searches an in-memory store, a real disk, or
    /// an adopter's remote workspace without knowing which it has. That is
    /// the reason this lives in the library instead of in each host.
    async fn search(&self, root: &str, pattern: &regex::Regex) -> Result<(Vec<String>, bool)> {
        let mut matches = Vec::new();
        let mut queue = vec![root.trim_matches('/').to_string()];
        let mut files_seen = 0usize;
        let mut truncated = false;

        while let Some(directory) = queue.pop() {
            let entries = match self.store.list_directory(&directory).await {
                Ok(entries) => entries,
                // An unreadable directory is skipped, not fatal: a search that
                // dies on one permission error is useless on a real tree.
                Err(_) => continue,
            };
            for entry in entries {
                let path = match directory.is_empty() {
                    true => entry.name.clone(),
                    false => format!("{directory}/{}", entry.name),
                };
                if entry.is_dir {
                    queue.push(path);
                    continue;
                }
                files_seen += 1;
                if files_seen > MAX_GREP_FILES {
                    truncated = true;
                    return Ok((matches, truncated));
                }
                // Binary and unreadable files are skipped the same way.
                let Ok(content) = self.store.read_file(&path).await else {
                    continue;
                };
                for (number, line) in content.lines().enumerate() {
                    if pattern.is_match(line) {
                        matches.push(format!("{path}:{}: {}", number + 1, line.trim_end()));
                        if matches.len() >= MAX_GREP_MATCHES {
                            truncated = true;
                            return Ok((matches, truncated));
                        }
                    }
                }
            }
        }
        Ok((matches, truncated))
    }
}

#[async_trait]
impl Tool for GrepFilesTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            "grep_files",
            "Search the workspace for a regular expression, returning \
             `path:line: text` matches. Prefer this over listing and reading \
             files one by one to find something.",
            json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string", "description": "Rust regular expression, matched per line."},
                    "path": {"type": "string", "description": "Directory to search under, relative to the workspace root. Defaults to the root."},
                },
                "required": ["pattern"],
            }),
        )
        .with_metadata(hints(true, false))
    }

    async fn execute(&self, arguments: Value, _context: &ToolContext) -> ToolOutput {
        let Some(pattern) = arguments["pattern"].as_str() else {
            return ToolOutput::error("missing required argument `pattern`");
        };
        let regex = match regex::Regex::new(pattern) {
            Ok(regex) => regex,
            // The model wrote the pattern, so it is the one that can fix it.
            Err(error) => return ToolOutput::error(format!("invalid pattern: {error}")),
        };
        let root = arguments["path"].as_str().unwrap_or("");
        match self.search(root, &regex).await {
            Ok((matches, truncated)) if matches.is_empty() => ToolOutput::text("(no matches)")
                .with_metadata(json!({"matches": 0, "truncated": truncated})),
            Ok((matches, truncated)) => {
                let count = matches.len();
                let mut body = matches.join("\n");
                if truncated {
                    body.push_str("\n… more matches were not shown; narrow the pattern or path");
                }
                ToolOutput::text(body)
                    .with_metadata(json!({"matches": count, "truncated": truncated}))
            }
            Err(error) => ToolOutput::error(error.to_string()),
        }
    }
}

struct StatFileTool {
    store: Arc<dyn FileSystem>,
}

#[async_trait]
impl Tool for StatFileTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            "stat_file",
            "Report whether a path exists, whether it is a directory, and \
             its size — without reading it.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Path relative to the workspace root."},
                },
                "required": ["path"],
            }),
        )
        .with_metadata(hints(true, false))
    }

    async fn execute(&self, arguments: Value, _context: &ToolContext) -> ToolOutput {
        let Some(path) = arguments["path"].as_str() else {
            return ToolOutput::error("missing required argument `path`");
        };
        match self.store.stat(path).await {
            // A missing path is a successful answer, not a tool error: the
            // model asked a question and got one.
            Ok(stat) if !stat.exists => ToolOutput::text(format!("{path}: does not exist"))
                .with_metadata(json!({"exists": false, "path": path})),
            Ok(stat) => {
                let kind = if stat.is_dir { "directory" } else { "file" };
                ToolOutput::text(format!("{path}: {kind}, {} bytes", stat.size)).with_metadata(
                    json!({"exists": true, "path": path, "is_dir": stat.is_dir, "size": stat.size}),
                )
            }
            Err(error) => ToolOutput::error(error.to_string()),
        }
    }
}

/// A capability exposing the workspace tools a coding agent needs:
/// `read_file` (whole or by line window), `write_file`, `edit_file`,
/// `list_directory`, `grep_files`, `stat_file`, and `delete_file`, all backed
/// by one [`FileSystem`].
///
/// Every definition carries risk hints in
/// [`ToolDefinition::metadata`](agentyk_core::tool::ToolDefinition::metadata)
/// under `"hints"`, so an approval middleware can gate the mutating ones
/// without hard-coding tool names.
pub struct FileSystemCapability {
    store: Arc<dyn FileSystem>,
}

impl FileSystemCapability {
    /// Expose a workspace to the model.
    pub fn new(store: impl FileSystem + 'static) -> Self {
        Self {
            store: Arc::new(store),
        }
    }

    /// Expose a workspace you already hold as an `Arc` — e.g. one shared
    /// with the rest of the host.
    pub fn from_arc(store: Arc<dyn FileSystem>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl Capability for FileSystemCapability {
    fn id(&self) -> &str {
        "file_system"
    }

    fn description(&self) -> &str {
        "Tools to read, search, write, edit, and delete files in the agent's workspace."
    }

    async fn tools(&self) -> Result<Vec<Arc<dyn Tool>>> {
        Ok(vec![
            Arc::new(ReadFileTool {
                store: self.store.clone(),
            }),
            Arc::new(WriteFileTool {
                store: self.store.clone(),
            }),
            Arc::new(EditFileTool {
                store: self.store.clone(),
            }),
            Arc::new(ListDirectoryTool {
                store: self.store.clone(),
            }),
            Arc::new(GrepFilesTool {
                store: self.store.clone(),
            }),
            Arc::new(StatFileTool {
                store: self.store.clone(),
            }),
            Arc::new(DeleteFileTool {
                store: self.store.clone(),
            }),
        ])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn real_disk_round_trips_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = RealDiskFileSystem::new(dir.path()).unwrap();
        store.write_file("notes/todo.txt", "hi").await.unwrap();
        assert_eq!(store.read_file("notes/todo.txt").await.unwrap(), "hi");
    }

    #[tokio::test]
    async fn real_disk_rejects_path_traversal() {
        let dir = tempfile::tempdir().unwrap();
        let store = RealDiskFileSystem::new(dir.path()).unwrap();
        let error = store.read_file("../escape.txt").await.unwrap_err();
        assert!(error.to_string().contains("escapes"));
    }

    #[tokio::test]
    async fn real_disk_lists_and_deletes() {
        let dir = tempfile::tempdir().unwrap();
        let store = RealDiskFileSystem::new(dir.path()).unwrap();
        store.write_file("a.txt", "1").await.unwrap();
        store.write_file("sub/b.txt", "2").await.unwrap();

        let entries = store.list_directory("").await.unwrap();
        assert_eq!(entries.len(), 2);
        assert!(entries.iter().any(|e| e.name == "a.txt" && !e.is_dir));
        assert!(entries.iter().any(|e| e.name == "sub" && e.is_dir));

        store.delete_file("a.txt", false).await.unwrap();
        assert!(store.read_file("a.txt").await.is_err());

        assert!(store.delete_file("sub", false).await.is_err());
        store.delete_file("sub", true).await.unwrap();
    }

    #[tokio::test]
    async fn in_memory_round_trips_and_lists() {
        let store = InMemoryFileSystem::new();
        store.write_file("a.txt", "1").await.unwrap();
        store.write_file("dir/b.txt", "22").await.unwrap();

        assert_eq!(store.read_file("a.txt").await.unwrap(), "1");

        let entries = store.list_directory("").await.unwrap();
        assert_eq!(entries.len(), 2);
        let file = entries.iter().find(|e| e.name == "a.txt").unwrap();
        assert_eq!(file.size, 1);
        let subdir = entries.iter().find(|e| e.name == "dir").unwrap();
        assert!(subdir.is_dir);
    }

    #[tokio::test]
    async fn in_memory_delete_requires_recursive_for_directories() {
        let store = InMemoryFileSystem::new();
        store.write_file("dir/b.txt", "x").await.unwrap();
        assert!(store.delete_file("dir", false).await.is_err());
        store.delete_file("dir", true).await.unwrap();
        assert!(store.list_directory("").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn write_blocklist_blocks_writes_but_not_reads() {
        let inner = Arc::new(InMemoryFileSystem::new());
        inner.write_file(".git/HEAD", "ref: main").await.unwrap();
        let guarded = WriteBlocklistFileSystem::wrap(inner);

        assert_eq!(guarded.read_file(".git/HEAD").await.unwrap(), "ref: main");
        assert!(guarded.write_file(".git/HEAD", "tampered").await.is_err());
        assert!(
            guarded
                .write_file("src/main.rs", "fn main() {}")
                .await
                .is_ok()
        );
    }
}
