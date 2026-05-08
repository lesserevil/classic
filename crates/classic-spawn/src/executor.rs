//! Executor state machine. The half that runs on the chosen node:
//! revalidate the predicate against the local ad, allocate caps, set up
//! the cgroup scope, fork+exec the user binary, and report exit.
//!
//! For testability — and because real cgroup placement requires root —
//! the cgroup-scope step is captured by a `ScopeProvider` trait. Tests
//! pass a `NoOpScopeProvider`; the daemon-level impl will eventually
//! wire `classic-cap`'s `ScopeHandle` + `BpfLoader::attach`.

use classic_proto::{NodeId, SpawnRequest};

use crate::error::SpawnError;
use crate::exec::{exec_command, ChildHandle};

/// Trait the executor uses to project a node's ad into a "matches the
/// predicate?" answer. Tests substitute a closure-backed mock; the
/// daemon wires this to classic-place's `matches()` against the local
/// AdStore.
pub trait LocalAdMatcher: Send + Sync {
    fn matches(&self, requires: &str) -> Result<bool, SpawnError>;
}

/// Sets up + tears down the per-task cgroup scope. Real impl lives at
/// the daemon level (root-required); tests use NoOp.
pub trait ScopeProvider: Send + Sync {
    /// Create the scope and return an opaque handle whose Drop tears
    /// it down. The handle is `Send + 'static` so it can ride alongside
    /// the spawned child.
    fn enter_scope(
        &self,
        req: &SpawnRequest,
    ) -> Result<Box<dyn std::any::Any + Send>, SpawnError>;
}

pub struct NoOpScopeProvider;
impl ScopeProvider for NoOpScopeProvider {
    fn enter_scope(
        &self,
        _req: &SpawnRequest,
    ) -> Result<Box<dyn std::any::Any + Send>, SpawnError> {
        Ok(Box::new(()))
    }
}

/// Run the executor pipeline for one SpawnRequest.
///
/// Returns the `(NodeId, ChildHandle, scope_guard)` triple on success.
/// The caller relays stdio frames through the child handle's pumps and
/// awaits `wait()` for the exit status.
pub async fn run_executor(
    req: &SpawnRequest,
    self_id: NodeId,
    matcher: &dyn LocalAdMatcher,
    scope: &dyn ScopeProvider,
) -> Result<(NodeId, ChildHandle, Box<dyn std::any::Any + Send>), SpawnError> {
    // Step 1: revalidate the predicate against our current ad. Racing
    // ad updates can render an old placement decision stale.
    if !matcher.matches(&req.requires)? {
        return Err(SpawnError::PredicateNotSatisfied(req.requires.clone()));
    }

    // Step 2: cgroup scope (real impl: cap acquire + cgroup mkdir + BPF
    // attach; NoOp impl: nothing).
    let scope_guard = scope.enter_scope(req)?;

    // Step 3: parse env "KEY=VAL" pairs. Empty `env` means no env vars
    // beyond what the parent shell would have, but our exec path
    // already env_clear()s.
    let env: Vec<(String, String)> = req
        .env
        .iter()
        .filter_map(|kv| {
            let mut split = kv.splitn(2, '=');
            let k = split.next()?.to_string();
            let v = split.next()?.to_string();
            Some((k, v))
        })
        .collect();

    // Step 4: launch the child.
    let child = exec_command(req.req_id, &req.argv, &env, req.stdin_kind).await?;
    Ok((self_id, child, scope_guard))
}

#[cfg(test)]
mod tests {
    use super::*;
    use classic_proto::{NodeId, SpawnRequest};

    fn req(argv: &[&str]) -> SpawnRequest {
        SpawnRequest {
            req_id: 1,
            requires: "true".into(),
            rank: "".into(),
            argv: argv.iter().map(|s| s.to_string()).collect(),
            env: vec![],
            exclusive_device: false,
            stdin_kind: None,
            hop: 0,
        }
    }

    fn id(n: u8) -> NodeId {
        NodeId([n; 16])
    }

    struct AlwaysMatch;
    impl LocalAdMatcher for AlwaysMatch {
        fn matches(&self, _: &str) -> Result<bool, SpawnError> {
            Ok(true)
        }
    }

    struct NeverMatch;
    impl LocalAdMatcher for NeverMatch {
        fn matches(&self, _: &str) -> Result<bool, SpawnError> {
            Ok(false)
        }
    }

    #[tokio::test]
    async fn happy_path_runs_and_exits_zero() {
        let (node, mut child, _scope) = run_executor(
            &req(&["/bin/echo", "hi"]),
            id(7),
            &AlwaysMatch,
            &NoOpScopeProvider,
        )
        .await
        .unwrap();
        assert_eq!(node, id(7));
        let mut out = Vec::new();
        while let Some(chunk) = child.stdout.recv().await {
            if chunk.is_empty() {
                break;
            }
            out.extend(chunk);
        }
        let info = child.wait().await.unwrap();
        assert_eq!(info.code, Some(0));
        assert_eq!(&out, b"hi\n");
    }

    #[tokio::test]
    async fn predicate_revalidation_fails_with_predicate_not_satisfied() {
        let err = run_executor(
            &req(&["/bin/echo", "hi"]),
            id(1),
            &NeverMatch,
            &NoOpScopeProvider,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, SpawnError::PredicateNotSatisfied(_)));
    }

    #[tokio::test]
    async fn missing_binary_surfaces_exec_failed() {
        let err = run_executor(
            &req(&["/no/such/bin"]),
            id(1),
            &AlwaysMatch,
            &NoOpScopeProvider,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, SpawnError::ExecFailed(_)));
    }

    #[tokio::test]
    async fn env_pairs_parse_and_apply() {
        let mut r = req(&["/bin/sh", "-c", "echo $CLASSIC_X"]);
        r.env = vec!["CLASSIC_X=ok".into()];
        let (_node, mut child, _scope) =
            run_executor(&r, id(1), &AlwaysMatch, &NoOpScopeProvider)
                .await
                .unwrap();
        let mut out = Vec::new();
        while let Some(c) = child.stdout.recv().await {
            if c.is_empty() {
                break;
            }
            out.extend(c);
        }
        child.wait().await.unwrap();
        assert_eq!(&out, b"ok\n");
    }

    #[tokio::test]
    async fn malformed_env_entries_skipped() {
        let mut r = req(&["/bin/sh", "-c", "echo done"]);
        r.env = vec!["NOEQUALS_NO_VALUE".into(), "GOOD=v".into()];
        let (_n, mut child, _s) = run_executor(&r, id(1), &AlwaysMatch, &NoOpScopeProvider)
            .await
            .unwrap();
        let mut out = Vec::new();
        while let Some(c) = child.stdout.recv().await {
            if c.is_empty() {
                break;
            }
            out.extend(c);
        }
        child.wait().await.unwrap();
        assert_eq!(&out, b"done\n");
    }
}
