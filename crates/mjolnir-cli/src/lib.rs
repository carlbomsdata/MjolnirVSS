//! The command line, used for automated testing and diagnostics.
//!
//! The graphical interface is the product. This exists so that the engine can
//! be driven from a script, from a test and from a scheduled task, and so that
//! a failure can be reproduced without describing which buttons were pressed.
//! It shares every line of the engine with the window, so a bug found here is
//! the same bug the window would have had.
//!
//! Copyright (C) the MjolnirVSS contributors.
//! Licensed under the GNU General Public License, version 3 or later.

#![warn(missing_docs)]

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use mjolnir_core::cancel::CancelToken;
use mjolnir_core::error::{Error, Result};
use mjolnir_core::exit::ExitCode;
use mjolnir_core::ids::BackupName;
use mjolnir_core::progress::{Progress, SilentProgress, StderrProgress};
use mjolnir_core::timestamp::UtcTimestamp;

/// Backup and diagnostics from a terminal.
#[derive(Debug, Parser)]
#[command(
    name = "MjolnirVSS",
    version,
    about = "Portable bare metal backup for Windows",
    long_about = "MjolnirVSS backs up a whole Windows system disk while Windows is running.\n\
                  Run it with no arguments to open the window; the commands below exist for\n\
                  testing, diagnostics and scheduled backups."
)]
pub struct Cli {
    /// What to do.
    #[command(subcommand)]
    pub command: Command,

    /// Print machine readable output instead of prose.
    #[arg(long, global = true)]
    pub json: bool,

    /// Do not print progress.
    #[arg(long, global = true)]
    pub quiet: bool,
}

/// The available commands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Show what MjolnirVSS found on this computer, and what it would back up.
    Inspect,

    /// Back up the Windows system disk.
    Backup {
        /// Folder to write the backup into, normally on an external drive.
        #[arg(long)]
        destination: PathBuf,

        /// Name of the backup folder. Defaults to COMPUTERNAME_YYYY-MM-DD_HHMM.
        #[arg(long)]
        name: Option<String>,

        /// Capture only the first N bytes of each partition.
        ///
        /// For testing the pipeline end to end in seconds. The result is marked
        /// as a preview and can never be restored.
        #[arg(long, value_name = "BYTES")]
        preview: Option<u64>,
    },

    /// Check a backup: every chunk is decompressed and its digest compared.
    Verify {
        /// The backup folder.
        path: PathBuf,

        /// Only check that the files are present and the right size.
        #[arg(long)]
        quick: bool,
    },

    /// List the backups in a folder.
    List {
        /// Folder holding backup folders.
        path: PathBuf,
    },

    /// Show the shadow copies on this computer, and optionally clean up.
    CleanupSnapshots {
        /// Only report; never remove anything.
        #[arg(long, default_value_t = true)]
        owned_only: bool,
    },
}

/// Parses `args` and runs the command, returning the process exit code.
///
/// Keeps `clap` inside this crate, so the application binaries depend on the
/// command line only through this one function.
pub fn run_from_args<I, T>(args: I) -> ExitCode
where
    I: IntoIterator<Item = T>,
    T: Into<std::ffi::OsString> + Clone,
{
    match Cli::try_parse_from(args) {
        Ok(cli) => run(cli),
        Err(e) => {
            // clap has already chosen whether this is help, a version, or a
            // mistake, and printed it to the right stream.
            let _ = e.print();
            if e.use_stderr() {
                ExitCode::Usage
            } else {
                ExitCode::Success
            }
        }
    }
}

/// Runs a parsed command line and returns the process exit code.
pub fn run(cli: Cli) -> ExitCode {
    let cancel = CancelToken::new();
    install_cancel_handler(&cancel);

    let mut progress: Box<dyn Progress> = if cli.quiet || cli.json {
        Box::new(SilentProgress)
    } else {
        Box::new(StderrProgress::new())
    };

    let result = match &cli.command {
        Command::Inspect => cmd_inspect(&cli),
        Command::Backup {
            destination,
            name,
            preview,
        } => cmd_backup(
            &cli,
            destination.clone(),
            name.clone(),
            *preview,
            progress.as_mut(),
            &cancel,
        ),
        Command::Verify { path, quick } => {
            cmd_verify(&cli, path.clone(), *quick, progress.as_mut(), &cancel)
        }
        Command::List { path } => cmd_list(&cli, path.clone()),
        Command::CleanupSnapshots { owned_only } => cmd_cleanup(&cli, *owned_only),
    };

    match result {
        Ok(code) => code,
        Err(e) => {
            report_error(&e);
            e.exit()
        }
    }
}

/// Prints an error the way the product promises: what, why, and what next.
pub fn report_error(e: &Error) {
    eprintln!();
    eprintln!("MjolnirVSS could not finish.");
    eprintln!();
    eprintln!("  What happened: {}", e.what());
    eprintln!("  Why it matters: {}", e.why());
    eprintln!("  What to do next: {}", e.next_step());
    eprintln!();
    eprintln!("  (exit code {} - {})", e.exit().code(), e.exit().name());
}

fn install_cancel_handler(cancel: &CancelToken) {
    // Ctrl+C sets the flag instead of killing the process, so the shadow copy
    // is released and no half written chunk is left behind.
    let cancel = cancel.clone();
    let _ = std::thread::Builder::new()
        .name("mjolnir-cancel".to_owned())
        .spawn(move || {
            // A real console handler is installed by the application crate; this
            // keeps the token alive for the duration of the run.
            let _ = &cancel;
        });
}

#[cfg(windows)]
fn cmd_inspect(cli: &Cli) -> Result<ExitCode> {
    let system = mjolnir_storage::system::describe_system()?;
    let disks = mjolnir_storage::disks::enumerate_disks();
    let volumes = mjolnir_storage::volumes::enumerate_volumes()?;

    if cli.json {
        let value = serde_json::json!({
            "computer_name": system.computer_name,
            "machine_id": system.machine_id.as_str(),
            "firmware": format!("{:?}", system.firmware),
            "windows": {
                "product_name": system.windows.product_name,
                "build": system.windows.build,
                "edition": system.windows.edition,
            },
            "system_disk": system.system_disk_number,
            "disks": disks.iter().map(|d| serde_json::json!({
                "number": d.number,
                "model": d.model,
                "serial": d.serial,
                "size_bytes": d.size_bytes,
                "logical_sector_size": d.logical_sector_size,
                "physical_sector_size": d.physical_sector_size,
                "bus_type": d.bus_type.describe(),
                "partition_style": format!("{:?}", d.partition_style),
                "disk_guid": d.disk_guid,
                "partitions": d.partitions.iter().map(|p| serde_json::json!({
                    "number": p.number,
                    "starting_offset": p.starting_offset,
                    "length": p.length,
                    "type_guid": p.type_guid,
                    "name": p.name,
                })).collect::<Vec<_>>(),
            })).collect::<Vec<_>>(),
            "volumes": volumes.iter().map(|v| serde_json::json!({
                "guid_path": v.guid_path,
                "drive_letter": v.drive_letter(),
                "label": v.label,
                "filesystem": v.filesystem,
                "total_bytes": v.total_bytes,
                "free_bytes": v.free_bytes,
                "disk_number": v.disk_number(),
            })).collect::<Vec<_>>(),
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&value).unwrap_or_default()
        );
        return Ok(ExitCode::Success);
    }

    println!("Computer:     {}", system.computer_name);
    println!("Identifier:   {}", system.machine_id);
    if let Some(name) = &system.windows.product_name {
        let build = system.windows.build.as_deref().unwrap_or("?");
        println!("Windows:      {name} (build {build})");
    }
    println!("Firmware:     {:?}", system.firmware);
    println!("System disk:  {}", system.system_disk_number);
    println!();

    println!("Disks");
    for disk in &disks {
        let marker = if disk.number == system.system_disk_number {
            " <- Windows is installed here"
        } else {
            ""
        };
        println!("  {}{marker}", disk.describe());
        println!(
            "    sectors {} logical / {} physical, {:?}",
            disk.logical_sector_size, disk.physical_sector_size, disk.partition_style
        );
        for p in &disk.partitions {
            // The type GUID alone cannot tell a Windows partition from any
            // other basic data partition, so the volume Windows is running
            // from is matched by offset the same way planning does it.
            let is_windows = disk.number == system.system_disk_number
                && system.windows_volume.extents.iter().any(|e| {
                    e.disk_number == disk.number && e.starting_offset == p.starting_offset
                });
            let role = if is_windows {
                mjolnir_image::disk_layout::PartitionRole::Windows
            } else {
                mjolnir_image::disk_layout::PartitionRole::from_type_guid(&p.type_guid)
            };
            println!(
                "    partition {:<2} {:>12}  at {:>14}  {}",
                p.number,
                mjolnir_core::progress::format_bytes(p.length),
                p.starting_offset,
                role.describe()
            );
        }
    }
    println!();

    println!("Volumes");
    for v in &volumes {
        let disk = v
            .disk_number()
            .map(|d| format!("disk {d}"))
            .unwrap_or_else(|| format!("{} regions", v.extents.len()));
        println!("  {:<40} {disk}", v.describe());
    }
    println!();

    // What a backup would actually do, including any refusal.
    let request = mjolnir_backup::BackupRequest {
        destination: PathBuf::from("."),
        name: default_backup_name(&system.computer_name),
        scope: mjolnir_backup::BackupScope::SystemDisk,
        limit: mjolnir_backup::CaptureLimit::Everything,
    };
    match mjolnir_backup::plan(&request) {
        Ok(plan) => {
            println!("A backup of the system disk would capture:");
            for line in plan.summary_lines().iter().skip(2) {
                println!("{line}");
            }
            println!(
                "  Total to read: {}",
                mjolnir_core::progress::format_bytes(plan.source_bytes)
            );
            for w in &plan.warnings {
                println!("  Note: {w}");
            }
            Ok(ExitCode::Success)
        }
        Err(e) => {
            println!("This computer cannot be backed up by MjolnirVSS yet:");
            println!("  {}", e.what());
            println!("  {}", e.why());
            println!("  {}", e.next_step());
            Ok(e.exit())
        }
    }
}

#[cfg(not(windows))]
fn cmd_inspect(_cli: &Cli) -> Result<ExitCode> {
    Err(not_windows())
}

#[cfg(windows)]
fn cmd_backup(
    cli: &Cli,
    destination: PathBuf,
    name: Option<String>,
    preview: Option<u64>,
    progress: &mut dyn Progress,
    cancel: &CancelToken,
) -> Result<ExitCode> {
    let system = mjolnir_storage::system::describe_system()?;
    let name = match name {
        Some(raw) => BackupName::new(raw).map_err(|e| {
            Error::new(
                ExitCode::Usage,
                "the backup name cannot be used as a folder name",
                format!("{e}"),
                "choose a name made of letters, digits, dashes and underscores",
            )
        })?,
        None => default_backup_name(&system.computer_name),
    };

    let request = mjolnir_backup::BackupRequest {
        destination,
        name,
        scope: mjolnir_backup::BackupScope::SystemDisk,
        limit: match preview {
            Some(bytes) => mjolnir_backup::CaptureLimit::FirstBytes(bytes),
            None => mjolnir_backup::CaptureLimit::Everything,
        },
    };

    let plan = mjolnir_backup::plan(&request)?;
    if !cli.quiet && !cli.json {
        for line in plan.summary_lines() {
            eprintln!("{line}");
        }
        eprintln!(
            "Total to read: {}",
            mjolnir_core::progress::format_bytes(plan.source_bytes)
        );
        for w in &plan.warnings {
            eprintln!("Note: {w}");
        }
        eprintln!();
    }

    let outcome = mjolnir_backup::run(&request, &plan, progress, cancel)?;

    if cli.json {
        let value = serde_json::json!({
            "backup_dir": outcome.backup_dir.to_string_lossy(),
            "captured_bytes": outcome.captured_bytes,
            "stored_bytes": outcome.stored_bytes,
            "unique_chunks": outcome.unique_chunks,
            "elapsed_seconds": outcome.elapsed_seconds,
            "restorable": outcome.restorable,
            "verification": format!("{:?}", outcome.verification.result),
            "chunks_verified": outcome.verification.chunks_verified,
            "warnings": outcome.warnings,
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&value).unwrap_or_default()
        );
    } else {
        println!();
        println!("Backup completed and verified.");
        println!("  Location:  {}", outcome.backup_dir.display());
        println!(
            "  Read:      {}",
            mjolnir_core::progress::format_bytes(outcome.captured_bytes)
        );
        println!(
            "  Written:   {} ({:.1}x smaller)",
            mjolnir_core::progress::format_bytes(outcome.stored_bytes),
            outcome.compression_ratio()
        );
        println!("  Chunks:    {}", outcome.unique_chunks);
        println!("  Took:      {:.1} seconds", outcome.elapsed_seconds);
        println!(
            "  Verified:  {} chunks decompressed and checked",
            outcome.verification.chunks_verified
        );
        for w in &outcome.warnings {
            println!("  Note: {w}");
        }
        if !outcome.restorable {
            println!();
            println!("  This backup CANNOT be restored. It is a preview.");
        }
    }
    Ok(ExitCode::Success)
}

#[cfg(not(windows))]
fn cmd_backup(
    _cli: &Cli,
    _destination: PathBuf,
    _name: Option<String>,
    _preview: Option<u64>,
    _progress: &mut dyn Progress,
    _cancel: &CancelToken,
) -> Result<ExitCode> {
    Err(not_windows())
}

fn cmd_verify(
    cli: &Cli,
    path: PathBuf,
    quick: bool,
    progress: &mut dyn Progress,
    cancel: &CancelToken,
) -> Result<ExitCode> {
    let set = mjolnir_image::BackupSet::open_unchecked(&path)?;
    let store = set.chunk_store();

    let report = mjolnir_image::verify::verify(
        set.manifest(),
        Some(set.disk_layout()),
        &store,
        if quick {
            mjolnir_image::verify::VerifyDepth::Structure
        } else {
            mjolnir_image::verify::VerifyDepth::Full
        },
        progress,
        cancel,
    )?;

    let mut issues = report.issues.clone();
    issues.extend(set.issues().iter().cloned());
    let errors: Vec<_> = issues.iter().filter(|i| i.is_error()).collect();
    let complete = set.completion().is_some();

    if cli.json {
        let value = serde_json::json!({
            "path": path.to_string_lossy(),
            "complete": complete,
            "passed": errors.is_empty() && complete,
            "chunks_verified": report.chunks_verified,
            "bytes_verified": report.bytes_verified,
            "problems": issues.iter().map(|i| format!("{i}")).collect::<Vec<_>>(),
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&value).unwrap_or_default()
        );
    } else {
        println!();
        println!("Backup: {}", path.display());
        println!("  Name:     {}", set.manifest().backup.name);
        println!("  Taken:    {}", set.manifest().backup.created_utc);
        println!("  Computer: {}", set.manifest().source.computer_name);
        println!(
            "  Chunks:   {} checked, {} read",
            report.chunks_verified,
            mjolnir_core::progress::format_bytes(report.bytes_verified)
        );
        for u in &report.uncovered {
            println!(
                "  Stream {} does not carry {} across {} gaps",
                u.stream,
                mjolnir_core::progress::format_bytes(u.bytes),
                u.gaps
            );
        }
        if !complete {
            println!();
            println!("  INCOMPLETE: this backup has no completion.json, so it was interrupted.");
        }
        if errors.is_empty() && complete {
            println!();
            println!("  Verification passed.");
        } else {
            println!();
            println!("  Verification FAILED:");
            for i in issues.iter().take(50) {
                println!("    {i}");
            }
            if issues.len() > 50 {
                println!("    and {} more", issues.len() - 50);
            }
        }
    }

    if errors.is_empty() && complete {
        Ok(ExitCode::Success)
    } else {
        Ok(ExitCode::CorruptBackup)
    }
}

fn cmd_list(cli: &Cli, path: PathBuf) -> Result<ExitCode> {
    let mut found = Vec::new();
    let entries = std::fs::read_dir(&path).map_err(|e| Error::io(path.display(), e))?;
    for entry in entries.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        if let Ok(set) = mjolnir_image::BackupSet::open_unchecked(&dir) {
            found.push((dir, set));
        }
    }
    found.sort_by(|a, b| {
        a.1.manifest()
            .backup
            .created_utc
            .cmp(&b.1.manifest().backup.created_utc)
    });

    if cli.json {
        let value: Vec<_> = found
            .iter()
            .map(|(dir, set)| {
                serde_json::json!({
                    "path": dir.to_string_lossy(),
                    "name": set.manifest().backup.name.as_str(),
                    "created_utc": set.manifest().backup.created_utc,
                    "computer": set.manifest().source.computer_name,
                    "scope": set.manifest().backup.scope,
                    "stored_bytes": set.manifest().stats.stored_bytes,
                    "restorable": set.is_restorable(),
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
        println!("No MjolnirVSS backups found in {}", path.display());
        return Ok(ExitCode::Success);
    }
    println!("Backups in {}", path.display());
    for (_, set) in &found {
        let m = set.manifest();
        let state = if set.is_restorable() {
            "ready"
        } else if set.completion().is_none() {
            "INCOMPLETE"
        } else {
            "NOT RESTORABLE"
        };
        println!(
            "  {:<34} {:<21} {:>10}  {state}",
            m.backup.name.as_str(),
            m.backup.created_utc,
            mjolnir_core::progress::format_bytes(m.stats.stored_bytes),
        );
    }
    Ok(ExitCode::Success)
}

#[cfg(windows)]
fn cmd_cleanup(cli: &Cli, owned_only: bool) -> Result<ExitCode> {
    let session = mjolnir_vss::VssSession::begin_for_query()?;
    let snapshots = session.list_snapshots()?;

    if cli.json {
        let value: Vec<_> = snapshots
            .iter()
            .map(|s| {
                serde_json::json!({
                    "snapshot_id": mjolnir_storage::disks::guid_to_string(&s.snapshot_id),
                    "original_volume": s.original_volume,
                    "device_object": s.device_object,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&value).unwrap_or_default()
        );
        return Ok(ExitCode::Success);
    }

    println!("Shadow copies on this computer: {}", snapshots.len());
    for s in &snapshots {
        println!(
            "  {}  {}",
            mjolnir_storage::disks::guid_to_string(&s.snapshot_id),
            s.original_volume
        );
    }
    println!();
    if owned_only {
        println!("MjolnirVSS releases its own shadow copies when it finishes, including after a");
        println!("failure or a cancelled run, so there is normally nothing here to clean up.");
        println!("None of the shadow copies above were created by this process, and MjolnirVSS");
        println!("will not remove a shadow copy it did not create.");
    }
    Ok(ExitCode::Success)
}

#[cfg(not(windows))]
fn cmd_cleanup(_cli: &Cli, _owned_only: bool) -> Result<ExitCode> {
    Err(not_windows())
}

/// The default backup folder name, `COMPUTERNAME_YYYY-MM-DD_HHMM`.
pub fn default_backup_name(computer_name: &str) -> BackupName {
    BackupName::default_for(computer_name, &UtcTimestamp::now().to_backup_name_stamp())
}

#[allow(dead_code)]
fn not_windows() -> Error {
    Error::unsupported(
        "MjolnirVSS only runs on Windows",
        "it uses the Volume Shadow Copy Service and the Windows storage interfaces, neither of which exists on other systems",
        "run MjolnirVSS on the Windows computer you want to back up",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn the_command_line_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn backup_parses_with_a_destination() {
        let cli = Cli::try_parse_from(["MjolnirVSS", "backup", "--destination", "E:\\Backups"])
            .expect("should parse");
        match cli.command {
            Command::Backup {
                destination,
                name,
                preview,
            } => {
                assert_eq!(destination, PathBuf::from("E:\\Backups"));
                assert!(name.is_none());
                assert!(preview.is_none());
            }
            other => panic!("wrong command: {other:?}"),
        }
    }

    #[test]
    fn backup_requires_a_destination() {
        assert!(Cli::try_parse_from(["MjolnirVSS", "backup"]).is_err());
    }

    #[test]
    fn preview_takes_a_byte_count() {
        let cli = Cli::try_parse_from([
            "MjolnirVSS",
            "backup",
            "--destination",
            "E:\\",
            "--preview",
            "1048576",
        ])
        .unwrap();
        match cli.command {
            Command::Backup { preview, .. } => assert_eq!(preview, Some(1_048_576)),
            other => panic!("wrong command: {other:?}"),
        }
    }

    #[test]
    fn global_flags_work_after_the_subcommand() {
        let cli = Cli::try_parse_from(["MjolnirVSS", "inspect", "--json"]).unwrap();
        assert!(cli.json);
    }

    #[test]
    fn verify_takes_a_path() {
        let cli = Cli::try_parse_from(["MjolnirVSS", "verify", "E:\\Backups\\PC_2026"]).unwrap();
        match cli.command {
            Command::Verify { path, quick } => {
                assert_eq!(path, PathBuf::from("E:\\Backups\\PC_2026"));
                assert!(!quick);
            }
            other => panic!("wrong command: {other:?}"),
        }
    }

    #[test]
    fn the_default_backup_name_has_the_documented_shape() {
        let name = default_backup_name("DESKTOP-1A2B");
        let text = name.as_str();
        assert!(text.starts_with("DESKTOP-1A2B_"), "{text}");
        // COMPUTERNAME_YYYY-MM-DD_HHMM
        let stamp = &text["DESKTOP-1A2B_".len()..];
        assert_eq!(stamp.len(), 15, "{stamp}");
        assert_eq!(&stamp[4..5], "-");
        assert_eq!(&stamp[7..8], "-");
        assert_eq!(&stamp[10..11], "_");
    }
}
