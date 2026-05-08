//! Spawn-protocol payloads (`0x0300..=0x0304`). Owned by the classic-spawn
//! crate but hosted here so frame-kind discriminants and payload types
//! live together with the rest of the wire vocabulary.
//!
//! Encoding: bincode-v2 `legacy()` (fixed-int, little-endian) — same
//! config the rest of classic-proto uses, so these payloads piggy-back on
//! `encode_payload` / `decode_payload`.

use serde::{Deserialize, Serialize};

use crate::ids::NetId;

/// Hop counter limit. SpawnRequest frames with `hop > MAX_HOPS` are
/// dropped without forwarding (loop guard).
pub const MAX_HOPS: u8 = 2;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpawnRequest {
    pub req_id: u64,
    /// Plan-03 predicate source. Empty string is forbidden — callers that
    /// want unconditional placement should pass `"true"`.
    pub requires: String,
    /// Plan-03 rank source. Empty string means "use the default rank".
    pub rank: String,
    pub argv: Vec<String>,
    /// `KEY=VALUE` pairs. The CLI's own environment is NOT forwarded — only
    /// what the user explicitly listed via `--env`.
    pub env: Vec<String>,
    pub exclusive_device: bool,
    /// `None` means stdin should be `/dev/null`. `Some(_)` selects a source.
    pub stdin_kind: Option<StdinKind>,
    /// Routing hop counter. Originator sets to 0 when forwarding to an
    /// executor; executor increments before any further forward.
    pub hop: u8,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum StdinKind {
    /// Inherit the CLI's stdin (interactive terminals, pipes).
    Inherit,
    /// Stream stdin from a file the CLI reads on its end.
    File,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpawnAck {
    pub req_id: u64,
    pub net_id: NetId,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpawnDeny {
    pub req_id: u64,
    pub reason: DenyReason,
    pub detail: String,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DenyReason {
    /// Placement returned an empty candidate list.
    NoCandidates,
    /// Originator tried every ranked candidate; all denied.
    AllCandidatesRefused,
    /// Executor re-evaluated the predicate on its current ad and it
    /// did not match — racing ad updates can cause this.
    PredicateNotSatisfied,
    /// One or more devices the request asked for were taken.
    DeviceTaken,
    /// cgroup setup failed before fork.
    CgroupSetupFailed,
    /// `execve` failed in the helper child.
    ExecFailed,
    /// Hop counter exceeded `MAX_HOPS`.
    HopExceeded,
    /// Catch-all.
    Internal,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChildStdio {
    pub req_id: u64,
    pub stream: StdioStream,
    /// Empty `data` signals EOF for that stream.
    pub data: Vec<u8>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum StdioStream {
    Stdin,
    Stdout,
    Stderr,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChildExit {
    pub req_id: u64,
    /// Exit status code. `None` when the child died from a signal.
    pub code: Option<i32>,
    /// Signal number when the child died from a signal. `None` when the
    /// child exited normally.
    pub signal: Option<i32>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{MboxId, NodeId};
    use crate::proto::{decode_payload, encode_payload};

    fn rt<T>(v: &T) -> T
    where
        T: Serialize + serde::de::DeserializeOwned + std::fmt::Debug,
    {
        let bytes = encode_payload(v).unwrap();
        decode_payload::<T>(&bytes).unwrap()
    }

    #[test]
    fn frame_kinds_match_spec() {
        assert_eq!(crate::FrameKind::SpawnRequest as u16, 0x0300);
        assert_eq!(crate::FrameKind::SpawnAck as u16, 0x0301);
        assert_eq!(crate::FrameKind::SpawnDeny as u16, 0x0302);
        assert_eq!(crate::FrameKind::ChildStdio as u16, 0x0303);
        assert_eq!(crate::FrameKind::ChildExit as u16, 0x0304);
    }

    #[test]
    fn spawn_request_roundtrip() {
        let req = SpawnRequest {
            req_id: 7,
            requires: "any(gpu, gpu.vram_mb >= 80000)".into(),
            rank: "".into(),
            argv: vec!["python3".into(), "train.py".into()],
            env: vec!["RANK=0".into(), "WORLD_SIZE=1".into()],
            exclusive_device: true,
            stdin_kind: Some(StdinKind::Inherit),
            hop: 0,
        };
        assert_eq!(rt(&req), req);
    }

    #[test]
    fn spawn_request_no_stdin_roundtrips() {
        let req = SpawnRequest {
            req_id: 1,
            requires: "true".into(),
            rank: "".into(),
            argv: vec!["echo".into()],
            env: vec![],
            exclusive_device: false,
            stdin_kind: None,
            hop: 0,
        };
        assert_eq!(rt(&req), req);
    }

    #[test]
    fn spawn_ack_roundtrip() {
        let ack = SpawnAck {
            req_id: 42,
            net_id: NetId { node: NodeId([3; 16]), mbox: MboxId(17) },
        };
        assert_eq!(rt(&ack), ack);
    }

    #[test]
    fn spawn_deny_all_reasons_roundtrip() {
        for reason in [
            DenyReason::NoCandidates,
            DenyReason::AllCandidatesRefused,
            DenyReason::PredicateNotSatisfied,
            DenyReason::DeviceTaken,
            DenyReason::CgroupSetupFailed,
            DenyReason::ExecFailed,
            DenyReason::HopExceeded,
            DenyReason::Internal,
        ] {
            let d = SpawnDeny {
                req_id: 99,
                reason,
                detail: format!("{:?}", reason),
            };
            assert_eq!(rt(&d), d);
        }
    }

    #[test]
    fn child_stdio_roundtrip_each_stream() {
        for stream in [StdioStream::Stdin, StdioStream::Stdout, StdioStream::Stderr] {
            let f = ChildStdio {
                req_id: 1,
                stream,
                data: b"hello\n".to_vec(),
            };
            assert_eq!(rt(&f), f);
        }
    }

    #[test]
    fn child_stdio_eof_is_empty_data() {
        let f = ChildStdio { req_id: 1, stream: StdioStream::Stdout, data: vec![] };
        let back: ChildStdio = rt(&f);
        assert_eq!(back, f);
        assert!(back.data.is_empty());
    }

    #[test]
    fn child_exit_code_or_signal_roundtrip() {
        for ex in [
            ChildExit { req_id: 1, code: Some(0), signal: None },
            ChildExit { req_id: 2, code: Some(137), signal: None },
            ChildExit { req_id: 3, code: None, signal: Some(9) },
        ] {
            assert_eq!(rt(&ex), ex);
        }
    }
}
