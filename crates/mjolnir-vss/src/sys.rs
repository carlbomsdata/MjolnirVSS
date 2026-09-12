//! Raw declarations for `IVssBackupComponents`.
//!
//! # Why this file exists
//!
//! The `windows` crate carries the Volume Shadow Copy Service *types*
//! (`VSS_SNAPSHOT_PROP`, `VSS_CTX_BACKUP`, `IVssAsync` and so on) but not
//! `IVssBackupComponents`, because the Win32 metadata project does not describe
//! `vsbackup.h`. That header declares the interface as a C++ class rather than
//! through MIDL, so there is nothing for the generator to read.
//!
//! Rather than add a C++ bridge, the interface is declared here directly. It is
//! an ordinary COM interface deriving from `IUnknown`, so its binary layout is
//! the three `IUnknown` slots followed by its own methods in declaration order.
//!
//! # How the layout was established
//!
//! The vtable below mirrors, slot for slot, the declaration order of
//! `class IVssBackupComponents : public IUnknown` in the Windows SDK header
//! `um/vsbackup.h`. The order is reproduced in the `SLOT_NAMES` table at the end
//! of this file so that a reviewer can check it against the header without
//! reading the struct, and so the count is asserted by a test.
//!
//! Getting a slot wrong would mean calling the wrong function, so slots this
//! crate does not use are typed as opaque pointers rather than as callable
//! signatures. They exist only to occupy the right position.
//!
//! # Safety of the whole module
//!
//! Every function here is `unsafe` and every caller is inside this crate. The
//! safe wrapper in `lib.rs` is the only thing the rest of MjolnirVSS sees.

#![allow(non_snake_case)]

use core::ffi::c_void;

use windows::core::{BOOL, BSTR, GUID, HRESULT};
use windows::Win32::Storage::Vss::{
    VSS_BACKUP_TYPE, VSS_OBJECT_TYPE, VSS_SNAPSHOT_PROP, VSS_WRITER_STATE,
};

/// A vtable slot MjolnirVSS never calls.
///
/// Typed as a plain pointer on purpose: an unused slot with a plausible looking
/// function signature is an invitation to call it by mistake, and calling
/// through a wrong signature is undefined behaviour.
pub type UnusedSlot = *const c_void;

/// The `IVssBackupComponents` virtual function table.
///
/// The field order is the binary layout of the interface and must not be
/// rearranged. See the module comment for where the order comes from.
#[repr(C)]
pub struct IVssBackupComponentsVtbl {
    // --- IUnknown ---
    /// `IUnknown::QueryInterface`.
    pub QueryInterface:
        unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT,
    /// `IUnknown::AddRef`.
    pub AddRef: unsafe extern "system" fn(*mut c_void) -> u32,
    /// `IUnknown::Release`.
    pub Release: unsafe extern "system" fn(*mut c_void) -> u32,

    // --- IVssBackupComponents, in vsbackup.h declaration order ---
    /// 1
    pub GetWriterComponentsCount: UnusedSlot,
    /// 2
    pub GetWriterComponents: UnusedSlot,
    /// 3
    pub InitializeForBackup: unsafe extern "system" fn(*mut c_void, *const u16) -> HRESULT,
    /// 4. The three flags are C++ `bool`, one byte each, not the four byte
    ///    Win32 `BOOL`.
    pub SetBackupState:
        unsafe extern "system" fn(*mut c_void, u8, u8, VSS_BACKUP_TYPE, u8) -> HRESULT,
    /// 5
    pub InitializeForRestore: UnusedSlot,
    /// 6
    pub SetRestoreState: UnusedSlot,
    /// 7
    pub GatherWriterMetadata: unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> HRESULT,
    /// 8
    pub GetWriterMetadataCount: unsafe extern "system" fn(*mut c_void, *mut u32) -> HRESULT,
    /// 9
    pub GetWriterMetadata: UnusedSlot,
    /// 10
    pub FreeWriterMetadata: unsafe extern "system" fn(*mut c_void) -> HRESULT,
    /// 11
    pub AddComponent: UnusedSlot,
    /// 12
    pub PrepareForBackup: unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> HRESULT,
    /// 13
    pub AbortBackup: unsafe extern "system" fn(*mut c_void) -> HRESULT,
    /// 14
    pub GatherWriterStatus: unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> HRESULT,
    /// 15
    pub GetWriterStatusCount: unsafe extern "system" fn(*mut c_void, *mut u32) -> HRESULT,
    /// 16
    pub FreeWriterStatus: unsafe extern "system" fn(*mut c_void) -> HRESULT,
    /// 17
    pub GetWriterStatus: unsafe extern "system" fn(
        *mut c_void,
        u32,
        *mut GUID,
        *mut GUID,
        *mut BSTR,
        *mut VSS_WRITER_STATE,
        *mut HRESULT,
    ) -> HRESULT,
    /// 18
    pub SetBackupSucceeded: UnusedSlot,
    /// 19
    pub SetBackupOptions: UnusedSlot,
    /// 20
    pub SetSelectedForRestore: UnusedSlot,
    /// 21
    pub SetRestoreOptions: UnusedSlot,
    /// 22
    pub SetAdditionalRestores: UnusedSlot,
    /// 23
    pub SetPreviousBackupStamp: UnusedSlot,
    /// 24
    pub SaveAsXML: UnusedSlot,
    /// 25
    pub BackupComplete: unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> HRESULT,
    /// 26
    pub AddAlternativeLocationMapping: UnusedSlot,
    /// 27
    pub AddRestoreSubcomponent: UnusedSlot,
    /// 28
    pub SetFileRestoreStatus: UnusedSlot,
    /// 29
    pub AddNewTarget: UnusedSlot,
    /// 30
    pub SetRangesFilePath: UnusedSlot,
    /// 31
    pub PreRestore: UnusedSlot,
    /// 32
    pub PostRestore: UnusedSlot,
    /// 33
    pub SetContext: unsafe extern "system" fn(*mut c_void, i32) -> HRESULT,
    /// 34
    pub StartSnapshotSet: unsafe extern "system" fn(*mut c_void, *mut GUID) -> HRESULT,
    /// 35
    pub AddToSnapshotSet:
        unsafe extern "system" fn(*mut c_void, *const u16, GUID, *mut GUID) -> HRESULT,
    /// 36
    pub DoSnapshotSet: unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> HRESULT,
    /// 37
    pub DeleteSnapshots: unsafe extern "system" fn(
        *mut c_void,
        GUID,
        VSS_OBJECT_TYPE,
        BOOL,
        *mut i32,
        *mut GUID,
    ) -> HRESULT,
    /// 38
    pub ImportSnapshots: UnusedSlot,
    /// 39
    pub BreakSnapshotSet: UnusedSlot,
    /// 40
    pub GetSnapshotProperties:
        unsafe extern "system" fn(*mut c_void, GUID, *mut VSS_SNAPSHOT_PROP) -> HRESULT,
    /// 41
    pub Query: unsafe extern "system" fn(
        *mut c_void,
        GUID,
        VSS_OBJECT_TYPE,
        VSS_OBJECT_TYPE,
        *mut *mut c_void,
    ) -> HRESULT,
    /// 42
    pub IsVolumeSupported:
        unsafe extern "system" fn(*mut c_void, GUID, *const u16, *mut BOOL) -> HRESULT,
    /// 43
    pub DisableWriterClasses: UnusedSlot,
    /// 44
    pub EnableWriterClasses: UnusedSlot,
    /// 45
    pub DisableWriterInstances: UnusedSlot,
    /// 46
    pub ExposeSnapshot: UnusedSlot,
    /// 47
    pub RevertToSnapshot: UnusedSlot,
    /// 48
    pub QueryRevertStatus: UnusedSlot,
}

/// The names of the interface's own slots, in vtable order.
///
/// Kept so a reviewer can compare this binding against `vsbackup.h` in one
/// glance, and so the slot count is checked by a test rather than by eye.
pub const SLOT_NAMES: [&str; 48] = [
    "GetWriterComponentsCount",
    "GetWriterComponents",
    "InitializeForBackup",
    "SetBackupState",
    "InitializeForRestore",
    "SetRestoreState",
    "GatherWriterMetadata",
    "GetWriterMetadataCount",
    "GetWriterMetadata",
    "FreeWriterMetadata",
    "AddComponent",
    "PrepareForBackup",
    "AbortBackup",
    "GatherWriterStatus",
    "GetWriterStatusCount",
    "FreeWriterStatus",
    "GetWriterStatus",
    "SetBackupSucceeded",
    "SetBackupOptions",
    "SetSelectedForRestore",
    "SetRestoreOptions",
    "SetAdditionalRestores",
    "SetPreviousBackupStamp",
    "SaveAsXML",
    "BackupComplete",
    "AddAlternativeLocationMapping",
    "AddRestoreSubcomponent",
    "SetFileRestoreStatus",
    "AddNewTarget",
    "SetRangesFilePath",
    "PreRestore",
    "PostRestore",
    "SetContext",
    "StartSnapshotSet",
    "AddToSnapshotSet",
    "DoSnapshotSet",
    "DeleteSnapshots",
    "ImportSnapshots",
    "BreakSnapshotSet",
    "GetSnapshotProperties",
    "Query",
    "IsVolumeSupported",
    "DisableWriterClasses",
    "EnableWriterClasses",
    "DisableWriterInstances",
    "ExposeSnapshot",
    "RevertToSnapshot",
    "QueryRevertStatus",
];

/// An `IVssBackupComponents` instance: a pointer to its vtable pointer.
#[repr(C)]
pub struct IVssBackupComponentsRaw {
    /// Pointer to the interface's virtual function table, installed by the
    /// shadow copy service when the object was created.
    pub vtable: *const IVssBackupComponentsVtbl,
}

#[link(name = "vssapi")]
extern "system" {
    /// Creates an `IVssBackupComponents`.
    ///
    /// `vsbackup.h` exposes this through an inline `CreateVssBackupComponents`
    /// that forwards to this exported symbol, which is the one the import
    /// library actually carries.
    pub fn CreateVssBackupComponentsInternal(
        ppBackup: *mut *mut IVssBackupComponentsRaw,
    ) -> HRESULT;

    /// Frees the strings inside a `VSS_SNAPSHOT_PROP`.
    ///
    /// `GetSnapshotProperties` hands back a structure holding several
    /// separately allocated wide strings. Dropping the structure without this
    /// leaks all of them.
    pub fn VssFreeSnapshotPropertiesInternal(pProp: *mut VSS_SNAPSHOT_PROP);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_vtable_has_the_expected_number_of_slots() {
        // Three IUnknown slots plus the interface's own methods. If this ever
        // fails, the struct and the audit table have drifted apart and the
        // binding can no longer be trusted.
        let expected = (3 + SLOT_NAMES.len()) * core::mem::size_of::<*const c_void>();
        assert_eq!(core::mem::size_of::<IVssBackupComponentsVtbl>(), expected);
    }

    #[test]
    fn the_slot_names_are_unique() {
        let mut sorted = SLOT_NAMES;
        sorted.sort_unstable();
        let mut deduped = sorted.to_vec();
        deduped.dedup();
        assert_eq!(deduped.len(), SLOT_NAMES.len());
    }

    #[test]
    fn the_methods_this_crate_calls_sit_where_the_header_puts_them() {
        // Spot checks against vsbackup.h, counting the interface's own methods
        // from one. A mistake here would mean calling a different function.
        assert_eq!(SLOT_NAMES[2], "InitializeForBackup"); // 3rd
        assert_eq!(SLOT_NAMES[3], "SetBackupState"); // 4th
        assert_eq!(SLOT_NAMES[6], "GatherWriterMetadata"); // 7th
        assert_eq!(SLOT_NAMES[11], "PrepareForBackup"); // 12th
        assert_eq!(SLOT_NAMES[12], "AbortBackup"); // 13th
        assert_eq!(SLOT_NAMES[16], "GetWriterStatus"); // 17th
        assert_eq!(SLOT_NAMES[24], "BackupComplete"); // 25th
        assert_eq!(SLOT_NAMES[32], "SetContext"); // 33rd
        assert_eq!(SLOT_NAMES[33], "StartSnapshotSet"); // 34th
        assert_eq!(SLOT_NAMES[34], "AddToSnapshotSet"); // 35th
        assert_eq!(SLOT_NAMES[35], "DoSnapshotSet"); // 36th
        assert_eq!(SLOT_NAMES[36], "DeleteSnapshots"); // 37th
        assert_eq!(SLOT_NAMES[39], "GetSnapshotProperties"); // 40th
        assert_eq!(SLOT_NAMES[40], "Query"); // 41st
        assert_eq!(SLOT_NAMES[41], "IsVolumeSupported"); // 42nd
    }
}
