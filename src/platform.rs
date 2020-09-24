use std::{io, path::Path};

use anyhow::{ensure, Context, Result};
use image::RgbaImage;

macro_rules! wintry {
    ($expr:expr) => {
        if $expr != 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    };
}

#[cfg(windows)]
pub fn set_background(path: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use winapi::um::winuser::{SystemParametersInfoW, SPI_SETDESKWALLPAPER};

    ensure!(
        path.is_absolute(),
        "SystemParametersInfoW requires an absolute path"
    );

    let path_utf16 = path
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<u16>>();

    wintry!(unsafe {
        SystemParametersInfoW(SPI_SETDESKWALLPAPER, 0, path_utf16.as_ptr() as *mut _, 0)
    })
    .context(format!("Failed to set background to {:?}", path))
}

#[cfg(windows)]
pub fn copy_image(img: RgbaImage) -> Result<()> {
    use winapi::um::wingdi::{CreateBitmap, DeleteObject};
    use winapi::um::winuser::{
        CloseClipboard, GetForegroundWindow, OpenClipboard, SetClipboardData, CF_BITMAP,
    };

    // Open the clipboard
    wintry!(unsafe { OpenClipboard(GetForegroundWindow()) }).context("Failed to open clipboard")?;

    // Create the bitmap to be copied
    let w = img.width();
    let h = img.height();
    let pixel_sz = 4 * 8;
    let mut pixels = img.into_raw();
    let bmp = unsafe { CreateBitmap(w as _, h as _, 1, pixel_sz, pixels.as_mut_ptr() as *mut _) };

    // Set the clipboard data to it
    let set_result = wintry!(unsafe { SetClipboardData(CF_BITMAP, bmp as *mut _) } as usize)
        .context("Failed to set clipboard data");

    // Free the bitmap memory
    let delete_result =
        wintry!(unsafe { DeleteObject(bmp as *mut _) }).context("Failed to delete bitmap object");

    // Close the clipboard
    let close_result = wintry!(unsafe { CloseClipboard() }).context("Failed to close clipboard");

    // Now, check that all operations succeeded. We do this because we still
    // want to delete the bitmap object and close the clipboard even if any
    // preceding/succeeding operations fail
    set_result.and(delete_result).and(close_result)
}
