//! Backend-neutral plugin loader provenance and async ownership policy.

use anyhow::{anyhow, Result};
use fresh_core::api::{
    PluginCommandContext, PluginInstanceId, PluginInvocation, PluginResponse, TrustedBuiltinPlugin,
};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

pub const PRIVATE_OMP_COMPANION_SNAPSHOT_EVENT: &str = "omp_companion_snapshot";

pub fn may_subscribe_to_event(context: &PluginCommandContext, event_name: &str) -> bool {
    event_name != PRIVATE_OMP_COMPANION_SNAPSHOT_EVENT || context.is_trusted_orchestrator()
}

pub fn may_receive_private_event(
    context: &PluginCommandContext,
    event_name: &str,
    active_instances: &ActivePluginInstances,
) -> bool {
    may_subscribe_to_event(context, event_name)
        && context.is_trusted_orchestrator()
        && active_instances
            .read()
            .map(|active| active.contains(&context.plugin_instance_id))
            .unwrap_or(false)
}

pub type ModuleDigest = [u8; 32];

#[derive(Clone)]
pub struct TrustedBuiltinSpec {
    identity: TrustedBuiltinPlugin,
    root: PathBuf,
    entrypoint: PathBuf,
    files: Arc<HashMap<PathBuf, ModuleDigest>>,
    sources: Arc<HashMap<PathBuf, Arc<[u8]>>>,
}

impl TrustedBuiltinSpec {
    pub fn from_embedded_files<'a>(
        identity: TrustedBuiltinPlugin,
        root: PathBuf,
        entrypoint: PathBuf,
        files: impl IntoIterator<Item = (PathBuf, &'a [u8])>,
    ) -> Result<Self> {
        if !is_safe_relative_path(&entrypoint) {
            return Err(anyhow!(
                "trusted built-in entrypoint must be a safe relative path"
            ));
        }
        let mut digests = HashMap::new();
        let mut sources = HashMap::new();
        for (path, bytes) in files {
            if !is_safe_relative_path(&path) {
                return Err(anyhow!(
                    "trusted built-in manifest path is not a safe relative path: {}",
                    path.display()
                ));
            }
            let bytes: Arc<[u8]> = Arc::from(bytes);
            if digests
                .insert(path.clone(), digest_bytes(bytes.as_ref()))
                .is_some()
            {
                return Err(anyhow!(
                    "duplicate trusted built-in manifest path: {}",
                    path.display()
                ));
            }
            sources.insert(path, bytes);
        }
        if !digests.contains_key(&entrypoint) {
            return Err(anyhow!(
                "trusted built-in entrypoint is absent from its manifest: {}",
                entrypoint.display()
            ));
        }
        Ok(Self {
            identity,
            root,
            entrypoint,
            files: Arc::new(digests),
            sources: Arc::new(sources),
        })
    }

    pub fn identity(&self) -> TrustedBuiltinPlugin {
        self.identity
    }

    pub fn entrypoint(&self) -> &Path {
        &self.entrypoint
    }

    pub fn sources(&self) -> &HashMap<PathBuf, Arc<[u8]>> {
        self.sources.as_ref()
    }

    pub fn verify_entrypoint(&self, path: &Path) -> Result<()> {
        let expected_entrypoint = self.root.join(&self.entrypoint);
        if path != expected_entrypoint {
            return Err(anyhow!(
                "trusted built-in entrypoint mismatch: expected {}, got {}",
                expected_entrypoint.display(),
                path.display()
            ));
        }
        let metadata = std::fs::symlink_metadata(&self.root).map_err(|error| {
            anyhow!(
                "trusted built-in root is unavailable ({}): {error}",
                self.root.display()
            )
        })?;
        if !metadata.file_type().is_dir() {
            return Err(anyhow!(
                "trusted built-in root is not a real directory: {}",
                self.root.display()
            ));
        }

        let mut actual = HashMap::with_capacity(self.files.len());
        digest_tree(&self.root, &self.root, &mut actual)?;
        if &actual != self.files.as_ref() {
            return Err(anyhow!(
                "trusted built-in module graph failed digest verification under {}",
                self.root.display()
            ));
        }
        Ok(())
    }
}

impl std::fmt::Debug for TrustedBuiltinSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TrustedBuiltinSpec")
            .field("identity", &self.identity)
            .field("root", &self.root)
            .field("entrypoint", &self.entrypoint)
            .field("file_count", &self.files.len())
            .finish()
    }
}

#[derive(Clone, Default)]
pub struct TrustedBuiltinManifest {
    entries: Arc<HashMap<String, TrustedBuiltinSpec>>,
}

impl TrustedBuiltinManifest {
    pub fn new(entries: impl IntoIterator<Item = (String, TrustedBuiltinSpec)>) -> Self {
        Self {
            entries: Arc::new(entries.into_iter().collect()),
        }
    }

    pub fn identity(&self, plugin_name: &str) -> Option<TrustedBuiltinPlugin> {
        self.entries
            .get(plugin_name)
            .map(TrustedBuiltinSpec::identity)
    }

    pub fn verify_spec(
        &self,
        plugin_name: &str,
        path: &Path,
    ) -> Result<Option<TrustedBuiltinSpec>> {
        let Some(spec) = self.entries.get(plugin_name) else {
            return Ok(None);
        };
        spec.verify_entrypoint(path)?;
        Ok(Some(spec.clone()))
    }

    pub fn verify(&self, plugin_name: &str, path: &Path) -> Result<Option<TrustedBuiltinPlugin>> {
        Ok(self
            .verify_spec(plugin_name, path)?
            .map(|spec| spec.identity()))
    }
}

impl std::fmt::Debug for TrustedBuiltinManifest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_map().entries(self.entries.iter()).finish()
    }
}

#[derive(Clone, Default)]
pub enum PluginLoadKind {
    #[default]
    External,
    Bundled {
        trusted_builtins: TrustedBuiltinManifest,
    },
    AgentScript {
        request_id: u64,
        plugin_instance_id: PluginInstanceId,
        window_id: fresh_core::WindowId,
        authority: fresh_core::api::AuthorityStamp,
        state_snapshot: Arc<RwLock<fresh_core::api::EditorStateSnapshot>>,
    },
}

impl PluginLoadKind {
    pub fn bundled(trusted_builtins: TrustedBuiltinManifest) -> Self {
        Self::Bundled { trusted_builtins }
    }

    pub fn trusted_builtin(&self, plugin_name: &str) -> Option<TrustedBuiltinPlugin> {
        match self {
            Self::Bundled { trusted_builtins } => trusted_builtins.identity(plugin_name),
            _ => None,
        }
    }

    pub fn verify_trusted_builtin(
        &self,
        plugin_name: &str,
        path: &Path,
    ) -> Result<Option<TrustedBuiltinPlugin>> {
        match self {
            Self::Bundled { trusted_builtins } => trusted_builtins.verify(plugin_name, path),
            _ => Ok(None),
        }
    }

    pub fn verified_trusted_builtin(
        &self,
        plugin_name: &str,
        path: &Path,
    ) -> Result<Option<TrustedBuiltinSpec>> {
        match self {
            Self::Bundled { trusted_builtins } => trusted_builtins.verify_spec(plugin_name, path),
            _ => Ok(None),
        }
    }
}

impl std::fmt::Debug for PluginLoadKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::External => f.write_str("External"),
            Self::Bundled { trusted_builtins } => f
                .debug_struct("Bundled")
                .field("trusted_builtins", trusted_builtins)
                .finish(),
            Self::AgentScript {
                request_id,
                plugin_instance_id,
                window_id,
                ..
            } => f
                .debug_struct("AgentScript")
                .field("request_id", request_id)
                .field("plugin_instance_id", plugin_instance_id)
                .field("window_id", window_id)
                .finish(),
        }
    }
}

#[derive(Clone)]
pub(crate) struct PluginLoadRecord {
    pub(crate) kind: PluginLoadKind,
    pub(crate) context: PluginCommandContext,
}

pub type PendingResponses = Arc<Mutex<HashMap<u64, tokio::sync::oneshot::Sender<PluginResponse>>>>;

#[derive(Debug, Clone)]
pub(crate) struct CallbackOwner {
    pub(crate) plugin_name: String,
    pub(crate) plugin_instance_id: PluginInstanceId,
    pub(crate) invocation: Option<PluginInvocation>,
}

#[derive(Debug, Clone)]
pub struct AsyncResourceOwner {
    pub plugin_name: String,
    pub plugin_instance_id: PluginInstanceId,
    pub context: PluginCommandContext,
}

#[derive(Debug, Clone, Copy)]
pub enum TrackedAsyncResource {
    VirtualBuffer(fresh_core::BufferId),
    CompositeBuffer(fresh_core::BufferId),
    Terminal(fresh_core::WindowTerminalId),
    WatchHandle(u64),
    Window(fresh_core::WindowId),
}

pub type AsyncResourceOwners = Arc<Mutex<HashMap<u64, AsyncResourceOwner>>>;
pub type ActivePluginInstances = Arc<RwLock<HashSet<PluginInstanceId>>>;

fn is_safe_relative_path(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

fn digest_bytes(bytes: &[u8]) -> ModuleDigest {
    Sha256::digest(bytes).into()
}

fn digest_file(path: &Path) -> Result<ModuleDigest> {
    let mut file = std::fs::File::open(path)
        .map_err(|error| anyhow!("failed to open trusted module {}: {error}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).map_err(|error| {
            anyhow!("failed to read trusted module {}: {error}", path.display())
        })?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher.finalize().into())
}

fn digest_tree(root: &Path, dir: &Path, out: &mut HashMap<PathBuf, ModuleDigest>) -> Result<()> {
    let entries = std::fs::read_dir(dir).map_err(|error| {
        anyhow!(
            "failed to read trusted module directory {}: {error}",
            dir.display()
        )
    })?;
    for entry in entries {
        let entry = entry.map_err(|error| {
            anyhow!(
                "failed to enumerate trusted module directory {}: {error}",
                dir.display()
            )
        })?;
        let path = entry.path();
        let metadata = std::fs::symlink_metadata(&path).map_err(|error| {
            anyhow!(
                "failed to inspect trusted module path {}: {error}",
                path.display()
            )
        })?;
        if metadata.file_type().is_symlink() {
            return Err(anyhow!(
                "trusted module graph contains a symbolic link: {}",
                path.display()
            ));
        }
        if metadata.file_type().is_dir() {
            digest_tree(root, &path, out)?;
            continue;
        }
        if !metadata.file_type().is_file() {
            return Err(anyhow!(
                "trusted module graph contains a non-file entry: {}",
                path.display()
            ));
        }
        let relative = path
            .strip_prefix(root)
            .map_err(|_| anyhow!("trusted module escaped its root: {}", path.display()))?
            .to_path_buf();
        out.insert(relative, digest_file(&path)?);
    }
    Ok(())
}
