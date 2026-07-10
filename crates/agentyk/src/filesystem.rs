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
pub struct FileEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
}

/// A workspace a [`FileSystemCapability`] reads and writes. Mirrors
/// everruns' `SessionFileSystem`, minus the `session_id` parameter — see the
/// module docs.
#[async_trait]
pub trait FileSystem: Send + Sync {
    async fn read_file(&self, path: &str) -> Result<String>;
    async fn write_file(&self, path: &str, content: &str) -> Result<()>;
    async fn list_directory(&self, path: &str) -> Result<Vec<FileEntry>>;
    async fn delete_file(&self, path: &str, recursive: bool) -> Result<()>;
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
            entries.push(FileEntry {
                name: entry.file_name().to_string_lossy().into_owned(),
                is_dir: metadata.is_dir(),
                size: metadata.len(),
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
                    seen.entry(dir.to_string()).or_insert(FileEntry {
                        name: dir.to_string(),
                        is_dir: true,
                        size: 0,
                    });
                }
                None => {
                    seen.insert(
                        rest.to_string(),
                        FileEntry {
                            name: rest.to_string(),
                            is_dir: false,
                            size: content.len() as u64,
                        },
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
    pub fn wrap(inner: Arc<dyn FileSystem>) -> Self {
        Self::with_blocklist(inner, DEFAULT_WRITE_BLOCKLIST.iter().copied())
    }

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
}

struct ReadFileTool {
    store: Arc<dyn FileSystem>,
}

#[async_trait]
impl Tool for ReadFileTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "read_file".into(),
            description: "Read a file's full text content.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Path relative to the workspace root."}
                },
                "required": ["path"],
            }),
        }
    }

    async fn execute(&self, arguments: Value, _context: &ToolContext) -> ToolOutput {
        let Some(path) = arguments["path"].as_str() else {
            return ToolOutput::error("missing required argument `path`");
        };
        match self.store.read_file(path).await {
            Ok(content) => ToolOutput::text(content),
            Err(error) => ToolOutput::error(error.to_string()),
        }
    }
}

struct WriteFileTool {
    store: Arc<dyn FileSystem>,
}

#[async_trait]
impl Tool for WriteFileTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "write_file".into(),
            description: "Write (creating or overwriting) a file's full text content.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Path relative to the workspace root."},
                    "content": {"type": "string", "description": "The file's new full content."},
                },
                "required": ["path", "content"],
            }),
        }
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
        ToolDefinition {
            name: "list_directory".into(),
            description: "List the files and directories directly inside a directory.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Path relative to the workspace root. Defaults to the root."},
                },
            }),
        }
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
        ToolDefinition {
            name: "delete_file".into(),
            description: "Delete a file or (with recursive=true) a directory.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Path relative to the workspace root."},
                    "recursive": {"type": "boolean", "description": "Required to delete a non-empty directory. Default false."},
                },
                "required": ["path"],
            }),
        }
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

/// A capability exposing `read_file`/`write_file`/`list_directory`/
/// `delete_file` tools backed by a [`FileSystem`]. Everruns' equivalent also
/// has `edit_file` (content-hash compare-and-swap), `grep_files`, and
/// `stat_file`; not ported here — this is the minimal set a coding agent
/// needs to get started, not a full port of everruns' seven-tool surface.
pub struct FileSystemCapability {
    store: Arc<dyn FileSystem>,
}

impl FileSystemCapability {
    pub fn new(store: impl FileSystem + 'static) -> Self {
        Self {
            store: Arc::new(store),
        }
    }

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
        "Tools to read, write, list, and delete files in the agent's workspace."
    }

    async fn tools(&self) -> Result<Vec<Arc<dyn Tool>>> {
        Ok(vec![
            Arc::new(ReadFileTool {
                store: self.store.clone(),
            }),
            Arc::new(WriteFileTool {
                store: self.store.clone(),
            }),
            Arc::new(ListDirectoryTool {
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
