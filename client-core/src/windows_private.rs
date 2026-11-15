//! Private local files and locks backed by Windows handles and access controls.
//! Current user, SYSTEM, and local administrators are the only allowed readers.

use std::{
    fs::File,
    io,
    mem::size_of,
    os::windows::{
        ffi::OsStrExt,
        fs::MetadataExt,
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
    },
    path::Path,
    ptr::{null, null_mut},
};
use windows_sys::Win32::{
    Foundation::{LocalFree, GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE},
    Security::{
        Authorization::{
            ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
            GetSecurityInfo, SE_FILE_OBJECT,
        },
        GetAce, GetTokenInformation, IsValidSid, TokenUser, ACCESS_ALLOWED_ACE, ACE_HEADER,
        DACL_SECURITY_INFORMATION, INHERIT_ONLY_ACE, OWNER_SECURITY_INFORMATION,
        SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
    },
    Storage::FileSystem::{
        CreateDirectoryW, CreateFileW, LockFileEx, CREATE_NEW, DELETE, FILE_ATTRIBUTE_NORMAL,
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_DELETE_ON_CLOSE,
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY, OPEN_EXISTING, READ_CONTROL,
    },
    System::{
        Threading::{GetCurrentProcess, OpenProcessToken},
        IO::OVERLAPPED,
    },
};

fn denied(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, message)
}

struct LocalMemory(*mut std::ffi::c_void);
impl Drop for LocalMemory {
    fn drop(&mut self) {
        // These pointers come only from APIs documented to allocate with LocalAlloc.
        unsafe {
            LocalFree(self.0);
        }
    }
}

fn wide(path: &Path) -> io::Result<Vec<u16>> {
    let mut path: Vec<_> = path.as_os_str().encode_wide().collect();
    if path.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Path contains a NUL",
        ));
    }
    path.push(0);
    Ok(path)
}

fn sid_text(sid: *mut std::ffi::c_void) -> io::Result<String> {
    if sid.is_null() || unsafe { IsValidSid(sid) } == 0 {
        return Err(denied("Invalid file security identifier"));
    }
    let mut text = null_mut();
    if unsafe { ConvertSidToStringSidW(sid, &mut text) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let _memory = LocalMemory(text.cast());
    let mut len = 0;
    // ConvertSidToStringSidW returns a NUL-terminated UTF-16 allocation.
    while unsafe { *text.add(len) } != 0 {
        len += 1;
    }
    Ok(String::from_utf16_lossy(unsafe {
        std::slice::from_raw_parts(text, len)
    }))
}

fn current_user_sid() -> io::Result<String> {
    let mut token = null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let token = unsafe { OwnedHandle::from_raw_handle(token) };
    let mut bytes = 0;
    unsafe {
        GetTokenInformation(token.as_raw_handle(), TokenUser, null_mut(), 0, &mut bytes);
    }
    if bytes < size_of::<TOKEN_USER>() as u32 || bytes > 65536 {
        return Err(denied("Cannot inspect current user"));
    }
    // Word alignment also satisfies TOKEN_USER and its embedded SID.
    let mut storage = vec![0usize; (bytes as usize).div_ceil(size_of::<usize>())];
    if unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            storage.as_mut_ptr().cast(),
            bytes,
            &mut bytes,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let user = unsafe { &*storage.as_ptr().cast::<TOKEN_USER>() };
    sid_text(user.User.Sid)
}

fn private_descriptor() -> io::Result<LocalMemory> {
    let sid = current_user_sid()?;
    // Disable inherited grants; child files inherit only these trusted principals.
    let sddl: Vec<_> = format!("D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;FA;;;{sid})")
        .encode_utf16()
        .chain(Some(0))
        .collect();
    let mut descriptor = null_mut();
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            1,
            &mut descriptor,
            null_mut(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(LocalMemory(descriptor))
}

fn validate_acl(file: &File) -> io::Result<()> {
    let current = current_user_sid()?;
    let trusted = |sid: *mut std::ffi::c_void| -> io::Result<bool> {
        let sid = sid_text(sid)?;
        Ok(sid == current || sid == "S-1-5-18" || sid == "S-1-5-32-544")
    };
    let mut owner = null_mut();
    let mut dacl = null_mut();
    let mut descriptor = null_mut();
    let status = unsafe {
        GetSecurityInfo(
            file.as_raw_handle(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            null_mut(),
            &mut dacl,
            null_mut(),
            &mut descriptor,
        )
    };
    if status != 0 {
        return Err(io::Error::from_raw_os_error(status as i32));
    }
    let _memory = LocalMemory(descriptor);
    if !trusted(owner)? || dacl.is_null() {
        return Err(denied(
            "File must have a private, owner-controlled Windows ACL",
        ));
    }
    for index in 0..u32::from(unsafe { (*dacl).AceCount }) {
        let mut ace = null_mut();
        if unsafe { GetAce(dacl, index, &mut ace) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let header = unsafe { &*ace.cast::<ACE_HEADER>() };
        // Inherit-only entries do not grant access to this object; deny entries
        // can only restrict access. Unrecognized grant formats fail closed.
        if u32::from(header.AceFlags) & INHERIT_ONLY_ACE != 0 || header.AceType == 1 {
            continue;
        }
        if header.AceType != 0 || usize::from(header.AceSize) < size_of::<ACCESS_ALLOWED_ACE>() {
            return Err(denied("Unsupported private file access control entry"));
        }
        let allow = ace.cast::<ACCESS_ALLOWED_ACE>();
        if unsafe { (*allow).Mask } != 0
            && !trusted(unsafe { std::ptr::addr_of_mut!((*allow).SidStart).cast() })?
        {
            return Err(denied(
                "File access is granted to another Windows user or group",
            ));
        }
    }
    Ok(())
}

fn open_existing(path: &Path, access: u32, directory: bool) -> io::Result<File> {
    let path = wide(path)?;
    let flags = FILE_FLAG_OPEN_REPARSE_POINT
        | if directory {
            FILE_FLAG_BACKUP_SEMANTICS
        } else {
            0
        };
    let sharing =
        FILE_SHARE_READ | FILE_SHARE_WRITE | if directory { 0 } else { FILE_SHARE_DELETE };
    let handle = unsafe {
        CreateFileW(
            path.as_ptr(),
            access | READ_CONTROL,
            sharing,
            null(),
            OPEN_EXISTING,
            flags,
            null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let file = unsafe { File::from_raw_handle(handle) };
    let meta = file.metadata()?;
    if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
        || (directory && !meta.is_dir())
        || (!directory && !meta.is_file())
    {
        return Err(denied(
            "Private paths cannot be reparse points or special files",
        ));
    }
    validate_acl(&file)?;
    Ok(file)
}

/// Keep the directory handle open during operations to prevent its replacement.
pub fn open_directory(path: &Path) -> io::Result<File> {
    open_existing(path, 0, true)
}

/// Create one private directory. Existing directories must already be private.
pub fn create_directory(path: &Path) -> io::Result<File> {
    let descriptor = private_descriptor()?;
    let attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: 0,
    };
    if unsafe { CreateDirectoryW(wide(path)?.as_ptr(), &attributes) } == 0 {
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::AlreadyExists {
            return Err(error);
        }
    }
    open_directory(path)
}

/// Open and validate the actual file handle, then bound its size before reading.
pub fn open_read(path: &Path, limit: u64) -> io::Result<File> {
    let parent = path
        .parent()
        .ok_or_else(|| denied("Private file needs a parent directory"))?;
    let _parent = open_directory(parent)?;
    let file = open_existing(path, GENERIC_READ, false)?;
    if file.metadata()?.len() > limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Private file exceeds its size limit",
        ));
    }
    Ok(file)
}

/// CREATE_NEW prevents overwriting a file or following a pre-existing link.
/// Ephemeral files disappear when their last open handle closes, even on a crash.
pub fn create_file(path: &Path, ephemeral: bool) -> io::Result<File> {
    let parent = path
        .parent()
        .ok_or_else(|| denied("Private file needs a parent directory"))?;
    let _parent = open_directory(parent)?;
    let descriptor = private_descriptor()?;
    let attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: 0,
    };
    let access = GENERIC_READ | GENERIC_WRITE | if ephemeral { DELETE } else { 0 };
    let flags = FILE_ATTRIBUTE_NORMAL
        | FILE_FLAG_OPEN_REPARSE_POINT
        | if ephemeral {
            FILE_FLAG_DELETE_ON_CLOSE
        } else {
            0
        };
    let handle = unsafe {
        CreateFileW(
            wide(path)?.as_ptr(),
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            &attributes,
            CREATE_NEW,
            flags,
            null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let file = unsafe { File::from_raw_handle(handle) };
    validate_acl(&file)?;
    Ok(file)
}

pub fn session_lock(path: &Path) -> io::Result<File> {
    let file = match create_file(path, false) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            open_existing(path, GENERIC_READ | GENERIC_WRITE, false)?
        }
        Err(error) => return Err(error),
    };
    let mut overlapped = OVERLAPPED::default();
    // Synchronous, nonblocking exclusive lock of byte zero; dropping the owning
    // File releases the lock. No process-local flag substitutes for the OS lock.
    if unsafe {
        LockFileEx(
            file.as_raw_handle(),
            LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
            0,
            1,
            0,
            &mut overlapped,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    fn directory() -> std::path::PathBuf {
        let mut random = [0; 16];
        crate::entropy::fill(&mut random).unwrap();
        let name: String = random.iter().map(|b| format!("{b:02x}")).collect();
        let path = std::env::temp_dir().join(format!("rove-private-{name}"));
        drop(create_directory(&path).unwrap());
        path
    }

    #[test]
    fn private_files_are_bounded_and_ephemeral_files_are_removed() {
        let root = directory();
        let path = root.join("endpoint.json");
        let mut file = create_file(&path, true).unwrap();
        file.write_all(b"test data").unwrap();
        file.flush().unwrap();
        let mut data = String::new();
        open_read(&path, 9)
            .unwrap()
            .read_to_string(&mut data)
            .unwrap();
        assert_eq!(data, "test data");
        assert!(open_read(&path, 8).is_err());
        assert!(create_file(&path, true).is_err());
        drop(file);
        assert!(!path.exists());
        std::fs::remove_dir(root).unwrap();
    }

    #[test]
    fn lease_is_exclusive_and_released_by_handle_close() {
        let root = directory();
        let path = root.join("lease");
        let first = session_lock(&path).unwrap();
        assert!(session_lock(&path).is_err());
        drop(first);
        drop(session_lock(&path).unwrap());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn file_readable_by_everyone_is_rejected() {
        use windows_sys::Win32::Security::{
            Authorization::SetSecurityInfo, GetSecurityDescriptorDacl,
        };
        let root = directory();
        let path = root.join("synthetic-credential");
        drop(create_file(&path, false).unwrap());
        let file = open_existing(
            &path,
            windows_sys::Win32::Storage::FileSystem::WRITE_DAC,
            false,
        )
        .unwrap();
        let sddl: Vec<_> = "D:P(A;;FA;;;WD)".encode_utf16().chain(Some(0)).collect();
        let mut descriptor = null_mut();
        assert_ne!(
            unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    sddl.as_ptr(),
                    1,
                    &mut descriptor,
                    null_mut(),
                )
            },
            0
        );
        let _memory = LocalMemory(descriptor);
        let mut present = 0;
        let mut defaulted = 0;
        let mut dacl = null_mut();
        assert_ne!(
            unsafe {
                GetSecurityDescriptorDacl(descriptor, &mut present, &mut dacl, &mut defaulted)
            },
            0
        );
        assert_eq!(
            unsafe {
                SetSecurityInfo(
                    file.as_raw_handle(),
                    SE_FILE_OBJECT,
                    DACL_SECURITY_INFORMATION
                        | windows_sys::Win32::Security::PROTECTED_DACL_SECURITY_INFORMATION,
                    null_mut(),
                    null_mut(),
                    dacl,
                    null_mut(),
                )
            },
            0
        );
        drop(file);
        assert!(open_read(&path, 1024).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
}
