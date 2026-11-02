use anyhow::{Context, Result, ensure};
use gtk::{gio, glib, prelude::*};
use image::{ImageEncoder, RgbaImage};
use std::{io::Write, path::Path, time::Duration};

pub fn png(image: &RgbaImage) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    image::codecs::png::PngEncoder::new(&mut bytes).write_image(
        image.as_raw(),
        image.width(),
        image.height(),
        image::ExtendedColorType::Rgba8,
    )?;
    Ok(bytes)
}

pub fn save(image: &RgbaImage, path: &Path) -> Result<()> {
    if path.as_os_str() == "-" {
        let mut stdout = std::io::stdout().lock();
        image::codecs::png::PngEncoder::new(&mut stdout).write_image(
            image.as_raw(),
            image.width(),
            image.height(),
            image::ExtendedColorType::Rgba8,
        )?;
        stdout.flush()?;
        return Ok(());
    }
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Cannot create {}", parent.display()))?;
    }
    crate::document::save_new_png(image, path)
}

pub async fn copy_command(image: RgbaImage, argv: &[String]) -> Result<()> {
    ensure!(!argv.is_empty(), "Copy command is empty");
    let encoded = gio::spawn_blocking(move || png(&image))
        .await
        .map_err(|_| anyhow::anyhow!("PNG encoding worker failed"))??;
    let args: Vec<_> = argv.iter().map(std::ffi::OsStr::new).collect();
    let process = gio::Subprocess::newv(
        &args,
        gio::SubprocessFlags::STDIN_PIPE
            | gio::SubprocessFlags::STDOUT_SILENCE
            | gio::SubprocessFlags::STDERR_SILENCE,
    )
    .with_context(|| format!("Cannot start copy command {}", argv[0]))?;
    let weak = process.downgrade();
    let expired = std::rc::Rc::new(std::cell::Cell::new(false));
    let fired = expired.clone();
    let timer = glib::timeout_add_local_once(Duration::from_secs(30), move || {
        fired.set(true);
        // shortcut: only the direct copier is killed; multi-process hooks need their own cancellation.
        if let Some(process) = weak.upgrade() {
            process.force_exit();
        }
    });
    let result = process
        .communicate_future(Some(&glib::Bytes::from_owned(encoded)))
        .await;
    let waited = process.wait_check_future().await;
    if !expired.get() {
        timer.remove();
    }
    ensure!(
        !expired.get(),
        "Copy command timed out after 30 seconds; run the configured command in a terminal to diagnose it"
    );
    waited.context("Copy command failed; run it in a terminal to diagnose it")?;
    result.context("Cannot send PNG to copy command")?;
    Ok(())
}

pub async fn store_clipboard(clipboard: &gtk::gdk::Clipboard) -> Result<()> {
    let cancellable = gio::Cancellable::new();
    let cancel = cancellable.clone();
    let expired = std::rc::Rc::new(std::cell::Cell::new(false));
    let fired = expired.clone();
    let timer = glib::timeout_add_local_once(Duration::from_secs(3), move || {
        fired.set(true);
        cancel.cancel();
    });
    let (sender, receiver) = async_channel::bounded(1);
    clipboard.store_async(glib::Priority::DEFAULT, Some(&cancellable), move |result| {
        let _ = sender.try_send(result);
    });
    let result = receiver
        .recv()
        .await
        .context("Clipboard persistence did not complete")?;
    if !expired.get() {
        timer.remove();
    }
    result.context("Clipboard cannot survive closing this editor; use a clipboard manager or a copy command such as wl-copy")?;
    Ok(())
}

pub fn notify(app: &gtk::Application, text: &str, thumbnail: Option<Vec<u8>>) {
    let notification = gio::Notification::new("SharpShot");
    notification.set_body(Some(text));
    if let Some(bytes) = thumbnail {
        notification.set_icon(&gio::BytesIcon::new(&glib::Bytes::from_owned(bytes)));
    } else {
        notification.set_icon(&gio::ThemedIcon::new("image-x-generic-symbolic"));
    }
    app.send_notification(Some("image-operation"), &notification);
}

pub fn thumbnail(image: &RgbaImage) -> Result<Vec<u8>> {
    png(&image::imageops::thumbnail(image, 160, 100))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn png_and_nested_output_preserve_pixels_and_never_replace_a_file() -> Result<()> {
        let image = RgbaImage::from_pixel(3, 2, image::Rgba([25, 50, 75, 128]));
        assert_eq!(image::load_from_memory(&png(&image)?)?.into_rgba8(), image);
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("nested/shot.png");
        save(&image, &path)?;
        let before = std::fs::read(&path)?;
        assert!(save(&RgbaImage::new(1, 1), &path).is_err());
        assert_eq!(std::fs::read(&path)?, before);
        assert_eq!(image::open(path)?.into_rgba8(), image);
        Ok(())
    }
}
