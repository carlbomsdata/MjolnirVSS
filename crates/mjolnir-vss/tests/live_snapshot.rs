//! A live shadow copy against the running Windows installation.
//!
//! This is the feasibility proof for the whole product: that MjolnirVSS can
//! drive the Volume Shadow Copy Service from Rust, read frozen data out of the
//! resulting device, and leave nothing behind.
//!
//! # Why it is opt in
//!
//! It needs administrator rights and it briefly changes system state, so it is
//! skipped unless `MJOLNIR_VSS_LIVE=1` is set. It never writes to a disk and
//! never touches a shadow copy it did not create, but it is still a test that
//! reaches outside the process, and those do not belong in an ordinary
//! `cargo test` run or in continuous integration.
//!
//! Run it with:
//!
//! ```text
//! $env:MJOLNIR_VSS_LIVE = "1"
//! cargo test -p mjolnir-vss --test live_snapshot -- --nocapture
//! ```
//!
//! from an elevated prompt.

#![cfg(windows)]

use mjolnir_core::cancel::CancelToken;
use mjolnir_storage::device::Device;
use mjolnir_vss::VssSession;

fn live_tests_enabled() -> bool {
    std::env::var("MJOLNIR_VSS_LIVE").as_deref() == Ok("1")
}

/// Whether the shadow copy service still lists a snapshot, machine wide.
///
/// The device path is deliberately not used for this. A shadow copy device
/// stays openable inside the process that created it for a long time after the
/// service has released it, so its presence proves nothing. Asking the service
/// is the only answer that means anything, and it is also what an operator
/// running `vssadmin list shadows` would see.
fn service_lists_snapshot(snapshot_id: windows::core::GUID) -> bool {
    let session = VssSession::begin_for_query().expect("start a query session");
    let all = session.list_snapshots().expect("list shadow copies");
    all.iter().any(|s| s.snapshot_id == snapshot_id)
}

/// Takes a real shadow copy, reads from it, and cleans it up.
///
/// Every assertion is about MjolnirVSS behaving correctly, not about this
/// particular computer, so it holds on any Windows 10 or 11 machine.
#[test]
fn a_shadow_copy_can_be_created_read_and_removed() {
    if !live_tests_enabled() {
        eprintln!("skipped: set MJOLNIR_VSS_LIVE=1 and run elevated to enable");
        return;
    }

    let cancel = CancelToken::new();
    let system = mjolnir_storage::system::describe_system().expect("describe this machine");
    let volume = system.windows_volume.guid_path.clone();
    eprintln!("Windows volume: {volume}");
    eprintln!("System disk:    {}", system.system_disk_number);

    let mut session = VssSession::begin(&cancel).expect("start a shadow copy session");

    let supported = session
        .is_volume_supported(&volume)
        .expect("ask whether the volume can be shadow copied");
    assert!(supported, "the Windows volume should support shadow copies");

    let snapshots = session
        .snapshot(std::slice::from_ref(&volume), &cancel)
        .expect("create the shadow copy");

    assert_eq!(snapshots.len(), 1, "one volume in, one shadow copy out");
    let snapshot = &snapshots[0];
    eprintln!("Shadow copy:    {}", snapshot.device_object);

    assert!(
        snapshot
            .device_object
            .starts_with("\\\\?\\GLOBALROOT\\Device\\HarddiskVolumeShadowCopy"),
        "unexpected shadow copy device path: {}",
        snapshot.device_object
    );
    assert!(
        session.snapshot_set_id().is_some(),
        "the session should know its own snapshot set"
    );

    // The whole point of the shadow copy is that it can be read as a block
    // device. One megabyte from the start is enough to prove the device is
    // real: the first sector of an NTFS volume is its boot sector, which
    // carries a recognisable signature.
    {
        let device = Device::open_read(&snapshot.device_object, 512, 0)
            .expect("open the shadow copy for reading");
        let mut buffer = vec![0u8; 1024 * 1024];
        device
            .read_at(0, &mut buffer)
            .expect("read from the shadow copy");

        assert_eq!(
            &buffer[3..11],
            b"NTFS    ",
            "the shadow copy should start with an NTFS boot sector"
        );
        assert_eq!(
            [buffer[510], buffer[511]],
            [0x55, 0xAA],
            "the boot sector should carry the usual signature"
        );
        assert!(
            buffer.iter().any(|&b| b != 0),
            "a megabyte of nothing but zeroes means the read did not really happen"
        );
    }

    // Reading the same range twice must give identical bytes: that is what
    // "frozen" means, and it is the property the whole backup depends on.
    {
        let device = Device::open_read(&snapshot.device_object, 512, 0).expect("reopen");
        let mut first = vec![0u8; 65536];
        let mut second = vec![0u8; 65536];
        device.read_at(0, &mut first).expect("first read");
        std::thread::sleep(std::time::Duration::from_millis(250));
        device.read_at(0, &mut second).expect("second read");
        assert_eq!(first, second, "the shadow copy changed under us");
    }

    let writers = session
        .writer_status(&cancel)
        .expect("collect writer status");
    eprintln!("Writers:        {}", writers.len());
    for w in &writers {
        if !w.succeeded() {
            eprintln!(
                "  failed: {} ({}) {:#010x}",
                w.name, w.state_text, w.failure
            );
        }
    }
    let failed: Vec<&str> = writers
        .iter()
        .filter(|w| !w.succeeded())
        .map(|w| w.name.as_str())
        .collect();
    assert!(
        failed.is_empty(),
        "these writers did not finish cleanly: {failed:?}"
    );

    session.complete(&cancel).expect("finish the backup");

    let deleted = session
        .delete_own_snapshots()
        .expect("remove the shadow copy");
    assert_eq!(deleted, 1, "exactly the one shadow copy we made");

    // The session itself must agree the snapshot is gone.
    assert!(
        !session
            .snapshot_exists(snapshot.snapshot_id)
            .expect("ask whether the snapshot still exists"),
        "the session still reports its snapshot after deleting it"
    );

    drop(session);

    // And so must the service, machine wide. This is the assertion that would
    // catch MjolnirVSS leaving clutter on a user's computer.
    assert!(
        !service_lists_snapshot(snapshot.snapshot_id),
        "the shadow copy is still listed by the service after cleanup"
    );
    eprintln!("Shadow copy created, read and removed cleanly.");
}

/// Dropping a session without completing it must still clean up.
///
/// This is the crash path: if MjolnirVSS panics or the user closes the window
/// mid backup, the shadow copy must not be left behind.
#[test]
fn dropping_a_session_removes_its_shadow_copy() {
    if !live_tests_enabled() {
        eprintln!("skipped: set MJOLNIR_VSS_LIVE=1 and run elevated to enable");
        return;
    }

    let cancel = CancelToken::new();
    let system = mjolnir_storage::system::describe_system().expect("describe this machine");
    let volume = system.windows_volume.guid_path.clone();

    let snapshot_id = {
        let mut session = VssSession::begin(&cancel).expect("start a shadow copy session");
        let snapshots = session
            .snapshot(std::slice::from_ref(&volume), &cancel)
            .expect("create the shadow copy");
        let id = snapshots[0].snapshot_id;

        // Prove it is real while the session is alive.
        Device::open_read(&snapshots[0].device_object, 512, 0)
            .expect("the shadow copy should be readable");
        assert!(
            session.snapshot_exists(id).expect("ask the service"),
            "the snapshot should exist before the session is dropped"
        );

        // No complete, no explicit delete: just drop it, the way a panic would.
        id
    };

    assert!(
        !service_lists_snapshot(snapshot_id),
        "dropping the session left shadow copy {snapshot_id:?} behind"
    );
    eprintln!("Dropping the session cleaned up after itself.");
}

/// Cleanup must never touch a shadow copy MjolnirVSS did not create.
///
/// Two sessions run at once; deleting one must leave the other's shadow copy
/// untouched. This is the property that stops MjolnirVSS from destroying a
/// restore point or another backup tool's working set.
#[test]
fn cleanup_only_removes_our_own_shadow_copies() {
    if !live_tests_enabled() {
        eprintln!("skipped: set MJOLNIR_VSS_LIVE=1 and run elevated to enable");
        return;
    }

    let cancel = CancelToken::new();
    let system = mjolnir_storage::system::describe_system().expect("describe this machine");
    let volume = system.windows_volume.guid_path.clone();

    let mut keeper = VssSession::begin(&cancel).expect("start the first session");
    let kept = keeper
        .snapshot(std::slice::from_ref(&volume), &cancel)
        .expect("create the first shadow copy");
    let kept_device = kept[0].device_object.clone();

    {
        let mut doomed = VssSession::begin(&cancel).expect("start the second session");
        let made = doomed
            .snapshot(std::slice::from_ref(&volume), &cancel)
            .expect("create the second shadow copy");
        assert_ne!(
            made[0].snapshot_id, kept[0].snapshot_id,
            "two sessions should produce two different shadow copies"
        );
        assert_ne!(
            doomed.snapshot_set_id(),
            keeper.snapshot_set_id(),
            "two sessions should have different snapshot sets"
        );

        let deleted = doomed.delete_own_snapshots().expect("remove the second");
        assert_eq!(deleted, 1, "only the second session's own shadow copy");
    }

    // The first session's shadow copy must have survived the second's cleanup.
    Device::open_read(&kept_device, 512, 0)
        .expect("the first session's shadow copy was destroyed by the second session's cleanup");
    assert!(
        keeper
            .snapshot_exists(kept[0].snapshot_id)
            .expect("ask the service"),
        "the second session's cleanup removed the first session's shadow copy"
    );

    let kept_id = kept[0].snapshot_id;
    keeper.complete(&cancel).ok();
    let deleted = keeper.delete_own_snapshots().expect("remove the first");
    assert_eq!(deleted, 1);
    drop(keeper);
    assert!(
        !service_lists_snapshot(kept_id),
        "the first session's shadow copy outlived the test"
    );
    eprintln!("Cleanup left the other session's shadow copy alone.");
}

/// A shadow copy device path that was never used must not open.
///
/// Kept because the cleanup tests depend on knowing that device paths are not
/// simply always openable: they are not, which is what makes the lingering of a
/// deleted one a real and specific Windows behaviour rather than a bug here.
#[test]
fn an_unused_shadow_copy_device_path_does_not_open() {
    if !live_tests_enabled() {
        eprintln!("skipped: set MJOLNIR_VSS_LIVE=1 and run elevated to enable");
        return;
    }
    for n in [900u32, 901, 902] {
        let path = format!(r"\\?\GLOBALROOT\Device\HarddiskVolumeShadowCopy{n}");
        assert!(
            Device::open_read(&path, 512, 0).is_err(),
            "{path} opened, so device openability says nothing about existence"
        );
    }
}
