//! npd — a persistent fact store for iterating on nixpkgs changes.
//!
//! See DESIGN.md for the architecture. The pure data model lives in [`model`];
//! orchestration (eval / diff / build / hydra / report) is being built
//! spine-first, and unimplemented subcommands fail loudly rather than pretending.

// Scaffolding: some model types are defined ahead of the orchestration that will
// consume them (see DESIGN.md build order). Drop this once build/report land.
#![allow(dead_code)]

mod eval;
mod model;

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

use crate::model::Existence;

#[derive(Parser)]
#[command(name = "npd", version, about = "A persistent fact store for iterating on nixpkgs changes")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Evaluate a revision into an attr->drv map (cached; pure).
    Eval {
        /// Git commit / revision to evaluate.
        commit: String,
        /// Attribute paths to scope to (dotted, e.g. `python3Packages.numpy`).
        /// Omit to evaluate the whole package set (that result is cached).
        attrs: Vec<String>,
        /// nixpkgs repo to resolve the commit in (default: `$NPD_NIXPKGS`).
        #[arg(long)]
        nixpkgs: Option<PathBuf>,
        /// Systems to evaluate for (repeatable); defaults to the host system.
        #[arg(long)]
        system: Vec<String>,
        /// Eval profile (config npd owns); defaults to `default`.
        #[arg(long)]
        profile: Option<String>,
    },
    /// Diff two revisions into a set of changed attrs (optionally three-way via merge base).
    Diff {
        base: String,
        head: String,
        /// Also evaluate the merge base to attribute each change (git-3-way style).
        #[arg(long)]
        three_way: bool,
    },
    /// Build derivations, consulting (and appending to) the observation log.
    Build {
        attrs: Vec<String>,
        #[arg(long)]
        recheck: bool,
        #[arg(long)]
        retry: bool,
        #[arg(long)]
        prefer_local: bool,
    },
    /// Fetch facts from Hydra on demand and record them as observations.
    Hydra { attrs: Vec<String> },
    /// Render a Markdown report from stored facts.
    Report,
}

/// The host Nix system double, e.g. `aarch64-linux`.
fn host_system() -> String {
    let arch = std::env::consts::ARCH; // e.g. "aarch64", "x86_64"
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        other => other, // "linux"
    };
    format!("{arch}-{os}")
}

fn cmd_eval(
    commit: String,
    attrs: Vec<String>,
    nixpkgs: Option<PathBuf>,
    system: Vec<String>,
    profile: Option<String>,
) -> Result<()> {
    let repo = nixpkgs
        .or_else(|| std::env::var_os("NPD_NIXPKGS").map(PathBuf::from))
        .context("no nixpkgs repo: pass --nixpkgs <path> or set $NPD_NIXPKGS")?;
    let systems = if system.is_empty() {
        vec![host_system()]
    } else {
        system
    };
    let profile = profile.unwrap_or_else(|| eval::DEFAULT_PROFILE.to_string());

    for e in eval::eval_commit(&repo, &commit, &systems, &profile, &attrs)? {
        let (mut buildable, mut blocked, mut errored) = (0, 0, 0);
        for a in &e.attrs {
            match a.existence {
                Existence::Buildable => buildable += 1,
                Existence::Blocked => blocked += 1,
                Existence::Error => errored += 1,
                Existence::Absent => {}
            }
        }
        let origin = if e.from_cache { "cached" } else { "fresh" };
        println!(
            "{}: {} attrs (buildable={buildable} blocked={blocked} error={errored}) [{origin}]",
            e.system,
            e.attrs.len()
        );
        // For a scoped eval, show each attr's verdict — that's the whole point.
        if !attrs.is_empty() {
            for a in &e.attrs {
                match &a.drv_path {
                    Some(d) => println!("  {:?}  {}  {d}", a.existence, a.attr),
                    None => println!("  {:?}  {}", a.existence, a.attr),
                }
            }
        }
    }
    Ok(())
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Eval {
            commit,
            attrs,
            nixpkgs,
            system,
            profile,
        } => cmd_eval(commit, attrs, nixpkgs, system, profile),
        Command::Diff { .. } => bail!("npd diff: not implemented yet (see DESIGN.md build order)"),
        Command::Build { .. } => bail!("npd build: not implemented yet (see DESIGN.md build order)"),
        Command::Hydra { .. } => bail!("npd hydra: not implemented yet (see DESIGN.md build order)"),
        Command::Report => bail!("npd report: not implemented yet (see DESIGN.md build order)"),
    }
}
