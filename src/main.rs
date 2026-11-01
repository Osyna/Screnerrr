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
mod shapes;
mod smart_redact;
mod vector_export;

use anyhow::{Context, Result};
use clap::Parser;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

#[derive(Parser)]
#[command(
    version,
    about = "Capture and annotate Wayland screenshots with offline smart shapes"
)]
struct Args {
    /// Edit an existing image instead of capturing the screen
    #[arg(short, long)]
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
    /// Capture directly to a new PNG without opening the editor
    #[arg(long, conflicts_with = "input")]
    capture: Option<PathBuf>,
    /// Guided scrolling capture of a region; scroll the source yourself between frames
    #[arg(long, conflicts_with_all = ["input", "project", "capture", "list_outputs", "output", "toggle_pins"])]
    scroll: bool,
    /// Hide/show pinned references (suitable for a compositor keyboard binding)
    #[arg(long, conflicts_with_all = ["input", "project", "capture", "list_outputs", "output", "geometry", "region", "cursor"])]
    toggle_pins: bool,
    #[arg(long = "pin-helper", hide = true, conflicts_with_all = ["input", "project", "capture", "list_outputs", "output", "geometry", "region", "cursor", "scroll", "toggle_pins"])]
    pin_helper: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();
    if args.pin_helper {
        return pin::run_helper();
    }
    if args.toggle_pins {
        return pin::toggle_all();
    }
    if let Some(path) = args.project {
        return editor::run(project::Snapshot::load(&path)?.into_document()?);
    }
    if args.list_outputs {
        for name in capture::outputs()? {
            println!("{name}");
        }
        return Ok(());
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
        return editor::run(document::Document::new(image)?);
    }
    let image = match args.input {
        Some(path) => image::open(&path)
            .with_context(|| format!("Cannot open {}", path.display()))?
            .into_rgba8(),
        None => match geometry {
            Some(region) => capture::capture_region(region, args.cursor)?,
            None => capture::capture(args.output.as_deref(), args.cursor)?,
        },
    };
    if let Some(path) = args.capture {
        document::save_new_png(&image, &path)?;
        println!(
            "Saved {} × {} to {}",
            image.width(),
            image.height(),
            path.display()
        );
        return Ok(());
    }
    editor::run(document::Document::new(image)?)
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
}
