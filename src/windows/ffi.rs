use std::cell::RefCell;
use std::ffi::OsStr;
use std::fs::File;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::ptr::{null, null_mut};
use std::time::Duration;
use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
use windows_sys::Wdk::Storage::FileSystem::{
    FILE_DIRECTORY_FILE, FILE_FULL_DIR_INFORMATION, FILE_NON_DIRECTORY_FILE, FILE_OPEN,
    FILE_OPEN_FOR_BACKUP_INTENT, FILE_SEQUENTIAL_ONLY, FILE_SYNCHRONOUS_IO_NONALERT,
    FileFullDirectoryInformation, NtCreateFile, NtQueryDirectoryFile,
};
use windows_sys::Win32::Foundation::{
    CloseHandle, HANDLE, INVALID_HANDLE_VALUE, NTSTATUS, RtlNtStatusToDosError,
    STATUS_NO_MORE_FILES, STATUS_NO_SUCH_FILE, UNICODE_STRING,
};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, CreateFileW, FILE_ATTRIBUTE_REPARSE_POINT,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_ID_INFO, FILE_LIST_DIRECTORY, FILE_READ_DATA,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FileIdInfo, GetFileInformationByHandle,
    GetFileInformationByHandleEx, GetFileSizeEx, OPEN_EXISTING, SYNCHRONIZE,
};
use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;

const SHARE_ALL: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;
const LIST_BUFFER_WORDS: usize = 64 * 1024 / 8;

pub struct Handle(HANDLE);

unsafe impl Send for Handle {}
unsafe impl Sync for Handle {}

impl Handle {
    fn checked(raw: HANDLE) -> io::Result<Handle> {
        if raw.is_null() || raw == INVALID_HANDLE_VALUE {
            Err(io::Error::last_os_error())
        } else {
            Ok(Handle(raw))
        }
    }

    pub fn wrap(raw: HANDLE) -> Handle {
        Handle(raw)
    }

    pub fn raw(&self) -> HANDLE {
        self.0
    }

    pub fn into_file(self) -> File {
        let raw = self.0;
        std::mem::forget(self);
        unsafe { File::from_raw_handle(raw) }
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

pub fn wide(s: impl AsRef<OsStr>) -> Vec<u16> {
    s.as_ref().encode_wide().chain(Some(0)).collect()
}

pub fn from_wide(s: &[u16]) -> String {
    String::from_utf16_lossy(s)
}

fn nt_error(status: NTSTATUS) -> io::Error {
    io::Error::from_raw_os_error(unsafe { RtlNtStatusToDosError(status) } as i32)
}

pub fn verbatim(path: &str) -> String {
    if path.starts_with(r"\\?\") || path.starts_with(r"\\.\") {
        path.to_owned()
    } else if let Some(unc) = path.strip_prefix(r"\\") {
        format!(r"\\?\UNC\{unc}")
    } else {
        format!(r"\\?\{path}")
    }
}

pub fn open_path(path: &str, access: u32) -> io::Result<Handle> {
    let name = wide(path);
    let raw = unsafe {
        CreateFileW(
            name.as_ptr(),
            access,
            SHARE_ALL,
            null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            null_mut(),
        )
    };
    Handle::checked(raw)
}

pub fn open_dir(path: &str) -> io::Result<Handle> {
    open_path(&verbatim(path), FILE_LIST_DIRECTORY | SYNCHRONIZE)
}

pub fn open_relative(parent: &Handle, name: &[u16], directory: bool) -> io::Result<Handle> {
    let bytes = (name.len() * 2) as u16;
    let object = UNICODE_STRING {
        Length: bytes,
        MaximumLength: bytes,
        Buffer: name.as_ptr() as *mut u16,
    };
    let attributes = OBJECT_ATTRIBUTES {
        Length: size_of::<OBJECT_ATTRIBUTES>() as u32,
        RootDirectory: parent.raw(),
        ObjectName: &object,
        Attributes: 0,
        SecurityDescriptor: null(),
        SecurityQualityOfService: null(),
    };
    let (access, options) = if directory {
        (
            FILE_LIST_DIRECTORY | SYNCHRONIZE,
            FILE_DIRECTORY_FILE | FILE_SYNCHRONOUS_IO_NONALERT | FILE_OPEN_FOR_BACKUP_INTENT,
        )
    } else {
        (
            FILE_READ_DATA | SYNCHRONIZE,
            FILE_NON_DIRECTORY_FILE
                | FILE_SYNCHRONOUS_IO_NONALERT
                | FILE_SEQUENTIAL_ONLY
                | FILE_OPEN_FOR_BACKUP_INTENT,
        )
    };
    let mut raw = null_mut();
    let mut iosb = IO_STATUS_BLOCK::default();
    let status = unsafe {
        NtCreateFile(
            &mut raw,
            access,
            &attributes,
            &mut iosb,
            null(),
            0,
            SHARE_ALL,
            FILE_OPEN,
            options,
            null(),
            0,
        )
    };
    if status < 0 {
        Err(nt_error(status))
    } else {
        Ok(Handle(raw))
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Identity {
    volume_serial: u64,
    file_id: u128,
}

pub fn identity(handle: &Handle) -> io::Result<Identity> {
    let mut info = FILE_ID_INFO::default();
    let ok = unsafe {
        GetFileInformationByHandleEx(
            handle.raw(),
            FileIdInfo,
            (&mut info as *mut FILE_ID_INFO).cast(),
            size_of::<FILE_ID_INFO>() as u32,
        )
    };
    if ok != 0 {
        return Ok(Identity {
            volume_serial: info.VolumeSerialNumber,
            file_id: u128::from_le_bytes(info.FileId.Identifier),
        });
    }
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    if unsafe { GetFileInformationByHandle(handle.raw(), &mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(Identity {
        volume_serial: u64::from(info.dwVolumeSerialNumber),
        file_id: (u128::from(info.nFileIndexHigh) << 32) | u128::from(info.nFileIndexLow),
    })
}

pub fn file_size(file: &File) -> io::Result<u64> {
    let mut size = 0i64;
    if unsafe { GetFileSizeEx(file.as_raw_handle(), &mut size) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(size as u64)
}

pub struct DirEntry<'a> {
    pub name: &'a [u16],
    pub attributes: u32,
    pub size: u64,
    pub reparse_tag: u32,
}

thread_local! {
    static LIST_BUFFER: RefCell<Vec<u64>> = RefCell::new(vec![0u64; LIST_BUFFER_WORDS]);
}

pub fn list_dir(dir: &Handle, mut each: impl FnMut(DirEntry<'_>)) -> io::Result<()> {
    LIST_BUFFER.with_borrow_mut(|buf| {
        loop {
            let mut iosb = IO_STATUS_BLOCK::default();
            let status = unsafe {
                NtQueryDirectoryFile(
                    dir.raw(),
                    null_mut(),
                    None,
                    null(),
                    &mut iosb,
                    buf.as_mut_ptr().cast(),
                    (buf.len() * 8) as u32,
                    FileFullDirectoryInformation,
                    false,
                    null(),
                    false,
                )
            };
            if status == STATUS_NO_MORE_FILES || status == STATUS_NO_SUCH_FILE {
                return Ok(());
            }
            if status < 0 {
                return Err(nt_error(status));
            }
            if iosb.Information == 0 {
                return Ok(());
            }

            let base = buf.as_ptr().cast::<u8>();
            let mut offset = 0usize;
            loop {
                let entry = unsafe { &*base.add(offset).cast::<FILE_FULL_DIR_INFORMATION>() };
                let name = unsafe {
                    std::slice::from_raw_parts(
                        std::ptr::addr_of!(entry.FileName).cast::<u16>(),
                        entry.FileNameLength as usize / 2,
                    )
                };
                if name != [b'.' as u16] && name != [b'.' as u16, b'.' as u16] {
                    let reparse = entry.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0;
                    each(DirEntry {
                        name,
                        attributes: entry.FileAttributes,
                        size: entry.EndOfFile as u64,
                        reparse_tag: if reparse { entry.EaSize } else { 0 },
                    });
                }
                if entry.NextEntryOffset == 0 {
                    break;
                }
                offset += entry.NextEntryOffset as usize;
            }
        }
    })
}

pub fn cpu_time() -> Option<(Duration, Duration)> {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
    let mut times = [FILETIME::default(); 4];
    let [created, exited, kernel, user] = &mut times;
    if unsafe { GetProcessTimes(GetCurrentProcess(), created, exited, kernel, user) } == 0 {
        return None;
    }
    let ticks = |t: &FILETIME| (u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime);
    let span = |t: &FILETIME| Duration::from_nanos(ticks(t) * 100);
    Some((span(kernel), span(user)))
}

pub fn physical_cores() -> Option<usize> {
    use windows_sys::Win32::System::SystemInformation::{
        GetLogicalProcessorInformationEx, RelationProcessorCore,
        SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX,
    };
    let mut len = 0u32;
    unsafe { GetLogicalProcessorInformationEx(RelationProcessorCore, null_mut(), &mut len) };
    if len == 0 {
        return None;
    }
    let mut buf = vec![0u64; (len as usize).div_ceil(8)];
    let ok = unsafe {
        GetLogicalProcessorInformationEx(
            RelationProcessorCore,
            buf.as_mut_ptr()
                .cast::<SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX>(),
            &mut len,
        )
    };
    if ok == 0 {
        return None;
    }
    let bytes = unsafe { std::slice::from_raw_parts(buf.as_ptr().cast::<u8>(), len as usize) };
    let mut cores = 0;
    let mut offset = 0;
    while offset + 8 <= bytes.len() {
        let size = u32::from_le_bytes(bytes[offset + 4..offset + 8].try_into().ok()?) as usize;
        if size == 0 {
            break;
        }
        cores += 1;
        offset += size;
    }
    (cores > 0).then_some(cores)
}
