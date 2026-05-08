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
        Err(e) => {
            eprintln!("classic: {e}");
            ExitCode::from(1)
        }
    }
}

async fn run(args: Args) -> Result<(), Box<dyn std::error::Error>> {
    match args.command {
        Command::Ad { cmd } => match cmd {
            AdCommand::List { json } => ad_list(&args.state_dir, json).await,
            AdCommand::Show { hostname, json } => ad_show(&args.state_dir, &hostname, json).await,
        },
    }
}

async fn ad_list(state_dir: &std::path::Path, json: bool) -> Result<(), Box<dyn std::error::Error>> {
    let v = control::send_request(state_dir, &Request::AdList).await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&v)?);
    } else {
        if let Some(arr) = v.get("ads").and_then(|x| x.as_array()) {
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
    }
    Ok(())
}

async fn ad_show(
    state_dir: &std::path::Path,
    hostname: &str,
    json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let v = control::send_request(
        state_dir,
        &Request::AdShow { hostname: hostname.to_string() },
    )
    .await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&v)?);
    } else if v.is_null() {
        eprintln!("classic: no ad found for hostname '{}'", hostname);
        return Err("not found".into());
    } else {
        println!("{}", serde_json::to_string_pretty(&v)?);
    }
    Ok(())
}
