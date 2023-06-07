use std::{
    convert::TryFrom,
    io,
    path::{Path, PathBuf},
};

use eyre::{ensure, format_err, Result, WrapErr};

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
    use winapi::um::winuser::{GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN};

    let (width, height) = unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) };

    // try_winapi! is useless here as GetSystemMetrics does not use GetLastError
    ensure!(width != 0, "GetSystemMetrics's returned width was zero");
    ensure!(height != 0, "GetSystemMetrics's returned height was zero");

    Ok((u32::try_from(width)?, u32::try_from(height)?))
}

#[cfg(windows)]
pub fn set_background(path: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use winapi::um::winuser::{SystemParametersInfoW, SPI_SETDESKWALLPAPER};

    ensure!(path.is_absolute(), "SystemParametersInfoW requires an absolute path");

    let path_utf16 = path.as_os_str().encode_wide().chain(Some(0)).collect::<Vec<u16>>();

    wintry!(unsafe { SystemParametersInfoW(SPI_SETDESKWALLPAPER, 0, path_utf16.as_ptr() as *mut _, 0) })
        .wrap_err(format!("Failed to set background to {path:?}"))
}

#[cfg(windows)]
pub fn copy_image(img: &image::DynamicImage) -> Result<()> {
    use std::convert::TryInto;

    use winapi::um::{
        wingdi::{CreateBitmap, DeleteObject},
        winuser::{CloseClipboard, EmptyClipboard, GetForegroundWindow, OpenClipboard, SetClipboardData, CF_BITMAP},
    };

    // The image create has stopped supporting BGRA8, so we'll need to convert our image to it from
    // RGBA8 ourselves once we call into_raw
    let img = img.to_rgba8();

    // Open the clipboard
    wintry!(unsafe { OpenClipboard(GetForegroundWindow()) }).wrap_err("Failed to open clipboard")?;

    // Empty the clipboard
    // For whatever reason you can't overwrite it if it's got an image in it. ¯\_(ツ)_/¯
    wintry!(unsafe { EmptyClipboard() }).wrap_err("Failed to empty clipboard")?;

    // Create the bitmap to be copied
    let w: i32 = img.width().try_into()?;
    let h: i32 = img.height().try_into()?;
    let pixel_sz = 4 * 8;
    let mut pixels = img.into_raw();
    pixels.chunks_exact_mut(4).for_each(|chunk| chunk[0..3].reverse());
    let bmp = unsafe { CreateBitmap(w, h, 1, pixel_sz, pixels.as_mut_ptr().cast()) };

    // Set the clipboard data to it
    let set_result =
        wintry!(unsafe { SetClipboardData(CF_BITMAP, bmp.cast()) } as usize).wrap_err("Failed to set clipboard data");

    // Free the bitmap memory
    let delete_result = wintry!(unsafe { DeleteObject(bmp.cast()) }).wrap_err("Failed to delete bitmap object");

    // Close the clipboard
    let close_result = wintry!(unsafe { CloseClipboard() }).wrap_err("Failed to close clipboard");

    // Now, check that all operations succeeded. We do this because we still
    // want to delete the bitmap object and close the clipboard even if any
    // preceding/succeeding operations fail
    set_result.and(delete_result).and(close_result)
}

pub struct Notifier {
    pub title: String,
    pub icon: PathBuf,
}

#[derive(Default)]
struct NotifierVisit {
    message: Option<String>,
    fields: String,
}

impl tracing::field::Visit for NotifierVisit {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        use std::fmt::Write;

        if field.name() == "message" {
            self.message = Some(format!("{value:?}"));
            return;
        }

        if !self.fields.is_empty() {
            let _ = write!(self.fields, " | ");
        }
        let _ = write!(self.fields, "{}: {:?}", field.name(), value);
    }
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Notifier {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: tracing_subscriber::layer::Context<'_, S>) {
        use winrt_notification::{Duration, IconCrop, Toast};

        let mut visitor = NotifierVisit::default();
        event.record(&mut visitor);

        let meta = event.metadata();

        let _ = Toast::new(Toast::POWERSHELL_APP_ID)
            .title(&format!(
                "{} ({}:{})",
                self.title,
                meta.file().unwrap_or("<unknown>"),
                meta.line().unwrap_or(0xCAFE_BABE),
            ))
            .text1(visitor.message.as_deref().unwrap_or("no message"))
            .text2(&visitor.fields)
            .duration(Duration::Short)
            .icon(
                &self.icon,
                IconCrop::Square,
                self.icon.file_stem().and_then(std::ffi::OsStr::to_str).unwrap_or(""),
            )
            .show()
            .map_err(|err| {
                format_err!(
                    "Failed to show notification: {:?} (CODE {:?}, WIN32 CODE {:?})",
                    err.message(),
                    err.code(),
                    err.win32_error()
                )
            });
    }
}
