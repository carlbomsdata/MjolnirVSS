//! Volume Shadow Copy Service orchestration.
//!
//! This crate drives one backup shaped VSS session: it asks the writers to
//! quiesce, takes a coordinated snapshot of several volumes at one instant,
//! hands back the shadow copy device paths to read from, and then releases
//! everything again.
//!
//! # Why it is a crate of its own
//!
//! `vssapi.dll` does not exist in a base Windows PE image. The recovery
//! application has to start there, so it must not import the library at all.
//! Keeping the shadow copy code in a separate crate that only the backup side
//! depends on makes that a build time guarantee rather than a promise: nothing
//! in `mjolnir-restore` can reach this code, so nothing can accidentally add
//! the import.
//!
//! # Leaving nothing behind
//!
//! Snapshots are taken in `VSS_CTX_BACKUP`, which makes them non persistent:
//! the service releases them when the session object is released, even if the
//! process crashes. On top of that, [`VssSession`] deletes its own snapshot set
//! explicitly on every exit path, and deletes it *by snapshot set identifier*,
//! so a snapshot somebody else created is never touched.
//!
//! # Threading
//!
//! The session initialises COM as multi threaded and must be used from one
//! thread for its whole life. The graphical interface runs it on a worker
//! thread so the window keeps responding.
//!
//! Copyright (C) the MjolnirVSS contributors.
//! Licensed under the GNU General Public License, version 3 or later.

#![warn(missing_docs)]
#![cfg(windows)]

pub mod sys;

use mjolnir_core::cancel::CancelToken;
use mjolnir_core::error::{Error, Result};
use mjolnir_core::exit::ExitCode;
use windows::core::{Interface, BSTR, GUID, HRESULT};
use windows::Win32::Foundation::{FALSE, RPC_E_CHANGED_MODE, RPC_E_TOO_LATE, S_FALSE};
use windows::Win32::Storage::Vss::{
    IVssAsync, IVssEnumObject, VSS_BT_COPY, VSS_CTX_ALL, VSS_CTX_BACKUP, VSS_OBJECT_NONE,
    VSS_OBJECT_PROP, VSS_OBJECT_SNAPSHOT, VSS_OBJECT_SNAPSHOT_SET, VSS_SNAPSHOT_PROP,
    VSS_WRITER_STATE,
};
use windows::Win32::System::Com::{
    CoInitializeEx, CoInitializeSecurity, CoUninitialize, COINIT_MULTITHREADED, EOAC_NONE,
    RPC_C_AUTHN_LEVEL_PKT_PRIVACY, RPC_C_IMP_LEVEL_IDENTIFY,
};

/// Status codes from `vss.h` that MjolnirVSS reacts to by name.
///
/// Declared here rather than taken from the `windows` crate because the same
/// metadata gap that omits `IVssBackupComponents` omits several of these.
mod codes {
    /// The asynchronous operation is still running.
    pub const VSS_S_ASYNC_PENDING: i32 = 0x0004_2309u32 as i32;
    /// The asynchronous operation finished.
    pub const VSS_S_ASYNC_FINISHED: i32 = 0x0004_230Au32 as i32;
    /// The asynchronous operation was cancelled.
    pub const VSS_S_ASYNC_CANCELLED: i32 = 0x0004_230Bu32 as i32;

    /// The object is not in the right state for the call.
    pub const VSS_E_BAD_STATE: i32 = 0x8004_2301u32 as i32;
    /// The shadow copy or other object does not exist.
    pub const VSS_E_OBJECT_NOT_FOUND: i32 = 0x8004_2308u32 as i32;
    /// The provider refused the operation.
    pub const VSS_E_PROVIDER_VETO: i32 = 0x8004_2306u32 as i32;
    /// The volume cannot be shadow copied.
    pub const VSS_E_VOLUME_NOT_SUPPORTED: i32 = 0x8004_230Cu32 as i32;
    /// No provider handles this volume.
    pub const VSS_E_VOLUME_NOT_SUPPORTED_BY_PROVIDER: i32 = 0x8004_230Eu32 as i32;
    /// Too many volumes in one snapshot set.
    pub const VSS_E_MAXIMUM_NUMBER_OF_VOLUMES_REACHED: i32 = 0x8004_2312u32 as i32;
    /// Writes could not be flushed in time.
    pub const VSS_E_FLUSH_WRITES_TIMEOUT: i32 = 0x8004_2313u32 as i32;
    /// Writes could not be held long enough to take the snapshot.
    pub const VSS_E_HOLD_WRITES_TIMEOUT: i32 = 0x8004_2314u32 as i32;
    /// A writer failed.
    pub const VSS_E_UNEXPECTED_WRITER_ERROR: i32 = 0x8004_2315u32 as i32;
    /// Another snapshot set is being created.
    pub const VSS_E_SNAPSHOT_SET_IN_PROGRESS: i32 = 0x8004_2316u32 as i32;
    /// The shadow copy storage area is too small or full.
    pub const VSS_E_INSUFFICIENT_STORAGE: i32 = 0x8004_231Du32 as i32;
    /// The writer service is not working.
    pub const VSS_E_WRITER_INFRASTRUCTURE: i32 = 0x8004_2318u32 as i32;
    /// A writer did not respond.
    pub const VSS_E_WRITER_NOT_RESPONDING: i32 = 0x8004_2319u32 as i32;

    /// Access was denied, almost always because the process is not elevated.
    pub const E_ACCESSDENIED: i32 = 0x8007_0005u32 as i32;
}

/// One shadow copy created by this session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    /// Shadow copy identifier.
    pub snapshot_id: GUID,
    /// The volume that was snapshotted, as a volume GUID path.
    pub original_volume: String,
    /// The device to read the frozen contents from, for example
    /// `\\?\GLOBALROOT\Device\HarddiskVolumeShadowCopy3`.
    pub device_object: String,
}

/// What one VSS writer reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriterStatus {
    /// Writer name, for example `Registry Writer`.
    pub name: String,
    /// Writer class identifier.
    pub writer_id: GUID,
    /// Writer instance identifier.
    pub instance_id: GUID,
    /// Raw state value.
    pub state: i32,
    /// State in words.
    pub state_text: String,
    /// The failure the writer reported, as a raw `HRESULT`.
    pub failure: i32,
}

/// Whether a writer state value is one of the failure states.
///
/// `vss.h` numbers the states so that 1 through 5 describe a writer moving
/// normally through the snapshot lifecycle and 6 through 15 are the
/// `VSS_WS_FAILED_AT_*` values. Between `DoSnapshotSet` and `BackupComplete` a
/// healthy writer sits in `VSS_WS_WAITING_FOR_BACKUP_COMPLETE`, so treating
/// anything other than `VSS_WS_STABLE` as a failure would condemn every
/// successful backup.
pub fn is_failure_state(state: i32) -> bool {
    (6..=15).contains(&state)
}

impl WriterStatus {
    /// Whether the writer took part in the snapshot without failing it.
    pub fn succeeded(&self) -> bool {
        self.failure == 0 && !is_failure_state(self.state)
    }
}

/// Describes a writer state value in words.
fn describe_writer_state(state: VSS_WRITER_STATE) -> String {
    let text = match state.0 {
        0 => "unknown",
        1 => "stable",
        2 => "waiting for freeze",
        3 => "waiting for thaw",
        4 => "waiting for post snapshot",
        5 => "waiting for backup complete",
        6 => "failed while identifying",
        7 => "failed while preparing for backup",
        8 => "failed while preparing the snapshot",
        9 => "failed while freezing",
        10 => "failed while thawing",
        11 => "failed after the snapshot",
        12 => "failed at backup complete",
        13 => "failed before restore",
        14 => "failed after restore",
        15 => "failed at backup shutdown",
        _ => "unrecognised",
    };
    text.to_owned()
}

/// Turns a VSS `HRESULT` into an error an operator can act on.
fn vss_error(operation: &str, hr: HRESULT) -> Error {
    let code = hr.0;
    let (why, next) = match code {
        codes::E_ACCESSDENIED => (
            "the Volume Shadow Copy Service refused the request because this process is not running with administrator rights".to_owned(),
            "close MjolnirVSS and start it again, choosing Yes when Windows asks for permission".to_owned(),
        ),
        codes::VSS_E_SNAPSHOT_SET_IN_PROGRESS => (
            "another program is creating a shadow copy right now, and Windows allows only one at a time".to_owned(),
            "wait for the other backup to finish and try again; Windows Backup and File History both use shadow copies".to_owned(),
        ),
        codes::VSS_E_INSUFFICIENT_STORAGE => (
            "there is not enough room for the shadow copy storage area that holds changes made while the backup runs".to_owned(),
            "free up space on the system disk, then try again; a few gigabytes is usually enough".to_owned(),
        ),
        codes::VSS_E_VOLUME_NOT_SUPPORTED | codes::VSS_E_VOLUME_NOT_SUPPORTED_BY_PROVIDER => (
            "one of the volumes cannot be shadow copied, which usually means it is not NTFS or is locked by encryption".to_owned(),
            "check that the Windows volume is NTFS and unlocked, then try again".to_owned(),
        ),
        codes::VSS_E_FLUSH_WRITES_TIMEOUT | codes::VSS_E_HOLD_WRITES_TIMEOUT => (
            "the system was too busy to pause disk writes for the moment it takes to create a consistent snapshot".to_owned(),
            "close programs that are writing heavily, such as virtual machines, backup tools or large downloads, then try again".to_owned(),
        ),
        codes::VSS_E_WRITER_INFRASTRUCTURE | codes::VSS_E_WRITER_NOT_RESPONDING => (
            "one of the Windows components that prepares applications for backup did not respond".to_owned(),
            "restart the computer and try again; if it keeps happening, run `vssadmin list writers` from an administrator command prompt to see which one is failing".to_owned(),
        ),
        codes::VSS_E_UNEXPECTED_WRITER_ERROR => (
            "an application that registers with the shadow copy service failed while preparing for the backup".to_owned(),
            "restart the computer and try again; run `vssadmin list writers` from an administrator command prompt to see which writer is failing".to_owned(),
        ),
        codes::VSS_E_PROVIDER_VETO => (
            "the shadow copy provider refused the request, which normally points at a storage driver or disk problem".to_owned(),
            "check the Windows event log under Application for VSS and VolSnap entries, and check the disk's health".to_owned(),
        ),
        codes::VSS_E_BAD_STATE => (
            "the shadow copy session was asked to do something out of order".to_owned(),
            "this is an internal error; please report it with the command you ran".to_owned(),
        ),
        codes::VSS_E_MAXIMUM_NUMBER_OF_VOLUMES_REACHED => (
            "the snapshot set would hold more volumes than Windows allows at once".to_owned(),
            "back up fewer volumes in one run".to_owned(),
        ),
        _ => (
            format!("the Volume Shadow Copy Service reported {code:#010x}"),
            "check the Windows event log under Application for VSS entries, and try again after a restart".to_owned(),
        ),
    };

    Error::new(
        ExitCode::VssFailure,
        format!("{operation} failed"),
        why,
        next,
    )
}

/// Converts a Rust string into a null terminated wide string.
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Reads a wide string the shadow copy service allocated.
///
/// # Safety
///
/// `ptr` must be null or point at a null terminated wide string that stays
/// valid for the duration of the call.
unsafe fn read_wide(ptr: *const u16) -> String {
    if ptr.is_null() {
        return String::new();
    }
    // SAFETY: the caller guarantees a null terminated wide string. The length
    // is measured before any of it is read as a slice, and the scan stops at
    // the terminator the service wrote.
    unsafe {
        let mut len = 0usize;
        while *ptr.add(len) != 0 {
            len += 1;
            // A string this long is not something the service produces; stop
            // rather than run off the end of an unterminated buffer.
            if len > 32 * 1024 {
                break;
            }
        }
        String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len))
    }
}

/// Guards the COM apartment this session initialised.
struct ComApartment {
    /// Whether this guard is the one that must call `CoUninitialize`.
    owned: bool,
}

impl ComApartment {
    fn enter() -> Result<Self> {
        // SAFETY: CoInitializeEx takes no pointers that have to outlive the
        // call. S_FALSE means the apartment was already initialised by this
        // thread, which still requires a matching CoUninitialize.
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if hr == RPC_E_CHANGED_MODE {
            return Err(Error::new(
                ExitCode::VssFailure,
                "the shadow copy session could not start",
                "this thread has already joined a single threaded COM apartment, and the shadow copy service requires a multi threaded one",
                "this is an internal error; please report it with the command you ran",
            ));
        }
        if hr.is_err() {
            return Err(vss_error("preparing COM for the shadow copy service", hr));
        }
        let owned = hr != S_FALSE;

        // The shadow copy service documents these exact security settings for a
        // backup application. Calling it twice in one process is refused with
        // RPC_E_TOO_LATE, which is harmless: whoever called it first has
        // already set the process wide policy.
        //
        // SAFETY: every pointer argument is null, which the call documents as
        // "use the defaults".
        let hr = unsafe {
            CoInitializeSecurity(
                None,
                -1,
                None,
                None,
                RPC_C_AUTHN_LEVEL_PKT_PRIVACY,
                RPC_C_IMP_LEVEL_IDENTIFY,
                None,
                EOAC_NONE,
                None,
            )
        };
        if let Err(e) = hr {
            if e.code() != RPC_E_TOO_LATE {
                return Err(vss_error(
                    "setting the COM security level for the shadow copy service",
                    e.code(),
                ));
            }
        }

        Ok(Self { owned })
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        if self.owned {
            // SAFETY: paired with the CoInitializeEx in enter, on the same
            // thread, and only when that call was the one that initialised the
            // apartment.
            unsafe { CoUninitialize() };
        }
    }
}

/// A backup shaped shadow copy session.
///
/// Dropping the session releases the shadow copies it created, whether or not
/// the backup succeeded.
pub struct VssSession {
    /// Held so the apartment outlives the interface pointer.
    _apartment: ComApartment,
    backup: *mut sys::IVssBackupComponentsRaw,
    snapshot_set_id: Option<GUID>,
    snapshots: Vec<Snapshot>,
    completed: bool,
}

// SAFETY: the session owns its `IVssBackupComponents` pointer exclusively and
// hands out no references to it. The shadow copy service is entered through a
// multi threaded apartment, which is what the COM rules require for an
// interface to be usable from a thread other than the one that created it, and
// `ComApartment` establishes that before the interface exists. The session is
// used from one thread at a time; moving it to the worker thread that runs the
// backup is the whole reason it is Send.
unsafe impl Send for VssSession {}

/// What MjolnirVSS tells the writers this backup is.
///
/// Named rather than written inline so the choice can be asserted on: a change
/// from copy to full would be invisible in a diff of one argument, and would
/// silently start truncating other people's transaction logs. See
/// [`VssSession::configure`] for why it is copy.
const BACKUP_TYPE: windows::Win32::Storage::Vss::VSS_BACKUP_TYPE = VSS_BT_COPY;

impl VssSession {
    /// Starts a session and gathers writer metadata.
    ///
    /// Requires administrator rights. The error explains that plainly when the
    /// service refuses.
    pub fn begin(cancel: &CancelToken) -> Result<Self> {
        let apartment = ComApartment::enter()?;

        let mut raw: *mut sys::IVssBackupComponentsRaw = std::ptr::null_mut();
        // SAFETY: the out pointer is a valid, writable location for one
        // interface pointer. On success the service writes an interface with
        // one reference, which this type owns and releases on drop.
        let hr = unsafe { sys::CreateVssBackupComponentsInternal(&mut raw) };
        if hr.is_err() || raw.is_null() {
            return Err(vss_error("starting a shadow copy session", hr));
        }

        let mut session = Self {
            _apartment: apartment,
            backup: raw,
            snapshot_set_id: None,
            snapshots: Vec::new(),
            completed: false,
        };

        session.initialize_for_backup()?;
        session.gather_writer_metadata(cancel)?;
        Ok(session)
    }

    /// Starts a session that can only look, not take snapshots.
    ///
    /// Uses the all contexts view so that every shadow copy on the machine is
    /// visible, and skips gathering writer metadata, which querying does not
    /// need and which is the slow part of starting a session.
    pub fn begin_for_query() -> Result<Self> {
        let apartment = ComApartment::enter()?;

        let mut raw: *mut sys::IVssBackupComponentsRaw = std::ptr::null_mut();
        // SAFETY: see begin.
        let hr = unsafe { sys::CreateVssBackupComponentsInternal(&mut raw) };
        if hr.is_err() || raw.is_null() {
            return Err(vss_error("starting a shadow copy session", hr));
        }

        let session = Self {
            _apartment: apartment,
            backup: raw,
            snapshot_set_id: None,
            snapshots: Vec::new(),
            // Nothing to tell the writers about, so the session is already
            // finished as far as the abort path is concerned.
            completed: true,
        };

        // SAFETY: a null XML document asks for a fresh session.
        let hr =
            unsafe { (session.vtable().InitializeForBackup)(session.this(), std::ptr::null()) };
        if hr.is_err() {
            return Err(vss_error("starting a shadow copy session", hr));
        }
        // SAFETY: the context value is one of the documented constants.
        let hr = unsafe { (session.vtable().SetContext)(session.this(), VSS_CTX_ALL.0) };
        if hr.is_err() {
            return Err(vss_error("configuring the shadow copy session", hr));
        }
        Ok(session)
    }

    /// The vtable of the owned interface.
    ///
    /// # Safety
    ///
    /// The caller must only use the returned reference while `self` is alive.
    unsafe fn vtable(&self) -> &sys::IVssBackupComponentsVtbl {
        // SAFETY: `backup` is non null for the whole life of the session,
        // checked when it was created, and the service guarantees the vtable
        // pointer it installed stays valid until the interface is released.
        unsafe { &*(*self.backup).vtable }
    }

    /// The interface pointer as COM expects it.
    fn this(&self) -> *mut core::ffi::c_void {
        self.backup.cast()
    }

    fn initialize_for_backup(&mut self) -> Result<()> {
        // SAFETY: `self.backup` is non null and points at an interface the
        // service created, checked when the session was built, so the vtable
        // slot resolved below is the one vsbackup.h declares at that position.
        // A null XML document is the documented way to ask for a fresh session
        // rather than resume a saved one, so no buffer lifetime is involved.
        let hr = unsafe { (self.vtable().InitializeForBackup)(self.this(), std::ptr::null()) };
        if hr.is_err() {
            return Err(vss_error("starting a shadow copy session", hr));
        }

        // A non persistent, backup context snapshot. This is what makes the
        // service release the shadow copies when the session goes away.
        //
        // SAFETY: the interface is alive for the whole session. The only
        // argument is a plain i32 taken from a constant the windows crate
        // generated from vss.h, so there is no pointer and no range the service
        // could read past.
        let hr = unsafe { (self.vtable().SetContext)(self.this(), VSS_CTX_BACKUP.0) };
        if hr.is_err() {
            return Err(vss_error("configuring the shadow copy session", hr));
        }

        // No component selection, because MjolnirVSS captures whole volumes
        // rather than application components. Bootable system state is
        // requested so writers prepare for a bare metal backup.
        //
        // The type is **copy**, not full, and that is a deliberate and load
        // bearing choice rather than a detail.
        //
        // A full backup, in the words of vss.h, means "each file's backup
        // history will be updated to reflect that it was backed up". Writers
        // act on that: SQL Server and Exchange treat a completed full backup as
        // theirs to account for, and truncate their transaction logs. A copy
        // backup is defined as copying the files "regardless of the state of
        // each file's backup history", and the history "will not be updated".
        //
        // MjolnirVSS takes an image of a disk. It cannot restore a database
        // component, it keeps no backup history, and it is in no position to
        // take responsibility for anybody's log chain. Declaring a full backup
        // would tell every writer on the machine something untrue, and on a
        // server running SQL Server or Exchange the cost of that is somebody
        // else's backup chain broken by a tool that was only meant to be
        // reading.
        //
        // On a machine with no application writers the two are the same. That
        // is exactly why this was easy to get wrong and easy to miss.
        //
        // SAFETY: the interface is alive for the whole session. The three
        // flags are declared `u8` in sys.rs because vsbackup.h declares them as
        // C++ `bool`, which is one byte, not the four byte Win32 `BOOL`; on
        // x64 each still occupies its own register slot and only the low byte
        // is read, so passing 0 and 1 is exactly what `false` and `true` are.
        // The backup type is a constant from vss.h.
        let hr = unsafe {
            (self.vtable().SetBackupState)(
                self.this(),
                0, // bSelectComponents
                1, // bBackupBootableSystemState
                BACKUP_TYPE,
                0, // bPartialFileSupport
            )
        };
        if hr.is_err() {
            return Err(vss_error("configuring the shadow copy session", hr));
        }
        Ok(())
    }

    fn gather_writer_metadata(&mut self, cancel: &CancelToken) -> Result<()> {
        let mut async_ptr: *mut core::ffi::c_void = std::ptr::null_mut();
        // SAFETY: `async_ptr` is a live local, initialised to null, and exactly
        // one pointer wide, which is what the slot writes into. On success the
        // service stores an IVssAsync with one reference in it; that reference
        // is handed to wait_for below, which owns and releases it, so it is
        // neither leaked nor released twice.
        let hr = unsafe { (self.vtable().GatherWriterMetadata)(self.this(), &mut async_ptr) };
        if hr.is_err() {
            return Err(vss_error(
                "asking Windows which applications need preparing",
                hr,
            ));
        }
        wait_for(
            async_ptr,
            cancel,
            "asking Windows which applications need preparing",
        )?;

        // The metadata itself is not used: MjolnirVSS captures whole volumes,
        // not per application components. It is gathered because the writers
        // will not take part in the snapshot otherwise. Freeing it here keeps
        // the session's memory flat.
        //
        // SAFETY: reached only after GatherWriterMetadata returned success,
        // which is the state the documentation requires for this call, and the
        // gathered metadata has not been handed out to anything that could
        // still be holding it. The call takes no arguments beyond the
        // interface.
        let hr = unsafe { (self.vtable().FreeWriterMetadata)(self.this()) };
        if hr.is_err() {
            return Err(vss_error("releasing writer metadata", hr));
        }
        Ok(())
    }

    /// Whether the shadow copy service can snapshot a volume.
    ///
    /// `volume` is a volume GUID path with a trailing backslash, for example
    /// `\\?\Volume{...}\`.
    pub fn is_volume_supported(&self, volume: &str) -> Result<bool> {
        let name = wide(volume);
        let mut supported = FALSE;
        // SAFETY: `name` is a null terminated wide string that outlives the
        // call, the provider GUID of all zeroes selects the default provider,
        // and `supported` is a valid writable BOOL.
        let hr = unsafe {
            (self.vtable().IsVolumeSupported)(
                self.this(),
                GUID::zeroed(),
                name.as_ptr(),
                &mut supported,
            )
        };
        if hr.is_err() {
            // A volume the provider does not handle is an answer, not a
            // failure, so it is reported as "not supported" rather than as an
            // error the operator has to interpret.
            if hr.0 == codes::VSS_E_VOLUME_NOT_SUPPORTED
                || hr.0 == codes::VSS_E_VOLUME_NOT_SUPPORTED_BY_PROVIDER
            {
                return Ok(false);
            }
            return Err(vss_error(
                &format!("checking whether {volume} can be shadow copied"),
                hr,
            ));
        }
        Ok(supported.as_bool())
    }

    /// Creates one coordinated snapshot of every volume in `volumes`.
    ///
    /// All of the volumes are frozen at the same instant, which is what makes a
    /// multi volume Windows installation restorable as a unit.
    pub fn snapshot(&mut self, volumes: &[String], cancel: &CancelToken) -> Result<Vec<Snapshot>> {
        if volumes.is_empty() {
            return Err(Error::new(
                ExitCode::Failure,
                "no volumes were given to snapshot",
                "a shadow copy set has to contain at least one volume",
                "this is an internal error; please report it with the command you ran",
            ));
        }
        if self.snapshot_set_id.is_some() {
            return Err(Error::new(
                ExitCode::Failure,
                "this shadow copy session already holds a snapshot set",
                "one session creates one snapshot set, so that everything in the backup comes from the same instant",
                "this is an internal error; please report it with the command you ran",
            ));
        }

        let mut set_id = GUID::zeroed();
        // SAFETY: `set_id` is a live local of exactly the 16 byte type the slot
        // writes, and it is only read after the call reported success. The
        // session has been initialised for backup and given a context, which is
        // the state StartSnapshotSet requires.
        let hr = unsafe { (self.vtable().StartSnapshotSet)(self.this(), &mut set_id) };
        if hr.is_err() {
            return Err(vss_error("starting a shadow copy set", hr));
        }
        // Recorded before anything else can fail, so that cleanup always knows
        // which set belongs to this process.
        self.snapshot_set_id = Some(set_id);

        let mut pending: Vec<(GUID, String)> = Vec::with_capacity(volumes.len());
        for volume in volumes {
            cancel.check()?;
            let name = wide(volume);
            let mut snapshot_id = GUID::zeroed();
            // SAFETY: `name` is a null terminated wide string held in a local
            // that outlives the call; the service copies it rather than
            // retaining the pointer. An all zero provider GUID is the
            // documented way to ask for the default provider. `snapshot_id` is
            // a live local of the type the slot writes, read only on success.
            let hr = unsafe {
                (self.vtable().AddToSnapshotSet)(
                    self.this(),
                    name.as_ptr(),
                    GUID::zeroed(),
                    &mut snapshot_id,
                )
            };
            if hr.is_err() {
                return Err(vss_error(
                    &format!("adding {volume} to the shadow copy set"),
                    hr,
                ));
            }
            pending.push((snapshot_id, volume.clone()));
        }

        cancel.check()?;
        let mut async_ptr: *mut core::ffi::c_void = std::ptr::null_mut();
        // SAFETY: as for GatherWriterMetadata: a live, null initialised
        // pointer local, and the returned reference is handed to wait_for,
        // which owns it. PrepareForBackup is valid once a snapshot set has been
        // started, which StartSnapshotSet above did.
        let hr = unsafe { (self.vtable().PrepareForBackup)(self.this(), &mut async_ptr) };
        if hr.is_err() {
            return Err(vss_error("preparing applications for the backup", hr));
        }
        wait_for(async_ptr, cancel, "preparing applications for the backup")?;

        cancel.check()?;
        let mut async_ptr: *mut core::ffi::c_void = std::ptr::null_mut();
        // SAFETY: as above. DoSnapshotSet is valid once the writers have been
        // prepared, which the PrepareForBackup wait completed before this line
        // is reached.
        let hr = unsafe { (self.vtable().DoSnapshotSet)(self.this(), &mut async_ptr) };
        if hr.is_err() {
            return Err(vss_error("creating the shadow copy", hr));
        }
        wait_for(async_ptr, cancel, "creating the shadow copy")?;

        let mut snapshots = Vec::with_capacity(pending.len());
        for (snapshot_id, original_volume) in pending {
            snapshots.push(self.properties_of(snapshot_id, original_volume)?);
        }
        self.snapshots = snapshots.clone();
        Ok(snapshots)
    }

    fn properties_of(&self, snapshot_id: GUID, original_volume: String) -> Result<Snapshot> {
        let mut prop = VSS_SNAPSHOT_PROP::default();
        // SAFETY: `prop` is a live, zeroed local of the exact layout the slot
        // writes. The snapshot identifier was returned by AddToSnapshotSet on
        // this same session, so it names a snapshot the service knows about. On
        // success the structure owns several separately allocated wide strings;
        // every one is read before, and freed by, the
        // VssFreeSnapshotPropertiesInternal call below, and the structure is
        // not touched afterwards.
        let hr =
            unsafe { (self.vtable().GetSnapshotProperties)(self.this(), snapshot_id, &mut prop) };
        if hr.is_err() {
            return Err(vss_error("reading the shadow copy details", hr));
        }

        // SAFETY: GetSnapshotProperties succeeded, so each of these fields is
        // either null or a null terminated wide string the service allocated.
        // Both are read here, before the properties are freed below, so the
        // memory is still owned by `prop` at this point. read_wide is
        // documented to accept null.
        let device_object = unsafe { read_wide(prop.m_pwszSnapshotDeviceObject) };
        let reported_volume = unsafe { read_wide(prop.m_pwszOriginalVolumeName) };

        // SAFETY: `prop` was filled by a successful GetSnapshotProperties and
        // has not been freed. Every string in it was copied into owned Strings
        // on the two lines above, so nothing borrows from it, and the structure
        // is not read again. This is the documented release call, and the
        // matching one for that allocation.
        unsafe { sys::VssFreeSnapshotPropertiesInternal(&mut prop) };

        if device_object.is_empty() {
            return Err(Error::new(
                ExitCode::VssFailure,
                "the shadow copy has no device to read from",
                "Windows created the shadow copy but did not report a device path for it, so there is nothing to copy the data out of",
                "restart the computer and try again; if it keeps happening, check the Windows event log under Application for VSS entries",
            ));
        }

        Ok(Snapshot {
            snapshot_id,
            original_volume: if reported_volume.is_empty() {
                original_volume
            } else {
                reported_volume
            },
            device_object,
        })
    }

    /// The shadow copies this session created.
    pub fn snapshots(&self) -> &[Snapshot] {
        &self.snapshots
    }

    /// The identifier of this session's snapshot set, once one exists.
    pub fn snapshot_set_id(&self) -> Option<GUID> {
        self.snapshot_set_id
    }

    /// Whether the shadow copy service still knows about a snapshot.
    ///
    /// This asks the service rather than looking at the device path. A shadow
    /// copy device stays openable inside the process that created it for some
    /// time after the service has released it, so the device disappearing is
    /// not a reliable signal and its continued presence is not evidence of a
    /// leak.
    pub fn snapshot_exists(&self, snapshot_id: GUID) -> Result<bool> {
        let mut prop = VSS_SNAPSHOT_PROP::default();
        // SAFETY: `prop` is a valid writable structure. On success the service
        // fills it with allocated strings, freed immediately below.
        let hr =
            unsafe { (self.vtable().GetSnapshotProperties)(self.this(), snapshot_id, &mut prop) };
        if hr.0 == codes::VSS_E_OBJECT_NOT_FOUND {
            return Ok(false);
        }
        if hr.is_err() {
            return Err(vss_error("checking whether a shadow copy still exists", hr));
        }
        // SAFETY: filled by a successful call and not read afterwards.
        unsafe { sys::VssFreeSnapshotPropertiesInternal(&mut prop) };
        Ok(true)
    }

    /// Lists every shadow copy the service reports in this session's context.
    ///
    /// A session started with [`VssSession::begin_for_query`] sees all of them,
    /// which is what the cleanup command needs in order to show an operator
    /// what is on the machine before touching anything.
    pub fn list_snapshots(&self) -> Result<Vec<Snapshot>> {
        let mut raw_enum: *mut core::ffi::c_void = std::ptr::null_mut();
        // SAFETY: the out pointer is a valid location for one interface
        // pointer. A null queried object identifier with VSS_OBJECT_NONE asks
        // for everything, which is the documented way to enumerate.
        let hr = unsafe {
            (self.vtable().Query)(
                self.this(),
                GUID::zeroed(),
                VSS_OBJECT_NONE,
                VSS_OBJECT_SNAPSHOT,
                &mut raw_enum,
            )
        };
        if hr.0 == codes::VSS_E_OBJECT_NOT_FOUND {
            // No shadow copies at all is an answer, not a failure.
            return Ok(Vec::new());
        }
        if hr.is_err() {
            return Err(vss_error("listing the shadow copies on this computer", hr));
        }
        if raw_enum.is_null() {
            return Ok(Vec::new());
        }

        // SAFETY: the service returned an IVssEnumObject with one reference,
        // which this wrapper now owns and releases when it drops.
        let enumerator: IVssEnumObject = unsafe { IVssEnumObject::from_raw(raw_enum) };

        let mut out = Vec::new();
        loop {
            let mut item = VSS_OBJECT_PROP::default();
            let mut fetched = 0u32;
            // SAFETY: `item` is a valid writable element and `fetched` a valid
            // writable count. The enumerator is alive for the whole loop.
            let stepped = unsafe { enumerator.Next(std::slice::from_mut(&mut item), &mut fetched) };
            if stepped.is_err() || fetched == 0 {
                break;
            }
            if item.Type != VSS_OBJECT_SNAPSHOT {
                continue;
            }

            // SAFETY: the snapshot arm of the union is the live one, which the
            // type field just established.
            let mut snap = unsafe { item.Obj.Snap };
            // SAFETY: both fields are null or null terminated wide strings the
            // service allocated, valid until the properties are freed below.
            let device_object = unsafe { read_wide(snap.m_pwszSnapshotDeviceObject) };
            let original_volume = unsafe { read_wide(snap.m_pwszOriginalVolumeName) };
            let snapshot_id = snap.m_SnapshotId;
            // SAFETY: the structure was filled by the enumerator and is not
            // read again afterwards.
            unsafe { sys::VssFreeSnapshotPropertiesInternal(&mut snap) };

            out.push(Snapshot {
                snapshot_id,
                original_volume,
                device_object,
            });
        }
        Ok(out)
    }

    /// Collects what each writer reported.
    pub fn writer_status(&self, cancel: &CancelToken) -> Result<Vec<WriterStatus>> {
        let mut async_ptr: *mut core::ffi::c_void = std::ptr::null_mut();
        // SAFETY: valid out pointer; ownership of the IVssAsync passes on.
        let hr = unsafe { (self.vtable().GatherWriterStatus)(self.this(), &mut async_ptr) };
        if hr.is_err() {
            return Err(vss_error("collecting the status of each application", hr));
        }
        wait_for(
            async_ptr,
            cancel,
            "collecting the status of each application",
        )?;

        let mut count = 0u32;
        // SAFETY: the interface is alive, and GatherWriterStatus succeeded
        // above, which is the state this call requires. `count` is a live local
        // of the type written.
        let hr = unsafe { (self.vtable().GetWriterStatusCount)(self.this(), &mut count) };
        if hr.is_err() {
            // The status list has to be released even when reading it failed.
            // SAFETY: paired with the GatherWriterStatus that succeeded above.
            // Freeing the status list is required exactly once per successful
            // gather, and this is the only other place that does it, on the
            // error path where the entries were never read.
            let _ = unsafe { (self.vtable().FreeWriterStatus)(self.this()) };
            return Err(vss_error("collecting the status of each application", hr));
        }

        let mut out = Vec::with_capacity(count as usize);
        for i in 0..count {
            let mut instance_id = GUID::zeroed();
            let mut writer_id = GUID::zeroed();
            let mut name = BSTR::new();
            let mut state = VSS_WRITER_STATE::default();
            let mut failure = HRESULT(0);

            // SAFETY: the status list gathered above is still held, so index
            // `i` is in range for the count just read. Every out parameter is a
            // live local of the declared type. The name comes back as a BSTR
            // the service allocated; writing it into `name` transfers ownership
            // to that wrapper, whose Drop calls SysFreeString, so it is neither
            // leaked nor freed twice.
            let hr = unsafe {
                (self.vtable().GetWriterStatus)(
                    self.this(),
                    i,
                    &mut instance_id,
                    &mut writer_id,
                    &mut name,
                    &mut state,
                    &mut failure,
                )
            };
            if hr.is_err() {
                continue;
            }
            out.push(WriterStatus {
                name: name.to_string(),
                writer_id,
                instance_id,
                state: state.0,
                state_text: describe_writer_state(state),
                failure: failure.0,
            });
        }

        // SAFETY: paired with the GatherWriterStatus that succeeded above.
        // Every entry has already been copied into owned Rust values, so
        // nothing borrows from the list the service is about to release.
        let _ = unsafe { (self.vtable().FreeWriterStatus)(self.this()) };
        Ok(out)
    }

    /// Tells the writers the backup finished.
    ///
    /// Called after every byte has been read from the shadow copies. Skipping
    /// it leaves applications believing a backup is still running.
    pub fn complete(&mut self, cancel: &CancelToken) -> Result<()> {
        if self.completed {
            return Ok(());
        }
        let mut async_ptr: *mut core::ffi::c_void = std::ptr::null_mut();
        // SAFETY: valid out pointer; ownership of the IVssAsync passes on.
        let hr = unsafe { (self.vtable().BackupComplete)(self.this(), &mut async_ptr) };
        if hr.is_err() {
            return Err(vss_error("telling applications the backup finished", hr));
        }
        wait_for(
            async_ptr,
            cancel,
            "telling applications the backup finished",
        )?;
        self.completed = true;
        Ok(())
    }

    /// Abandons the backup, telling the writers it did not finish.
    pub fn abort(&mut self) {
        if self.completed {
            return;
        }
        // SAFETY: the interface is alive, and the session was initialised for
        // backup in `begin`, which is the only constructor, so AbortBackup is
        // being called in a state that permits it. `completed` is set
        // immediately afterwards so it cannot run twice.
        let _ = unsafe { (self.vtable().AbortBackup)(self.this()) };
        self.completed = true;
    }

    /// Deletes the snapshot set this session created, and nothing else.
    ///
    /// Returns how many shadow copies were removed. Deleting by snapshot set
    /// identifier is what keeps a shadow copy created by Windows Backup, File
    /// History or another tool from being caught up in the cleanup.
    pub fn delete_own_snapshots(&mut self) -> Result<i32> {
        let Some(set_id) = self.snapshot_set_id.take() else {
            return Ok(0);
        };
        let mut deleted = 0i32;
        let mut first_failure = GUID::zeroed();

        // SAFETY: the interface is alive. `set_id` was taken from this
        // session's own `snapshot_set_id`, which only StartSnapshotSet ever
        // writes, so the object named is a set this process created and no
        // other program's shadow copies are in scope. Both out parameters are
        // live locals of the declared types. bForceDelete is FALSE, so a shadow
        // copy still in use is reported rather than torn away underneath its
        // user.
        let hr = unsafe {
            (self.vtable().DeleteSnapshots)(
                self.this(),
                set_id,
                VSS_OBJECT_SNAPSHOT_SET,
                FALSE,
                &mut deleted,
                &mut first_failure,
            )
        };
        self.snapshots.clear();
        if hr.is_err() {
            return Err(vss_error("removing the temporary shadow copy", hr));
        }
        Ok(deleted)
    }
}

impl Drop for VssSession {
    fn drop(&mut self) {
        // Every exit path ends here, including a panic unwinding through the
        // backup.
        //
        // Order matters, and it is the opposite of what it looks like it should
        // be: the shadow copies are removed first, and only then are the
        // writers told the backup was abandoned. AbortBackup moves the session
        // into a state where DeleteSnapshots is refused, which would leave a
        // shadow copy behind on exactly the path that most needs cleaning up.
        let _ = self.delete_own_snapshots();
        if !self.completed {
            // SAFETY: see abort.
            let _ = unsafe { (self.vtable().AbortBackup)(self.this()) };
            self.completed = true;
        }

        if !self.backup.is_null() {
            // SAFETY: the session owns exactly one reference, taken by
            // CreateVssBackupComponentsInternal and never cloned or handed out,
            // so this Release is the matching decrement and drops the last
            // reference. The pointer is set to null immediately afterwards and
            // Drop runs once, so it cannot be released twice or used after.
            unsafe {
                ((*(*self.backup).vtable).Release)(self.this());
            }
            self.backup = std::ptr::null_mut();
        }
    }
}

/// Waits for a VSS asynchronous operation, staying responsive to cancellation.
///
/// Takes ownership of the raw `IVssAsync` pointer.
fn wait_for(
    async_ptr: *mut core::ffi::c_void,
    cancel: &CancelToken,
    operation: &str,
) -> Result<()> {
    if async_ptr.is_null() {
        // A null async object means the call finished synchronously.
        return Ok(());
    }
    // SAFETY: the caller passed a non null pointer the service produced,
    // carrying one reference. from_raw adopts that reference rather than adding
    // another, so the count stays right, and the wrapper releases it when it
    // drops at the end of this function.
    let async_op: IVssAsync = unsafe { IVssAsync::from_raw(async_ptr) };

    loop {
        if cancel.is_cancelled() {
            // SAFETY: `async_op` owns its reference and is alive here.
            // Cancelling an operation that has already finished is documented
            // as harmless, which matters because the flag can be set at any
            // moment relative to the service's progress.
            let _ = unsafe { async_op.Cancel() };
            return Err(Error::cancelled());
        }

        // A bounded wait rather than an infinite one, so Ctrl+C and the Cancel
        // button are noticed within half a second instead of whenever the
        // service happens to finish.
        // SAFETY: `async_op` owns a reference taken at the top of this
        // function and is alive until it drops at the end, so the interface
        // cannot have been released underneath this call. The only argument is
        // a timeout in milliseconds.
        let waited = unsafe { async_op.Wait(500) };
        if let Err(e) = waited {
            return Err(vss_error(operation, e.code()));
        }

        let mut status = HRESULT(0);
        // SAFETY: `async_op` is alive as above. `status` is a live local of
        // exactly the type written, and the second argument is documented as
        // reserved, which is why null is the correct value rather than a
        // missing one.
        let queried = unsafe { async_op.QueryStatus(&mut status, std::ptr::null_mut()) };
        if let Err(e) = queried {
            return Err(vss_error(operation, e.code()));
        }

        match status.0 {
            codes::VSS_S_ASYNC_PENDING => continue,
            codes::VSS_S_ASYNC_FINISHED => return Ok(()),
            codes::VSS_S_ASYNC_CANCELLED => return Err(Error::cancelled()),
            other if HRESULT(other).is_err() => return Err(vss_error(operation, HRESULT(other))),
            // Any other success code counts as finished.
            _ => return Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wide_strings_are_null_terminated() {
        let w = wide("C:\\");
        assert_eq!(w.last(), Some(&0));
        assert_eq!(w.len(), 4);
        assert_eq!(wide(""), vec![0]);
    }

    #[test]
    fn reading_a_null_wide_string_is_empty_rather_than_a_crash() {
        // SAFETY: read_wide documents null as valid input.
        assert_eq!(unsafe { read_wide(std::ptr::null()) }, "");
    }

    #[test]
    fn reading_a_wide_string_stops_at_the_terminator() {
        let buffer: Vec<u16> = "\\\\?\\GLOBALROOT\\Device\\HarddiskVolumeShadowCopy3"
            .encode_utf16()
            .chain(std::iter::once(0))
            .chain("ignored".encode_utf16())
            .collect();
        // SAFETY: the buffer holds a null terminated wide string and outlives
        // the call.
        let text = unsafe { read_wide(buffer.as_ptr()) };
        assert_eq!(text, "\\\\?\\GLOBALROOT\\Device\\HarddiskVolumeShadowCopy3");
    }

    #[test]
    fn writer_states_are_described_in_words() {
        assert_eq!(describe_writer_state(VSS_WRITER_STATE(1)), "stable");
        assert_eq!(
            describe_writer_state(VSS_WRITER_STATE(9)),
            "failed while freezing"
        );
        assert_eq!(describe_writer_state(VSS_WRITER_STATE(999)), "unrecognised");
    }

    #[test]
    fn a_writer_only_counts_as_successful_when_stable_and_unfailed() {
        let ok = WriterStatus {
            name: "Registry Writer".to_owned(),
            writer_id: GUID::zeroed(),
            instance_id: GUID::zeroed(),
            state: 1,
            state_text: "stable".to_owned(),
            failure: 0,
        };
        assert!(ok.succeeded());

        // Waiting for backup complete is where a healthy writer sits between
        // the snapshot and the end of the backup.
        let waiting = WriterStatus {
            state: 5,
            ..ok.clone()
        };
        assert!(waiting.succeeded());

        let failed_state = WriterStatus {
            state: 9,
            ..ok.clone()
        };
        assert!(!failed_state.succeeded());

        let failed_hresult = WriterStatus {
            failure: codes::VSS_E_WRITER_NOT_RESPONDING,
            ..ok
        };
        assert!(!failed_hresult.succeeded());
    }

    #[test]
    fn access_denied_tells_the_user_to_run_as_administrator() {
        let e = vss_error("creating the shadow copy", HRESULT(codes::E_ACCESSDENIED));
        assert_eq!(e.exit(), ExitCode::VssFailure);
        assert!(e.why().contains("administrator"));
        assert!(e.next_step().contains("permission"));
    }

    #[test]
    fn a_snapshot_already_in_progress_is_explained_not_dumped_as_a_code() {
        let e = vss_error(
            "creating the shadow copy",
            HRESULT(codes::VSS_E_SNAPSHOT_SET_IN_PROGRESS),
        );
        assert!(e.why().contains("another program"));
        assert!(!e.why().contains("0x8004"));
    }

    #[test]
    fn an_unrecognised_code_still_produces_all_three_parts() {
        let e = vss_error("creating the shadow copy", HRESULT(0x8004_2399u32 as i32));
        assert!(!e.what().is_empty());
        assert!(e.why().contains("0x80042399"));
        assert!(!e.next_step().is_empty());
    }

    #[test]
    fn a_null_async_pointer_means_the_call_already_finished() {
        let cancel = CancelToken::new();
        assert!(wait_for(std::ptr::null_mut(), &cancel, "test").is_ok());
    }

    /// A backup that says it is a full backup is telling every writer on the
    /// machine that it has taken responsibility for their data, and SQL Server
    /// and Exchange answer that by truncating their transaction logs.
    /// MjolnirVSS images a disk; it restores no components and keeps no backup
    /// history, so it has no business claiming that. If this assertion ever
    /// fails, somebody has quietly made MjolnirVSS break other people's backup
    /// chains.
    #[test]
    fn the_backup_is_declared_a_copy_and_never_a_full_backup() {
        assert_eq!(BACKUP_TYPE, VSS_BT_COPY);
        assert_ne!(
            BACKUP_TYPE,
            windows::Win32::Storage::Vss::VSS_BT_FULL,
            "a full backup updates every file's backup history and truncates logs"
        );
    }
}
