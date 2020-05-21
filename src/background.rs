use std::io;
use std::path::Path;

use anyhow::{Context, Result};

#[cfg(windows)]
pub fn set(path: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use winapi::um::winuser::SystemParametersInfoW;

    anyhow::ensure!(
        path.is_absolute(),
        "SystemParametersInfoW requires an absolute path"
    );

    let path_utf16 = path
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<u16>>();

    let rv = unsafe { SystemParametersInfoW(20, 0, path_utf16.as_ptr() as *mut _, 0) };

    if rv != 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
            .context(format!("Failed to set background to {:?}", path))?
    }
}
