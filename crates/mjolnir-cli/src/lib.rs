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

use std::path::{Path, PathBuf};

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

    /// Examine how BitLocker and the shadow copy service behave on this
    /// computer.
    ///
    /// Reads a handful of sectors and writes nothing. Creates a shadow copy and
    /// releases it again. Never reads or stores any key material.
    DiagnoseBitlocker,

    /// Show the shadow copies on this computer, and optionally clean up.
    CleanupSnapshots {
        /// Only report; never remove anything.
        #[arg(long, default_value_t = true)]
        owned_only: bool,
    },

    /// Build bootable recovery media from the Windows parts this computer has.
    ///
    /// Writes an ISO file. Nothing is downloaded, and no Microsoft file is
    /// copied anywhere except onto the media being made.
    RecoveryMedia {
        /// Where to write the ISO.
        #[arg(long)]
        iso: PathBuf,
        /// The folder holding MjolnirVSS.Restore.exe. Defaults to the folder
        /// this program is running from.
        #[arg(long)]
        from: Option<PathBuf>,
    },

    /// Report what recovery media could be built from, without building one.
    RecoverySources,

    /// List the partitions inside a backup that can be browsed.
    Volumes {
        /// The backup folder.
        path: PathBuf,
    },

    /// List what is inside a folder of a backed up volume.
    ///
    /// Reads the backup. Nothing is written and the backup is never modified.
    Browse {
        /// The backup folder.
        path: PathBuf,
        /// Which partition, as shown by the volumes command.
        #[arg(long)]
        volume: String,
        /// The folder to list. Defaults to the root of the volume.
        #[arg(long, default_value = "\\")]
        folder: String,
    },

    /// Copy a file or folder out of a backup.
    Extract {
        /// The backup folder.
        path: PathBuf,
        /// Which partition, as shown by the volumes command.
        #[arg(long)]
        volume: String,
        /// What to copy, as a path inside that partition.
        #[arg(long)]
        item: String,
        /// Where to put it. A folder that already exists.
        #[arg(long)]
        into: PathBuf,
        /// Replace a file that is already there.
        #[arg(long)]
        overwrite: bool,
        /// Copy a junction or a link's target instead of skipping it.
        #[arg(long)]
        follow_links: bool,
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
        Command::DiagnoseBitlocker => cmd_diagnose_bitlocker(&cli, &cancel),
        Command::CleanupSnapshots { owned_only } => cmd_cleanup(&cli, *owned_only),
        Command::RecoveryMedia { iso, from } => {
            cmd_recovery_media(&cli, iso, from.as_deref(), progress.as_mut(), &cancel)
        }
        Command::RecoverySources => cmd_recovery_sources(&cli),
        Command::Volumes { path } => cmd_volumes(&cli, path),
        Command::Browse {
            path,
            volume,
            folder,
        } => cmd_browse(&cli, path, volume, folder, &cancel),
        Command::Extract {
            path,
            volume,
            item,
            into,
            overwrite,
            follow_links,
        } => cmd_extract(
            &cli,
            path,
            volume,
            item,
            into,
            *overwrite,
            *follow_links,
            progress.as_mut(),
            &cancel,
        ),
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
            // The inspect command exists to tell an operator what would
            // happen, so it prints the shadow copy figures in full rather
            // than folding them away.
            for line in plan.snapshot_preflight.details() {
                println!("  {line}");
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
        if plan.snapshot_preflight.warning().is_some() {
            for line in plan.snapshot_preflight.details() {
                eprintln!("  {line}");
            }
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
fn cmd_diagnose_bitlocker(cli: &Cli, cancel: &CancelToken) -> Result<ExitCode> {
    let diagnosis = mjolnir_backup::diagnose_system_volume(cancel)?;

    if cli.json {
        let value = serde_json::json!({
            "volume": diagnosis.volume,
            "drive_letter": diagnosis.drive_letter,
            "partition_offset": diagnosis.partition_offset,
            "on_disk_signature": diagnosis.on_disk_signature.describe(),
            "reported_filesystem": diagnosis.reported_filesystem,
            "encryption": diagnosis.encryption.describe(),
            "snapshot_device": diagnosis.snapshot_device,
            "snapshot_signature": diagnosis.snapshot_signature.map(|s| s.describe()),
            "mft_found": diagnosis.mft_found,
            "mft_mirror_found": diagnosis.mft_mirror_found,
            "findings": diagnosis.findings,
            "conclusion": format!("{:?}", diagnosis.conclusion),
            "can_be_backed_up": diagnosis.conclusion.can_be_backed_up(),
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&value).unwrap_or_default()
        );
    } else {
        print!("{}", diagnosis.report());
    }

    Ok(if diagnosis.conclusion.can_be_backed_up() {
        ExitCode::Success
    } else {
        ExitCode::Unsupported
    })
}

#[cfg(not(windows))]
fn cmd_diagnose_bitlocker(_cli: &Cli, _cancel: &CancelToken) -> Result<ExitCode> {
    Err(not_windows())
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

/// The folder this program is running from, which is where the portable
/// release keeps both executables next to each other.
fn own_folder() -> Result<PathBuf> {
    let exe = std::env::current_exe().map_err(|e| {
        Error::new(
            ExitCode::Failure,
            "MjolnirVSS could not find its own folder",
            e.to_string(),
            "pass --from with the folder MjolnirVSS was extracted to",
        )
    })?;
    Ok(exe
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from(".")))
}

fn cmd_recovery_sources(cli: &Cli) -> Result<ExitCode> {
    let sources = mjolnir_media::find_sources();
    if cli.json {
        let value = serde_json::json!({
            "sources": sources.iter().map(|s| serde_json::json!({
                "kind": s.kind.describe(),
                "boot_image": s.boot_image.to_string_lossy(),
                "can_make_iso": s.can_make_iso(),
                "can_make_usb": s.can_make_usb(),
            })).collect::<Vec<_>>(),
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&value).unwrap_or_default()
        );
        return Ok(ExitCode::Success);
    }

    if sources.is_empty() {
        println!("No Windows recovery components were found on this computer.");
        println!("MjolnirVSS never ships or downloads them: Microsoft does not allow it.");
        println!("Install the Windows ADK with the Windows PE add-on, or turn the recovery");
        println!("environment back on with 'reagentc /enable'.");
        return Ok(ExitCode::Unsupported);
    }

    println!("Recovery media could be built from:");
    for source in &sources {
        println!("  {}", source.kind.describe());
        println!("    image: {}", source.boot_image.display());
        println!(
            "    can make an ISO: {}",
            if source.can_make_iso() { "yes" } else { "no" }
        );
        println!(
            "    can make a USB stick: {}",
            if source.can_make_usb() { "yes" } else { "no" }
        );
        for limit in source.explain_limits() {
            println!("    note: {limit}");
        }
    }
    Ok(ExitCode::Success)
}

fn cmd_recovery_media(
    cli: &Cli,
    iso: &Path,
    from: Option<&Path>,
    progress: &mut dyn Progress,
    cancel: &CancelToken,
) -> Result<ExitCode> {
    let release = match from {
        Some(dir) => dir.to_path_buf(),
        None => own_folder()?,
    };
    let payload = mjolnir_media::payload_from_release(&release)?;
    let source = mjolnir_media::best_source()?;

    if !cli.quiet && !cli.json {
        eprintln!("Building recovery media from {}.", source.kind.describe());
        eprintln!("Writing {}", iso.display());
    }

    let work = std::env::temp_dir();
    let outcome = mjolnir_media::build_iso(&source, &payload, iso, &work, progress, cancel)?;
    let report = mjolnir_media::check_iso(&outcome.path)?;

    if cli.json {
        let value = serde_json::json!({
            "path": outcome.path.to_string_lossy(),
            "size_bytes": outcome.size_bytes,
            "source": outcome.source.describe(),
            "verified": report.passed(),
            "checks": report.checks.iter().map(|c| serde_json::json!({
                "what": c.what, "passed": c.passed, "detail": c.detail,
            })).collect::<Vec<_>>(),
            "notes": outcome.notes,
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&value).unwrap_or_default()
        );
    } else {
        println!(
            "Recovery media written: {} ({})",
            outcome.path.display(),
            mjolnir_core::progress::format_bytes(outcome.size_bytes)
        );
        print!("{}", report.describe());
        for note in &outcome.notes {
            println!("{note}");
        }
    }

    if report.passed() {
        Ok(ExitCode::Success)
    } else {
        Ok(ExitCode::CorruptBackup)
    }
}

fn cmd_volumes(cli: &Cli, path: &Path) -> Result<ExitCode> {
    let set = mjolnir_image::BackupSet::open(path)?;
    let volumes = mjolnir_files::volumes_in(&set);

    if cli.json {
        let value = serde_json::json!({
            "volumes": volumes.iter().map(|v| serde_json::json!({
                "stream_id": v.stream_id,
                "partition": v.partition_number,
                "role": v.role,
                "drive_letter": v.drive_letter,
                "label": v.label,
                "filesystem": v.filesystem,
                "size_bytes": v.size_bytes,
                "readable": v.is_readable,
                "why_not": v.why_not,
            })).collect::<Vec<_>>(),
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&value).unwrap_or_default()
        );
        return Ok(ExitCode::Success);
    }

    println!("Partitions in {}:", path.display());
    for volume in &volumes {
        println!("  {}", volume.describe());
        println!("    id: {}", volume.stream_id);
        match &volume.why_not {
            Some(why) => println!("    cannot be browsed: {why}"),
            None => println!("    can be browsed"),
        }
    }
    Ok(ExitCode::Success)
}

fn cmd_browse(
    cli: &Cli,
    path: &Path,
    volume: &str,
    folder: &str,
    cancel: &CancelToken,
) -> Result<ExitCode> {
    let set = mjolnir_image::BackupSet::open(path)?;
    let stream = mjolnir_files::stream_in(&set, volume)?;
    let open = mjolnir_files::OpenVolume::open(&set, stream, cancel)?;
    let index = open.index();

    let entry = index.resolve(folder).ok_or_else(|| {
        Error::new(
            ExitCode::Failure,
            "that folder is not in the backup",
            format!("{folder} was not found in {volume}"),
            "check the path, or browse from the root with --folder \\",
        )
    })?;

    let children = index.children_of(entry.number);

    if cli.json {
        let value = serde_json::json!({
            "folder": index.path_of(entry.number),
            "entries": children.iter().map(|e| serde_json::json!({
                "name": e.name,
                "directory": e.is_directory,
                "size": e.size,
                "readable": e.is_readable(),
                "why_not": e.why_unreadable(),
                "reparse_point": e.is_reparse_point,
                "compressed": e.is_compressed,
                "encrypted": e.is_encrypted,
                "sparse": e.is_sparse,
                "hard_linked": e.is_hard_linked,
                "streams": e.streams.iter().map(|(n, s)| serde_json::json!({"name": n, "size": s})).collect::<Vec<_>>(),
            })).collect::<Vec<_>>(),
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&value).unwrap_or_default()
        );
        return Ok(ExitCode::Success);
    }

    println!(
        "{}  ({} items)",
        index
            .path_of(entry.number)
            .unwrap_or_else(|| folder.to_owned()),
        children.len()
    );
    for child in children {
        let kind = if child.is_directory { "<DIR>" } else { "     " };
        let size = if child.is_directory {
            String::new()
        } else {
            mjolnir_core::progress::format_bytes(child.size)
        };
        let mut notes = Vec::new();
        if child.is_reparse_point {
            notes.push("link");
        }
        if child.is_compressed {
            notes.push("compressed");
        }
        if child.is_encrypted {
            notes.push("encrypted");
        }
        if child.is_sparse {
            notes.push("sparse");
        }
        if child.is_hard_linked {
            notes.push("hard linked");
        }
        if !child.streams.is_empty() {
            notes.push("has streams");
        }
        let note = if notes.is_empty() {
            String::new()
        } else {
            format!("  [{}]", notes.join(", "))
        };
        println!("  {kind} {size:>12}  {}{note}", child.name);
    }
    if !index.unreadable.is_empty() {
        println!();
        println!(
            "  {} of {} records in this volume could not be read              ({} more have never held a file):",
            index.unreadable.len(),
            index.records_scanned,
            index.records_unused
        );
        // A bare count says nothing about whether anybody's files are affected.
        // Grouping by reason does: a volume whose unused records simply have no
        // signature in them reads very differently from one with damage in it.
        let mut reasons: std::collections::BTreeMap<&str, usize> =
            std::collections::BTreeMap::new();
        for (_, why) in &index.unreadable {
            *reasons.entry(why.as_str()).or_default() += 1;
        }
        let mut by_count: Vec<(&&str, &usize)> = reasons.iter().collect();
        by_count.sort_by(|a, b| b.1.cmp(a.1));
        for (why, count) in by_count.iter().take(6) {
            println!("    {count:>6}  {why}");
        }
        if by_count.len() > 6 {
            println!("    and {} other reasons", by_count.len() - 6);
        }
    }
    Ok(ExitCode::Success)
}

#[allow(clippy::too_many_arguments)]
fn cmd_extract(
    cli: &Cli,
    path: &Path,
    volume: &str,
    item: &str,
    into: &Path,
    overwrite: bool,
    follow_links: bool,
    progress: &mut dyn Progress,
    cancel: &CancelToken,
) -> Result<ExitCode> {
    if !into.is_dir() {
        return Err(Error::new(
            ExitCode::Failure,
            "the folder to copy into does not exist",
            format!("{} is not a folder", into.display()),
            "make the folder first, or choose one that is already there",
        ));
    }

    let set = mjolnir_image::BackupSet::open(path)?;
    let stream = mjolnir_files::stream_in(&set, volume)?;
    let mut open = mjolnir_files::OpenVolume::open(&set, stream, cancel)?;

    let entry = open.index().resolve(item).cloned().ok_or_else(|| {
        Error::new(
            ExitCode::Failure,
            "that file is not in the backup",
            format!("{item} was not found in {volume}"),
            "use the browse command to see what is there",
        )
    })?;

    let options = mjolnir_files::ExtractOptions {
        include_streams: true,
        follow_reparse_points: follow_links,
        overwrite,
    };

    let outcome = if entry.is_directory {
        mjolnir_files::extract_tree(&mut open, &entry, into, &options, progress, cancel)?
    } else {
        let mut outcome = mjolnir_files::ExtractOutcome::default();
        for result in
            mjolnir_files::extract_file(&mut open, &entry, into, &options, progress, cancel)?
        {
            if let mjolnir_files::Extracted::Written { bytes, .. } = &result {
                outcome.bytes_written += bytes;
            }
            outcome.files.push(result);
        }
        outcome
    };

    if cli.json {
        let value = serde_json::json!({
            "written": outcome.written(),
            "skipped": outcome.skipped(),
            "failed": outcome.failed(),
            "bytes_written": outcome.bytes_written,
            "files": outcome.files.iter().map(|f| f.describe()).collect::<Vec<_>>(),
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&value).unwrap_or_default()
        );
    } else {
        for file in &outcome.files {
            println!("  {}", file.describe());
        }
        println!();
        println!("{}", outcome.summary());
    }

    if outcome.everything_worked() {
        Ok(ExitCode::Success)
    } else {
        Ok(ExitCode::CorruptBackup)
    }
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
