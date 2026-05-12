//! Per-spawn namespace assembly. Mounts a local root plus zero-or-more
//! remote bind-mounts into a single rooted tree; the assembled
//! namespace is what a per-task FUSE bridge surfaces under
//! `/run/classic/ns/<MboxId>`.
//!
//! This commit lands the assembly + routing (pure-Rust, fully tested);
//! the FUSE bridge itself is gated behind a future `hw-fuse` feature
//! because `fuser` + the actual `mount(2)` syscall need root +
//! `CAP_SYS_ADMIN` at runtime.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use classic_proto::{MboxId, NodeId};

use crate::server::tree::Tree;

/// Source of one mount inside an assembled namespace. `Local` reuses
/// the per-daemon synthetic tree; `Remote` references a remote-fs
/// client (typed below as a trait so classic-4w3 can fill in the real
/// thing without this module taking a transport dep).
pub enum MountSource {
    Local(Arc<dyn Tree>),
    Remote {
        node: NodeId,
        client: Arc<dyn RemoteFs>,
        /// Path on the remote root the mount projects from. Empty
        /// string means "the remote's root".
        remote_root: String,
    },
}

/// Cross-node fs client. classic-4w3 implements this against the
/// Classic transport; tests pass a stub.
pub trait RemoteFs: Send + Sync {
    /// Validate the remote is alive. Called once during namespace
    /// assembly so a Bad NodeId fails fast.
    fn attach(&self) -> Result<(), NamespaceError>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BindRemote {
    pub node: NodeId,
    /// Absolute, non-root mount point inside the assembled namespace.
    pub at: PathBuf,
}

pub struct SpawnSpec {
    pub mbox: MboxId,
    pub bind_remote: Vec<BindRemote>,
}

#[derive(Debug, thiserror::Error)]
pub enum NamespaceError {
    #[error("`at` path must be absolute and non-root")]
    BadMountPoint,
    #[error("duplicate mount point: {0:?}")]
    DuplicateMountPoint(PathBuf),
    #[error("cannot bind-remote to self ({0:?})")]
    SelfBind(NodeId),
    #[error("remote attach failed for node {0:?}")]
    RemoteAttachFailed(NodeId),
}

pub struct Mount {
    pub at: PathBuf,
    pub source: MountSource,
}

impl std::fmt::Debug for Mount {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let kind = match &self.source {
            MountSource::Local(_) => "Local",
            MountSource::Remote { node, .. } => return write!(
                f,
                "Mount {{ at: {:?}, Remote(node={:?}) }}",
                self.at, node
            ),
        };
        write!(f, "Mount {{ at: {:?}, source: {} }}", self.at, kind)
    }
}

#[derive(Debug)]
pub struct Namespace {
    pub mbox: MboxId,
    pub mounts: Vec<Mount>,
    /// Where the FUSE bridge mounts the assembled namespace. Even when
    /// FUSE is disabled this is the canonical path the daemon refers
    /// to in logs / control-socket responses.
    pub fuse_mountpoint: PathBuf,
}

impl Namespace {
    /// Build a namespace per the plan's assembly algorithm. Doesn't
    /// touch FUSE; that's `mount_fuse()`'s job.
    pub fn build_for_spawn(
        spec: &SpawnSpec,
        local: Arc<dyn Tree>,
        local_node: NodeId,
        resolve_remote: impl Fn(NodeId) -> Result<Arc<dyn RemoteFs>, NamespaceError>,
    ) -> Result<Self, NamespaceError> {
        let mut mounts: Vec<Mount> = vec![Mount {
            at: PathBuf::from("/"),
            source: MountSource::Local(local),
        }];

        for bind in &spec.bind_remote {
            if bind.node == local_node {
                return Err(NamespaceError::SelfBind(bind.node));
            }
            if !bind.at.is_absolute() || bind.at == Path::new("/") {
                return Err(NamespaceError::BadMountPoint);
            }
            if mounts.iter().any(|m| m.at == bind.at) {
                return Err(NamespaceError::DuplicateMountPoint(bind.at.clone()));
            }
            let client = resolve_remote(bind.node)?;
            client
                .attach()
                .map_err(|_| NamespaceError::RemoteAttachFailed(bind.node))?;
            mounts.push(Mount {
                at: bind.at.clone(),
                source: MountSource::Remote {
                    node: bind.node,
                    client,
                    remote_root: String::new(),
                },
            });
        }

        // Longest-prefix-first dispatch ordering (deepest paths win).
        mounts.sort_by_key(|m| -(m.at.components().count() as i64));

        Ok(Self {
            mbox: spec.mbox,
            mounts,
            fuse_mountpoint: PathBuf::from(format!("/run/classic/ns/{}", spec.mbox.0)),
        })
    }

    /// Resolve `path` to the (mount, residual-path) the caller should
    /// dispatch the 9P walk through. `path` is always absolute starting
    /// with `/`.
    pub fn route<'a>(&'a self, path: &Path) -> Option<(&'a Mount, PathBuf)> {
        for mount in &self.mounts {
            if let Ok(residual) = path.strip_prefix(&mount.at) {
                let mut res = PathBuf::from("/");
                res.push(residual);
                return Some((mount, res));
            }
        }
        None
    }
}

/// Placeholder for a real FUSE handle. The cargo-feature-gated FUSE
/// bridge will replace this with an `fuser::BackgroundSession` wrapper.
pub struct FuseHandle {
    pub mountpoint: PathBuf,
    /// True only when `mount_fuse` succeeded against a real FUSE
    /// device. The default impl flips it false.
    pub mounted: bool,
}

impl Namespace {
    /// Mount the namespace under `fuse_mountpoint`. The default-feature
    /// build returns a `FuseHandle { mounted: false }` after creating
    /// the directory — the real mount requires `hw-fuse` + root.
    pub fn mount_fuse(&self) -> std::io::Result<FuseHandle> {
        std::fs::create_dir_all(&self.fuse_mountpoint).ok();
        Ok(FuseHandle {
            mountpoint: self.fuse_mountpoint.clone(),
            mounted: false,
        })
    }

    /// Unmount handle. Symmetric with `mount_fuse`; on the no-op path
    /// it just rmdirs the mountpoint best-effort.
    pub fn unmount(handle: FuseHandle) {
        if !handle.mounted {
            let _ = std::fs::remove_dir(&handle.mountpoint);
        }
        // Real FUSE bridge: umount2(handle.mountpoint, MNT_DETACH).
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::tree::EmptyTree;

    fn nid(n: u8) -> NodeId {
        let mut b = [0u8; 16];
        b[0] = n;
        NodeId(b)
    }

    struct OkRemote;
    impl RemoteFs for OkRemote {
        fn attach(&self) -> Result<(), NamespaceError> {
            Ok(())
        }
    }

    struct FailRemote;
    impl RemoteFs for FailRemote {
        fn attach(&self) -> Result<(), NamespaceError> {
            Err(NamespaceError::RemoteAttachFailed(nid(0)))
        }
    }

    fn local() -> Arc<dyn Tree> {
        Arc::new(EmptyTree)
    }

    #[test]
    fn local_only_namespace_has_just_root_mount() {
        let spec = SpawnSpec { mbox: MboxId(1), bind_remote: vec![] };
        let ns = Namespace::build_for_spawn(&spec, local(), nid(1), |_| {
            Ok::<_, NamespaceError>(Arc::new(OkRemote))
        })
        .unwrap();
        assert_eq!(ns.mounts.len(), 1);
        assert_eq!(ns.mounts[0].at, Path::new("/"));
        assert_eq!(
            ns.fuse_mountpoint.to_string_lossy(),
            "/run/classic/ns/1"
        );
    }

    #[test]
    fn bind_remote_appended_and_attach_validated() {
        let spec = SpawnSpec {
            mbox: MboxId(2),
            bind_remote: vec![BindRemote {
                node: nid(2),
                at: PathBuf::from("/cluster/B"),
            }],
        };
        let ns = Namespace::build_for_spawn(&spec, local(), nid(1), |_| {
            Ok::<_, NamespaceError>(Arc::new(OkRemote))
        })
        .unwrap();
        assert_eq!(ns.mounts.len(), 2);
        // Longest-prefix-first ordering: /cluster/B comes before /.
        assert_eq!(ns.mounts[0].at, Path::new("/cluster/B"));
        assert_eq!(ns.mounts[1].at, Path::new("/"));
    }

    #[test]
    fn self_bind_rejected() {
        let spec = SpawnSpec {
            mbox: MboxId(3),
            bind_remote: vec![BindRemote {
                node: nid(1), // same as local
                at: PathBuf::from("/cluster/me"),
            }],
        };
        let err = Namespace::build_for_spawn(&spec, local(), nid(1), |_| {
            Ok::<_, NamespaceError>(Arc::new(OkRemote))
        })
        .unwrap_err();
        assert!(matches!(err, NamespaceError::SelfBind(_)));
    }

    #[test]
    fn root_mount_point_rejected() {
        let spec = SpawnSpec {
            mbox: MboxId(4),
            bind_remote: vec![BindRemote {
                node: nid(2),
                at: PathBuf::from("/"),
            }],
        };
        let err = Namespace::build_for_spawn(&spec, local(), nid(1), |_| {
            Ok::<_, NamespaceError>(Arc::new(OkRemote))
        })
        .unwrap_err();
        assert!(matches!(err, NamespaceError::BadMountPoint));
    }

    #[test]
    fn relative_mount_point_rejected() {
        let spec = SpawnSpec {
            mbox: MboxId(5),
            bind_remote: vec![BindRemote {
                node: nid(2),
                at: PathBuf::from("relative/path"),
            }],
        };
        let err = Namespace::build_for_spawn(&spec, local(), nid(1), |_| {
            Ok::<_, NamespaceError>(Arc::new(OkRemote))
        })
        .unwrap_err();
        assert!(matches!(err, NamespaceError::BadMountPoint));
    }

    #[test]
    fn duplicate_mount_point_rejected() {
        let spec = SpawnSpec {
            mbox: MboxId(6),
            bind_remote: vec![
                BindRemote { node: nid(2), at: PathBuf::from("/cluster/x") },
                BindRemote { node: nid(3), at: PathBuf::from("/cluster/x") },
            ],
        };
        let err = Namespace::build_for_spawn(&spec, local(), nid(1), |_| {
            Ok::<_, NamespaceError>(Arc::new(OkRemote))
        })
        .unwrap_err();
        assert!(matches!(err, NamespaceError::DuplicateMountPoint(_)));
    }

    #[test]
    fn remote_attach_failure_surfaces() {
        let spec = SpawnSpec {
            mbox: MboxId(7),
            bind_remote: vec![BindRemote {
                node: nid(2),
                at: PathBuf::from("/cluster/B"),
            }],
        };
        let err = Namespace::build_for_spawn(&spec, local(), nid(1), |_| {
            Ok::<_, NamespaceError>(Arc::new(FailRemote))
        })
        .unwrap_err();
        assert!(matches!(err, NamespaceError::RemoteAttachFailed(_)));
    }

    #[test]
    fn route_picks_longest_prefix() {
        let spec = SpawnSpec {
            mbox: MboxId(8),
            bind_remote: vec![
                BindRemote { node: nid(2), at: PathBuf::from("/cluster") },
                BindRemote { node: nid(3), at: PathBuf::from("/cluster/B") },
            ],
        };
        let ns = Namespace::build_for_spawn(&spec, local(), nid(1), |_| {
            Ok::<_, NamespaceError>(Arc::new(OkRemote))
        })
        .unwrap();
        let (mount, residual) = ns.route(Path::new("/cluster/B/dev/gpu/0")).unwrap();
        assert_eq!(mount.at, Path::new("/cluster/B"));
        assert_eq!(residual, Path::new("/dev/gpu/0"));
        let (mount, _) = ns.route(Path::new("/etc/hostname")).unwrap();
        // No bind on /etc — falls through to /.
        assert_eq!(mount.at, Path::new("/"));
    }

    #[test]
    fn mount_fuse_default_creates_dir_returns_unmounted_handle() {
        let spec = SpawnSpec { mbox: MboxId(99), bind_remote: vec![] };
        let ns = Namespace::build_for_spawn(&spec, local(), nid(1), |_| {
            Ok::<_, NamespaceError>(Arc::new(OkRemote))
        })
        .unwrap();
        // Override fuse_mountpoint to a temp path so the test doesn't
        // pollute /run/classic/ns.
        let mut ns = ns;
        let tmp = tempfile::tempdir().unwrap();
        ns.fuse_mountpoint = tmp.path().join("99");
        let handle = ns.mount_fuse().unwrap();
        assert!(!handle.mounted);
        assert!(handle.mountpoint.exists());
        Namespace::unmount(handle);
        // Cleanup should rmdir the (empty) mount point.
        assert!(!tmp.path().join("99").exists());
    }
}
