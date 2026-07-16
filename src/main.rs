use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
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
    #[arg(
        long,
        value_name = "DAYS",
        help = "Minimum release age in days. Overrides [registry].min-publish-age from .cargo/config.toml. Required if no config file provides one."
    )]
    min_age: Option<i64>,

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

    #[arg(
        long,
        conflicts_with_all = ["dry_run", "iterate"],
        help = "Read-only CI gate: exit 1 if Cargo.lock contains any direct dep locked to a version younger than min-age (and an age-eligible replacement exists). Does not modify Cargo.lock. Mutually exclusive with --dry-run and --iterate."
    )]
    check: bool,
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

    match run(cli) {
        Ok(code) => std::process::exit(code),
        Err(err) => {
            eprintln!("error: {err:#}");
            std::process::exit(1);
        }
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

fn run(cli: Cli) -> Result<i32> {
    let deps = parse_manifest(&cli.manifest_path)
        .with_context(|| format!("failed to parse manifest at {}", cli.manifest_path.display()))?;

    let Some((min_age, source_note)) = resolve_min_age(&cli)? else {
        return Err(anyhow!(
            "no minimum release age configured. Set --min-age <DAYS> on the command line, \
             or add `[registry] min-publish-age = \"14 days\"` to .cargo/config.toml."
        ));
    };

    let client = reqwest::blocking::Client::builder()
        .user_agent(USER_AGENT)
        .build()
        .context("failed to build HTTP client")?;

    if cli.iterate && cli.dry_run {
        eprintln!("note: --iterate has no effect with --dry-run; running a single pass.");
    }

    if let Some(note) = &source_note {
        println!("{}", note);
    }

    if cli.check {
        return Ok(run_check(&client, &cli, min_age, &deps, CRATES_IO_API));
    }

    let iterate = cli.iterate && !cli.dry_run;

    if !iterate {
        let (updated, skipped) = run_pass(&client, &cli, min_age, &deps)?;
        println!("Summary: {} updated, {} skipped.", updated, skipped);
        return Ok(0);
    }

    let mut total_updated = 0usize;
    for pass in 1..=MAX_ITERATE_PASSES {
        println!("=== Pass {} ===", pass);
        let (updated, skipped) = run_pass(&client, &cli, min_age, &deps)?;
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
            return Ok(0);
        }
    }

    println!(
        "Stopped after {} passes (cap reached). Total updates applied: {}.",
        MAX_ITERATE_PASSES, total_updated
    );
    Ok(0)
}

/// Classification of a single dep for --check purposes.
/// The decision is: given crates.io versions and Cargo.lock state, would the
/// tool consider this dep to need pinning back to an older, age-eligible release?
#[derive(Debug, Clone)]
enum DepStatus {
    /// Locked to a version that isn't age-eligible, but an age-eligible
    /// version does exist on crates.io. This is the CI-fail signal.
    Fresh {
        locked_version: String,
        locked_age: Option<i64>,
        locked_published: Option<DateTime<Utc>>,
        eligible_top: String,
        eligible_top_age: i64,
        eligible_top_published: DateTime<Utc>,
    },
    /// Already locked to an age-eligible version.
    Ok {
        version: String,
        age: i64,
        published: DateTime<Utc>,
    },
    Skipped(SkipReason),
}

#[derive(Debug, Clone)]
enum SkipReason {
    Path,
    Git,
    Pinned(String),
    NoStable,
    NoEligible { newest_age: i64 },
    LookupFailed(String),
}

fn classify_dep(
    client: &reqwest::blocking::Client,
    dep: &Dependency,
    manifest_path: &Path,
    min_age: i64,
    now: DateTime<Utc>,
    base_url: &str,
) -> DepStatus {
    match &dep.kind {
        DepKind::Path => DepStatus::Skipped(SkipReason::Path),
        DepKind::Git => DepStatus::Skipped(SkipReason::Git),
        DepKind::Registry { req, pinned } => {
            if *pinned {
                return DepStatus::Skipped(SkipReason::Pinned(req.clone()));
            }
            let versions = match fetch_stable_versions_from(client, &dep.name, base_url) {
                Ok(v) => v,
                Err(e) => return DepStatus::Skipped(SkipReason::LookupFailed(e.to_string())),
            };
            if versions.is_empty() {
                return DepStatus::Skipped(SkipReason::NoStable);
            }
            let newest = &versions[0];
            let newest_age = (now - newest.created_at).num_days();

            let eligible: Vec<&StableVersion> = versions
                .iter()
                .filter(|v| (now - v.created_at).num_days() >= min_age)
                .collect();

            if eligible.is_empty() {
                return DepStatus::Skipped(SkipReason::NoEligible { newest_age });
            }

            let locked = locked_versions(manifest_path, &dep.name);
            if let Some(current) = eligible.iter().find(|v| locked.iter().any(|l| l == &v.num)) {
                let age = (now - current.created_at).num_days();
                return DepStatus::Ok {
                    version: current.num.clone(),
                    age,
                    published: current.created_at,
                };
            }

            let top = eligible[0];
            let top_age = (now - top.created_at).num_days();

            let (locked_version, locked_age, locked_published) = if let Some(l) = locked.first() {
                let v = versions.iter().find(|v| &v.num == l);
                (
                    l.clone(),
                    v.map(|v| (now - v.created_at).num_days()),
                    v.map(|v| v.created_at),
                )
            } else {
                (newest.num.clone(), Some(newest_age), Some(newest.created_at))
            };

            DepStatus::Fresh {
                locked_version,
                locked_age,
                locked_published,
                eligible_top: top.num.clone(),
                eligible_top_age: top_age,
                eligible_top_published: top.created_at,
            }
        }
    }
}

fn run_check(
    client: &reqwest::blocking::Client,
    cli: &Cli,
    min_age: i64,
    deps: &[Dependency],
    base_url: &str,
) -> i32 {
    println!(
        "Checking {} dependencies (min-age: {} days)...",
        deps.len(),
        min_age
    );

    let now = Utc::now();
    let mut fresh_count = 0usize;
    let mut ok_count = 0usize;
    let mut skipped_count = 0usize;

    for dep in deps {
        let status = classify_dep(client, dep, &cli.manifest_path, min_age, now, base_url);
        match status {
            DepStatus::Fresh {
                locked_version,
                locked_age,
                locked_published,
                eligible_top,
                eligible_top_age,
                eligible_top_published,
            } => {
                let label = format!("{} {}", dep.name, locked_version);
                let age_str = locked_age
                    .map(|a| a.to_string())
                    .unwrap_or_else(|| "?".into());
                println!(
                    "  ✓ {:<24} — {} days old, TOO FRESH (min-age {})",
                    label, age_str, min_age
                );
                if cli.verbose {
                    if let Some(p) = locked_published {
                        println!("      (published {})", p.to_rfc3339());
                    }
                    println!(
                        "      would pin to {} {} ({} days old, published {})",
                        dep.name,
                        eligible_top,
                        eligible_top_age,
                        eligible_top_published.to_rfc3339()
                    );
                }
                fresh_count += 1;
            }
            DepStatus::Ok {
                version,
                age,
                published,
            } => {
                let label = format!("{} {}", dep.name, version);
                println!(
                    "  = {:<24} — {} days old, already age-eligible",
                    label, age
                );
                if cli.verbose {
                    println!("      (published {})", published.to_rfc3339());
                }
                ok_count += 1;
            }
            DepStatus::Skipped(reason) => {
                match reason {
                    SkipReason::Path => {
                        println!("  ✗ {:<24} (path dep)    — skipping", dep.name);
                    }
                    SkipReason::Git => {
                        println!("  ✗ {:<24} (git dep)     — skipping", dep.name);
                    }
                    SkipReason::Pinned(req) => {
                        println!("  ✗ {:<24} (= {}) pinned  — skipping", dep.name, req);
                    }
                    SkipReason::NoStable => {
                        eprintln!(
                            "  ! {:<24} no stable release found — skipping",
                            dep.name
                        );
                    }
                    SkipReason::NoEligible { newest_age } => {
                        let label = dep.name.clone();
                        println!(
                            "  ✗ {:<24} — newest is {} days old, no age-eligible version, skipping",
                            label, newest_age
                        );
                    }
                    SkipReason::LookupFailed(e) => {
                        eprintln!(
                            "  ! {:<24} crates.io lookup failed ({}) — skipping",
                            dep.name, e
                        );
                    }
                }
                skipped_count += 1;
            }
        }
    }

    println!(
        "Check: {} too fresh, {} ok, {} skipped.",
        fresh_count, ok_count, skipped_count
    );

    if fresh_count > 0 {
        eprintln!(
            "error: {} direct dependency(ies) violate min-age of {} days.",
            fresh_count, min_age
        );
        1
    } else {
        0
    }
}

fn run_pass(
    client: &reqwest::blocking::Client,
    cli: &Cli,
    min_age: i64,
    deps: &[Dependency],
) -> Result<(usize, usize)> {
    println!(
        "Checking {} dependencies (min-age: {} days)...",
        deps.len(),
        min_age
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

                        if newest_age < min_age {
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
                            .filter(|v| (now - v.created_at).num_days() >= min_age)
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
    parse_manifest_str(&contents)
}

fn parse_manifest_str(contents: &str) -> Result<Vec<Dependency>> {
    let value: toml::Value = toml::from_str(contents).context("invalid TOML in manifest")?;

    let mut out: BTreeMap<String, Dependency> = BTreeMap::new();

    let sections = ["dependencies", "dev-dependencies", "build-dependencies"];

    for section in sections {
        if let Some(table) = value.get(section).and_then(|v| v.as_table()) {
            collect_deps(table, &mut out);
        }
    }

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
    fetch_stable_versions_from(client, name, CRATES_IO_API)
}

fn fetch_stable_versions_from(
    client: &reqwest::blocking::Client,
    name: &str,
    base_url: &str,
) -> Result<Vec<StableVersion>> {
    let url = format!("{}/{}", base_url.trim_end_matches('/'), name);
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

fn resolve_min_age(cli: &Cli) -> Result<Option<(i64, Option<String>)>> {
    if let Some(v) = cli.min_age {
        if v < 0 {
            return Err(anyhow!("--min-age must be non-negative, got {}", v));
        }
        return Ok(Some((v, None)));
    }

    if let Some((days, path)) = find_config_min_age(&cli.manifest_path)? {
        return Ok(Some((
            days,
            Some(format!(
                "Using min-publish-age = {} days from {} ([registry].min-publish-age)",
                days,
                path.display()
            )),
        )));
    }

    Ok(None)
}

fn parse_min_publish_age(raw: &str) -> Result<i64> {
    let s = raw.trim();
    if s.is_empty() {
        return Err(anyhow!("empty min-publish-age value"));
    }

    if let Ok(n) = s.parse::<i64>() {
        if n < 0 {
            return Err(anyhow!("min-publish-age must be non-negative, got {}", n));
        }
        return Ok(n);
    }

    let (num_str, unit) = s
        .split_once(char::is_whitespace)
        .ok_or_else(|| anyhow!("expected '<N> <unit>' (e.g. '14 days'), got {:?}", raw))?;
    let n: i64 = num_str
        .parse()
        .with_context(|| format!("invalid number {:?}", num_str))?;
    if n < 0 {
        return Err(anyhow!("min-publish-age must be non-negative, got {}", n));
    }
    let unit = unit.trim().to_lowercase();
    let days = match unit.as_str() {
        "day" | "days" | "d" => n,
        "week" | "weeks" | "w" => n * 7,
        _ => {
            return Err(anyhow!(
                "unsupported unit {:?} in min-publish-age; expected day(s) or week(s)",
                unit
            ))
        }
    };
    Ok(days)
}

fn find_config_min_age(manifest_path: &Path) -> Result<Option<(i64, PathBuf)>> {
    find_config_min_age_with_home(manifest_path, cargo_home().as_deref())
}

fn find_config_min_age_with_home(
    manifest_path: &Path,
    cargo_home: Option<&Path>,
) -> Result<Option<(i64, PathBuf)>> {
    let start = manifest_path
        .canonicalize()
        .unwrap_or_else(|_| manifest_path.to_path_buf());
    let start_dir = start
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));

    let mut current: Option<PathBuf> = Some(start_dir);
    while let Some(dir) = current {
        for candidate in [".cargo/config.toml", ".cargo/config"] {
            let path = dir.join(candidate);
            if path.is_file() {
                if let Some(days) = read_config_min_age_from_file(&path)? {
                    return Ok(Some((days, path)));
                }
            }
        }
        current = dir.parent().map(Path::to_path_buf);
    }

    if let Some(home) = cargo_home {
        for candidate in ["config.toml", "config"] {
            let path = home.join(candidate);
            if path.is_file() {
                if let Some(days) = read_config_min_age_from_file(&path)? {
                    return Ok(Some((days, path)));
                }
            }
        }
    }

    Ok(None)
}

fn read_config_min_age_from_file(path: &Path) -> Result<Option<i64>> {
    let contents = fs::read_to_string(path)
        .with_context(|| format!("could not read {}", path.display()))?;
    read_config_min_age_from_str(&contents, &path.display().to_string())
}

fn read_config_min_age_from_str(contents: &str, source: &str) -> Result<Option<i64>> {
    let value: toml::Value = toml::from_str(contents)
        .with_context(|| format!("invalid TOML in {}", source))?;

    let Some(registry) = value.get("registry").and_then(|v| v.as_table()) else {
        return Ok(None);
    };
    let Some(raw) = registry.get("min-publish-age") else {
        return Ok(None);
    };
    let s = raw.as_str().ok_or_else(|| {
        anyhow!(
            "[registry].min-publish-age in {} must be a string like \"14 days\"",
            source
        )
    })?;

    Ok(Some(parse_min_publish_age(s).with_context(|| {
        format!("in {} ([registry].min-publish-age)", source)
    })?))
}

fn cargo_home() -> Option<PathBuf> {
    if let Ok(h) = std::env::var("CARGO_HOME") {
        return Some(PathBuf::from(h));
    }
    if let Ok(h) = std::env::var("HOME") {
        return Some(PathBuf::from(h).join(".cargo"));
    }
    None
}

fn locked_versions(manifest_path: &Path, name: &str) -> Vec<String> {
    let lock_path = manifest_path
        .parent()
        .map(|p| p.join("Cargo.lock"))
        .unwrap_or_else(|| PathBuf::from("Cargo.lock"));

    let Ok(contents) = fs::read_to_string(&lock_path) else {
        return Vec::new();
    };
    locked_versions_from_str(&contents, name)
}

fn locked_versions_from_str(contents: &str, name: &str) -> Vec<String> {
    let Ok(value) = toml::from_str::<toml::Value>(contents) else {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    // ---------------- parse_min_publish_age ----------------

    #[test]
    fn parses_days_singular_and_plural() {
        assert_eq!(parse_min_publish_age("1 day").unwrap(), 1);
        assert_eq!(parse_min_publish_age("14 days").unwrap(), 14);
        assert_eq!(parse_min_publish_age("0 days").unwrap(), 0);
    }

    #[test]
    fn parses_weeks_singular_and_plural() {
        assert_eq!(parse_min_publish_age("1 week").unwrap(), 7);
        assert_eq!(parse_min_publish_age("2 weeks").unwrap(), 14);
        assert_eq!(parse_min_publish_age("52 weeks").unwrap(), 364);
    }

    #[test]
    fn parses_short_unit_forms() {
        assert_eq!(parse_min_publish_age("30 d").unwrap(), 30);
        assert_eq!(parse_min_publish_age("2 w").unwrap(), 14);
    }

    #[test]
    fn parses_bare_integer_as_days() {
        assert_eq!(parse_min_publish_age("30").unwrap(), 30);
        assert_eq!(parse_min_publish_age("0").unwrap(), 0);
    }

    #[test]
    fn trims_surrounding_whitespace() {
        assert_eq!(parse_min_publish_age("  14 days  ").unwrap(), 14);
        assert_eq!(parse_min_publish_age("\t7 days\n").unwrap(), 7);
    }

    #[test]
    fn accepts_case_insensitive_units() {
        assert_eq!(parse_min_publish_age("14 DAYS").unwrap(), 14);
        assert_eq!(parse_min_publish_age("2 Weeks").unwrap(), 14);
    }

    #[test]
    fn tolerates_multiple_spaces_between_number_and_unit() {
        // split_once whitespace grabs first ws; unit is trimmed → still parses.
        assert_eq!(parse_min_publish_age("14   days").unwrap(), 14);
    }

    #[test]
    fn rejects_empty_string() {
        assert!(parse_min_publish_age("").is_err());
        assert!(parse_min_publish_age("   ").is_err());
    }

    #[test]
    fn rejects_negative_values() {
        assert!(parse_min_publish_age("-1").is_err());
        assert!(parse_min_publish_age("-7 days").is_err());
    }

    #[test]
    fn rejects_unsupported_unit() {
        let err = parse_min_publish_age("6 months").unwrap_err().to_string();
        assert!(err.contains("months") || err.contains("unit"));
        assert!(parse_min_publish_age("1 fortnight").is_err());
        assert!(parse_min_publish_age("14 hours").is_err());
    }

    #[test]
    fn rejects_missing_number() {
        assert!(parse_min_publish_age("days").is_err());
        assert!(parse_min_publish_age("abc days").is_err());
    }

    #[test]
    fn rejects_missing_unit_when_not_bare_integer() {
        // A non-integer with no unit
        assert!(parse_min_publish_age("14.5").is_err());
    }

    // ---------------- is_stable ----------------

    #[test]
    fn plain_release_is_stable() {
        assert!(is_stable("1.0.0"));
        assert!(is_stable("0.0.1"));
        assert!(is_stable("100.200.300"));
    }

    #[test]
    fn build_metadata_is_still_stable() {
        assert!(is_stable("1.0.0+build.123"));
        assert!(is_stable("1.1.2+spec-1.1.0"));
    }

    #[test]
    fn prerelease_suffix_is_not_stable() {
        assert!(!is_stable("1.0.0-alpha"));
        assert!(!is_stable("1.0.0-beta.1"));
        assert!(!is_stable("2.0.0-rc.1"));
        assert!(!is_stable("1.0.0-0.3.7"));
    }

    #[test]
    fn prerelease_with_build_metadata_is_not_stable() {
        assert!(!is_stable("1.0.0-alpha+build.123"));
        assert!(!is_stable("1.0.0-beta+exp.sha.5114f85"));
    }

    // ---------------- classify ----------------

    #[test]
    fn classifies_string_shorthand_as_registry() {
        let v = toml::Value::String("1.0".into());
        let d = classify("serde", &v);
        assert_eq!(d.name, "serde");
        match d.kind {
            DepKind::Registry { req, pinned } => {
                assert_eq!(req, "1.0");
                assert!(!pinned);
            }
            _ => panic!("expected Registry"),
        }
    }

    #[test]
    fn classifies_string_with_equals_pin() {
        let v = toml::Value::String("=1.0.210".into());
        let d = classify("serde", &v);
        match d.kind {
            DepKind::Registry { req, pinned } => {
                assert_eq!(req, "=1.0.210");
                assert!(pinned);
            }
            _ => panic!("expected Registry"),
        }
    }

    #[test]
    fn classifies_table_with_version() {
        let toml_str = r#"version = "0.4""#;
        let v: toml::Value = toml::from_str(toml_str).unwrap();
        let d = classify("chrono", &v);
        match d.kind {
            DepKind::Registry { req, pinned } => {
                assert_eq!(req, "0.4");
                assert!(!pinned);
            }
            _ => panic!("expected Registry"),
        }
    }

    #[test]
    fn classifies_table_with_equals_pin() {
        let v: toml::Value = toml::from_str(r#"version = "=1.0.210""#).unwrap();
        let d = classify("serde", &v);
        match d.kind {
            DepKind::Registry { pinned, .. } => assert!(pinned),
            _ => panic!("expected Registry"),
        }
    }

    #[test]
    fn classifies_path_dependency() {
        let v: toml::Value = toml::from_str(r#"path = "../local""#).unwrap();
        let d = classify("local", &v);
        assert!(matches!(d.kind, DepKind::Path));
    }

    #[test]
    fn path_wins_over_version_field() {
        let v: toml::Value =
            toml::from_str(r#"path = "../local"
version = "1.0""#).unwrap();
        let d = classify("local", &v);
        assert!(matches!(d.kind, DepKind::Path));
    }

    #[test]
    fn classifies_git_dependency() {
        let v: toml::Value =
            toml::from_str(r#"git = "https://github.com/foo/bar""#).unwrap();
        let d = classify("bar", &v);
        assert!(matches!(d.kind, DepKind::Git));
    }

    #[test]
    fn honors_package_rename() {
        let v: toml::Value = toml::from_str(
            r#"package = "actual-crate"
version = "1.0""#,
        )
        .unwrap();
        let d = classify("alias", &v);
        assert_eq!(d.name, "actual-crate");
    }

    #[test]
    fn missing_version_yields_empty_req_unpinned() {
        let v: toml::Value = toml::from_str(r#"features = ["derive"]"#).unwrap();
        let d = classify("serde", &v);
        match d.kind {
            DepKind::Registry { req, pinned } => {
                assert_eq!(req, "");
                assert!(!pinned);
            }
            _ => panic!("expected Registry"),
        }
    }

    // ---------------- parse_manifest_str ----------------

    fn dep_names(deps: &[Dependency]) -> Vec<&str> {
        let mut v: Vec<&str> = deps.iter().map(|d| d.name.as_str()).collect();
        v.sort();
        v
    }

    #[test]
    fn parses_plain_dependencies_section() {
        let manifest = r#"
[package]
name = "x"
version = "0.1.0"

[dependencies]
serde = "1.0"
tokio = { version = "1", features = ["rt"] }
"#;
        let deps = parse_manifest_str(manifest).unwrap();
        assert_eq!(dep_names(&deps), vec!["serde", "tokio"]);
    }

    #[test]
    fn merges_dev_and_build_dependencies() {
        let manifest = r#"
[dependencies]
a = "1"
[dev-dependencies]
b = "1"
[build-dependencies]
c = "1"
"#;
        let deps = parse_manifest_str(manifest).unwrap();
        assert_eq!(dep_names(&deps), vec!["a", "b", "c"]);
    }

    #[test]
    fn parses_target_cfg_dependencies() {
        let manifest = r#"
[target.'cfg(unix)'.dependencies]
nix = "0.28"

[target.x86_64-pc-windows-msvc.dependencies]
winapi = "0.3"
"#;
        let deps = parse_manifest_str(manifest).unwrap();
        assert_eq!(dep_names(&deps), vec!["nix", "winapi"]);
    }

    #[test]
    fn parses_workspace_dependencies() {
        let manifest = r#"
[workspace]
members = []
[workspace.dependencies]
shared = "2.0"
"#;
        let deps = parse_manifest_str(manifest).unwrap();
        assert_eq!(dep_names(&deps), vec!["shared"]);
    }

    #[test]
    fn dedupes_across_sections_preferring_signals() {
        // Same crate in [dependencies] as registry AND in [dev-dependencies] as path
        // should end up as Path (signals override plain registry).
        let manifest = r#"
[dependencies]
mylib = "1.0"

[dev-dependencies]
mylib = { path = "../mylib" }
"#;
        let deps = parse_manifest_str(manifest).unwrap();
        assert_eq!(deps.len(), 1);
        assert!(matches!(deps[0].kind, DepKind::Path));
    }

    #[test]
    fn empty_manifest_yields_no_deps() {
        assert!(parse_manifest_str("").unwrap().is_empty());
        let just_package = r#"
[package]
name = "x"
version = "0.1.0"
"#;
        assert!(parse_manifest_str(just_package).unwrap().is_empty());
    }

    #[test]
    fn invalid_toml_errors() {
        assert!(parse_manifest_str("this is not = valid ]").is_err());
    }

    // ---------------- locked_versions_from_str ----------------

    #[test]
    fn locked_versions_finds_single_entry() {
        let lock = r#"
[[package]]
name = "serde"
version = "1.0.210"
source = "registry+https://github.com/rust-lang/crates.io-index"
"#;
        assert_eq!(locked_versions_from_str(lock, "serde"), vec!["1.0.210"]);
    }

    #[test]
    fn locked_versions_finds_multiple_entries_same_name() {
        let lock = r#"
[[package]]
name = "foo"
version = "1.0.0"

[[package]]
name = "foo"
version = "2.0.0"

[[package]]
name = "bar"
version = "0.5.0"
"#;
        let mut versions = locked_versions_from_str(lock, "foo");
        versions.sort();
        assert_eq!(versions, vec!["1.0.0", "2.0.0"]);
        assert_eq!(locked_versions_from_str(lock, "bar"), vec!["0.5.0"]);
    }

    #[test]
    fn locked_versions_empty_for_missing_name() {
        let lock = r#"
[[package]]
name = "serde"
version = "1.0.210"
"#;
        assert!(locked_versions_from_str(lock, "tokio").is_empty());
    }

    #[test]
    fn locked_versions_empty_for_malformed_lockfile() {
        assert!(locked_versions_from_str("garbage ]] {{", "serde").is_empty());
        assert!(locked_versions_from_str("", "serde").is_empty());
    }

    // ---------------- read_config_min_age_from_str ----------------

    #[test]
    fn reads_min_publish_age_from_config() {
        let cfg = r#"
[registry]
min-publish-age = "14 days"
"#;
        assert_eq!(
            read_config_min_age_from_str(cfg, "test").unwrap(),
            Some(14)
        );
    }

    #[test]
    fn returns_none_when_registry_section_missing() {
        let cfg = r#"
[build]
target = "x86_64-unknown-linux-gnu"
"#;
        assert_eq!(read_config_min_age_from_str(cfg, "test").unwrap(), None);
    }

    #[test]
    fn returns_none_when_registry_present_but_key_missing() {
        let cfg = r#"
[registry]
default = "crates-io"
"#;
        assert_eq!(read_config_min_age_from_str(cfg, "test").unwrap(), None);
    }

    #[test]
    fn errors_when_min_publish_age_is_not_string() {
        let cfg = r#"
[registry]
min-publish-age = 14
"#;
        let err = read_config_min_age_from_str(cfg, "some/path")
            .unwrap_err()
            .to_string();
        assert!(err.contains("must be a string"));
        assert!(err.contains("some/path"));
    }

    #[test]
    fn errors_on_invalid_duration_string() {
        let cfg = r#"
[registry]
min-publish-age = "gibberish"
"#;
        assert!(read_config_min_age_from_str(cfg, "test").is_err());
    }

    #[test]
    fn errors_on_invalid_toml() {
        assert!(read_config_min_age_from_str("[[[not toml", "test").is_err());
    }

    #[test]
    fn empty_config_returns_none() {
        assert_eq!(read_config_min_age_from_str("", "test").unwrap(), None);
    }

    // ---------------- find_config_min_age_with_home (fs) ----------------

    fn write(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, contents).unwrap();
    }

    const CFG_14: &str = "[registry]\nmin-publish-age = \"14 days\"\n";
    const CFG_30: &str = "[registry]\nmin-publish-age = \"30 days\"\n";

    #[test]
    fn find_config_in_same_directory_as_manifest() {
        let td = TempDir::new().unwrap();
        write(&td.path().join("Cargo.toml"), "[package]\nname = \"x\"\n");
        write(&td.path().join(".cargo/config.toml"), CFG_14);

        let (days, path) = find_config_min_age_with_home(
            &td.path().join("Cargo.toml"),
            None,
        )
        .unwrap()
        .expect("should find config");
        assert_eq!(days, 14);
        assert!(path.ends_with(".cargo/config.toml"));
    }

    #[test]
    fn find_config_walks_up_to_parent_directory() {
        let td = TempDir::new().unwrap();
        // config in parent, manifest in subdir/
        write(&td.path().join(".cargo/config.toml"), CFG_30);
        write(
            &td.path().join("subdir/Cargo.toml"),
            "[package]\nname = \"x\"\n",
        );

        let (days, _path) = find_config_min_age_with_home(
            &td.path().join("subdir/Cargo.toml"),
            None,
        )
        .unwrap()
        .expect("should find config in parent");
        assert_eq!(days, 30);
    }

    #[test]
    fn nearest_config_wins_over_ancestor() {
        let td = TempDir::new().unwrap();
        // Ancestor says 30 days, nearest says 14.
        write(&td.path().join(".cargo/config.toml"), CFG_30);
        write(&td.path().join("child/.cargo/config.toml"), CFG_14);
        write(
            &td.path().join("child/Cargo.toml"),
            "[package]\nname = \"x\"\n",
        );

        let (days, path) = find_config_min_age_with_home(
            &td.path().join("child/Cargo.toml"),
            None,
        )
        .unwrap()
        .expect("should find nearest");
        assert_eq!(days, 14);
        assert!(path.to_string_lossy().contains("child/.cargo"));
    }

    #[test]
    fn falls_back_to_cargo_home_when_no_project_config() {
        let project = TempDir::new().unwrap();
        let home = TempDir::new().unwrap();
        write(
            &project.path().join("Cargo.toml"),
            "[package]\nname = \"x\"\n",
        );
        write(&home.path().join("config.toml"), CFG_14);

        let (days, path) = find_config_min_age_with_home(
            &project.path().join("Cargo.toml"),
            Some(home.path()),
        )
        .unwrap()
        .expect("should find in cargo home");
        assert_eq!(days, 14);
        assert!(path.starts_with(home.path()));
    }

    #[test]
    fn cargo_home_ignored_when_project_config_present() {
        let project = TempDir::new().unwrap();
        let home = TempDir::new().unwrap();
        write(
            &project.path().join("Cargo.toml"),
            "[package]\nname = \"x\"\n",
        );
        write(&project.path().join(".cargo/config.toml"), CFG_14);
        write(&home.path().join("config.toml"), CFG_30); // should be ignored

        let (days, _) = find_config_min_age_with_home(
            &project.path().join("Cargo.toml"),
            Some(home.path()),
        )
        .unwrap()
        .expect("project config wins");
        assert_eq!(days, 14);
    }

    #[test]
    fn returns_none_when_no_config_anywhere() {
        let project = TempDir::new().unwrap();
        let home = TempDir::new().unwrap();
        write(
            &project.path().join("Cargo.toml"),
            "[package]\nname = \"x\"\n",
        );

        let found = find_config_min_age_with_home(
            &project.path().join("Cargo.toml"),
            Some(home.path()),
        )
        .unwrap();
        assert!(found.is_none());
    }

    #[test]
    fn config_without_min_publish_age_is_skipped_and_walk_continues() {
        let td = TempDir::new().unwrap();
        // Nearest config has no key; ancestor does.
        write(&td.path().join(".cargo/config.toml"), CFG_14);
        write(
            &td.path().join("child/.cargo/config.toml"),
            "[build]\ntarget = \"x86_64-unknown-linux-gnu\"\n",
        );
        write(
            &td.path().join("child/Cargo.toml"),
            "[package]\nname = \"x\"\n",
        );

        let (days, _) = find_config_min_age_with_home(
            &td.path().join("child/Cargo.toml"),
            None,
        )
        .unwrap()
        .expect("should fall through to ancestor");
        assert_eq!(days, 14);
    }

    #[test]
    fn also_reads_legacy_dot_cargo_config() {
        let td = TempDir::new().unwrap();
        // Old-style .cargo/config (no .toml suffix)
        write(&td.path().join(".cargo/config"), CFG_14);
        write(&td.path().join("Cargo.toml"), "[package]\nname = \"x\"\n");

        let (days, path) = find_config_min_age_with_home(
            &td.path().join("Cargo.toml"),
            None,
        )
        .unwrap()
        .expect("should find legacy config");
        assert_eq!(days, 14);
        assert!(path.ends_with(".cargo/config"));
    }

    // ---------------- resolve_min_age precedence ----------------

    fn cli_with(min_age: Option<i64>, manifest_path: PathBuf) -> Cli {
        Cli {
            min_age,
            manifest_path,
            dry_run: false,
            verbose: false,
            iterate: false,
            check: false,
        }
    }

    #[test]
    fn resolve_min_age_uses_cli_when_provided() {
        let td = TempDir::new().unwrap();
        write(
            &td.path().join("Cargo.toml"),
            "[package]\nname = \"x\"\n",
        );
        // Also drop a config that would give a different value — CLI should win.
        write(&td.path().join(".cargo/config.toml"), CFG_30);

        let cli = cli_with(Some(7), td.path().join("Cargo.toml"));
        let (days, note) = resolve_min_age(&cli).unwrap().unwrap();
        assert_eq!(days, 7);
        assert!(note.is_none(), "CLI-sourced value should have no note");
    }

    #[test]
    fn resolve_min_age_rejects_negative_cli_value() {
        let td = TempDir::new().unwrap();
        write(
            &td.path().join("Cargo.toml"),
            "[package]\nname = \"x\"\n",
        );
        let cli = cli_with(Some(-1), td.path().join("Cargo.toml"));
        assert!(resolve_min_age(&cli).is_err());
    }

    // ---------------- fetch_stable_versions (mocked crates.io) ----------------

    fn test_client() -> reqwest::blocking::Client {
        reqwest::blocking::Client::builder()
            .user_agent(USER_AGENT)
            .build()
            .unwrap()
    }

    #[test]
    fn fetch_returns_stable_versions_sorted_newest_first() {
        let mut server = mockito::Server::new();
        let _m = server
            .mock("GET", "/serde")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{
                    "versions": [
                        {"num": "1.0.0", "created_at": "2024-01-01T00:00:00Z", "yanked": false},
                        {"num": "1.0.5", "created_at": "2024-03-01T00:00:00Z", "yanked": false},
                        {"num": "1.0.3", "created_at": "2024-02-01T00:00:00Z", "yanked": false}
                    ]
                }"#,
            )
            .create();

        let versions =
            fetch_stable_versions_from(&test_client(), "serde", &server.url()).unwrap();
        assert_eq!(
            versions.iter().map(|v| v.num.as_str()).collect::<Vec<_>>(),
            vec!["1.0.5", "1.0.3", "1.0.0"]
        );
    }

    #[test]
    fn fetch_filters_out_yanked_versions() {
        let mut server = mockito::Server::new();
        let _m = server
            .mock("GET", "/serde")
            .with_body(
                r#"{
                    "versions": [
                        {"num": "1.0.0", "created_at": "2024-01-01T00:00:00Z", "yanked": false},
                        {"num": "1.0.1", "created_at": "2024-02-01T00:00:00Z", "yanked": true},
                        {"num": "1.0.2", "created_at": "2024-03-01T00:00:00Z", "yanked": false}
                    ]
                }"#,
            )
            .create();

        let versions =
            fetch_stable_versions_from(&test_client(), "serde", &server.url()).unwrap();
        let nums: Vec<&str> = versions.iter().map(|v| v.num.as_str()).collect();
        assert_eq!(nums, vec!["1.0.2", "1.0.0"]);
    }

    #[test]
    fn fetch_filters_out_prerelease_versions() {
        let mut server = mockito::Server::new();
        let _m = server
            .mock("GET", "/serde")
            .with_body(
                r#"{
                    "versions": [
                        {"num": "1.0.0", "created_at": "2024-01-01T00:00:00Z", "yanked": false},
                        {"num": "2.0.0-alpha", "created_at": "2024-02-01T00:00:00Z", "yanked": false},
                        {"num": "2.0.0-rc.1", "created_at": "2024-03-01T00:00:00Z", "yanked": false},
                        {"num": "1.5.0", "created_at": "2024-01-15T00:00:00Z", "yanked": false}
                    ]
                }"#,
            )
            .create();

        let versions =
            fetch_stable_versions_from(&test_client(), "serde", &server.url()).unwrap();
        let nums: Vec<&str> = versions.iter().map(|v| v.num.as_str()).collect();
        assert_eq!(nums, vec!["1.5.0", "1.0.0"]);
    }

    #[test]
    fn fetch_keeps_versions_with_build_metadata() {
        let mut server = mockito::Server::new();
        let _m = server
            .mock("GET", "/toml")
            .with_body(
                r#"{
                    "versions": [
                        {"num": "1.1.2+spec-1.1.0", "created_at": "2025-04-01T00:00:00Z", "yanked": false}
                    ]
                }"#,
            )
            .create();

        let versions =
            fetch_stable_versions_from(&test_client(), "toml", &server.url()).unwrap();
        assert_eq!(versions.len(), 1);
        assert_eq!(versions[0].num, "1.1.2+spec-1.1.0");
    }

    #[test]
    fn fetch_returns_empty_when_all_versions_are_prerelease_or_yanked() {
        let mut server = mockito::Server::new();
        let _m = server
            .mock("GET", "/newcrate")
            .with_body(
                r#"{
                    "versions": [
                        {"num": "0.1.0-alpha", "created_at": "2024-01-01T00:00:00Z", "yanked": false},
                        {"num": "0.1.0-beta", "created_at": "2024-01-05T00:00:00Z", "yanked": false},
                        {"num": "0.1.0", "created_at": "2024-01-10T00:00:00Z", "yanked": true}
                    ]
                }"#,
            )
            .create();

        let versions =
            fetch_stable_versions_from(&test_client(), "newcrate", &server.url()).unwrap();
        assert!(versions.is_empty());
    }

    #[test]
    fn fetch_returns_empty_when_versions_array_is_empty() {
        let mut server = mockito::Server::new();
        let _m = server
            .mock("GET", "/empty")
            .with_body(r#"{"versions": []}"#)
            .create();

        let versions =
            fetch_stable_versions_from(&test_client(), "empty", &server.url()).unwrap();
        assert!(versions.is_empty());
    }

    #[test]
    fn fetch_errors_on_404_crate_not_found() {
        let mut server = mockito::Server::new();
        let _m = server
            .mock("GET", "/nonexistent")
            .with_status(404)
            .with_body(r#"{"errors": [{"detail": "Not Found"}]}"#)
            .create();

        let err = fetch_stable_versions_from(&test_client(), "nonexistent", &server.url())
            .unwrap_err()
            .to_string();
        assert!(err.contains("not found"), "unexpected error: {}", err);
    }

    #[test]
    fn fetch_errors_on_5xx() {
        let mut server = mockito::Server::new();
        let _m = server
            .mock("GET", "/broken")
            .with_status(503)
            .create();

        let err = fetch_stable_versions_from(&test_client(), "broken", &server.url())
            .unwrap_err()
            .to_string();
        assert!(err.contains("503") || err.contains("HTTP"), "unexpected: {}", err);
    }

    #[test]
    fn fetch_errors_on_malformed_json() {
        let mut server = mockito::Server::new();
        let _m = server
            .mock("GET", "/bad")
            .with_status(200)
            .with_body("this is definitely not json")
            .create();

        let err = fetch_stable_versions_from(&test_client(), "bad", &server.url())
            .unwrap_err()
            .to_string();
        assert!(err.contains("JSON") || err.contains("invalid"), "unexpected: {}", err);
    }

    #[test]
    fn fetch_errors_on_unexpected_json_shape() {
        let mut server = mockito::Server::new();
        let _m = server
            .mock("GET", "/wrong")
            .with_body(r#"{"not_versions": []}"#)
            .create();

        assert!(fetch_stable_versions_from(&test_client(), "wrong", &server.url()).is_err());
    }

    #[test]
    fn fetch_trims_trailing_slash_from_base_url() {
        let mut server = mockito::Server::new();
        let _m = server
            .mock("GET", "/foo")
            .with_body(r#"{"versions": []}"#)
            .create();

        let mut url = server.url();
        url.push('/');
        // Should NOT try to hit /<empty>/foo — trailing slash is trimmed.
        assert!(fetch_stable_versions_from(&test_client(), "foo", &url).is_ok());
    }

    #[test]
    fn fetch_yanked_defaults_to_false_when_field_missing() {
        // The serde default on VersionInfo.yanked ensures API responses without
        // the `yanked` key are treated as not-yanked.
        let mut server = mockito::Server::new();
        let _m = server
            .mock("GET", "/legacy")
            .with_body(
                r#"{
                    "versions": [
                        {"num": "1.0.0", "created_at": "2024-01-01T00:00:00Z"}
                    ]
                }"#,
            )
            .create();

        let versions =
            fetch_stable_versions_from(&test_client(), "legacy", &server.url()).unwrap();
        assert_eq!(versions.len(), 1);
    }

    #[test]
    fn resolve_min_age_returns_none_when_neither_set() {
        let td = TempDir::new().unwrap();
        write(
            &td.path().join("Cargo.toml"),
            "[package]\nname = \"x\"\n",
        );
        let cli = cli_with(None, td.path().join("Cargo.toml"));
        // Note: this test can be affected by user's real ~/.cargo/config.toml if
        // it happens to define min-publish-age. Skip assertion in that case.
        if let Ok(Some((_, _))) = resolve_min_age(&cli) {
            eprintln!("skipping: real cargo home has min-publish-age set");
            return;
        }
        assert!(resolve_min_age(&cli).unwrap().is_none());
    }

    // ---------------- --check mode ----------------

    use chrono::Duration;

    fn check_cli(manifest_path: PathBuf) -> Cli {
        Cli {
            min_age: Some(30),
            manifest_path,
            dry_run: false,
            verbose: false,
            iterate: false,
            check: true,
        }
    }

    fn iso(dt: DateTime<Utc>) -> String {
        dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    }

    fn write_manifest_and_lock(td: &TempDir, manifest: &str, lock: &str) -> PathBuf {
        let manifest_path = td.path().join("Cargo.toml");
        fs::write(&manifest_path, manifest).unwrap();
        fs::write(td.path().join("Cargo.lock"), lock).unwrap();
        manifest_path
    }

    #[test]
    fn check_returns_zero_when_all_locked_versions_are_age_eligible() {
        let now = Utc::now();
        let old_a = now - Duration::days(200);
        let old_b = now - Duration::days(150);

        let mut server = mockito::Server::new();
        let _ma = server
            .mock("GET", "/serde")
            .with_body(format!(
                r#"{{"versions":[{{"num":"1.0.100","created_at":"{}","yanked":false}}]}}"#,
                iso(old_a)
            ))
            .create();
        let _mb = server
            .mock("GET", "/tokio")
            .with_body(format!(
                r#"{{"versions":[{{"num":"1.30.0","created_at":"{}","yanked":false}}]}}"#,
                iso(old_b)
            ))
            .create();

        let td = TempDir::new().unwrap();
        let manifest = r#"
[package]
name = "x"
version = "0.1.0"
[dependencies]
serde = "1.0"
tokio = "1"
"#;
        let lock = r#"
[[package]]
name = "serde"
version = "1.0.100"

[[package]]
name = "tokio"
version = "1.30.0"
"#;
        let manifest_path = write_manifest_and_lock(&td, manifest, lock);
        let deps = parse_manifest(&manifest_path).unwrap();
        let cli = check_cli(manifest_path);
        let code = run_check(&test_client(), &cli, 30, &deps, &server.url());
        assert_eq!(code, 0);
    }

    #[test]
    fn check_returns_one_when_locked_version_is_too_fresh_and_eligible_exists() {
        let now = Utc::now();
        let old = now - Duration::days(200);
        let fresh = now - Duration::days(5);

        let mut server = mockito::Server::new();
        let _m = server
            .mock("GET", "/serde")
            .with_body(format!(
                r#"{{"versions":[
                    {{"num":"1.0.200","created_at":"{}","yanked":false}},
                    {{"num":"1.0.100","created_at":"{}","yanked":false}}
                ]}}"#,
                iso(fresh),
                iso(old)
            ))
            .create();

        let td = TempDir::new().unwrap();
        let manifest = "[package]\nname = \"x\"\nversion = \"0.1.0\"\n[dependencies]\nserde = \"1.0\"\n";
        // Cargo.lock is holding the too-fresh version.
        let lock = "[[package]]\nname = \"serde\"\nversion = \"1.0.200\"\n";
        let manifest_path = write_manifest_and_lock(&td, manifest, lock);
        let deps = parse_manifest(&manifest_path).unwrap();
        let cli = check_cli(manifest_path);
        let code = run_check(&test_client(), &cli, 30, &deps, &server.url());
        assert_eq!(code, 1);
    }

    #[test]
    fn check_treats_dep_with_no_eligible_replacement_as_skipped_not_fresh() {
        let now = Utc::now();
        // Every published version is too fresh.
        let fresh_a = now - Duration::days(5);
        let fresh_b = now - Duration::days(10);

        let mut server = mockito::Server::new();
        let _m = server
            .mock("GET", "/newcrate")
            .with_body(format!(
                r#"{{"versions":[
                    {{"num":"0.2.0","created_at":"{}","yanked":false}},
                    {{"num":"0.1.0","created_at":"{}","yanked":false}}
                ]}}"#,
                iso(fresh_a),
                iso(fresh_b)
            ))
            .create();

        let td = TempDir::new().unwrap();
        let manifest = "[package]\nname = \"x\"\nversion = \"0.1.0\"\n[dependencies]\nnewcrate = \"0.2\"\n";
        let lock = "[[package]]\nname = \"newcrate\"\nversion = \"0.2.0\"\n";
        let manifest_path = write_manifest_and_lock(&td, manifest, lock);
        let deps = parse_manifest(&manifest_path).unwrap();
        let cli = check_cli(manifest_path);
        let code = run_check(&test_client(), &cli, 30, &deps, &server.url());
        assert_eq!(code, 0);
    }

    #[test]
    fn check_conflicts_with_dry_run_at_parse_time() {
        let err = Cli::try_parse_from([
            "cargo-aged",
            "--check",
            "--dry-run",
            "--min-age",
            "30",
        ])
        .unwrap_err();
        // clap surfaces the conflict as an ArgumentConflict error kind.
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn check_conflicts_with_iterate_at_parse_time() {
        let err = Cli::try_parse_from([
            "cargo-aged",
            "--check",
            "--iterate",
            "--min-age",
            "30",
        ])
        .unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn check_does_not_touch_cargo_lock() {
        let now = Utc::now();
        let fresh = now - Duration::days(5);
        let old = now - Duration::days(200);

        let mut server = mockito::Server::new();
        let _m = server
            .mock("GET", "/serde")
            .with_body(format!(
                r#"{{"versions":[
                    {{"num":"1.0.200","created_at":"{}","yanked":false}},
                    {{"num":"1.0.100","created_at":"{}","yanked":false}}
                ]}}"#,
                iso(fresh),
                iso(old)
            ))
            .create();

        let td = TempDir::new().unwrap();
        let manifest = "[package]\nname = \"x\"\nversion = \"0.1.0\"\n[dependencies]\nserde = \"1.0\"\n";
        // Lock is at the too-fresh version — the "guard would act" case.
        let lock = "[[package]]\nname = \"serde\"\nversion = \"1.0.200\"\n";
        let manifest_path = write_manifest_and_lock(&td, manifest, lock);
        let lock_path = td.path().join("Cargo.lock");

        let before = fs::read(&lock_path).unwrap();
        let deps = parse_manifest(&manifest_path).unwrap();
        let cli = check_cli(manifest_path);
        let code = run_check(&test_client(), &cli, 30, &deps, &server.url());
        let after = fs::read(&lock_path).unwrap();

        assert_eq!(code, 1);
        assert_eq!(before, after, "Cargo.lock must be byte-identical after --check");
    }

    #[test]
    fn classify_dep_marks_path_dep_as_skipped() {
        let td = TempDir::new().unwrap();
        let manifest_path = td.path().join("Cargo.toml");
        fs::write(&manifest_path, "[package]\nname=\"x\"\nversion=\"0.1.0\"\n").unwrap();

        let dep = Dependency {
            name: "local".into(),
            kind: DepKind::Path,
        };
        let status = classify_dep(
            &test_client(),
            &dep,
            &manifest_path,
            30,
            Utc::now(),
            "http://unused.invalid",
        );
        assert!(matches!(status, DepStatus::Skipped(SkipReason::Path)));
    }

    #[test]
    fn classify_dep_marks_equals_pin_as_skipped() {
        let td = TempDir::new().unwrap();
        let manifest_path = td.path().join("Cargo.toml");
        fs::write(&manifest_path, "[package]\nname=\"x\"\nversion=\"0.1.0\"\n").unwrap();

        let dep = Dependency {
            name: "serde".into(),
            kind: DepKind::Registry {
                req: "=1.0.210".into(),
                pinned: true,
            },
        };
        let status = classify_dep(
            &test_client(),
            &dep,
            &manifest_path,
            30,
            Utc::now(),
            "http://unused.invalid",
        );
        assert!(matches!(
            status,
            DepStatus::Skipped(SkipReason::Pinned(_))
        ));
    }

    #[test]
    fn classify_dep_marks_locked_eligible_version_as_ok() {
        let now = Utc::now();
        let old = now - Duration::days(200);

        let mut server = mockito::Server::new();
        let _m = server
            .mock("GET", "/serde")
            .with_body(format!(
                r#"{{"versions":[{{"num":"1.0.100","created_at":"{}","yanked":false}}]}}"#,
                iso(old)
            ))
            .create();

        let td = TempDir::new().unwrap();
        let manifest_path = td.path().join("Cargo.toml");
        fs::write(&manifest_path, "[package]\nname=\"x\"\nversion=\"0.1.0\"\n").unwrap();
        fs::write(
            td.path().join("Cargo.lock"),
            "[[package]]\nname=\"serde\"\nversion=\"1.0.100\"\n",
        )
        .unwrap();

        let dep = Dependency {
            name: "serde".into(),
            kind: DepKind::Registry {
                req: "1.0".into(),
                pinned: false,
            },
        };
        let status =
            classify_dep(&test_client(), &dep, &manifest_path, 30, now, &server.url());
        match status {
            DepStatus::Ok { version, .. } => assert_eq!(version, "1.0.100"),
            other => panic!("expected Ok, got {:?}", other),
        }
    }

    #[test]
    fn classify_dep_marks_locked_fresh_version_with_older_eligible_as_fresh() {
        let now = Utc::now();
        let fresh = now - Duration::days(5);
        let old = now - Duration::days(200);

        let mut server = mockito::Server::new();
        let _m = server
            .mock("GET", "/serde")
            .with_body(format!(
                r#"{{"versions":[
                    {{"num":"1.0.200","created_at":"{}","yanked":false}},
                    {{"num":"1.0.100","created_at":"{}","yanked":false}}
                ]}}"#,
                iso(fresh),
                iso(old)
            ))
            .create();

        let td = TempDir::new().unwrap();
        let manifest_path = td.path().join("Cargo.toml");
        fs::write(&manifest_path, "[package]\nname=\"x\"\nversion=\"0.1.0\"\n").unwrap();
        fs::write(
            td.path().join("Cargo.lock"),
            "[[package]]\nname=\"serde\"\nversion=\"1.0.200\"\n",
        )
        .unwrap();

        let dep = Dependency {
            name: "serde".into(),
            kind: DepKind::Registry {
                req: "1.0".into(),
                pinned: false,
            },
        };
        let status =
            classify_dep(&test_client(), &dep, &manifest_path, 30, now, &server.url());
        match status {
            DepStatus::Fresh {
                locked_version,
                eligible_top,
                ..
            } => {
                assert_eq!(locked_version, "1.0.200");
                assert_eq!(eligible_top, "1.0.100");
            }
            other => panic!("expected Fresh, got {:?}", other),
        }
    }
}
