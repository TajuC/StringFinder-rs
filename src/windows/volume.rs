use super::ffi::{Handle, from_wide, open_path, wide};
use std::path::PathBuf;
use std::ptr::{null, null_mut};
use windows_sys::Win32::Foundation::{
    ERROR_MORE_DATA, ERROR_NOT_ALL_ASSIGNED, GetLastError, HANDLE, INVALID_HANDLE_VALUE, LUID,
};
use windows_sys::Win32::Security::{
    AdjustTokenPrivileges, LUID_AND_ATTRIBUTES, LookupPrivilegeValueW, SE_BACKUP_NAME,
    SE_PRIVILEGE_ENABLED, TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES, TOKEN_QUERY,
};
use windows_sys::Win32::Storage::FileSystem::{
    FindFirstVolumeW, FindNextVolumeW, FindVolumeClose, GetDriveTypeW, GetLogicalDrives,
    GetVolumeInformationW, GetVolumeNameForVolumeMountPointW, GetVolumePathNameW,
    GetVolumePathNamesForVolumeNameW,
};
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::Ioctl::{
    DEVICE_SEEK_PENALTY_DESCRIPTOR, IOCTL_STORAGE_QUERY_PROPERTY, PropertyStandardQuery,
    STORAGE_PROPERTY_QUERY, StorageDeviceSeekPenaltyProperty,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

const DRIVE_REMOVABLE: u32 = 2;
const DRIVE_FIXED: u32 = 3;
const DRIVE_REMOTE: u32 = 4;

fn scannable(root: &str) -> bool {
    let kind = unsafe { GetDriveTypeW(wide(root).as_ptr()) };
    matches!(kind, DRIVE_FIXED | DRIVE_REMOVABLE | DRIVE_REMOTE)
}

pub fn logical_drives() -> Vec<PathBuf> {
    let mask = unsafe { GetLogicalDrives() };
    (0..26u8)
        .filter(|i| mask & (1 << i) != 0)
        .map(|i| format!("{}:\\", (b'A' + i) as char))
        .filter(|root| scannable(root))
        .map(PathBuf::from)
        .collect()
}

fn mount_paths(volume: &[u16]) -> Vec<String> {
    let mut buf = vec![0u16; 512];
    loop {
        let mut needed = 0u32;
        let ok = unsafe {
            GetVolumePathNamesForVolumeNameW(
                volume.as_ptr(),
                buf.as_mut_ptr(),
                buf.len() as u32,
                &mut needed,
            )
        };
        if ok != 0 {
            break;
        }
        if unsafe { GetLastError() } != ERROR_MORE_DATA || needed as usize <= buf.len() {
            return Vec::new();
        }
        buf.resize(needed as usize, 0);
    }
    buf.split(|&c| c == 0)
        .take_while(|s| !s.is_empty())
        .map(from_wide)
        .collect()
}

pub fn mounted_folders() -> Vec<PathBuf> {
    let mut name = [0u16; 64];
    let find = unsafe { FindFirstVolumeW(name.as_mut_ptr(), name.len() as u32) };
    if find == INVALID_HANDLE_VALUE {
        return Vec::new();
    }
    let mut folders = Vec::new();
    loop {
        for path in mount_paths(&name) {
            let drive_root = path.len() == 3 && path.ends_with(":\\");
            if !drive_root && scannable(&path) {
                folders.push(PathBuf::from(path));
            }
        }
        if unsafe { FindNextVolumeW(find, name.as_mut_ptr(), name.len() as u32) } == 0 {
            break;
        }
    }
    unsafe { FindVolumeClose(find) };
    folders
}

pub fn mount_point(path: &str) -> Option<String> {
    let path_w = wide(path);
    let mut mount = [0u16; 1024];
    if unsafe { GetVolumePathNameW(path_w.as_ptr(), mount.as_mut_ptr(), mount.len() as u32) } == 0 {
        return None;
    }
    Some(from_wide(trim_nul(&mount)).to_lowercase())
}

pub struct Volume {
    pub device: Option<String>,
    pub filesystem: String,
}

fn trim_nul(buf: &[u16]) -> &[u16] {
    &buf[..buf.iter().position(|&c| c == 0).unwrap_or(buf.len())]
}

pub fn volume_of(path: &str) -> Option<Volume> {
    let path_w = wide(path);
    let mut mount = [0u16; 1024];
    if unsafe { GetVolumePathNameW(path_w.as_ptr(), mount.as_mut_ptr(), mount.len() as u32) } == 0 {
        return None;
    }
    let mount = trim_nul(&mount).to_vec();
    let mount_z: Vec<u16> = mount.iter().copied().chain(Some(0)).collect();

    let mut fs = [0u16; 64];
    let ok = unsafe {
        GetVolumeInformationW(
            mount_z.as_ptr(),
            null_mut(),
            0,
            null_mut(),
            null_mut(),
            null_mut(),
            fs.as_mut_ptr(),
            fs.len() as u32,
        )
    };
    let filesystem = if ok != 0 {
        from_wide(trim_nul(&fs))
    } else {
        String::new()
    };

    let mut guid = [0u16; 64];
    let device = (unsafe {
        GetVolumeNameForVolumeMountPointW(mount_z.as_ptr(), guid.as_mut_ptr(), guid.len() as u32)
    } != 0)
        .then(|| from_wide(trim_nul(&guid)).trim_end_matches('\\').to_owned());

    Some(Volume { device, filesystem })
}

pub fn seek_penalty(device: &str) -> Option<bool> {
    let volume = open_path(device, 0).ok()?;
    let query = STORAGE_PROPERTY_QUERY {
        PropertyId: StorageDeviceSeekPenaltyProperty,
        QueryType: PropertyStandardQuery,
        AdditionalParameters: [0],
    };
    let mut out = DEVICE_SEEK_PENALTY_DESCRIPTOR::default();
    let mut returned = 0u32;
    let ok = unsafe {
        DeviceIoControl(
            volume.raw(),
            IOCTL_STORAGE_QUERY_PROPERTY,
            (&query as *const STORAGE_PROPERTY_QUERY).cast(),
            size_of::<STORAGE_PROPERTY_QUERY>() as u32,
            (&mut out as *mut DEVICE_SEEK_PENALTY_DESCRIPTOR).cast(),
            size_of::<DEVICE_SEEK_PENALTY_DESCRIPTOR>() as u32,
            &mut returned,
            null_mut(),
        )
    };
    (ok != 0 && returned as usize >= size_of::<DEVICE_SEEK_PENALTY_DESCRIPTOR>())
        .then_some(out.IncursSeekPenalty)
}

pub fn enable_backup_privilege() -> bool {
    let mut token: HANDLE = null_mut();
    if unsafe {
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
            &mut token,
        )
    } == 0
    {
        return false;
    }
    let token = Handle::wrap(token);
    let mut luid = LUID::default();
    if unsafe { LookupPrivilegeValueW(null(), SE_BACKUP_NAME, &mut luid) } == 0 {
        return false;
    }
    let privileges = TOKEN_PRIVILEGES {
        PrivilegeCount: 1,
        Privileges: [LUID_AND_ATTRIBUTES {
            Luid: luid,
            Attributes: SE_PRIVILEGE_ENABLED,
        }],
    };
    let ok =
        unsafe { AdjustTokenPrivileges(token.raw(), 0, &privileges, 0, null_mut(), null_mut()) };
    ok != 0 && unsafe { GetLastError() } != ERROR_NOT_ALL_ASSIGNED
}
