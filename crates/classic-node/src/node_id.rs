use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use classic_proto::NodeId;

const NODE_ID_FILENAME: &str = "node_id";
const STATE_DIR_MODE: u32 = 0o700;
const NODE_ID_MODE: u32 = 0o600;

#[derive(Debug, thiserror::Error)]
pub enum NodeIdError {
    #[error("io error on {path}: {source}")]
    Io { path: PathBuf, source: std::io::Error },
    #[error("node_id file at {path} is {actual} bytes; expected 16")]
    Truncated { path: PathBuf, actual: usize },
    #[error("getrandom failed: {0}")]
    Random(getrandom::Error),
}

/// Read or create the persistent `NodeId` for this daemon.
///
/// On first start the state dir is created with mode 0700, 16 random bytes
/// are written atomically (write + rename) to `<state_dir>/node_id` with
/// mode 0600, and that NodeId is returned. On subsequent starts the existing
/// file is read verbatim. A truncated or oversized file is treated as
/// corruption — the operator must intervene rather than have the daemon
/// silently mint a new identity.
pub fn ensure_node_id(state_dir: &Path) -> Result<NodeId, NodeIdError> {
    ensure_state_dir(state_dir)?;

    let path = state_dir.join(NODE_ID_FILENAME);
    if path.exists() {
        return read_existing(&path);
    }
    create_fresh(state_dir, &path)
}

fn ensure_state_dir(state_dir: &Path) -> Result<(), NodeIdError> {
    if !state_dir.exists() {
        fs::create_dir_all(state_dir).map_err(|source| NodeIdError::Io {
            path: state_dir.to_path_buf(),
            source,
        })?;
    }
    fs::set_permissions(state_dir, fs::Permissions::from_mode(STATE_DIR_MODE)).map_err(|source| {
        NodeIdError::Io {
            path: state_dir.to_path_buf(),
            source,
        }
    })
}

fn read_existing(path: &Path) -> Result<NodeId, NodeIdError> {
    let mut buf = Vec::with_capacity(16);
    File::open(path)
        .and_then(|mut f| f.read_to_end(&mut buf))
        .map_err(|source| NodeIdError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    if buf.len() != 16 {
        return Err(NodeIdError::Truncated {
            path: path.to_path_buf(),
            actual: buf.len(),
        });
    }
    let mut id = [0u8; 16];
    id.copy_from_slice(&buf);
    Ok(NodeId(id))
}

fn create_fresh(state_dir: &Path, final_path: &Path) -> Result<NodeId, NodeIdError> {
    let mut id = [0u8; 16];
    getrandom::getrandom(&mut id).map_err(NodeIdError::Random)?;

    let tmp_name = format!(".{}.tmp.{}", NODE_ID_FILENAME, std::process::id());
    let tmp_path = state_dir.join(tmp_name);
    {
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(NODE_ID_MODE)
            .open(&tmp_path)
            .map_err(|source| NodeIdError::Io {
                path: tmp_path.clone(),
                source,
            })?;
        f.write_all(&id).map_err(|source| NodeIdError::Io {
            path: tmp_path.clone(),
            source,
        })?;
        f.sync_all().map_err(|source| NodeIdError::Io {
            path: tmp_path.clone(),
            source,
        })?;
    }
    fs::rename(&tmp_path, final_path).map_err(|source| NodeIdError::Io {
        path: final_path.to_path_buf(),
        source,
    })?;
    Ok(NodeId(id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn mode_of(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn generate_and_persist() {
        let dir = TempDir::new().unwrap();
        let state = dir.path().join("state");
        let id1 = ensure_node_id(&state).unwrap();
        assert_eq!(mode_of(&state), STATE_DIR_MODE);
        let file = state.join(NODE_ID_FILENAME);
        assert_eq!(mode_of(&file), NODE_ID_MODE);
        assert_eq!(fs::read(&file).unwrap().len(), 16);

        let id2 = ensure_node_id(&state).unwrap();
        assert_eq!(id1.as_bytes(), id2.as_bytes());
    }

    #[test]
    fn malformed_node_id_file_is_fatal() {
        let dir = TempDir::new().unwrap();
        let state = dir.path().join("state");
        fs::create_dir_all(&state).unwrap();
        fs::write(state.join(NODE_ID_FILENAME), b"shorty").unwrap();
        let err = ensure_node_id(&state).unwrap_err();
        match err {
            NodeIdError::Truncated { actual, .. } => assert_eq!(actual, 6),
            other => panic!("expected Truncated, got {other:?}"),
        }
    }

    #[test]
    fn fresh_state_dir_gets_0700() {
        let dir = TempDir::new().unwrap();
        let state = dir.path().join("nested").join("state");
        ensure_node_id(&state).unwrap();
        assert_eq!(mode_of(&state), STATE_DIR_MODE);
    }

    #[test]
    fn existing_state_dir_permissions_are_tightened() {
        let dir = TempDir::new().unwrap();
        let state = dir.path().join("state");
        fs::create_dir_all(&state).unwrap();
        fs::set_permissions(&state, fs::Permissions::from_mode(0o755)).unwrap();
        ensure_node_id(&state).unwrap();
        assert_eq!(mode_of(&state), STATE_DIR_MODE);
    }

    #[test]
    fn two_fresh_dirs_get_distinct_ids() {
        let a = TempDir::new().unwrap();
        let b = TempDir::new().unwrap();
        let id_a = ensure_node_id(a.path()).unwrap();
        let id_b = ensure_node_id(b.path()).unwrap();
        assert_ne!(id_a.as_bytes(), id_b.as_bytes());
    }
}
