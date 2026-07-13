use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Utc};
use clap::Parser;
use serde::Deserialize;

const USER_AGENT: &str = concat!("cargo-aged/", env!("CARGO_PKG_VERSION"));
const CRATES_IO_API: &str = "https://crates.io/api/v1/crates";

#[derive(Parser, Debug)]
#[command(
    name = "cargo-aged",
    bin_name = "cargo aged",
    about = "Update Cargo dependencies only when their latest stable release has aged past a threshold.",
    version
)]
struct Cli {
    #[arg(long, default_value_t = 30, help = "Minimum release age in days")]
    min_age: i64,

    #[arg(
        long,
        default_value = "./Cargo.toml",
        help = "Path to Cargo.toml"
    )]
    manifest_path: PathBuf,

    #[arg(long, help = "Print what would be updated without making changes")]
    dry_run: bool,

    #[arg(long, help = "Show publish dates and age for all crates checked")]
    verbose: bool,
}

fn main() {
    let raw: Vec<String> = std::env::args().collect();
    let filtered: Vec<String> = if raw.len() > 1 && raw[1] == "aged" {
        let mut v = Vec::with_capacity(raw.len() - 1);
        v.push(raw[0].clone());
        v.extend(raw.iter().skip(2).cloned());
        v
    } else {
        raw
    };

    let cli = Cli::parse_from(filtered);

    if let Err(err) = run(cli) {
        eprintln!("error: {err:#}");
        std::process::exit(1);
    }
}

#[derive(Debug, Clone)]
enum DepKind {
    Registry { req: String, pinned: bool },
    Path,
    Git,
}

#[derive(Debug, Clone)]
struct Dependency {
    name: String,
    kind: DepKind,
}

fn run(cli: Cli) -> Result<()> {
    let deps = parse_manifest(&cli.manifest_path)
        .with_context(|| format!("failed to parse manifest at {}", cli.manifest_path.display()))?;

    println!(
        "Checking {} dependencies (min-age: {} days)...",
        deps.len(),
        cli.min_age
    );

    let client = reqwest::blocking::Client::builder()
        .user_agent(USER_AGENT)
        .build()
        .context("failed to build HTTP client")?;

    let mut updated = 0usize;
    let mut skipped = 0usize;
    let now = Utc::now();

    for dep in &deps {
        match &dep.kind {
            DepKind::Path => {
                println!("  ✗ {:<24} (path dep)    — skipping", dep.name);
                skipped += 1;
                continue;
            }
            DepKind::Git => {
                println!("  ✗ {:<24} (git dep)     — skipping", dep.name);
                skipped += 1;
                continue;
            }
            DepKind::Registry { req, pinned } => {
                if *pinned {
                    println!(
                        "  ✗ {:<24} (= {}) pinned  — skipping",
                        dep.name, req
                    );
                    skipped += 1;
                    continue;
                }

                match fetch_latest_stable(&client, &dep.name) {
                    Ok(Some(latest)) => {
                        let age_days = (now - latest.created_at).num_days();
                        if age_days >= cli.min_age {
                            let label = format!("{} {}", dep.name, latest.num);
                            if cli.dry_run {
                                println!(
                                    "  ✓ {:<24} — {} days old, would update (dry-run)",
                                    label, age_days
                                );
                                updated += 1;
                            } else {
                                println!(
                                    "  ✓ {:<24} — {} days old, updating...",
                                    label, age_days
                                );
                                match run_cargo_update(&dep.name, &latest.num, &cli.manifest_path) {
                                    Ok(()) => updated += 1,
                                    Err(e) => {
                                        eprintln!(
                                            "    ! cargo update failed for {}: {}",
                                            dep.name, e
                                        );
                                        skipped += 1;
                                    }
                                }
                            }
                        } else {
                            let label = format!("{} {}", dep.name, latest.num);
                            println!(
                                "  ✗ {:<24} — {} days old, skipping",
                                label, age_days
                            );
                            skipped += 1;
                        }

                        if cli.verbose {
                            println!(
                                "      (published {})",
                                latest.created_at.to_rfc3339()
                            );
                        }
                    }
                    Ok(None) => {
                        eprintln!(
                            "  ! {:<24} no stable release found — skipping",
                            dep.name
                        );
                        skipped += 1;
                    }
                    Err(e) => {
                        eprintln!(
                            "  ! {:<24} crates.io lookup failed ({}) — skipping",
                            dep.name, e
                        );
                        skipped += 1;
                    }
                }
            }
        }
    }

    println!("Summary: {} updated, {} skipped.", updated, skipped);
    Ok(())
}

fn parse_manifest(path: &PathBuf) -> Result<Vec<Dependency>> {
    let contents = fs::read_to_string(path)
        .with_context(|| format!("could not read {}", path.display()))?;
    let value: toml::Value = toml::from_str(&contents).context("invalid TOML in manifest")?;

    let mut out: BTreeMap<String, Dependency> = BTreeMap::new();

    let sections = [
        "dependencies",
        "dev-dependencies",
        "build-dependencies",
    ];

    for section in sections {
        if let Some(table) = value.get(section).and_then(|v| v.as_table()) {
            collect_deps(table, &mut out);
        }
    }

    // Target-specific dependencies: [target."cfg(...)".dependencies]
    if let Some(targets) = value.get("target").and_then(|v| v.as_table()) {
        for (_target, tv) in targets {
            if let Some(t) = tv.as_table() {
                for section in sections {
                    if let Some(table) = t.get(section).and_then(|v| v.as_table()) {
                        collect_deps(table, &mut out);
                    }
                }
            }
        }
    }

    // Workspace dependencies: [workspace.dependencies]
    if let Some(ws) = value.get("workspace").and_then(|v| v.as_table()) {
        if let Some(table) = ws.get("dependencies").and_then(|v| v.as_table()) {
            collect_deps(table, &mut out);
        }
    }

    Ok(out.into_values().collect())
}

fn collect_deps(table: &toml::value::Table, out: &mut BTreeMap<String, Dependency>) {
    for (name, val) in table {
        let dep = classify(name, val);
        // First-seen wins so we don't downgrade a resolved registry dep by a later duplicate,
        // but we also don't want to lose a path/git signal — merge conservatively.
        out.entry(dep.name.clone())
            .and_modify(|existing| {
                match (&existing.kind, &dep.kind) {
                    (DepKind::Registry { .. }, DepKind::Path)
                    | (DepKind::Registry { .. }, DepKind::Git) => {
                        existing.kind = dep.kind.clone();
                    }
                    _ => {}
                }
            })
            .or_insert(dep);
    }
}

fn classify(name: &str, val: &toml::Value) -> Dependency {
    match val {
        toml::Value::String(s) => Dependency {
            name: name.to_string(),
            kind: DepKind::Registry {
                req: s.clone(),
                pinned: s.trim_start().starts_with('='),
            },
        },
        toml::Value::Table(t) => {
            let renamed = t
                .get("package")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| name.to_string());

            if t.get("path").is_some() {
                return Dependency {
                    name: renamed,
                    kind: DepKind::Path,
                };
            }
            if t.get("git").is_some() {
                return Dependency {
                    name: renamed,
                    kind: DepKind::Git,
                };
            }

            let req = t
                .get("version")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let pinned = req.trim_start().starts_with('=');

            Dependency {
                name: renamed,
                kind: DepKind::Registry { req, pinned },
            }
        }
        _ => Dependency {
            name: name.to_string(),
            kind: DepKind::Registry {
                req: String::new(),
                pinned: false,
            },
        },
    }
}

#[derive(Debug, Deserialize)]
struct CratesResponse {
    versions: Vec<VersionInfo>,
}

#[derive(Debug, Deserialize)]
struct VersionInfo {
    num: String,
    created_at: DateTime<Utc>,
    #[serde(default)]
    yanked: bool,
}

#[derive(Debug, Clone)]
struct LatestStable {
    num: String,
    created_at: DateTime<Utc>,
}

fn fetch_latest_stable(
    client: &reqwest::blocking::Client,
    name: &str,
) -> Result<Option<LatestStable>> {
    let url = format!("{}/{}", CRATES_IO_API, name);
    let resp = client
        .get(&url)
        .send()
        .with_context(|| format!("request to {} failed", url))?;

    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Err(anyhow!("crate not found on crates.io"));
    }
    if !resp.status().is_success() {
        return Err(anyhow!("HTTP {}", resp.status()));
    }

    let body: CratesResponse = resp.json().context("invalid JSON from crates.io")?;

    let latest = body
        .versions
        .into_iter()
        .filter(|v| !v.yanked && is_stable(&v.num))
        .max_by_key(|v| v.created_at)
        .map(|v| LatestStable {
            num: v.num,
            created_at: v.created_at,
        });

    Ok(latest)
}

fn is_stable(version: &str) -> bool {
    // A SemVer version is a pre-release if the part after `-` (before any `+` build metadata)
    // is non-empty.
    let core = version.split('+').next().unwrap_or(version);
    match core.split_once('-') {
        Some((_, pre)) => pre.is_empty(),
        None => true,
    }
}

fn run_cargo_update(name: &str, version: &str, manifest_path: &PathBuf) -> Result<()> {
    let status = Command::new("cargo")
        .arg("update")
        .arg("-p")
        .arg(name)
        .arg("--precise")
        .arg(version)
        .arg("--manifest-path")
        .arg(manifest_path)
        .status()
        .context("failed to spawn `cargo update`")?;

    if !status.success() {
        return Err(anyhow!(
            "cargo update exited with status {}",
            status.code().unwrap_or(-1)
        ));
    }
    Ok(())
}
