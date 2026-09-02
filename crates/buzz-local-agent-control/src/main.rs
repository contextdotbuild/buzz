use std::{path::PathBuf, process::ExitCode};

use buzz_local_agent_control::{execute, CliOptions, ErrorReceipt};
use clap::{error::ErrorKind, Parser};

#[derive(Debug, Parser)]
#[command(
    name = "buzz-local-agent-control",
    about = "Patch existing managed Buzz agents while Buzz Desktop and buzz-acp are stopped"
)]
struct Cli {
    /// JSON request conforming to a supported bounded control contract.
    #[arg(long)]
    request: PathBuf,

    /// Exact absolute path to the existing managed-agents.json store.
    #[arg(long)]
    store: PathBuf,

    /// Validate and report the candidate without mutating the store.
    #[arg(long)]
    dry_run: bool,
}

fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
            ) =>
        {
            let _ = error.print();
            return ExitCode::SUCCESS;
        }
        Err(_) => return emit_error(ErrorReceipt::invalid_cli()),
    };
    match execute(CliOptions {
        request_path: cli.request,
        store_path: cli.store,
        dry_run: cli.dry_run,
    }) {
        Ok(receipt) => match serde_json::to_string(&receipt) {
            Ok(json) => {
                println!("{json}");
                ExitCode::SUCCESS
            }
            Err(_) => emit_error(ErrorReceipt::internal_serialization()),
        },
        Err(error) => emit_error(error.receipt()),
    }
}

fn emit_error(error: ErrorReceipt) -> ExitCode {
    match serde_json::to_string(&error) {
        Ok(json) => eprintln!("{json}"),
        Err(_) => eprintln!(
            "{{\"schemaVersion\":1,\"status\":\"error\",\"code\":\"internal_serialization\",\"message\":\"failed to serialize structured error\"}}"
        ),
    }
    ExitCode::FAILURE
}
