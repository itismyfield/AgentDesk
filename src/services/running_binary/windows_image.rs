use std::{
    fs::OpenOptions,
    os::windows::{fs::OpenOptionsExt, io::AsRawHandle},
    path::Path,
};

use sha2::{Digest, Sha256};
use windows_sys::Win32::{
    Storage::FileSystem::{FILE_SHARE_READ, GetFinalPathNameByHandleW, VOLUME_NAME_NT},
    System::{
        LibraryLoader::GetModuleHandleW, ProcessStatus::GetMappedFileNameW,
        Threading::GetCurrentProcess,
    },
};

pub(super) fn current_image_digest(path: &Path) -> Option<String> {
    let mut file = OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ)
        .open(path)
        .ok()?;
    let mut opened = vec![0u16; 32_768];
    let mut mapped = vec![0u16; opened.len()];
    // SAFETY: file remains open, both vectors are writable and the null module name
    // asks for the executable's already-loaded module.
    let (opened_size, mapped_size) = unsafe {
        let module = GetModuleHandleW(std::ptr::null());
        if module.is_null() {
            return None;
        }
        (
            GetFinalPathNameByHandleW(
                file.as_raw_handle(),
                opened.as_mut_ptr(),
                opened.len() as u32,
                VOLUME_NAME_NT,
            ),
            GetMappedFileNameW(
                GetCurrentProcess(),
                module.cast(),
                mapped.as_mut_ptr(),
                mapped.len() as u32,
            ),
        )
    };
    let size = opened_size as usize;
    if size == 0
        || size >= opened.len()
        || opened_size != mapped_size
        || opened[..size] != mapped[..size]
    {
        return None;
    }
    let mut digest = Sha256::new();
    std::io::copy(&mut file, &mut digest).ok()?;
    Some(format!("{:x}", digest.finalize()))
}
