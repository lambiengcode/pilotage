//! CLI surface.
//!
//! Content-first: a bare `pilotage` prints live data, not help text. Every command
//! returns TOON, every command ends with a `next:` line, and no command ever waits
//! for input.

pub mod inspect;
pub mod manage;
pub mod research;

use crate::cache::Cache;
use crate::exit;
use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Parser, Debug)]
#[command(
    name = "pilotage",
    version,
    about = "Agent-ergonomic research: market, projects, technical due diligence",
    long_about = None,
    disable_help_subcommand = true
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Research a target and return a verdict with its signals
    Research(ResearchArgs),

    /// Research several targets and pivot their metrics side by side
    Compare(CompareArgs),

    /// Re-render a stored run
    Report(ReportArgs),

    /// Inspect what a run actually read
    Source {
        #[command(subcommand)]
        cmd: SourceCmd,
    },

    /// Track targets over time and diff them against their last run
    Watch {
        #[command(subcommand)]
        cmd: WatchCmd,
    },

    /// List, show and validate packs
    Pack {
        #[command(subcommand)]
        cmd: PackCmd,
    },

    /// Cache maintenance
    Cache {
        #[command(subcommand)]
        cmd: CacheCmd,
    },

    /// Install shell and agent integration
    Setup {
        #[command(subcommand)]
        cmd: SetupCmd,
    },

    /// Ambient context line for session hooks
    #[command(hide = true)]
    Ambient,
}

#[derive(clap::Args, Debug)]
pub struct ResearchArgs {
    /// URL, slug or owner/repo
    pub target: String,
    /// Force a pack instead of detecting one
    #[arg(long)]
    pub pack: Option<String>,
    /// quick (default) or deep
    #[arg(long, default_value = "quick")]
    pub depth: String,
    /// Hard USD ceiling for model extraction
    #[arg(long)]
    pub budget: Option<f64>,
    /// Columns for the signals table, e.g. metric,value,status,conf
    #[arg(long)]
    pub fields: Option<String>,
    /// Show every signal, including the healthy ones
    #[arg(long)]
    pub full: bool,
    /// Ignore cached responses and refetch
    #[arg(long)]
    pub no_cache: bool,
    /// Emit the full run as JSON instead of TOON
    #[arg(long)]
    pub json: bool,
    /// Also write an HTML report to this path
    #[arg(long)]
    pub html: Option<PathBuf>,
}

#[derive(clap::Args, Debug)]
pub struct CompareArgs {
    /// Two or more targets
    #[arg(required = true, num_args = 2..)]
    pub targets: Vec<String>,
    #[arg(long)]
    pub pack: Option<String>,
    #[arg(long, default_value = "quick")]
    pub depth: String,
    /// Ceiling shared across every target in the comparison
    #[arg(long)]
    pub budget: Option<f64>,
    #[arg(long)]
    pub json: bool,
    /// Write a self-contained side-by-side HTML report
    #[arg(long)]
    pub html: Option<PathBuf>,
    /// Heading for the report (default: the targets, joined)
    #[arg(long)]
    pub title: Option<String>,
}

#[derive(clap::Args, Debug)]
pub struct ReportArgs {
    /// Run id, or omit for the most recent run
    pub run: Option<String>,
    /// Write a self-contained HTML report
    #[arg(long)]
    pub html: bool,
    /// Where to write it (default: a temporary file)
    #[arg(long)]
    pub out: Option<PathBuf>,
    #[arg(long)]
    pub full: bool,
    #[arg(long)]
    pub fields: Option<String>,
    #[arg(long)]
    pub json: bool,
}

#[derive(Subcommand, Debug)]
pub enum SourceCmd {
    /// Sources a run read, with status, size and fact count
    List {
        #[arg(long)]
        run: Option<String>,
    },
    /// The raw cached body a source returned
    Get {
        id: String,
        #[arg(long)]
        run: Option<String>,
        /// Print the whole body instead of the first lines
        #[arg(long)]
        full: bool,
        #[arg(long, default_value = "40")]
        head: usize,
    },
}

#[derive(Subcommand, Debug)]
pub enum WatchCmd {
    Add {
        target: String,
        #[arg(long)]
        pack: Option<String>,
    },
    List,
    Rm {
        target: String,
        #[arg(long)]
        pack: Option<String>,
    },
    /// Re-run every watch and report what changed
    Check {
        #[arg(long, default_value = "quick")]
        depth: String,
        #[arg(long)]
        budget: Option<f64>,
    },
}

#[derive(Subcommand, Debug)]
pub enum PackCmd {
    List,
    Show {
        name: String,
    },
    /// Validate built-in and user packs, or one file
    Validate {
        file: Option<PathBuf>,
    },
}

#[derive(Subcommand, Debug)]
pub enum CacheCmd {
    Stats,
    /// Drop expired blobs and old runs. Extracted facts are kept.
    Prune {
        #[arg(long, default_value = "7d")]
        blobs: String,
        #[arg(long, default_value = "90d")]
        runs: String,
    },
}

#[derive(Subcommand, Debug)]
pub enum SetupCmd {
    /// Print (or install) the SessionStart hook that gives agents ambient context
    Hooks {
        /// Write it into ~/.claude/settings.json, keeping a .bak
        #[arg(long)]
        install: bool,
    },
}

pub async fn dispatch(cli: Cli) -> Result<i32> {
    let cache = Arc::new(Cache::open_default()?);
    match cli.command {
        None => dashboard(&cache),
        Some(Command::Ambient) => ambient(&cache),
        Some(Command::Research(a)) => research::research(cache, a).await,
        Some(Command::Compare(a)) => research::compare(cache, a).await,
        Some(Command::Report(a)) => inspect::report(&cache, a),
        Some(Command::Source { cmd }) => inspect::source(&cache, cmd),
        Some(Command::Watch { cmd }) => manage::watch(cache, cmd).await,
        Some(Command::Pack { cmd }) => manage::pack(cmd),
        Some(Command::Cache { cmd }) => manage::cache_cmd(&cache, cmd),
        Some(Command::Setup { cmd }) => manage::setup(cmd),
    }
}

/// Bare `pilotage`: what has been researched lately, what is being watched, and how
/// big the cache has grown. Live data, never help text.
fn dashboard(cache: &Cache) -> Result<i32> {
    let mut t = crate::toon::Toon::new();
    let stats = cache.stats()?;
    let runs = cache.recent_runs(8)?;
    let watches = cache.watch_list()?;
    let now = crate::util::now();

    t.kv(
        "pilotage",
        format!(
            "{} run(s), {} watch(es), {} cached",
            stats.runs,
            stats.watches,
            bytes(stats.db_bytes.max(stats.blob_bytes))
        ),
    );

    let rows: Vec<Vec<String>> = runs
        .iter()
        .map(|r| {
            vec![
                r.id.clone(),
                r.target.clone(),
                r.pack.clone(),
                r.verdict.clone(),
                crate::util::age(now - r.created_at),
            ]
        })
        .collect();
    t.table(
        "recent",
        &["run", "target", "pack", "verdict", "age"],
        &rows,
    );

    let wrows: Vec<Vec<String>> = watches
        .iter()
        .map(|w| {
            let last = w
                .last_run
                .as_deref()
                .and_then(|id| cache.get_run(id).ok().flatten());
            vec![
                w.target.clone(),
                w.pack.clone(),
                last.as_ref()
                    .map(|r| r.verdict.clone())
                    .unwrap_or_else(|| "never-run".into()),
                last.as_ref()
                    .map(|r| crate::util::age(now - r.created_at))
                    .unwrap_or_else(|| "-".into()),
            ]
        })
        .collect();
    t.table("watch", &["target", "pack", "verdict", "age"], &wrows);

    t.kv(
        "cache",
        format!(
            "{} blob(s) {}, {} extraction(s), {} run(s)",
            stats.blobs,
            bytes(stats.blob_bytes),
            stats.facts,
            stats.runs
        ),
    );

    let mut next = vec!["pilotage research <target>".to_string()];
    if !watches.is_empty() {
        next.push("pilotage watch check".into());
    }
    if let Some(r) = runs.first() {
        next.push(format!("pilotage report {} --html", r.id));
    }
    next.push("pilotage pack list".into());
    t.next(&next);
    t.print();
    Ok(exit::OK)
}

/// One compact block for a SessionStart hook: enough for an agent to know what is
/// being tracked without asking.
fn ambient(cache: &Cache) -> Result<i32> {
    let mut t = crate::toon::Toon::new();
    let stats = cache.stats()?;
    let now = crate::util::now();

    t.kv(
        "bin",
        std::env::current_exe()
            .map(|p| p.display().to_string())
            .unwrap_or_default(),
    );
    t.kv(
        "description",
        "Agent-ergonomic research over projects, markets and repositories. Returns TOON, not pages.",
    );

    let runs = cache.recent_runs(3)?;
    let rows: Vec<Vec<String>> = runs
        .iter()
        .map(|r| {
            vec![
                r.target.clone(),
                r.verdict.clone(),
                crate::util::age(now - r.created_at),
            ]
        })
        .collect();
    t.table("recent", &["target", "verdict", "age"], &rows);
    t.kv("watches", stats.watches);
    t.list(
        "help",
        &[
            "Run `pilotage` to see recent runs and watches",
            "Run `pilotage research <url|owner/repo|slug>` for a verdict with signals",
            "Run `pilotage --help` for the full command list",
        ],
    );
    t.print();
    Ok(exit::OK)
}

// ---------------------------------------------------------------------------
// Shared rendering
// ---------------------------------------------------------------------------

pub const DEFAULT_SIGNAL_FIELDS: &[&str] = &["metric", "value", "status", "w"];

/// Parses `--fields a,b,c`, rejecting unknown columns rather than silently dropping
/// them — a typo that quietly returns fewer columns is worse than an error.
pub fn parse_fields(spec: Option<&str>) -> Result<Vec<String>> {
    const KNOWN: &[&str] = &[
        "metric",
        "label",
        "value",
        "status",
        "w",
        "weight",
        "conf",
        "confidence",
        "source",
        "url",
    ];
    match spec {
        None => Ok(DEFAULT_SIGNAL_FIELDS
            .iter()
            .map(|s| s.to_string())
            .collect()),
        Some(s) => {
            let fields: Vec<String> = s
                .split(',')
                .map(|f| f.trim().to_string())
                .filter(|f| !f.is_empty())
                .collect();
            if fields.is_empty() {
                anyhow::bail!("--fields was empty (known columns: {})", KNOWN.join(", "));
            }
            if let Some(bad) = fields.iter().find(|f| !KNOWN.contains(&f.as_str())) {
                anyhow::bail!(
                    "unknown field '{bad}' (known columns: {})",
                    KNOWN.join(", ")
                );
            }
            Ok(fields)
        }
    }
}

pub fn signal_cell(sig: &crate::score::Signal, field: &str) -> String {
    match field {
        "metric" => sig.id.clone(),
        "label" => sig.label.clone(),
        "value" => sig.display.clone(),
        "status" => sig.status.as_str().to_string(),
        "w" | "weight" => trim_num(sig.weight),
        "conf" | "confidence" => format!("{:.0}%", sig.confidence * 100.0),
        "source" => sig.source.clone().unwrap_or_else(|| "-".into()),
        "url" => sig.source_url.clone().unwrap_or_else(|| "-".into()),
        _ => String::new(),
    }
}

pub fn trim_num(n: f64) -> String {
    if n.fract().abs() < 1e-9 {
        format!("{}", n as i64)
    } else {
        format!("{n:.1}")
    }
}

pub fn bytes(n: i64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n}B")
    } else {
        format!("{v:.1}{}", UNITS[i])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn bare_invocation_has_no_subcommand_so_it_shows_content() {
        let cli = Cli::try_parse_from(["pilotage"]).unwrap();
        assert!(cli.command.is_none());
    }

    #[test]
    fn research_parses_its_flags() {
        let cli = Cli::try_parse_from([
            "pilotage",
            "research",
            "github.com/a/b",
            "--pack",
            "oss",
            "--depth",
            "deep",
            "--budget",
            "0.05",
            "--full",
        ])
        .unwrap();
        match cli.command {
            Some(Command::Research(a)) => {
                assert_eq!(a.target, "github.com/a/b");
                assert_eq!(a.pack.as_deref(), Some("oss"));
                assert_eq!(a.depth, "deep");
                assert_eq!(a.budget, Some(0.05));
                assert!(a.full);
            }
            other => panic!("expected research, got {other:?}"),
        }
    }

    #[test]
    fn compare_demands_at_least_two_targets() {
        assert!(Cli::try_parse_from(["pilotage", "compare", "a"]).is_err());
        assert!(Cli::try_parse_from(["pilotage", "compare", "a", "b"]).is_ok());
    }

    #[test]
    fn default_fields_are_the_compact_set() {
        assert_eq!(parse_fields(None).unwrap(), DEFAULT_SIGNAL_FIELDS);
    }

    #[test]
    fn field_typos_are_rejected_rather_than_silently_dropped() {
        assert!(parse_fields(Some("metric,valu")).is_err());
        assert!(parse_fields(Some("")).is_err());
        assert_eq!(
            parse_fields(Some("metric, conf ")).unwrap(),
            ["metric", "conf"]
        );
    }

    #[test]
    fn byte_sizes_stay_short() {
        assert_eq!(bytes(512), "512B");
        assert_eq!(bytes(2048), "2.0KB");
        assert_eq!(bytes(5_242_880), "5.0MB");
    }

    #[test]
    fn weights_drop_pointless_decimals() {
        assert_eq!(trim_num(3.0), "3");
        assert_eq!(trim_num(2.5), "2.5");
    }
}
