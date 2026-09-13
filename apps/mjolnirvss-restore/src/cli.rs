//! The recovery command line.
//!
//! Kept separate from the backup tool's command line on purpose: this binary
//! must not link the shadow copy code, because `vssapi.dll` is not present in a
//! base Windows PE image and importing it would stop the recovery application
//! from starting at the one moment it is needed.
//!
//! The graphical wizard is the normal way to use this. These commands exist so
//! a restore can be rehearsed, scripted and tested.

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use mjolnir_core::cancel::CancelToken;
use mjolnir_core::error::{Error, Result};
use mjolnir_core::exit::ExitCode;
use mjolnir_core::progress::{Progress, SilentProgress, StderrProgress};
use mjolnir_image::BackupSet;

/// Recovery from a MjolnirVSS backup.
#[derive(Debug, Parser)]
#[command(
    name = "MjolnirVSS.Restore",
    version,
    about = "Restore a Windows system disk from a MjolnirVSS backup",
    long_about = "Run with no arguments to open the recovery wizard.\n\
                  The commands below exist for rehearsing and scripting a restore."
)]
pub struct Cli {
    /// What to do.
    #[command(subcommand)]
    pub command: Command,

    /// Print machine readable output instead of prose.
    #[arg(long, global = true)]
    pub json: bool,

    /// Read the password for an encrypted backup from this file.
    ///
    /// Without it, an encrypted backup is asked about at the console. Never an
    /// argument: arguments are visible to anything that can list processes.
    #[arg(long, global = true, value_name = "FILE")]
    pub password_file: Option<PathBuf>,
}

/// The available commands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Show what a backup contains and whether it can be restored.
    InspectBackup {
        /// The backup folder.
        path: PathBuf,
    },

    /// Search the attached drives for backups.
    FindBackups,

    /// List the disks on this computer.
    ListDisks,

    /// Repair the boot configuration of an already restored disk.
    ///
    /// For a disk that holds a restored Windows which will not start. It
    /// rewrites the boot files on that disk's EFI system partition and changes
    /// nothing else: no partition is touched and no file of yours is read.
    RepairBoot {
        /// The disk number, as `list-disks` reports it.
        #[arg(long)]
        disk: u32,

        /// Report what would be done and change nothing.
        #[arg(long)]
        dry_run: bool,
    },

    /// Restore a backup onto a disk.
    ///
    /// Without `--dry-run` this erases the target disk.
    Restore {
        /// The backup folder.
        path: PathBuf,

        /// The disk to restore onto, for example `\\.\PhysicalDrive1`.
        #[arg(long)]
        target: String,

        /// Check everything and report what would happen, writing nothing.
        #[arg(long)]
        dry_run: bool,

        /// The erase confirmation, for scripted use.
        ///
        /// Must be exactly the phrase `inspect-backup` and the wizard show for
        /// the chosen disk, such as `ERASE S4NV7X0T123`. Without it an
        /// interactive restore asks; a non interactive one refuses.
        #[arg(long, value_name = "PHRASE")]
        confirm: Option<String>,
    },
}

/// Parses `args` and runs the command.
pub fn run_from_args<I, T>(args: I) -> ExitCode
where
    I: IntoIterator<Item = T>,
    T: Into<std::ffi::OsString> + Clone,
{
    match Cli::try_parse_from(args) {
        Ok(cli) => run(cli),
        Err(e) => {
            let _ = e.print();
            if e.use_stderr() {
                ExitCode::Usage
            } else {
                ExitCode::Success
            }
        }
    }
}

fn run(cli: Cli) -> ExitCode {
    let cancel = CancelToken::new();
    let mut progress: Box<dyn Progress> = if cli.json {
        Box::new(SilentProgress)
    } else {
        Box::new(StderrProgress::new())
    };

    let result = match &cli.command {
        Command::InspectBackup { path } => cmd_inspect_backup(&cli, path.clone()),
        Command::FindBackups => cmd_find_backups(&cli),
        Command::ListDisks => cmd_list_disks(&cli),
        Command::RepairBoot { disk, dry_run } => cmd_repair_boot(&cli, *disk, *dry_run),
        Command::Restore {
            path,
            target,
            dry_run,
            confirm,
        } => cmd_restore(
            &cli,
            path.clone(),
            target.clone(),
            *dry_run,
            confirm.clone(),
            progress.as_mut(),
            &cancel,
        ),
    };

    match result {
        Ok(code) => code,
        Err(e) => {
            eprintln!();
            eprintln!("MjolnirVSS could not finish.");
            eprintln!();
            eprintln!("  What happened: {}", e.what());
            eprintln!("  Why it matters: {}", e.why());
            eprintln!("  What to do next: {}", e.next_step());
            eprintln!();
            eprintln!("  (exit code {} - {})", e.exit().code(), e.exit().name());
            e.exit()
        }
    }
}

/// Supplies the password when the backup has one.
///
/// A backup that is not encrypted is never asked about, so nothing changes for
/// anybody who does not use encryption. An encrypted one cannot be restored
/// without this, which is the whole reason the recovery application needs it:
/// a backup that can be made but not recovered is worse than none.
fn unlock_if_needed(cli: &Cli, set: &mut BackupSet) -> Result<()> {
    if !set.is_encrypted() {
        return Ok(());
    }
    let source = mjolnir_crypto::password::PasswordSource {
        file: cli.password_file.clone(),
    };
    let password = source.read("Password for this backup: ")?;
    set.unlock(&password)
}

fn cmd_inspect_backup(cli: &Cli, path: PathBuf) -> Result<ExitCode> {
    let set = BackupSet::open_unchecked(&path)?;
    let m = set.manifest();
    let layout = set.disk_layout();

    if cli.json {
        let value = serde_json::json!({
            "path": path.to_string_lossy(),
            "name": m.backup.name.as_str(),
            "created_utc": m.backup.created_utc,
            "computer": m.source.computer_name,
            "scope": m.backup.scope,
            "restorable": set.is_restorable(),
            "required_restore_bytes": m.required_restore_bytes,
            "captured_bytes": m.stats.captured_bytes,
            "stored_bytes": m.stats.stored_bytes,
            "problems": set.issues().iter().map(|i| format!("{i}")).collect::<Vec<_>>(),
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&value).unwrap_or_default()
        );
        return Ok(if set.is_restorable() {
            ExitCode::Success
        } else {
            ExitCode::CorruptBackup
        });
    }

    println!("Backup:    {}", m.backup.name);
    println!("Taken:     {}", m.backup.created_utc);
    println!("Computer:  {}", m.source.computer_name);
    if let Some(product) = &m.source.windows.product_name {
        println!("Windows:   {product}");
    }
    println!("Scope:     {}", m.backup.scope);
    println!(
        "Holds:     {} read, {} stored",
        mjolnir_core::progress::format_bytes(m.stats.captured_bytes),
        mjolnir_core::progress::format_bytes(m.stats.stored_bytes)
    );
    println!(
        "Needs a target disk of at least {}",
        mjolnir_core::progress::format_bytes(m.required_restore_bytes)
    );
    println!();

    for disk in &layout.disks {
        println!(
            "Disk {} - {} - {}",
            disk.disk_number,
            disk.model.as_deref().unwrap_or("unknown model"),
            mjolnir_core::progress::format_bytes(disk.size_bytes)
        );
        for p in &disk.partitions {
            println!(
                "  Partition {:<2} {:>12}  at {:>14}  {}",
                p.number,
                mjolnir_core::progress::format_bytes(p.length),
                p.starting_offset,
                p.role.describe()
            );
        }
    }
    println!();

    if set.is_restorable() {
        println!("This backup can be restored.");
        Ok(ExitCode::Success)
    } else {
        println!("This backup CANNOT be restored:");
        if set.completion().is_none() {
            println!("  It has no completion marker, so the backup was interrupted.");
        }
        for issue in set.issues().iter().filter(|i| i.is_error()).take(20) {
            println!("  {issue}");
        }
        Ok(ExitCode::CorruptBackup)
    }
}

fn cmd_find_backups(cli: &Cli) -> Result<ExitCode> {
    let found = crate::discover::search_all_drives();

    if cli.json {
        let value: Vec<_> = found
            .iter()
            .map(|f| {
                serde_json::json!({
                    "path": f.path.to_string_lossy(),
                    "name": f.set.manifest().backup.name.as_str(),
                    "restorable": f.set.is_restorable(),
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&value).unwrap_or_default()
        );
        return Ok(ExitCode::Success);
    }

    if found.is_empty() {
        println!("No MjolnirVSS backups were found on the attached drives.");
        println!();
        println!("Check that the drive holding the backup is plugged in. If it is, run");
        println!("`MjolnirVSS.Restore.exe inspect-backup <folder>` with the folder directly.");
        return Ok(ExitCode::Success);
    }

    println!("Backups found:");
    for f in &found {
        println!("  {}", f.path.display());
        println!("    {}", f.describe());
    }
    Ok(ExitCode::Success)
}

#[cfg(windows)]
fn cmd_repair_boot(cli: &Cli, disk: u32, dry_run: bool) -> Result<ExitCode> {
    if dry_run {
        let (report, decision) = mjolnir_restore::inspect_disk(disk)?;
        if cli.json {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "disk": disk,
                    "dry_run": true,
                    "before": report.before,
                    "would_change": decision.writes_anything(),
                    "decision": format!("{decision:?}"),
                }))
                .unwrap_or_default()
            );
        } else {
            println!("Boot configuration of disk {disk}:");
            for line in &report.before {
                println!("  {line}");
            }
            println!();
            if decision.writes_anything() {
                println!("A repair would rewrite the boot files on this disk.");
            } else {
                println!("Nothing would be changed.");
            }
            println!("Nothing was changed: this was a dry run.");
        }
        return Ok(ExitCode::Success);
    }

    let report = mjolnir_restore::repair_disk(disk)?;
    if cli.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "disk": disk,
                "succeeded": report.succeeded,
                "changed_anything": report.changed_anything(),
                "before": report.before,
                "changed": report.changed,
                "notes": report.notes,
            }))
            .unwrap_or_default()
        );
    } else {
        println!("{}", report.describe());
    }
    if report.succeeded {
        Ok(ExitCode::Success)
    } else {
        Ok(ExitCode::Failure)
    }
}

#[cfg(not(windows))]
fn cmd_repair_boot(_cli: &Cli, _disk: u32, _dry_run: bool) -> Result<ExitCode> {
    Err(not_windows())
}

#[cfg(windows)]
fn cmd_list_disks(cli: &Cli) -> Result<ExitCode> {
    let targets = mjolnir_restore::enumerate_targets(&[]);

    if cli.json {
        let value: Vec<_> = targets
            .iter()
            .map(|t| {
                serde_json::json!({
                    "number": t.number,
                    "device_path": t.device_path,
                    "model": t.model,
                    "serial": t.serial,
                    "size_bytes": t.size_bytes,
                    "logical_sector_size": t.logical_sector_size,
                    "bus": t.bus,
                    "erase_phrase": t.erase_phrase(),
                    "partitions": t.existing_partitions,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&value).unwrap_or_default()
        );
        return Ok(ExitCode::Success);
    }

    println!("Disks on this computer:");
    for t in &targets {
        println!();
        println!("  {}", t.describe());
        println!(
            "    Path: {}   Sectors: {} bytes",
            t.device_path, t.logical_sector_size
        );
        if t.existing_partitions.is_empty() {
            println!("    No partitions (blank disk)");
        } else {
            for p in &t.existing_partitions {
                println!("    {p}");
            }
        }
        println!(
            "    To erase this disk you would type: {}",
            t.erase_phrase()
        );
    }
    Ok(ExitCode::Success)
}

#[cfg(not(windows))]
fn cmd_list_disks(_cli: &Cli) -> Result<ExitCode> {
    Err(not_windows())
}

#[cfg(windows)]
#[allow(clippy::too_many_arguments)]
fn cmd_restore(
    cli: &Cli,
    path: PathBuf,
    target_path: String,
    dry_run: bool,
    confirm: Option<String>,
    progress: &mut dyn Progress,
    cancel: &CancelToken,
) -> Result<ExitCode> {
    let mut set = BackupSet::open(&path)?;
    unlock_if_needed(cli, &mut set)?;
    let set = set;
    let backup_disks = crate::discover::disks_holding(&path);

    let targets = mjolnir_restore::enumerate_targets(&backup_disks);
    let target = targets
        .iter()
        .find(|t| t.device_path.eq_ignore_ascii_case(&target_path))
        .ok_or_else(|| {
            Error::new(
                ExitCode::Usage,
                format!("there is no disk at {target_path}"),
                "the disk was not found among the ones attached to this computer",
                "run `MjolnirVSS.Restore.exe list-disks` to see what is available",
            )
        })?
        .clone();

    let plan = mjolnir_restore::plan(&set, &target)?;

    if !cli.json {
        println!();
        println!("About to restore:");
        for line in plan.summary_lines() {
            println!("  {line}");
        }
        println!();
        println!("Onto:");
        println!("  {}", target.describe());
        if target.existing_partitions.is_empty() {
            println!("  No partitions (blank disk)");
        } else {
            println!("  Everything below will be ERASED:");
            for p in &target.existing_partitions {
                println!("    {p}");
            }
        }
        for w in &plan.warnings {
            println!();
            println!("  Note: {w}");
        }
        println!();
    }

    if dry_run {
        mjolnir_restore::check_chunks_present(&set, progress, cancel)?;
        println!("Dry run finished. Nothing was written.");
        println!(
            "  {} would be written across {} partitions.",
            mjolnir_core::progress::format_bytes(plan.total_bytes),
            plan.writes.len()
        );
        return Ok(ExitCode::Success);
    }

    // A real restore needs the phrase. Reading it from the terminal is only
    // offered when there is a terminal to read from.
    let typed = match confirm {
        Some(phrase) => phrase,
        None => {
            use std::io::{BufRead, IsTerminal, Write};
            if !std::io::stdin().is_terminal() {
                return Err(Error::unsafe_target(
                    "the erase confirmation was not given",
                    "this restore would erase a disk, and it was started without a terminal to ask on",
                    format!("pass --confirm \"{}\" if you are certain", target.erase_phrase()),
                ));
            }
            print!("Type {} to erase this disk: ", target.erase_phrase());
            let _ = std::io::stdout().flush();
            let mut line = String::new();
            std::io::stdin()
                .lock()
                .read_line(&mut line)
                .map_err(|e| Error::io("the terminal", e))?;
            line
        }
    };

    let confirmation = mjolnir_restore::EraseConfirmation::check(&target, &typed)?;
    let mut disk = mjolnir_restore::WritableDisk::open(&target)?;

    let mut outcome = mjolnir_restore::restore(
        &set,
        &plan,
        &target,
        &confirmation,
        &mut disk,
        progress,
        cancel,
    )?;
    disk.refresh_partition_table()?;

    // The disk has to be closed before Windows will show its new partitions,
    // and the boot repair needs to see them.
    drop(disk);

    progress.begin(mjolnir_restore::stages::REPAIRING_BOOT, None);
    match mjolnir_restore::repair_disk(target.number) {
        Ok(report) => outcome.boot_repair = Some(report),
        Err(e) => {
            // A restore that worked is not undone by a boot repair that did
            // not. What happened is reported and the operator decides.
            eprintln!();
            eprintln!("The restore finished, but the boot configuration could not be checked:");
            eprintln!("  {}", e.what());
            eprintln!("  {}", e.why());
        }
    }
    progress.end();

    println!();
    println!("Restore completed.");
    println!(
        "  Written: {}",
        mjolnir_core::progress::format_bytes(outcome.written_bytes)
    );
    println!("  Partitions restored: {}", outcome.partitions_restored);
    if outcome.unallocated_bytes > 0 {
        println!(
            "  Left unallocated: {}",
            mjolnir_core::progress::format_bytes(outcome.unallocated_bytes)
        );
    }
    if let Some(report) = &outcome.boot_repair {
        println!();
        print!("{}", report.describe());
    }

    println!();
    println!("  Restart the computer and remove the recovery media.");
    println!("  If Windows does not start, boot the recovery media again and run");
    println!("  Startup Repair; see docs/bare-metal-restore.md.");

    Ok(ExitCode::Success)
}

#[cfg(not(windows))]
#[allow(clippy::too_many_arguments)]
fn cmd_restore(
    _cli: &Cli,
    _path: PathBuf,
    _target_path: String,
    _dry_run: bool,
    _confirm: Option<String>,
    _progress: &mut dyn Progress,
    _cancel: &CancelToken,
) -> Result<ExitCode> {
    Err(not_windows())
}

#[allow(dead_code)]
fn not_windows() -> Error {
    Error::unsupported(
        "MjolnirVSS only runs on Windows",
        "it restores Windows disks using the Windows storage interfaces",
        "run the recovery application from Windows installation or recovery media",
    )
}

#[cfg(test)]
mod tests {

    /// The command that rewrites how a computer starts has to be reachable on
    /// its own, for a disk that was restored earlier and will not boot.
    #[test]
    fn repair_boot_parses() {
        let cli = Cli::try_parse_from(["MjolnirVSS.Restore.exe", "repair-boot", "--disk", "0"])
            .expect("repair-boot should parse");
        match cli.command {
            Command::RepairBoot { disk, dry_run } => {
                assert_eq!(disk, 0);
                assert!(!dry_run, "it writes unless a dry run is asked for");
            }
            other => panic!("wrong command: {other:?}"),
        }
    }

    /// And it has to be possible to look without touching.
    #[test]
    fn repair_boot_has_a_dry_run() {
        let cli = Cli::try_parse_from([
            "MjolnirVSS.Restore.exe",
            "repair-boot",
            "--disk",
            "2",
            "--dry-run",
        ])
        .expect("a dry run should parse");
        match cli.command {
            Command::RepairBoot { disk, dry_run } => {
                assert_eq!(disk, 2);
                assert!(dry_run);
            }
            other => panic!("wrong command: {other:?}"),
        }
    }

    /// A disk number is not optional: there is no sensible default for which
    /// disk to rewrite the boot configuration of.
    #[test]
    fn repair_boot_requires_a_disk() {
        assert!(Cli::try_parse_from(["MjolnirVSS.Restore.exe", "repair-boot"]).is_err());
    }
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn the_command_line_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn restore_requires_a_target() {
        assert!(Cli::try_parse_from(["r", "restore", "E:\\backup"]).is_err());
    }

    #[test]
    fn a_dry_run_parses() {
        let cli = Cli::try_parse_from([
            "r",
            "restore",
            "E:\\backup",
            "--target",
            "\\\\.\\PhysicalDrive1",
            "--dry-run",
        ])
        .unwrap();
        match cli.command {
            Command::Restore {
                target,
                dry_run,
                confirm,
                ..
            } => {
                assert_eq!(target, "\\\\.\\PhysicalDrive1");
                assert!(dry_run);
                assert!(confirm.is_none());
            }
            other => panic!("wrong command: {other:?}"),
        }
    }

    #[test]
    fn the_confirmation_phrase_can_be_passed_for_scripting() {
        let cli = Cli::try_parse_from([
            "r",
            "restore",
            "E:\\backup",
            "--target",
            "\\\\.\\PhysicalDrive1",
            "--confirm",
            "ERASE ABC123",
        ])
        .unwrap();
        match cli.command {
            Command::Restore { confirm, .. } => {
                assert_eq!(confirm.as_deref(), Some("ERASE ABC123"));
            }
            other => panic!("wrong command: {other:?}"),
        }
    }

    #[test]
    fn inspect_backup_parses() {
        let cli = Cli::try_parse_from(["r", "inspect-backup", "E:\\backup"]).unwrap();
        assert!(matches!(cli.command, Command::InspectBackup { .. }));
    }
}
