//! Entry point.
//!
//! Errors go to stderr in a structured, single-line form so an agent can read the
//! reason without parsing a backtrace, and the exit code carries the category.

use clap::Parser;
use pilotage::cmd::{Cli, dispatch};
use pilotage::exit;

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    match dispatch(cli).await {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("error: {}", first_line(&e.to_string()));
            // Context frames, outermost first, so the cause chain is readable
            // without being a wall of text.
            for cause in e.chain().skip(1) {
                eprintln!("cause: {}", first_line(&cause.to_string()));
            }
            std::process::exit(classify(&e));
        }
    }
}

/// Maps a failure onto the documented exit codes. Anything unrecognised is a usage
/// error, which is the safest default: it tells the caller to change the invocation
/// rather than to retry.
fn classify(e: &anyhow::Error) -> i32 {
    let text = e
        .chain()
        .map(|c| c.to_string())
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    if text.contains("budget ceiling") {
        exit::BUDGET
    } else if text.contains("is not set") || text.contains("api_key") {
        exit::CONFIG
    } else if text.contains("replay miss") {
        exit::NO_DATA
    } else {
        exit::USAGE
    }
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or(s).trim().to_string()
}
