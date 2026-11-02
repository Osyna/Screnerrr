mod caption;
mod capture;
mod chrome;
mod context_menu;
mod designer;
mod document;
mod editor;
mod inspector;
mod interaction;
mod ocr;
mod ocr_ui;
mod palette;
mod pin;
mod presentation;
mod presets;
mod project;
mod recovery;
mod scrolling;
mod settings;
mod shapes;
mod smart_redact;
mod vector_export;
mod workflow;

use anyhow::{Context, Result, ensure};
use clap::Parser;
use image::RgbaImage;
use std::io::{BufRead, Read, Seek, Write};
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

/// Bound compressed input and decoded pixels before allocating an image.
const MAX_INPUT_BYTES: u64 = 256 << 20;
const MAX_INPUT_SIDE: u32 = 32_767;
const MAX_INPUT_PIXELS: u64 = 1 << 27;

#[derive(Parser)]
#[command(
    version,
    about = "Capture and annotate Wayland screenshots with offline smart shapes"
)]
struct Args {
    /// Edit an existing image instead of capturing the screen; `-` reads PNG/JPEG/WebP from stdin
    #[arg(short, long, visible_alias = "filename", value_name = "FILE|-")]
    input: Option<PathBuf>,
    /// Reopen an editable SharpShot project (contains the original screenshot)
    #[arg(long, conflicts_with_all = ["input", "output", "cursor", "region", "geometry", "list_outputs", "capture"])]
    project: Option<PathBuf>,
    /// Monitor name to capture (see --list-outputs); defaults to the first output
    #[arg(short, long)]
    output: Option<String>,
    /// Include the pointer in the capture
    #[arg(long)]
    cursor: bool,
    /// Drag a screen zone to capture (uses slurp for selection only; Escape cancels)
    #[arg(short, long, conflicts_with_all = ["input", "output", "geometry", "list_outputs"])]
    region: bool,
    /// Capture a global logical rectangle, e.g. -100,20 640x480
    #[arg(short = 'g', long, value_name = "X,Y WxH", allow_hyphen_values = true,
        conflicts_with_all = ["input", "output", "region", "list_outputs"])]
    geometry: Option<capture::Region>,
    /// Print available Wayland outputs and exit
    #[arg(long, conflicts_with_all = ["input", "capture", "region", "geometry"])]
    list_outputs: bool,
    /// Capture directly to a new PNG without opening the editor; `-` writes the PNG to stdout
    #[arg(long, conflicts_with = "input", value_name = "FILE|-")]
    capture: Option<PathBuf>,
    /// Guided scrolling capture of a region; scroll the source yourself between frames
    #[arg(long, conflicts_with_all = ["input", "project", "capture", "list_outputs", "output", "toggle_pins"])]
    scroll: bool,
    /// Hide/show pinned references (suitable for a compositor keyboard binding)
    #[arg(long, conflicts_with_all = ["input", "project", "capture", "list_outputs", "output", "geometry", "region", "cursor"])]
    toggle_pins: bool,
    #[arg(long = "pin-helper", hide = true, conflicts_with_all = ["input", "project", "capture", "list_outputs", "output", "geometry", "region", "cursor", "scroll", "toggle_pins"])]
    pin_helper: bool,
    #[command(flatten)]
    launch: settings::LaunchArgs,
}

fn main() -> Result<()> {
    let started = std::time::Instant::now();
    let args = Args::parse();
    let profile = args.launch.profile_startup;
    if args.pin_helper {
        return pin::run_helper();
    }
    if args.toggle_pins {
        return pin::toggle_all();
    }
    if args.list_outputs {
        for name in capture::outputs()? {
            println!("{name}");
        }
        return Ok(());
    }
    // A direct capture never opens the editor, so a bad config must not block it.
    let settings = if args.capture.is_some() {
        settings::Settings::default()
    } else {
        args.launch.resolve()?
    };
    lap(profile, started, "settings resolved");
    if let Some(path) = args.project {
        let doc = project::Snapshot::load(&path)?.into_document()?;
        lap(profile, started, "project loaded");
        return editor::run(doc, settings.clone());
    }
    let geometry = if args.region || (args.scroll && args.geometry.is_none()) {
        match select_region()? {
            Some(region) => Some(region),
            None => return Ok(()),
        }
    } else {
        args.geometry
    };
    if args.scroll {
        let Some(image) =
            scrolling::run(geometry.expect("scroll region selected above"), args.cursor)?
        else {
            return Ok(());
        };
        let doc = document::Document::new(image)?;
        lap(profile, started, "document ready");
        return editor::run(doc, settings.clone());
    }
    let image = match args.input {
        Some(path) if path.as_os_str() == "-" => read_stdin_image()?,
        Some(path) => {
            let (file, metadata) = open_regular_file(&path)
                .with_context(|| format!("Cannot open {}", path.display()))?;
            ensure!(
                metadata.len() <= MAX_INPUT_BYTES,
                "Input must be a regular image no larger than 256 MiB"
            );
            decode_image(std::io::BufReader::new(file))
                .with_context(|| format!("Cannot decode {}", path.display()))?
        }
        None => match geometry {
            Some(region) => capture::capture_region(region, args.cursor)?,
            None => capture::capture(args.output.as_deref(), args.cursor)?,
        },
    };
    lap(profile, started, "image acquired");
    if let Some(path) = args.capture {
        if path.as_os_str() == "-" {
            // Stdout carries only PNG bytes; status goes to stderr.
            let mut out = std::io::stdout().lock();
            write_png(&image, &mut out)?;
            out.flush().context("Cannot write PNG to standard output")?;
            eprintln!(
                "Captured {} × {} to standard output",
                image.width(),
                image.height()
            );
            return Ok(());
        }
        document::save_new_png(&image, &path)?;
        println!(
            "Saved {} × {} to {}",
            image.width(),
            image.height(),
            path.display()
        );
        return Ok(());
    }
    let doc = document::Document::new(image)?;
    lap(profile, started, "document ready");
    editor::run(doc, settings.clone())
}

/// `--profile-startup`: elapsed time since launch, on stderr only; never stdout, never sent anywhere.
fn lap(enabled: bool, started: std::time::Instant, phase: &str) {
    if enabled {
        eprintln!(
            "[startup] {:>8.1} ms  {phase}",
            started.elapsed().as_secs_f64() * 1e3
        );
    }
}

fn write_png(image: &RgbaImage, out: &mut impl Write) -> Result<()> {
    use image::ImageEncoder;
    let mut out = std::io::BufWriter::new(out);
    image::codecs::png::PngEncoder::new(&mut out)
        .write_image(
            image.as_raw(),
            image.width(),
            image.height(),
            image::ExtendedColorType::Rgba8,
        )
        .context("Cannot encode PNG")?;
    out.flush().context("Cannot write PNG")
}

fn open_regular_file(
    path: &std::path::Path,
) -> std::io::Result<(std::fs::File, std::fs::Metadata)> {
    use rustix::fs::{Mode, OFlags};
    // NONBLOCK lets us reject a named pipe before waiting for its writer.
    let file: std::fs::File = rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(std::io::Error::from)?
    .into();
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "Not a regular file",
        ));
    }
    Ok((file, metadata))
}

fn read_stdin_image() -> Result<RgbaImage> {
    read_image(std::io::stdin().lock(), MAX_INPUT_BYTES)
        .context("Cannot use standard input as an image")
}

/// PNG, JPEG or WebP from `reader`: at most `max_bytes` read and the pixel budget checked
/// from the header before any pixel buffer is allocated.
fn read_image(reader: impl Read, max_bytes: u64) -> Result<RgbaImage> {
    let mut bytes = Vec::new();
    reader.take(max_bytes + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= max_bytes,
        "image data is larger than {} MiB",
        max_bytes >> 20
    );
    decode_image(std::io::Cursor::new(bytes))
}

fn decode_image(mut input: impl BufRead + Seek) -> Result<RgbaImage> {
    let reader = image::ImageReader::new(&mut input).with_guessed_format()?;
    ensure!(
        matches!(
            reader.format(),
            Some(image::ImageFormat::Png | image::ImageFormat::Jpeg | image::ImageFormat::WebP)
        ),
        "expected PNG, JPEG or WebP data"
    );
    let format = reader.format().expect("supported image format");
    let (w, h) = reader.into_dimensions()?;
    ensure!(
        w > 0
            && h > 0
            && w <= MAX_INPUT_SIDE
            && h <= MAX_INPUT_SIDE
            && u64::from(w) * u64::from(h) <= MAX_INPUT_PIXELS,
        "image size {w} × {h} is outside the supported range"
    );
    input.rewind()?;
    let mut reader = image::ImageReader::with_format(input, format);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_INPUT_SIDE);
    limits.max_image_height = Some(MAX_INPUT_SIDE);
    limits.max_alloc = Some(MAX_INPUT_PIXELS * 4 + (64 << 20));
    reader.limits(limits);
    Ok(reader.decode()?.into_rgba8())
}

fn select_region() -> Result<Option<capture::Region>> {
    let output = Command::new("slurp")
        .args(["-d", "-f", "%x,%y %wx%h"])
        .stdin(Stdio::null())
        .output()
        .context("Cannot start slurp; install it for --region or use --geometry instead")?;
    selected_region(output)
}

fn selected_region(output: Output) -> Result<Option<capture::Region>> {
    if output.status.success() {
        return std::str::from_utf8(&output.stdout)
            .context("slurp returned non-UTF-8 geometry")?
            .trim()
            .parse()
            .context("slurp returned an invalid capture zone")
            .map(Some);
    }
    let error = String::from_utf8_lossy(&output.stderr);
    if output.status.code() == Some(1)
        && output.stdout.is_empty()
        && matches!(error.trim(), "" | "selection cancelled")
    {
        return Ok(None);
    }
    anyhow::bail!(
        "Region selection failed ({}): {}",
        output.status,
        error.trim()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;
    #[test]
    fn named_pipes_are_rejected_without_waiting_for_a_writer() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("input.fifo");
        rustix::fs::mkfifoat(
            rustix::fs::CWD,
            &path,
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
        )?;
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = sender.send(open_regular_file(&path).err().map(|error| error.kind()));
        });
        assert_eq!(
            receiver.recv_timeout(std::time::Duration::from_secs(2))?,
            Some(std::io::ErrorKind::InvalidInput)
        );
        Ok(())
    }

    fn selector(code: i32, stdout: &[u8], stderr: &[u8]) -> Output {
        Output {
            status: std::process::ExitStatus::from_raw(code << 8),
            stdout: stdout.to_vec(),
            stderr: stderr.to_vec(),
        }
    }

    #[test]
    fn cancel_is_clean_but_selector_failure_is_not_silently_ignored() {
        assert!(
            selected_region(selector(1, b"", b"selection cancelled\n"))
                .unwrap()
                .is_none()
        );
        assert!(selected_region(selector(1, b"", b"failed to connect to Wayland")).is_err());
        assert!(selected_region(selector(1, b"100,100 20x20", b"")).is_err());
    }

    #[test]
    fn successful_selector_without_valid_geometry_never_means_full_screen() {
        for bytes in [b"".as_slice(), b"0,0 0x0", b"\xff"] {
            assert!(selected_region(selector(0, bytes, b"")).is_err());
        }
    }

    #[test]
    fn zone_options_reject_conflicting_capture_sources() {
        for args in [
            vec!["sharpshot", "--region", "--input", "image.png"],
            vec!["sharpshot", "--region", "--output", "DP-1"],
            vec!["sharpshot", "--geometry", "0,0 30x20", "--region"],
        ] {
            assert!(Args::try_parse_from(args).is_err());
        }
    }

    #[test]
    fn stdin_and_stdout_dashes_parse_and_old_conflicts_still_hold() {
        let ok = |a: &[&str]| {
            Args::try_parse_from(std::iter::once("sharpshot").chain(a.iter().copied())).is_ok()
        };
        assert!(ok(&["--input", "-"]));
        assert!(ok(&["--filename", "-"]));
        assert!(ok(&["--capture", "-", "--region"]));
        assert!(ok(&[
            "--fullscreen",
            "--title",
            "t",
            "--on-enter",
            "copy,exit"
        ]));
        assert!(!ok(&["--capture", "-", "--input", "-"]));
        assert!(!ok(&["--filename", "a.png", "--region"]));
        assert!(!ok(&["--project", "p.sharp", "--input", "-"]));
    }

    fn png(w: u32, h: u32) -> Vec<u8> {
        let mut bytes = vec![];
        write_png(
            &RgbaImage::from_pixel(w, h, image::Rgba([1, 2, 3, 200])),
            &mut bytes,
        )
        .unwrap();
        bytes
    }

    #[test]
    fn stdout_capture_is_exactly_one_png() {
        let bytes = png(3, 2);
        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
        let back = image::load_from_memory(&bytes).unwrap().into_rgba8();
        assert_eq!(back.dimensions(), (3, 2));
        assert_eq!(back.get_pixel(2, 1).0, [1, 2, 3, 200]);
    }

    #[test]
    fn stdin_images_are_bounded_and_must_be_known_formats() {
        let bytes = png(4, 4);
        assert_eq!(
            read_image(&bytes[..], MAX_INPUT_BYTES)
                .unwrap()
                .dimensions(),
            (4, 4)
        );
        assert!(read_image(&bytes[..], bytes.len() as u64 - 1).is_err());
        assert!(read_image(&b"GIF89a not supported"[..], MAX_INPUT_BYTES).is_err());
        assert!(read_image(&b""[..], MAX_INPUT_BYTES).is_err());
        assert!(read_image(&bytes[..bytes.len() / 2], MAX_INPUT_BYTES).is_err());
        assert!(read_image(&png(MAX_INPUT_SIDE + 1, 1)[..], MAX_INPUT_BYTES).is_err());
        assert!(read_image(std::io::repeat(0), 1 << 16).is_err());
    }
}
