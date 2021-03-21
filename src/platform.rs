use std::{convert::TryFrom, io, path::Path, path::PathBuf};

use eyre::{ensure, eyre, Result, WrapErr};
use slog::KV;

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
pub fn screen_size() -> Result<(u32, u32)> {
    use winapi::um::winuser::{GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN};

    let (width, height) = unsafe {
        (
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    };

    // try_winapi! is useless here as GetSystemMetrics does not use GetLastError
    eyre::ensure!(width != 0, "GetSystemMetrics's returned width was zero");
    eyre::ensure!(height != 0, "GetSystemMetrics's returned height was zero");
    dbg!(width, height);

    Ok((u32::try_from(width)?, u32::try_from(height)?))
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
    .wrap_err(format!("Failed to set background to {:?}", path))
}

#[cfg(windows)]
pub fn copy_image(img: image::DynamicImage) -> Result<()> {
    use winapi::um::wingdi::{CreateBitmap, DeleteObject};
    use winapi::um::winuser::{
        CloseClipboard, EmptyClipboard, GetForegroundWindow, OpenClipboard, SetClipboardData,
        CF_BITMAP,
    };

    let img = img.to_bgra8();

    // Open the clipboard
    wintry!(unsafe { OpenClipboard(GetForegroundWindow()) })
        .wrap_err("Failed to open clipboard")?;

    // Emptythe clipboard
    // For whatever reason you can't overwrite it if it's got an image in it. ¯\_(ツ)_/¯
    wintry!(unsafe { EmptyClipboard() }).wrap_err("Failed to empty clipboard")?;

    // Create the bitmap to be copied
    let w = img.width();
    let h = img.height();
    let pixel_sz = 4 * 8;
    let mut pixels = img.into_raw();
    let bmp = unsafe { CreateBitmap(w as _, h as _, 1, pixel_sz, pixels.as_mut_ptr() as *mut _) };

    // Set the clipboard data to it
    let set_result = wintry!(unsafe { SetClipboardData(CF_BITMAP, bmp as *mut _) } as usize)
        .wrap_err("Failed to set clipboard data");

    // Free the bitmap memory
    let delete_result =
        wintry!(unsafe { DeleteObject(bmp as *mut _) }).wrap_err("Failed to delete bitmap object");

    // Close the clipboard
    let close_result = wintry!(unsafe { CloseClipboard() }).wrap_err("Failed to close clipboard");

    // Now, check that all operations succeeded. We do this because we still
    // want to delete the bitmap object and close the clipboard even if any
    // preceding/succeeding operations fail
    set_result.and(delete_result).and(close_result)
}

pub struct NotifyDrain {
    pub title: String,
    pub icon: PathBuf,
}

#[derive(Default)]
struct Text2Serializer {
    result: String,
}

impl slog::Serializer for Text2Serializer {
    fn emit_arguments(&mut self, key: slog::Key, val: &std::fmt::Arguments) -> slog::Result {
        if !self.result.is_empty() {
            self.result += " | ";
        }

        self.result += &format!("{}: {}", key, val);

        Ok(())
    }
}

impl slog::Drain for NotifyDrain {
    type Ok = ();

    type Err = eyre::Report;

    #[cfg(windows)]
    fn log(&self, record: &slog::Record, kv: &slog::OwnedKVList) -> Result<Self::Ok, Self::Err> {
        use winrt_notification::{Duration, IconCrop, Toast};

        let mut text2_ser = Text2Serializer::default();
        let _ = record.kv().serialize(record, &mut text2_ser);
        let _ = kv.serialize(record, &mut text2_ser);

        Toast::new(Toast::POWERSHELL_APP_ID)
            .title(&format!(
                "{} ({}:{}:{})",
                self.title,
                record.file(),
                record.line(),
                record.column()
            ))
            .text1(&format!("{}", record.msg()))
            .text2(&text2_ser.result)
            .duration(Duration::Short)
            .icon(
                &self.icon,
                IconCrop::Square,
                self.icon
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .unwrap_or(""),
            )
            .show()
            .map_err(|err| {
                eyre!(
                    "Failed to show notification (HRESULT {:?})",
                    err.as_hresult()
                )
            })
    }
}
