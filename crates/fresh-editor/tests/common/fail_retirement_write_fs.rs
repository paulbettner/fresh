use fresh::model::filesystem::{
    default_search_file, DirEntry, FileMetadata, FilePermissions, FileReader, FileSearchCursor,
    FileSearchOptions, FileSystem, FileWriter, SearchMatch, StdFileSystem,
};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

enum FailureMode {
    Retirement,
    PendingError,
    DockModel,
    TargetLeaseAfterWorkspace { workspaces_dir: PathBuf },
}

pub struct FailRetirementWriteFs {
    inner: Arc<dyn FileSystem>,
    state_file: PathBuf,
    target: String,
    mode: FailureMode,
    armed: AtomicBool,
}

impl FailRetirementWriteFs {
    pub fn new(state_dir: PathBuf, target: String) -> Self {
        Self {
            inner: Arc::new(StdFileSystem),
            state_file: state_dir.join("orchestrator.json"),
            target,
            mode: FailureMode::Retirement,
            armed: AtomicBool::new(false),
        }
    }

    pub fn pending_error(state_dir: PathBuf, target: String) -> Self {
        Self {
            inner: Arc::new(StdFileSystem),
            state_file: state_dir.join("orchestrator.json"),
            target,
            mode: FailureMode::PendingError,
            armed: AtomicBool::new(false),
        }
    }

    pub fn dock_model(state_dir: PathBuf, folder_name: String) -> Self {
        Self {
            inner: Arc::new(StdFileSystem),
            state_file: state_dir.join("orchestrator.json"),
            target: folder_name,
            mode: FailureMode::DockModel,
            armed: AtomicBool::new(false),
        }
    }

    pub fn lose_target_lease_after_workspace(workspaces_dir: PathBuf, target: String) -> Self {
        Self {
            inner: Arc::new(StdFileSystem),
            state_file: PathBuf::new(),
            target,
            mode: FailureMode::TargetLeaseAfterWorkspace { workspaces_dir },
            armed: AtomicBool::new(false),
        }
    }

    pub fn is_armed(&self) -> bool {
        self.armed.load(Ordering::SeqCst)
    }

    pub fn arm(&self) {
        self.armed.store(true, Ordering::SeqCst);
    }

    fn state_at(
        &self,
        from: &Path,
        to: &Path,
    ) -> Option<serde_json::Map<String, serde_json::Value>> {
        if to != self.state_file {
            return None;
        }
        let bytes = self.inner.read_file(from).ok()?;
        let serde_json::Value::Object(state) = serde_json::from_slice(&bytes).ok()? else {
            return None;
        };
        Some(state)
    }

    fn is_target_retirement(&self, from: &Path, to: &Path) -> bool {
        let Some(state) = self.state_at(from, to) else {
            return false;
        };
        if state
            .keys()
            .any(|key| key.starts_with("orchestrator.create_journal:"))
        {
            return false;
        }
        state.iter().any(|(key, value)| {
            key.starts_with("orchestrator.pending:")
                && value["spec"]["projectPath"].as_str() == Some(self.target.as_str())
                && value["spec"]["name"]
                    .as_str()
                    .is_some_and(|name| !name.is_empty())
        })
    }

    fn is_target_pending_error(&self, from: &Path, to: &Path) -> bool {
        self.state_at(from, to).is_some_and(|state| {
            state.iter().any(|(key, value)| {
                key.starts_with("orchestrator.pending:")
                    && value["spec"]["projectPath"].as_str() == Some(self.target.as_str())
                    && value["phase"].as_str() == Some("error")
            })
        })
    }

    fn is_target_dock_model(&self, from: &Path, to: &Path) -> bool {
        self.state_at(from, to).is_some_and(|state| {
            state
                .get("orchestrator.dock.model")
                .and_then(|model| model["folders"].as_array())
                .is_some_and(|folders| {
                    folders
                        .iter()
                        .any(|folder| folder["name"].as_str() == Some(self.target.as_str()))
                })
        })
    }

    fn target_workspace_published(&self, workspaces_dir: &Path) -> bool {
        let Ok(entries) = self.inner.read_dir(workspaces_dir) else {
            return false;
        };
        entries.into_iter().any(|entry| {
            if !entry.is_file() || !entry.name.ends_with(".json") {
                return false;
            }
            self.inner
                .read_file(&workspaces_dir.join(entry.name))
                .ok()
                .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
                .and_then(|workspace| workspace["working_dir"].as_str().map(str::to_owned))
                .as_deref()
                == Some(self.target.as_str())
        })
    }

    fn should_lose_target_lease(&self, path: &Path, workspaces_dir: &Path) -> bool {
        if path.file_name().and_then(|name| name.to_str()) != Some("owner.json")
            || !self.target_workspace_published(workspaces_dir)
        {
            return false;
        }
        let expected_key = format!("workspace-target:{}", self.target);
        self.inner
            .read_file(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
            .and_then(|owner| owner["key"].as_str().map(str::to_owned))
            .as_deref()
            == Some(expected_key.as_str())
    }
}

impl FileSystem for FailRetirementWriteFs {
    fn read_file(&self, path: &Path) -> io::Result<Vec<u8>> {
        if self.armed.load(Ordering::SeqCst) {
            if let FailureMode::TargetLeaseAfterWorkspace { workspaces_dir } = &self.mode {
                if self.should_lose_target_lease(path, workspaces_dir) {
                    self.armed.store(false, Ordering::SeqCst);
                    return Ok(serde_json::to_vec(&serde_json::json!({
                        "key": format!("workspace-target:{}", self.target),
                        "token": "successor-test-token",
                    }))
                    .unwrap());
                }
            }
        }
        self.inner.read_file(path)
    }
    fn read_range(&self, path: &Path, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        self.inner.read_range(path, offset, len)
    }

    fn write_file(&self, path: &Path, data: &[u8]) -> io::Result<()> {
        self.inner.write_file(path, data)
    }

    fn create_file(&self, path: &Path) -> io::Result<Box<dyn FileWriter>> {
        self.inner.create_file(path)
    }

    fn open_file(&self, path: &Path) -> io::Result<Box<dyn FileReader>> {
        self.inner.open_file(path)
    }

    fn open_file_for_write(&self, path: &Path) -> io::Result<Box<dyn FileWriter>> {
        self.inner.open_file_for_write(path)
    }

    fn open_file_for_append(&self, path: &Path) -> io::Result<Box<dyn FileWriter>> {
        self.inner.open_file_for_append(path)
    }

    fn set_file_length(&self, path: &Path, len: u64) -> io::Result<()> {
        self.inner.set_file_length(path, len)
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        let fail = self.armed.load(Ordering::SeqCst)
            && match &self.mode {
                FailureMode::Retirement => self.is_target_retirement(from, to),
                FailureMode::PendingError => self.is_target_pending_error(from, to),
                FailureMode::DockModel => self.is_target_dock_model(from, to),
                FailureMode::TargetLeaseAfterWorkspace { .. } => false,
            };
        if fail {
            self.armed.store(false, Ordering::SeqCst);
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "fault-injected orchestrator state write failure",
            ));
        }
        self.inner.rename(from, to)
    }
    fn copy(&self, from: &Path, to: &Path) -> io::Result<u64> {
        self.inner.copy(from, to)
    }

    fn remove_file(&self, path: &Path) -> io::Result<()> {
        self.inner.remove_file(path)
    }

    fn remove_dir(&self, path: &Path) -> io::Result<()> {
        self.inner.remove_dir(path)
    }

    fn metadata(&self, path: &Path) -> io::Result<FileMetadata> {
        self.inner.metadata(path)
    }

    fn symlink_metadata(&self, path: &Path) -> io::Result<FileMetadata> {
        self.inner.symlink_metadata(path)
    }

    fn is_dir(&self, path: &Path) -> io::Result<bool> {
        self.inner.is_dir(path)
    }

    fn is_file(&self, path: &Path) -> io::Result<bool> {
        self.inner.is_file(path)
    }

    fn set_permissions(&self, path: &Path, permissions: &FilePermissions) -> io::Result<()> {
        self.inner.set_permissions(path, permissions)
    }

    fn read_dir(&self, path: &Path) -> io::Result<Vec<DirEntry>> {
        self.inner.read_dir(path)
    }

    fn create_dir(&self, path: &Path) -> io::Result<()> {
        self.inner.create_dir(path)
    }

    fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        self.inner.create_dir_all(path)
    }

    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        self.inner.canonicalize(path)
    }

    fn current_uid(&self) -> u32 {
        self.inner.current_uid()
    }

    fn search_file(
        &self,
        path: &Path,
        pattern: &str,
        options: &FileSearchOptions,
        cursor: &mut FileSearchCursor,
    ) -> io::Result<Vec<SearchMatch>> {
        default_search_file(&*self.inner, path, pattern, options, cursor)
    }

    fn sudo_write(
        &self,
        path: &Path,
        data: &[u8],
        mode: u32,
        uid: u32,
        gid: u32,
    ) -> io::Result<()> {
        self.inner.sudo_write(path, data, mode, uid, gid)
    }

    fn walk_files(
        &self,
        root: &Path,
        skip_dirs: &[&str],
        cancel: &AtomicBool,
        on_file: &mut dyn FnMut(&Path, &str) -> bool,
    ) -> io::Result<()> {
        self.inner.walk_files(root, skip_dirs, cancel, on_file)
    }
}
