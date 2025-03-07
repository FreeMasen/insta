use std::borrow::Cow;
use std::collections::HashSet;
use std::error::Error;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process;
use std::{env, fs};

use console::{set_colors_enabled, style, Key, Term};
use ignore::{Walk, WalkBuilder};
use insta::Snapshot;
use insta::_cargo_insta_support::print_snapshot_diff;
use serde::Serialize;
use structopt::clap::AppSettings;
use structopt::StructOpt;
use uuid::Uuid;

use crate::cargo::{
    find_packages, find_snapshots, get_cargo, get_package_metadata, Operation, Package,
    SnapshotContainer,
};
use crate::utils::{err_msg, QuietExit};

/// A helper utility to work with insta snapshots.
#[derive(StructOpt, Debug)]
#[structopt(
    bin_name = "cargo insta",
    setting = AppSettings::ArgRequiredElseHelp,
    global_setting = AppSettings::ColorNever,
    global_setting = AppSettings::UnifiedHelpMessage,
    global_setting = AppSettings::DeriveDisplayOrder,
    global_setting = AppSettings::DontCollapseArgsInUsage
)]
pub struct Opts {
    /// Coloring: auto, always, never
    #[structopt(long, global = true, value_name = "WHEN")]
    pub color: Option<String>,

    #[structopt(subcommand)]
    pub command: Command,
}

#[derive(StructOpt, Debug)]
#[structopt(bin_name = "cargo insta")]
pub enum Command {
    /// Interactively review snapshots
    #[structopt(name = "review", alias = "verify")]
    Review(ProcessCommand),
    /// Rejects all snapshots
    #[structopt(name = "reject")]
    Reject(ProcessCommand),
    /// Accept all snapshots
    #[structopt(name = "accept", alias = "approve")]
    Accept(ProcessCommand),
    /// Run tests and then reviews
    #[structopt(name = "test")]
    Test(TestCommand),
    /// Print a summary of all pending snapshots.
    #[structopt(name = "pending-snapshots")]
    PendingSnapshots(PendingSnapshotsCommand),
}

#[derive(StructOpt, Debug, Clone)]
#[structopt(rename_all = "kebab-case")]
pub struct TargetArgs {
    /// Path to Cargo.toml
    #[structopt(long, value_name = "PATH", parse(from_os_str))]
    pub manifest_path: Option<PathBuf>,
    /// Explicit path to the workspace root
    #[structopt(long, value_name = "PATH", parse(from_os_str))]
    pub workspace_root: Option<PathBuf>,
    /// Sets the extensions to consider.  Defaults to `.snap`
    #[structopt(short = "e", long, value_name = "EXTENSIONS", multiple = true)]
    pub extensions: Vec<String>,
    /// Work on all packages in the workspace
    #[structopt(long)]
    pub all: bool,
    /// Also walk into ignored paths.
    #[structopt(long)]
    pub no_ignore: bool,
}

#[derive(StructOpt, Debug)]
#[structopt(rename_all = "kebab-case")]
pub struct ProcessCommand {
    #[structopt(flatten)]
    pub target_args: TargetArgs,
    /// Limits the operation to one or more snapshots.
    #[structopt(long = "snapshot")]
    pub snapshot_filter: Option<Vec<String>>,
    /// Do not print to stdout.
    #[structopt(short = "q", long)]
    pub quiet: bool,
}

#[derive(StructOpt, Debug)]
#[structopt(rename_all = "kebab-case")]
pub struct TestCommand {
    #[structopt(flatten)]
    pub target_args: TargetArgs,
    /// Package to run tests for
    #[structopt(short = "p", long)]
    pub package: Option<String>,
    /// Disable force-passing of snapshot tests
    #[structopt(long)]
    pub no_force_pass: bool,
    /// Prevent running all tests regardless of failure
    #[structopt(long)]
    pub fail_fast: bool,
    /// Space-separated list of features to activate
    #[structopt(long, value_name = "FEATURES")]
    pub features: Option<String>,
    /// Number of parallel jobs, defaults to # of CPUs
    #[structopt(short = "j", long)]
    pub jobs: Option<usize>,
    /// Build artifacts in release mode, with optimizations
    #[structopt(long)]
    pub release: bool,
    /// Activate all available features
    #[structopt(long)]
    pub all_features: bool,
    /// Do not activate the `default` feature
    #[structopt(long)]
    pub no_default_features: bool,
    /// Follow up with review.
    #[structopt(long)]
    pub review: bool,
    /// Accept all snapshots after test.
    #[structopt(long, conflicts_with = "review")]
    pub accept: bool,
    /// Accept all new (previously unseen).
    #[structopt(long)]
    pub accept_unseen: bool,
    /// Do not reject pending snapshots before run.
    #[structopt(long)]
    pub keep_pending: bool,
    /// Update all snapshots even if they are still matching.
    #[structopt(long)]
    pub force_update_snapshots: bool,
    /// Delete unreferenced snapshots after the test run.
    #[structopt(long)]
    pub delete_unreferenced_snapshots: bool,
    /// Options passed to cargo test
    // Sets raw to true so that `--` is required
    #[structopt(name = "cargo_options", raw(true))]
    pub cargo_options: Vec<String>,
}

#[derive(StructOpt, Debug)]
#[structopt(rename_all = "kebab-case")]
pub struct PendingSnapshotsCommand {
    #[structopt(flatten)]
    pub target_args: TargetArgs,
    /// Changes the output from human readable to JSON.
    #[structopt(long)]
    pub as_json: bool,
}

#[allow(clippy::too_many_arguments)]
fn query_snapshot(
    workspace_root: &Path,
    term: &Term,
    new: &Snapshot,
    old: Option<&Snapshot>,
    pkg: Option<&Package>,
    line: Option<u32>,
    i: usize,
    n: usize,
    snapshot_file: Option<&Path>,
) -> Result<Operation, Box<dyn Error>> {
    term.clear_screen()?;
    println!(
        "{}{}{}",
        style("Reviewing [").bold(),
        style(format!("{}/{}", i, n)).yellow().bold(),
        style("]:").bold(),
    );

    if let Some(pkg) = pkg {
        println!("Package: {} ({})", style(pkg.name()).dim(), pkg.version());
    } else {
        println!();
    }

    print_snapshot_diff(workspace_root, new, old, snapshot_file, line);

    println!();
    println!(
        "  {} accept   {}",
        style("a").green().bold(),
        style("keep the new snapshot").dim()
    );
    println!(
        "  {} reject   {}",
        style("r").red().bold(),
        style("keep the old snapshot").dim()
    );
    println!(
        "  {} skip     {}",
        style("s").yellow().bold(),
        style("keep both for now").dim()
    );

    loop {
        match term.read_key()? {
            Key::Char('a') | Key::Enter => break Ok(Operation::Accept),
            Key::Char('r') | Key::Escape => break Ok(Operation::Reject),
            Key::Char('s') | Key::Char(' ') => break Ok(Operation::Skip),
            _ => {}
        }
    }
}

fn handle_color(color: &str) -> Result<(), Box<dyn Error>> {
    match color {
        "always" => set_colors_enabled(true),
        "auto" => {}
        "never" => set_colors_enabled(false),
        color => return Err(err_msg(format!("invalid value for --color: {}", color))),
    }
    Ok(())
}

#[derive(Serialize, Debug)]
#[serde(rename_all = "snake_case", tag = "type")]
enum SnapshotKey<'a> {
    NamedSnapshot {
        path: &'a Path,
    },
    InlineSnapshot {
        path: &'a Path,
        line: u32,
        name: Option<&'a str>,
        old_snapshot: Option<&'a str>,
        new_snapshot: &'a str,
        expression: Option<&'a str>,
    },
}

struct LocationInfo<'a> {
    workspace_root: PathBuf,
    packages: Option<Vec<Package>>,
    exts: Vec<&'a str>,
    no_ignore: bool,
}

fn handle_target_args(target_args: &TargetArgs) -> Result<LocationInfo<'_>, Box<dyn Error>> {
    let mut exts: Vec<&str> = target_args.extensions.iter().map(|x| x.as_str()).collect();
    if exts.is_empty() {
        exts.push("snap");
    }

    // if a workspace root is provided we first check if it points to a `Cargo.toml`.  If it
    // does we instead treat it as manifest path.  If both are provided we fail with an error
    // as this would indicate an error.
    let (workspace_root, manifest_path) = match (
        target_args.workspace_root.as_ref(),
        target_args.manifest_path.as_ref(),
    ) {
        (Some(_), Some(_)) => {
            return Err(err_msg(format!(
                "both manifest-path and workspace-root provided."
            )))
        }
        (None, Some(manifest)) => (None, Some(Cow::Borrowed(manifest))),
        (Some(root), manifest_path) => {
            let mut assumed_manifest = root.clone();
            assumed_manifest.push("Cargo.toml");
            if assumed_manifest.metadata().map_or(false, |x| x.is_file()) {
                (None, Some(Cow::Owned(assumed_manifest)))
            } else {
                (Some(root.as_path()), manifest_path.map(Cow::Borrowed))
            }
        }
        (None, None) => (None, None),
    };

    if let Some(workspace_root) = workspace_root {
        Ok(LocationInfo {
            workspace_root: workspace_root.to_owned(),
            packages: None,
            exts,
            no_ignore: target_args.no_ignore,
        })
    } else {
        let metadata = get_package_metadata(manifest_path.as_ref().map(|x| x.as_path()))?;
        let packages = find_packages(&metadata, target_args.all)?;
        Ok(LocationInfo {
            workspace_root: metadata.workspace_root().to_path_buf(),
            packages: Some(packages),
            exts,
            no_ignore: target_args.no_ignore,
        })
    }
}

fn load_snapshot_containers<'a>(
    loc: &'a LocationInfo,
) -> Result<Vec<(SnapshotContainer, Option<&'a Package>)>, Box<dyn Error>> {
    let mut snapshot_containers = vec![];
    match loc.packages {
        Some(ref packages) => {
            for package in packages.iter() {
                for snapshot_container in package.iter_snapshot_containers(&loc.exts, loc.no_ignore)
                {
                    snapshot_containers.push((snapshot_container?, Some(package)));
                }
            }
        }
        None => {
            for snapshot_container in
                find_snapshots(loc.workspace_root.clone(), &loc.exts, loc.no_ignore)
            {
                snapshot_containers.push((snapshot_container?, None));
            }
        }
    }
    Ok(snapshot_containers)
}

fn process_snapshots(cmd: ProcessCommand, op: Option<Operation>) -> Result<(), Box<dyn Error>> {
    let term = Term::stdout();

    let loc = handle_target_args(&cmd.target_args)?;
    let mut snapshot_containers = load_snapshot_containers(&loc)?;

    let snapshot_count = snapshot_containers.iter().map(|x| x.0.len()).sum();

    if snapshot_count == 0 {
        if !cmd.quiet {
            println!("{}: no snapshots to review", style("done").bold());
        }
        return Ok(());
    }

    let mut accepted = vec![];
    let mut rejected = vec![];
    let mut skipped = vec![];
    let mut num = 0;

    for (snapshot_container, package) in snapshot_containers.iter_mut() {
        let target_file = snapshot_container.target_file().to_path_buf();
        let snapshot_file = snapshot_container.snapshot_file().map(|x| x.to_path_buf());
        for snapshot_ref in snapshot_container.iter_snapshots() {
            // if a filter is provided, check if the snapshot reference is included
            if let Some(ref filter) = cmd.snapshot_filter {
                let key = if let Some(line) = snapshot_ref.line {
                    format!("{}:{}", target_file.display(), line)
                } else {
                    format!("{}", target_file.display())
                };
                if !filter.contains(&key) {
                    skipped.push(snapshot_ref.summary());
                    continue;
                }
            }

            num += 1;
            let op = match op {
                Some(op) => op,
                None => query_snapshot(
                    &loc.workspace_root,
                    &term,
                    &snapshot_ref.new,
                    snapshot_ref.old.as_ref(),
                    *package,
                    snapshot_ref.line,
                    num,
                    snapshot_count,
                    snapshot_file.as_ref().map(|x| x.as_path()),
                )?,
            };
            match op {
                Operation::Accept => {
                    snapshot_ref.op = Operation::Accept;
                    accepted.push(snapshot_ref.summary());
                }
                Operation::Reject => {
                    snapshot_ref.op = Operation::Reject;
                    rejected.push(snapshot_ref.summary());
                }
                Operation::Skip => {
                    skipped.push(snapshot_ref.summary());
                }
            }
        }
        snapshot_container.commit()?;
    }

    if op.is_none() {
        term.clear_screen()?;
    }

    if !cmd.quiet {
        println!("{}", style("insta review finished").bold());
        if !accepted.is_empty() {
            println!("{}:", style("accepted").green());
            for item in accepted {
                println!("  {}", item);
            }
        }
        if !rejected.is_empty() {
            println!("{}:", style("rejected").red());
            for item in rejected {
                println!("  {}", item);
            }
        }
        if !skipped.is_empty() {
            println!("{}:", style("skipped").yellow());
            for item in skipped {
                println!("  {}", item);
            }
        }
    }

    Ok(())
}

fn make_deletion_walker(loc: &LocationInfo) -> Walk {
    let roots: HashSet<_> = match loc.packages {
        Some(ref packages) => packages
            .iter()
            .filter_map(|x| x.manifest_path().parent().unwrap().canonicalize().ok())
            .collect(),
        None => {
            let mut hs = HashSet::new();
            hs.insert(loc.workspace_root.clone());
            hs
        }
    };

    WalkBuilder::new(&loc.workspace_root)
        .filter_entry(move |entry| {
            // we only filter down for directories
            if !entry.file_type().map_or(false, |x| x.is_dir()) {
                return true;
            }

            let canonicalized = match entry.path().canonicalize() {
                Ok(path) => path,
                Err(_) => return true,
            };

            // We always want to skip target even if it was not excluded by
            // ignore files.
            if entry.path().file_name() == Some(&OsStr::new("target"))
                && roots.contains(canonicalized.parent().unwrap())
            {
                return false;
            }

            // do not enter crates which are not in the list of known roots
            // of the workspace.
            if !roots.contains(&canonicalized)
                && entry
                    .path()
                    .join("Cargo.toml")
                    .metadata()
                    .map_or(false, |x| x.is_file())
            {
                return false;
            }

            true
        })
        .build()
}

fn test_run(mut cmd: TestCommand, color: &str) -> Result<(), Box<dyn Error>> {
    let mut proc = process::Command::new(get_cargo());
    proc.arg("test");

    // when unreferenced snapshots should be deleted we need to instruct
    // insta to dump referenced snapshots somewhere.
    let snapshot_ref_file = if cmd.delete_unreferenced_snapshots {
        let snapshot_ref_file = env::temp_dir().join(Uuid::new_v4().to_string());
        proc.env("INSTA_SNAPSHOT_REFERENCES_FILE", &snapshot_ref_file);
        Some(snapshot_ref_file)
    } else {
        None
    };

    // if INSTA_UPDATE is set as environment variable we're using it to
    // override some arguments.  The logic is is quite weird because we
    // don't support all of the same values and we also want to override
    // it through the command line switches.
    match env::var("INSTA_UPDATE").ok().as_deref() {
        Some("auto") | Some("new") => {}
        Some("always") => {
            if !cmd.accept && !cmd.accept_unseen && !cmd.review {
                cmd.review = false;
                cmd.accept = true;
            }
        }
        Some("unseen") => {
            if !cmd.accept {
                cmd.accept_unseen = true;
                cmd.review = true;
                cmd.accept = false;
            }
        }
        // silently ignored always
        None | Some("") | Some("no") => {}
        _ => {
            return Err(err_msg("invalid value for INSTA_UPDATE"));
        }
    }

    if cmd.target_args.all {
        proc.arg("--all");
    }
    if let Some(ref pkg) = cmd.package {
        proc.arg("--package");
        proc.arg(pkg);
    }
    if let Some(ref manifest_path) = cmd.target_args.manifest_path {
        proc.arg("--manifest-path");
        proc.arg(manifest_path);
    }
    if !cmd.fail_fast {
        proc.arg("--no-fail-fast");
    }
    if !cmd.no_force_pass {
        proc.env("INSTA_FORCE_PASS", "1");
    }
    proc.env(
        "INSTA_UPDATE",
        if cmd.accept_unseen { "unseen" } else { "new" },
    );
    if cmd.force_update_snapshots {
        proc.env("INSTA_FORCE_UPDATE_SNAPSHOTS", "1");
    }
    if cmd.release {
        proc.arg("--release");
    }
    if let Some(n) = cmd.jobs {
        proc.arg(format!("--jobs={}", n));
    }
    if let Some(ref features) = cmd.features {
        proc.arg("--features");
        proc.arg(features);
    }
    if cmd.all_features {
        proc.arg("--all-features");
    }
    if cmd.no_default_features {
        proc.arg("--no-default-features");
    }
    proc.arg("--color");
    proc.arg(color);
    proc.args(cmd.cargo_options);
    proc.arg("--");
    proc.arg("-q");

    if !cmd.keep_pending {
        process_snapshots(
            ProcessCommand {
                target_args: cmd.target_args.clone(),
                snapshot_filter: None,
                quiet: true,
            },
            Some(Operation::Reject),
        )?;
    }

    let status = proc.status()?;

    if !status.success() {
        if cmd.review {
            eprintln!(
                "{} non snapshot tests failed, skipping review",
                style("warning:").bold().yellow()
            );
        } else if cmd.accept {
            eprintln!(
                "{} non snapshot tests failed, not accepted changes",
                style("warning:").bold().yellow()
            );
        }
        return Err(QuietExit(1).into());
    }

    // delete unreferenced snapshots if we were instructed to do so
    if let Some(ref path) = snapshot_ref_file {
        let mut files = HashSet::new();
        for line in fs::read_to_string(path).unwrap().lines() {
            if let Ok(path) = fs::canonicalize(line) {
                files.insert(path);
            }
        }

        if let Ok(loc) = handle_target_args(&cmd.target_args) {
            let mut deleted_any = false;
            for entry in make_deletion_walker(&loc) {
                let rel_path = match entry {
                    Ok(ref entry) => entry.path(),
                    _ => continue,
                };
                if !rel_path.is_file()
                    || !rel_path
                        .file_name()
                        .map_or(false, |x| x.to_str().unwrap_or("").ends_with(".snap"))
                {
                    continue;
                }

                if let Ok(path) = fs::canonicalize(rel_path) {
                    if !files.contains(&path) {
                        if !deleted_any {
                            eprintln!("{}: deleted unreferenced snapshots:", style("info").bold());
                            deleted_any = true;
                        }
                        eprintln!("  {}", rel_path.display());
                        fs::remove_file(path).ok();
                    }
                }
            }
            if !deleted_any {
                eprintln!("{}: no unreferenced snapshots found", style("info").bold());
            }
        }

        fs::remove_file(&path).ok();
    }

    if cmd.review || cmd.accept {
        process_snapshots(
            ProcessCommand {
                target_args: cmd.target_args.clone(),
                snapshot_filter: None,
                quiet: false,
            },
            if cmd.accept {
                Some(Operation::Accept)
            } else {
                None
            },
        )?
    } else {
        let loc = handle_target_args(&cmd.target_args)?;
        let snapshot_containers = load_snapshot_containers(&loc)?;
        let snapshot_count = snapshot_containers.iter().map(|x| x.0.len()).sum::<usize>();
        if snapshot_count > 0 {
            eprintln!(
                "{}: {} snapshot{} to review",
                style("info").bold(),
                style(snapshot_count).yellow(),
                if snapshot_count != 1 { "s" } else { "" }
            );
            eprintln!("use `cargo insta review` to review snapshots");
        } else {
            println!("{}: no snapshots to review", style("info").bold());
        }
    }

    Ok(())
}

fn pending_snapshots_cmd(cmd: PendingSnapshotsCommand) -> Result<(), Box<dyn Error>> {
    let loc = handle_target_args(&cmd.target_args)?;
    let mut snapshot_containers = load_snapshot_containers(&loc)?;

    for (snapshot_container, _package) in snapshot_containers.iter_mut() {
        let target_file = snapshot_container.target_file().to_path_buf();
        let is_inline = snapshot_container.snapshot_file().is_none();
        for snapshot_ref in snapshot_container.iter_snapshots() {
            if cmd.as_json {
                let info = if is_inline {
                    SnapshotKey::InlineSnapshot {
                        path: &target_file,
                        line: snapshot_ref.line.unwrap(),
                        name: snapshot_ref.new.snapshot_name(),
                        old_snapshot: snapshot_ref.old.as_ref().map(|x| x.contents_str()),
                        new_snapshot: snapshot_ref.new.contents_str(),
                        expression: snapshot_ref.new.metadata().expression(),
                    }
                } else {
                    SnapshotKey::NamedSnapshot { path: &target_file }
                };
                println!("{}", serde_json::to_string(&info).unwrap());
            } else {
                if is_inline {
                    println!("{}:{}", target_file.display(), snapshot_ref.line.unwrap());
                } else {
                    println!("{}", target_file.display());
                }
            }
        }
    }

    Ok(())
}

pub fn run() -> Result<(), Box<dyn Error>> {
    // chop off cargo
    let mut args: Vec<_> = env::args_os().collect();
    if env::var("CARGO").is_ok() && args.get(1).and_then(|x| x.to_str()) == Some("insta") {
        args.remove(1);
    }

    let opts = Opts::from_iter(args);

    let color = opts.color.as_ref().map(|x| x.as_str()).unwrap_or("auto");
    handle_color(color)?;
    match opts.command {
        Command::Review(cmd) => process_snapshots(cmd, None),
        Command::Accept(cmd) => process_snapshots(cmd, Some(Operation::Accept)),
        Command::Reject(cmd) => process_snapshots(cmd, Some(Operation::Reject)),
        Command::Test(cmd) => test_run(cmd, color),
        Command::PendingSnapshots(cmd) => pending_snapshots_cmd(cmd),
    }
}
