use std::io;
use std::path::Path;

use anyhow::{Context, Result};

#[cfg(windows)]
pub fn screen_aspect_ratio() -> Result<f64> {
    use winapi::um::winuser::{GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN};

    let (width, height) = unsafe {
        (
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    };

    // try_winapi! is useless here as GetSystemMetrics does not use GetLastError
    anyhow::ensure!(width != 0, "GetSystemMetrics's returned width was zero");
    anyhow::ensure!(height != 0, "GetSystemMetrics's returned height was zero");

    Ok(f64::from(width) / f64::from(height))
}

#[cfg(windows)]
pub fn set(path: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use winapi::um::winuser::{SystemParametersInfoW, SPI_SETDESKWALLPAPER};

    anyhow::ensure!(
        path.is_absolute(),
        "SystemParametersInfoW requires an absolute path"
    );

    let path_utf16 = path
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<u16>>();

    let rv = unsafe { SystemParametersInfoW(SPI_SETDESKWALLPAPER, 0, path_utf16.as_ptr() as *mut _, 0) };

    if rv != 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
            .context(format!("Failed to set background to {:?}", path))?
    }
}
