use std::{io, path::Path, path::PathBuf};

use eyre::{ensure, eyre, Result, WrapErr};
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
    .wrap_err(format!("Failed to set background to {:?}", path))
}

#[cfg(windows)]
pub fn copy_image(img: RgbaImage) -> Result<()> {
    use winapi::um::wingdi::{CreateBitmap, DeleteObject};
    use winapi::um::winuser::{
        CloseClipboard, GetForegroundWindow, OpenClipboard, SetClipboardData, CF_BITMAP,
    };

    // Open the clipboard
    wintry!(unsafe { OpenClipboard(GetForegroundWindow()) })
        .wrap_err("Failed to open clipboard")?;

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

pub struct NotifyOnError {
    pub title: String,
    pub icon: PathBuf,
}

struct FindKey {
    key: &'static str,
    value: Option<String>,
}

impl slog::Serializer for FindKey {
    fn emit_arguments(&mut self, key: slog::Key, val: &std::fmt::Arguments) -> slog::Result {
        if key == self.key {
            self.value = Some(format!("{}", val));
        }

        Ok(())
    }
}

impl FindKey {
    fn find_key(key: &'static str, record: &slog::Record, kv: impl slog::KV) -> Option<String> {
        let mut this = Self { key, value: None };
        let _ = kv.serialize(record, &mut this);
        this.value
    }
}

impl slog::Drain for NotifyOnError {
    type Ok = ();

    type Err = eyre::Report;

    #[cfg(windows)]
    fn log(
        &self,
        record: &slog::Record,
        values: &slog::OwnedKVList,
    ) -> Result<Self::Ok, Self::Err> {
        use winrt_notification::{Duration, IconCrop, Toast};

        if !record.level().is_at_least(slog::Level::Error) {
            return Ok(());
        }

        Toast::new(Toast::POWERSHELL_APP_ID)
            .title(&self.title)
            .text1(&format!(
                "{}:{}:{}",
                record.file(),
                record.line(),
                record.column()
            ))
            .text2(
                &if let Some(error) = FindKey::find_key("error", record, values) {
                    format!("{} ({})", record.msg(), error)
                } else {
                    format!("{}", record.msg())
                },
            )
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
