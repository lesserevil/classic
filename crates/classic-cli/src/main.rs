use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

use classic_node::control::{self, Request};

#[derive(Parser, Debug)]
#[command(name = "classic", version, about = "Classic SSI cluster CLI")]
struct Args {
    /// Daemon state directory (where the admin socket lives). Defaults
    /// to `/var/lib/classicd`.
    #[arg(long, value_name = "DIR", default_value = "/var/lib/classicd")]
    state_dir: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Inspect node ads gossiped through the cluster.
    Ad {
        #[command(subcommand)]
        cmd: AdCommand,
    },
    /// Spawn a process on a cluster node matching the given predicate.
    Spawn(SpawnArgs),
}

#[derive(Subcommand, Debug)]
enum AdCommand {
    /// Print all known ads.
    List {
        /// Emit JSON (default: human-readable text).
        #[arg(long)]
        json: bool,
    },
    /// Print one ad by hostname.
    Show {
        hostname: String,
        #[arg(long)]
        json: bool,
    },
}

#[derive(clap::Args, Debug)]
struct SpawnArgs {
    /// Plan-03 predicate (REQUIRED). Empty string is forbidden — pass
    /// `true` for "any node".
    #[arg(long, value_name = "PREDICATE")]
    requires: String,
    /// Optional rank expression. Default: least-loaded with idle-GPU
    /// preference. `allow_hyphen_values` so a leading `-` (the common
    /// case for "negate cpu_pct" expressions) doesn't get parsed as
    /// a CLI flag.
    #[arg(long, value_name = "EXPR", allow_hyphen_values = true)]
    rank: Option<String>,
    /// Mark every acquired device cap exclusive. Default: shared.
    #[arg(long, default_value_t = false)]
    exclusive_device: bool,
    /// Stream `<file>` as the child's stdin. Use `-` to inherit the
    /// CLI's stdin. Omit for `/dev/null` stdin.
    #[arg(long, value_name = "FILE_OR_DASH")]
    stdin: Option<String>,
    /// Repeatable `KEY=VAL` env entries. The CLI's own env is NOT
    /// forwarded — only what's listed here.
    #[arg(long = "env", value_name = "KEY=VAL")]
    env: Vec<String>,
    /// The child's argv. Everything after `--` on the command line.
    #[arg(last = true, required = true)]
    argv: Vec<String>,
}

fn main() -> ExitCode {
    let args = Args::parse();
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("classic: failed to start runtime: {e}");
            return ExitCode::from(70);
        }
    };
    match runtime.block_on(run(args)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(CliErr::Usage(m)) => {
            eprintln!("classic: {m}");
            ExitCode::from(1)
        }
        Err(CliErr::Spawn(m)) => {
            eprintln!("classic: spawn failed: {m}");
            ExitCode::from(2)
        }
        Err(CliErr::Exit(code)) => ExitCode::from(code),
        Err(CliErr::Other(m)) => {
            eprintln!("classic: {m}");
            ExitCode::from(1)
        }
    }
}

#[derive(Debug)]
enum CliErr {
    Usage(String),
    Spawn(String),
    /// Propagated child exit code (0..=255) or `128 + signum`.
    Exit(u8),
    Other(String),
}

impl<E: std::fmt::Display> From<E> for CliErr {
    fn from(e: E) -> Self {
        CliErr::Other(e.to_string())
    }
}

async fn run(args: Args) -> Result<(), CliErr> {
    match args.command {
        Command::Ad { cmd } => match cmd {
            AdCommand::List { json } => ad_list(&args.state_dir, json).await,
            AdCommand::Show { hostname, json } => ad_show(&args.state_dir, &hostname, json).await,
        },
        Command::Spawn(s) => spawn(&args.state_dir, s).await,
    }
}

async fn ad_list(state_dir: &std::path::Path, json: bool) -> Result<(), CliErr> {
    let v = control::send_request(state_dir, &Request::AdList)
        .await
        .map_err(|e| CliErr::Other(e.to_string()))?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&v).map_err(|e| CliErr::Other(e.to_string()))?
        );
    } else if let Some(arr) = v.get("ads").and_then(|x| x.as_array()) {
        for ad in arr {
            let hn = ad.get("hostname").and_then(|x| x.as_str()).unwrap_or("?");
            let gen = ad.get("generation").and_then(|x| x.as_u64()).unwrap_or(0);
            let gpus = ad
                .get("gpus")
                .and_then(|x| x.as_array())
                .map(|a| a.len())
                .unwrap_or(0);
            println!("{:<24} gen={:<6} gpus={}", hn, gen, gpus);
        }
    } else {
        println!("(no ads)");
    }
    Ok(())
}

async fn ad_show(
    state_dir: &std::path::Path,
    hostname: &str,
    json: bool,
) -> Result<(), CliErr> {
    let v = control::send_request(
        state_dir,
        &Request::AdShow { hostname: hostname.to_string() },
    )
    .await
    .map_err(|e| CliErr::Other(e.to_string()))?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&v).map_err(|e| CliErr::Other(e.to_string()))?
        );
    } else if v.is_null() {
        eprintln!("classic: no ad found for hostname '{}'", hostname);
        return Err(CliErr::Spawn(format!("no ad for {hostname}")));
    } else {
        println!(
            "{}",
            serde_json::to_string_pretty(&v).map_err(|e| CliErr::Other(e.to_string()))?
        );
    }
    Ok(())
}

async fn spawn(state_dir: &std::path::Path, args: SpawnArgs) -> Result<(), CliErr> {
    validate_spawn_args(&args)?;
    // Daemon-side spawn handling lives in classic-spawn (originator +
    // executor state machines). The CLI ↔ daemon control-socket bridge
    // for spawn frames is a follow-up — see the partial-completion note
    // in the classic-vo5 commit. For now we emit a clear "not yet
    // wired" error so the CLI surface is in place and downstream
    // integration tests have something concrete to target.
    let _ = state_dir;
    Err(CliErr::Other(
        "spawn-via-control-socket not yet wired; arg parsing is in place \
         but the daemon-side bridge is a follow-up bead"
            .into(),
    ))
}

fn validate_spawn_args(args: &SpawnArgs) -> Result<(), CliErr> {
    if args.requires.trim().is_empty() {
        return Err(CliErr::Usage(
            "--requires must be a non-empty predicate (use \"true\" for any node)".into(),
        ));
    }
    if args.argv.is_empty() {
        return Err(CliErr::Usage(
            "argv after `--` must have at least one element".into(),
        ));
    }
    for kv in &args.env {
        if !kv.contains('=') {
            return Err(CliErr::Usage(format!(
                "--env entry {:?} must be of the form KEY=VAL",
                kv
            )));
        }
    }
    if let Some(s) = &args.stdin {
        if s != "-" && !std::path::Path::new(s).exists() {
            return Err(CliErr::Usage(format!(
                "--stdin file {:?} does not exist (use \"-\" to inherit stdin)",
                s
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_clap_definition_compiles() {
        // Smoke test: clap can build the parser for the full CLI surface.
        let _ = Args::command().debug_assert();
    }

    fn parse(argv: &[&str]) -> Result<Args, clap::Error> {
        Args::try_parse_from(argv)
    }

    #[test]
    fn spawn_parses_canonical_invocation() {
        let r = parse(&[
            "classic",
            "spawn",
            "--requires",
            "any(gpu, gpu.vram_mb >= 80000)",
            "--rank",
            "-load.cpu_pct",
            "--exclusive-device",
            "--env",
            "RANK=0",
            "--env",
            "WORLD_SIZE=1",
            "--",
            "python3",
            "train.py",
        ])
        .unwrap();
        match r.command {
            Command::Spawn(s) => {
                assert_eq!(s.requires, "any(gpu, gpu.vram_mb >= 80000)");
                assert_eq!(s.rank.as_deref(), Some("-load.cpu_pct"));
                assert!(s.exclusive_device);
                assert_eq!(s.env, vec!["RANK=0", "WORLD_SIZE=1"]);
                assert_eq!(s.argv, vec!["python3", "train.py"]);
            }
            other => panic!("expected Command::Spawn, got {other:?}"),
        }
    }

    #[test]
    fn spawn_missing_requires_is_clap_error() {
        let err = parse(&["classic", "spawn", "--", "echo"]).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("--requires"), "msg: {msg}");
    }

    #[test]
    fn spawn_missing_argv_is_clap_error() {
        let err = parse(&["classic", "spawn", "--requires", "true"]).unwrap_err();
        let msg = err.to_string();
        // clap surfaces this as "the following required arguments were not provided"
        // — pin the substring "argv".
        assert!(msg.to_lowercase().contains("argv"), "msg: {msg}");
    }

    #[test]
    fn validate_rejects_empty_requires() {
        let args = SpawnArgs {
            requires: "".into(),
            rank: None,
            exclusive_device: false,
            stdin: None,
            env: vec![],
            argv: vec!["echo".into()],
        };
        let err = validate_spawn_args(&args).unwrap_err();
        match err {
            CliErr::Usage(m) => assert!(m.contains("non-empty"), "msg: {m}"),
            other => panic!("expected Usage, got {other:?}"),
        }
    }

    #[test]
    fn validate_rejects_malformed_env() {
        let args = SpawnArgs {
            requires: "true".into(),
            rank: None,
            exclusive_device: false,
            stdin: None,
            env: vec!["NOEQUALS".into()],
            argv: vec!["echo".into()],
        };
        let err = validate_spawn_args(&args).unwrap_err();
        match err {
            CliErr::Usage(m) => assert!(m.contains("KEY=VAL"), "msg: {m}"),
            other => panic!("expected Usage, got {other:?}"),
        }
    }

    #[test]
    fn validate_accepts_dash_stdin() {
        let args = SpawnArgs {
            requires: "true".into(),
            rank: None,
            exclusive_device: false,
            stdin: Some("-".into()),
            env: vec![],
            argv: vec!["cat".into()],
        };
        validate_spawn_args(&args).unwrap();
    }

    #[test]
    fn validate_rejects_missing_stdin_file() {
        let args = SpawnArgs {
            requires: "true".into(),
            rank: None,
            exclusive_device: false,
            stdin: Some("/no/such/path-classicd-test".into()),
            env: vec![],
            argv: vec!["cat".into()],
        };
        let err = validate_spawn_args(&args).unwrap_err();
        match err {
            CliErr::Usage(m) => assert!(m.contains("does not exist"), "msg: {m}"),
            other => panic!("expected Usage, got {other:?}"),
        }
    }
}
