//! Writes a real backup folder to a chosen location, for trying the shipped
//! executables against.
//!
//! Ignored by default, because it leaves files behind on purpose. Run it when
//! you want a backup to point `MjolnirVSS.exe verify` or
//! `MjolnirVSS.Restore.exe inspect-backup` at:
//!
//! ```text
//! $env:MJOLNIR_FIXTURE_DIR = "C:\Temp\mjolnir-demo"
//! cargo test -p mjolnir-integration-tests --test make_fixture -- --ignored --nocapture
//! ```
//!
//! What it produces is an ordinary backup of a synthetic disk: the same
//! documents, the same blocks and the same completion marker a backup of a real
//! machine has. The only difference is where the bytes came from.

mod common;

use mjolnir_image::manifest::CaptureMethod;
use mjolnir_testkit::SyntheticDisk;

use common::back_up_with;

#[test]
#[ignore = "writes files outside the test temporary directory; run it deliberately"]
fn write_a_backup_fixture() {
    let dir = std::env::var("MJOLNIR_FIXTURE_DIR")
        .expect("set MJOLNIR_FIXTURE_DIR to the folder to write into");
    let dir = std::path::PathBuf::from(dir);
    std::fs::create_dir_all(&dir).expect("create the folder");

    let disk = SyntheticDisk::windows_like(512);
    let name = format!(
        "SYNTHETIC-PC_{}",
        mjolnir_core::timestamp::UtcTimestamp::now().to_backup_name_stamp()
    );

    let backup = back_up_with(&disk, &dir, &name, CaptureMethod::RawFull, None)
        .expect("the backup should succeed");

    eprintln!("Wrote a backup to {}", backup.display());
    eprintln!("Source disk was {} bytes", disk.size_bytes());
    eprintln!();
    eprintln!("Try it with:");
    eprintln!("  MjolnirVSS.exe list {}", dir.display());
    eprintln!("  MjolnirVSS.exe verify {}", backup.display());
    eprintln!(
        "  MjolnirVSS.Restore.exe inspect-backup {}",
        backup.display()
    );
}
