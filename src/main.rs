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
const MAX_UPDATE_ATTEMPTS: usize = 5;
const MAX_ITERATE_PASSES: usize = 10;

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

    #[arg(
        long,
        help = "Repeat passes until no more updates apply (fixed point). Useful when tightly-coupled dep families (e.g. serde + serde_json) can only be downgraded in stages."
    )]
    iterate: bool,
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

    let client = reqwest::blocking::Client::builder()
        .user_agent(USER_AGENT)
        .build()
        .context("failed to build HTTP client")?;

    if cli.iterate && cli.dry_run {
        eprintln!("note: --iterate has no effect with --dry-run; running a single pass.");
    }

    let iterate = cli.iterate && !cli.dry_run;

    if !iterate {
        let (updated, skipped) = run_pass(&client, &cli, &deps)?;
        println!("Summary: {} updated, {} skipped.", updated, skipped);
        return Ok(());
    }

    let mut total_updated = 0usize;
    for pass in 1..=MAX_ITERATE_PASSES {
        println!("=== Pass {} ===", pass);
        let (updated, skipped) = run_pass(&client, &cli, &deps)?;
        println!(
            "Pass {} summary: {} updated, {} skipped.",
            pass, updated, skipped
        );
        total_updated += updated;

        if updated == 0 {
            println!(
                "Converged after {} pass(es). Total updates applied: {}.",
                pass, total_updated
            );
            return Ok(());
        }
    }

    println!(
        "Stopped after {} passes (cap reached). Total updates applied: {}.",
        MAX_ITERATE_PASSES, total_updated
    );
    Ok(())
}

fn run_pass(
    client: &reqwest::blocking::Client,
    cli: &Cli,
    deps: &[Dependency],
) -> Result<(usize, usize)> {
    println!(
        "Checking {} dependencies (min-age: {} days)...",
        deps.len(),
        cli.min_age
    );

    let mut updated = 0usize;
    let mut skipped = 0usize;
    let now = Utc::now();

    for dep in deps {
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

                match fetch_stable_versions(&client, &dep.name) {
                    Ok(versions) if versions.is_empty() => {
                        eprintln!(
                            "  ! {:<24} no stable release found — skipping",
                            dep.name
                        );
                        skipped += 1;
                    }
                    Ok(versions) => {
                        let newest = &versions[0];
                        let newest_age = (now - newest.created_at).num_days();

                        if newest_age < cli.min_age {
                            let label = format!("{} {}", dep.name, newest.num);
                            println!(
                                "  ✗ {:<24} — {} days old, skipping",
                                label, newest_age
                            );
                            if cli.verbose {
                                println!(
                                    "      (published {})",
                                    newest.created_at.to_rfc3339()
                                );
                            }
                            skipped += 1;
                            continue;
                        }

                        let eligible: Vec<&StableVersion> = versions
                            .iter()
                            .filter(|v| (now - v.created_at).num_days() >= cli.min_age)
                            .collect();

                        let locked = locked_versions(&cli.manifest_path, &dep.name);
                        if let Some(current) =
                            eligible.iter().find(|v| locked.iter().any(|l| l == &v.num))
                        {
                            let age = (now - current.created_at).num_days();
                            let label = format!("{} {}", dep.name, current.num);
                            println!(
                                "  = {:<24} — {} days old, already age-eligible",
                                label, age
                            );
                            if cli.verbose {
                                println!(
                                    "      (published {})",
                                    current.created_at.to_rfc3339()
                                );
                            }
                            skipped += 1;
                            continue;
                        }

                        let attempts = eligible.len().min(MAX_UPDATE_ATTEMPTS);
                        let candidates = &eligible[..attempts];

                        let top = candidates[0];
                        let top_age = (now - top.created_at).num_days();
                        let top_label = format!("{} {}", dep.name, top.num);

                        if cli.dry_run {
                            println!(
                                "  ✓ {:<24} — {} days old, would update (dry-run)",
                                top_label, top_age
                            );
                            if cli.verbose {
                                println!(
                                    "      (published {})",
                                    top.created_at.to_rfc3339()
                                );
                            }
                            updated += 1;
                            continue;
                        }

                        println!(
                            "  ✓ {:<24} — {} days old, updating...",
                            top_label, top_age
                        );
                        if cli.verbose {
                            println!("      (published {})", top.created_at.to_rfc3339());
                        }

                        let mut pinned: Option<(&StableVersion, i64)> = None;
                        let mut last_err: Option<String> = None;

                        for (idx, candidate) in candidates.iter().enumerate() {
                            let age = (now - candidate.created_at).num_days();

                            if idx > 0 {
                                println!(
                                    "    → retrying with {} {} ({} days old)...",
                                    dep.name, candidate.num, age
                                );
                            }

                            match run_cargo_update(
                                &dep.name,
                                &candidate.num,
                                &cli.manifest_path,
                            ) {
                                Ok(()) => {
                                    pinned = Some((*candidate, age));
                                    break;
                                }
                                Err(e) => {
                                    last_err = Some(format!("{}", e));
                                    if cli.verbose {
                                        eprintln!(
                                            "      cargo update failed for {} {}: {}",
                                            dep.name, candidate.num, e
                                        );
                                    }
                                }
                            }
                        }

                        match pinned {
                            Some((v, age)) => {
                                if !std::ptr::eq(v, top) {
                                    println!(
                                        "    ✓ pinned {} {} ({} days old)",
                                        dep.name, v.num, age
                                    );
                                }
                                updated += 1;
                            }
                            None => {
                                let hint = if eligible.len() > attempts {
                                    format!(
                                        " (tried {} of {} eligible versions; increase MAX_UPDATE_ATTEMPTS to try more)",
                                        attempts,
                                        eligible.len()
                                    )
                                } else {
                                    format!(" (tried all {} eligible versions)", attempts)
                                };
                                eprintln!(
                                    "    ! no compatible age-eligible version for {}{}",
                                    dep.name, hint
                                );
                                if let Some(e) = last_err {
                                    eprintln!("      last error: {}", e);
                                }
                                skipped += 1;
                            }
                        }
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

    Ok((updated, skipped))
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
struct StableVersion {
    num: String,
    created_at: DateTime<Utc>,
}

fn fetch_stable_versions(
    client: &reqwest::blocking::Client,
    name: &str,
) -> Result<Vec<StableVersion>> {
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

    let mut versions: Vec<StableVersion> = body
        .versions
        .into_iter()
        .filter(|v| !v.yanked && is_stable(&v.num))
        .map(|v| StableVersion {
            num: v.num,
            created_at: v.created_at,
        })
        .collect();

    versions.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    Ok(versions)
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

fn locked_versions(manifest_path: &PathBuf, name: &str) -> Vec<String> {
    let lock_path = manifest_path
        .parent()
        .map(|p| p.join("Cargo.lock"))
        .unwrap_or_else(|| PathBuf::from("Cargo.lock"));

    let Ok(contents) = fs::read_to_string(&lock_path) else {
        return Vec::new();
    };
    let Ok(value) = toml::from_str::<toml::Value>(&contents) else {
        return Vec::new();
    };
    let Some(packages) = value.get("package").and_then(|v| v.as_array()) else {
        return Vec::new();
    };

    packages
        .iter()
        .filter_map(|p| {
            let table = p.as_table()?;
            let pkg_name = table.get("name")?.as_str()?;
            if pkg_name != name {
                return None;
            }
            table.get("version")?.as_str().map(str::to_string)
        })
        .collect()
}

fn run_cargo_update(name: &str, version: &str, manifest_path: &PathBuf) -> Result<()> {
    let output = Command::new("cargo")
        .arg("update")
        .arg("-p")
        .arg(name)
        .arg("--precise")
        .arg(version)
        .arg("--manifest-path")
        .arg(manifest_path)
        .output()
        .context("failed to spawn `cargo update`")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let msg = stderr.trim();
        let summary = msg.lines().next().unwrap_or("").trim();
        return Err(anyhow!(
            "cargo update exited with status {}: {}",
            output.status.code().unwrap_or(-1),
            if summary.is_empty() { "no stderr output" } else { summary }
        ));
    }
    Ok(())
}
