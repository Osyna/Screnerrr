//! Launch configuration: bounded strict JSON file plus command-line overrides.
//!
//! Precedence: built-in defaults < `config.json` < command line. Nothing here touches the
//! screen, the clipboard or the output folder; the editor owns every side effect.

use anyhow::{Context, Result, bail, ensure};
use clap::{Args, ValueEnum};
use gtk::gdk;
use gtk::glib::{
    self,
    translate::{IntoGlib, from_glib},
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};

/// A configuration file larger than this is refused instead of read.
const MAX_CONFIG_BYTES: u64 = 1 << 20;
const MAX_COLORS: usize = 32;
const MAX_LIST: usize = 16;
const MAX_STYLESHEET_BYTES: u64 = 256 << 10;

/// Tool names accepted by `initial_tool`.
pub const TOOLS: &[&str] = &[
    "select",
    "smart",
    "freehand",
    "arrow",
    "line",
    "rectangle",
    "ellipse",
    "text",
    "crop",
    "privacy",
    "step",
    "magnifier",
    "spotlight",
    "highlight",
    "highlighter",
];

/// Commands that `keybinds` may rebind; values use GTK accelerator syntax (`<Ctrl><Shift>s`).
pub const COMMANDS: &[&str] = &[
    "select",
    "smart",
    "freehand",
    "arrow",
    "line",
    "rectangle",
    "ellipse",
    "text",
    "crop",
    "privacy",
    "step",
    "magnifier",
    "spotlight",
    "highlight",
    "highlighter",
    "copy",
    "save",
    "save-as",
    "copy-path",
    "undo",
    "redo",
    "toolbar",
    "fullscreen",
    "fit",
    "actual-pixels",
    "quit",
    "fill",
    "round-caps",
    "clear",
    "inspector",
    "palette",
    "ocr",
    "open-project",
    "save-project",
    "zoom-in",
    "zoom-out",
    "delete",
    "select-all",
    "group",
    "ungroup",
    "duplicate",
    "front",
    "back",
    "color-1",
    "color-2",
    "color-3",
    "color-4",
    "color-5",
    "color-6",
    "color-7",
    "color-8",
    "color-9",
    "color-10",
];

/// What Enter, Escape or a right click runs, in order. Default: Enter copies; Escape and right
/// click empty keep SharpShot's cancel/context behaviour. Exit stops the list.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum Action {
    Copy,
    Save,
    SaveAs,
    CopyPath,
    Exit,
}

/// Completed export actions after which the editor closes (`early_exit`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum Trigger {
    Copy,
    Save,
    SaveAs,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub fullscreen: bool,
    pub no_window_decoration: bool,
    /// One-map, non-resizable window hint for tiling compositors.
    pub floating: bool,
    /// `current-screen` or `all`: monitors covered when `fullscreen` is set.
    pub fullscreen_mode: String,
    /// Initial window size; `None` keeps the built-in 1280 × 850.
    pub width: Option<i32>,
    pub height: Option<i32>,
    pub title: String,
    pub app_id: String,
    /// Display scale of the input image. The initial view zoom is `zoom_factor / input_scale`
    /// when either differs from 1; both 1 keeps the default fit-to-window view.
    pub input_scale: f64,
    /// Initial image zoom (1 = actual size relative to `input_scale`), not a wheel step.
    pub zoom_factor: f64,
    /// Distance in screen pixels moved per Alt+arrow pan.
    pub pan_step: f64,
    /// Document pixels a selected caption moves per arrow key; 1 keeps the existing nudge.
    pub text_move: f64,
    pub initial_tool: String,
    /// `block` or `freehand`: which highlighter the primary highlight shortcut selects.
    pub primary_highlighter: String,
    /// Optional user CSS file (`~/` allowed) layered over the app styling; not a GTK theme.
    pub stylesheet: Option<PathBuf>,
    /// `--profile-startup`: log startup phase durations to stderr. Never read from config.
    #[serde(skip)]
    pub profile_startup: bool,
    pub default_fill: bool,
    pub default_round_caps: bool,
    pub corner_radius: f64,
    /// Multiplier applied to line widths, text and arrowheads of newly created marks.
    pub annotation_size_factor: f64,
    /// Freehand smoothing window in points; 0 disables it.
    pub brush_smoothing: usize,
    pub font_family: String,
    pub font_style: String,
    pub font_fallback: Vec<String>,
    /// Swatch colours (`#RRGGBB` or `#RRGGBBAA`), first is the initial colour.
    pub palette: Vec<String>,
    /// Extra colours offered next to the palette.
    pub custom_colors: Vec<String>,
    pub hide_toolbars: bool,
    pub focus_toggle_toolbars: bool,
    /// Save destination; `~/` and strftime codes (`%Y-%m-%d_%H-%M-%S`) are expanded.
    pub output_filename: Option<String>,
    pub auto_copy: bool,
    pub save_after_copy: bool,
    pub early_exit: Vec<Trigger>,
    /// Program and arguments (never a shell line) receiving the rendered PNG on stdin.
    pub copy_command: Option<Vec<String>>,
    pub on_enter: Vec<Action>,
    pub on_escape: Vec<Action>,
    pub on_right_click: Vec<Action>,
    /// Command (see `COMMANDS`) → GTK accelerator. Unlisted commands keep their built-in keys.
    pub keybinds: BTreeMap<String, String>,
    pub disable_notifications: bool,
    pub notification_thumbnail: bool,
}

/// The six house colours, as `#RRGGBB`.
const HOUSE_PALETTE: [&str; 6] = [
    "#F59B34", "#F45360", "#79C554", "#36C6CA", "#A987ED", "#F3F6F7",
];

impl Default for Settings {
    fn default() -> Self {
        Self {
            fullscreen: false,
            no_window_decoration: false,
            floating: false,
            fullscreen_mode: "current-screen".into(),
            width: None,
            height: None,
            title: "SharpShot — Annotate".into(),
            app_id: "io.github.sharpshot.SharpShot".into(),
            input_scale: 1.0,
            zoom_factor: 1.0,
            pan_step: 50.0,
            text_move: 1.0,
            initial_tool: "smart".into(),
            primary_highlighter: "block".into(),
            stylesheet: None,
            profile_startup: false,
            default_fill: false,
            default_round_caps: true,
            corner_radius: 0.0,
            annotation_size_factor: 1.0,
            brush_smoothing: 0,
            font_family: "Sans".into(),
            font_style: "Regular".into(),
            font_fallback: vec![],
            palette: HOUSE_PALETTE.map(String::from).to_vec(),
            custom_colors: vec![],
            hide_toolbars: false,
            focus_toggle_toolbars: false,
            output_filename: None,
            auto_copy: false,
            save_after_copy: false,
            early_exit: vec![],
            copy_command: None,
            on_enter: vec![Action::Copy],
            on_escape: vec![],
            on_right_click: vec![],
            keybinds: BTreeMap::new(),
            disable_notifications: true,
            notification_thumbnail: false,
        }
    }
}

/// `#RRGGBB` or `#RRGGBBAA` → straight-alpha channels in 0..=1.
pub fn parse_color(text: &str) -> Result<[f64; 4]> {
    let hex = text
        .strip_prefix('#')
        .filter(|h| matches!(h.len(), 6 | 8) && h.bytes().all(|b| b.is_ascii_hexdigit()))
        .with_context(|| format!("invalid colour {text:?}; use #RRGGBB or #RRGGBBAA"))?;
    let mut rgba = [0.0, 0.0, 0.0, 1.0];
    for (i, channel) in rgba.iter_mut().enumerate().take(hex.len() / 2) {
        // The slice is ASCII hex by the filter above.
        *channel = f64::from(u8::from_str_radix(&hex[2 * i..2 * i + 2], 16)?) / 255.0;
    }
    Ok(rgba)
}

/// GTK accelerator → key and modifiers. Uses GTK's own parser through FFI because the safe
/// wrapper requires an initialised display, which does not exist while the config is read.
pub fn parse_accelerator(text: &str) -> Option<(gdk::Key, gdk::ModifierType)> {
    let text = std::ffi::CString::new(text).ok()?;
    let (mut key, mut mods) = (0, gdk::ModifierType::empty().into_glib());
    // SAFETY: NUL-terminated input and valid out-pointers; gtk_accelerator_parse only consults
    // the static keyval tables and needs no display.
    let ok = unsafe { gtk::ffi::gtk_accelerator_parse(text.as_ptr(), &mut key, &mut mods) };
    // SAFETY: both values were just produced by GTK.
    (ok != 0 && key != 0).then(|| unsafe { (from_glib(key), from_glib(mods)) })
}

fn text(name: &str, value: &str, max: usize) -> Result<()> {
    ensure!(!value.trim().is_empty(), "{name} must not be empty");
    ensure!(value.len() <= max, "{name} is longer than {max} bytes");
    ensure!(
        !value.chars().any(char::is_control),
        "{name} must not contain control characters"
    );
    Ok(())
}

fn range(name: &str, value: f64, lo: f64, hi: f64) -> Result<()> {
    ensure!(
        value.is_finite() && (lo..=hi).contains(&value),
        "{name} must be a number from {lo} to {hi}, got {value}"
    );
    Ok(())
}

fn family(name: &str, value: &str) -> Result<()> {
    text(name, value, 128)?;
    ensure!(
        !value.contains(','),
        "{name} must be one font family, without commas"
    );
    Ok(())
}

fn colors(name: &str, list: &[String], min: usize) -> Result<()> {
    ensure!(
        (min..=MAX_COLORS).contains(&list.len()),
        "{name} needs {min} to {MAX_COLORS} colours, got {}",
        list.len()
    );
    for c in list {
        parse_color(c).with_context(|| format!("in {name}"))?;
    }
    Ok(())
}

impl Settings {
    /// `$XDG_CONFIG_HOME/sharpshot/config.json` (falls back to `~/.config`).
    pub fn default_config_path() -> PathBuf {
        glib::user_config_dir()
            .join("sharpshot")
            .join("config.json")
    }

    /// Reads `explicit`, which must exist, or the default path, which may be absent.
    pub fn load(explicit: Option<&Path>) -> Result<Self> {
        let (path, required) = match explicit {
            Some(p) => (p.to_path_buf(), true),
            None => (Self::default_config_path(), false),
        };
        let (file, _) = match crate::open_regular_file(&path) {
            Ok(file) => file,
            Err(e) if !required && e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(e) => {
                return Err(e).with_context(|| format!("Cannot open config {}", path.display()));
            }
        };
        Self::from_reader(file, &path.display().to_string())
    }

    fn from_reader(reader: impl Read, origin: &str) -> Result<Self> {
        let mut bytes = Vec::new();
        reader
            .take(MAX_CONFIG_BYTES + 1)
            .read_to_end(&mut bytes)
            .with_context(|| format!("Cannot read config {origin}"))?;
        Self::from_json(&bytes, origin)
    }

    fn from_json(bytes: &[u8], origin: &str) -> Result<Self> {
        ensure!(
            bytes.len() as u64 <= MAX_CONFIG_BYTES,
            "Config {origin} is larger than {} KiB",
            MAX_CONFIG_BYTES >> 10
        );
        let settings: Self = serde_json::from_slice(bytes)
            .with_context(|| format!("Config {origin} is not valid SharpShot settings"))?;
        settings
            .validate()
            .with_context(|| format!("Config {origin} has an invalid value"))?;
        Ok(settings)
    }

    /// Contents of the user stylesheet, if one is configured. Bounded, regular file, UTF-8.
    pub fn read_stylesheet(&self) -> Result<Option<String>> {
        let Some(path) = &self.stylesheet else {
            return Ok(None);
        };
        let path = home_relative(&path.to_string_lossy(), &glib::home_dir());
        let (file, _) = crate::open_regular_file(&path)
            .with_context(|| format!("Cannot open stylesheet {}", path.display()))?;
        let mut bytes = Vec::new();
        file.take(MAX_STYLESHEET_BYTES + 1)
            .read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() as u64 <= MAX_STYLESHEET_BYTES,
            "Stylesheet {} is larger than {} KiB",
            path.display(),
            MAX_STYLESHEET_BYTES >> 10
        );
        String::from_utf8(bytes)
            .map(Some)
            .with_context(|| format!("Stylesheet {} is not UTF-8 text", path.display()))
    }

    pub fn validate(&self) -> Result<()> {
        for (name, side) in [("width", self.width), ("height", self.height)] {
            if let Some(side) = side {
                range(name, f64::from(side), 100.0, 16_384.0)?;
            }
        }
        text("title", &self.title, 256)?;
        text("app_id", &self.app_id, 255)?;
        ensure!(
            gtk::gio::Application::id_is_valid(&self.app_id),
            "app_id {:?} is not a valid application id (like org.example.App)",
            self.app_id
        );
        ensure!(
            matches!(self.fullscreen_mode.as_str(), "current-screen" | "all"),
            "fullscreen_mode must be current-screen or all, got {:?}",
            self.fullscreen_mode
        );
        ensure!(
            matches!(self.primary_highlighter.as_str(), "block" | "freehand"),
            "primary_highlighter must be block or freehand, got {:?}",
            self.primary_highlighter
        );
        if let Some(path) = &self.stylesheet {
            let path = path.to_string_lossy();
            text("stylesheet", &path, 4096)?;
        }
        range("input_scale", self.input_scale, 0.01, 64.0)?;
        range("zoom_factor", self.zoom_factor, 0.01, 64.0)?;
        range("pan_step", self.pan_step, 1.0, 10_000.0)?;
        range("text_move", self.text_move, 0.1, 1_000.0)?;
        ensure!(
            TOOLS.contains(&self.initial_tool.as_str()),
            "initial_tool {:?} is unknown; choose one of: {}",
            self.initial_tool,
            TOOLS.join(", ")
        );
        range("corner_radius", self.corner_radius, 0.0, 1_000.0)?;
        range(
            "annotation_size_factor",
            self.annotation_size_factor,
            0.1,
            10.0,
        )?;
        ensure!(
            self.brush_smoothing <= 64,
            "brush_smoothing must be 0 to 64"
        );
        family("font_family", &self.font_family)?;
        text("font_style", &self.font_style, 64)?;
        ensure!(
            self.font_fallback.len() <= MAX_LIST,
            "font_fallback has more than {MAX_LIST} entries"
        );
        for f in &self.font_fallback {
            family("font_fallback entry", f)?;
        }
        let combined = std::iter::once(self.font_family.as_str())
            .chain(self.font_fallback.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join(", ");
        ensure!(
            crate::document::valid_family(&combined),
            "font_family and font_fallback together exceed the model limit of {} bytes",
            crate::document::MAX_FAMILY
        );
        colors("palette", &self.palette, 1)?;
        colors("custom_colors", &self.custom_colors, 0)?;
        if let Some(template) = &self.output_filename {
            text("output_filename", template, 4096)?;
        }
        ensure!(
            !self.save_after_copy || self.output_filename.is_some(),
            "save_after_copy needs output_filename, because saving after a copy never opens a file picker"
        );
        ensure!(
            self.early_exit.len() <= 3,
            "early_exit has more than 3 entries"
        );
        if let Some(argv) = &self.copy_command {
            ensure!(
                (1..=64).contains(&argv.len()),
                "copy_command needs a program and up to 63 arguments"
            );
            ensure!(
                !argv[0].trim().is_empty(),
                "copy_command program must not be empty"
            );
            for arg in argv {
                ensure!(
                    arg.len() <= 4096 && !arg.contains('\0'),
                    "copy_command has an oversized argument or one containing NUL"
                );
            }
        }
        for (name, actions) in [
            ("on_enter", &self.on_enter),
            ("on_escape", &self.on_escape),
            ("on_right_click", &self.on_right_click),
        ] {
            ensure!(
                actions.len() <= MAX_LIST,
                "{name} has more than {MAX_LIST} actions"
            );
        }
        let mut seen = HashSet::new();
        for (command, accel) in &self.keybinds {
            ensure!(
                COMMANDS.contains(&command.as_str()),
                "keybinds: unknown command {command:?}; choose one of: {}",
                COMMANDS.join(", ")
            );
            let Some((key, mods)) = parse_accelerator(accel) else {
                bail!(
                    "keybinds: {accel:?} for {command} is not a GTK accelerator (like <Ctrl><Shift>s)"
                );
            };
            ensure!(
                !matches!(
                    key,
                    gdk::Key::Alt_L
                        | gdk::Key::Alt_R
                        | gdk::Key::Control_L
                        | gdk::Key::Control_R
                        | gdk::Key::Shift_L
                        | gdk::Key::Shift_R
                        | gdk::Key::Super_L
                        | gdk::Key::Super_R
                        | gdk::Key::Meta_L
                        | gdk::Key::Meta_R
                        | gdk::Key::Hyper_L
                        | gdk::Key::Hyper_R
                ),
                "keybinds: a modifier alone is not an action shortcut"
            );
            ensure!(
                seen.insert((key.to_lower().into_glib(), mods.bits())),
                "keybinds: {accel:?} is assigned to more than one command"
            );
        }
        Ok(())
    }

    /// Key and modifiers bound to `command`, if the user rebound it.
    pub fn accelerator(&self, command: &str) -> Option<(gdk::Key, gdk::ModifierType)> {
        self.keybinds
            .get(command)
            .and_then(|a| parse_accelerator(a))
    }

    /// Expanded `output_filename`; creates nothing. Relative templates stay relative.
    pub fn output_path(&self) -> Result<Option<PathBuf>> {
        let Some(template) = &self.output_filename else {
            return Ok(None);
        };
        let now = glib::DateTime::now_local().context("Cannot read the local time")?;
        expand_output(template, &now, &glib::home_dir()).map(Some)
    }
}

/// `~` is expanded before strftime so a `%` in the home path is never interpreted.
fn expand_output(template: &str, now: &glib::DateTime, home: &Path) -> Result<PathBuf> {
    ensure!(
        !template.is_empty() && !template.chars().any(char::is_control),
        "output_filename is empty or contains control characters"
    );
    let (base, rest) = match template {
        "~" => (Some(home), ""),
        t => match t.strip_prefix("~/") {
            Some(rest) => (Some(home), rest),
            None => (None, t),
        },
    };
    let rest = if rest.is_empty() {
        String::new()
    } else {
        now.format(rest)
            .with_context(|| format!("Bad date format in output_filename {template:?}"))?
            .to_string()
    };
    Ok(match base {
        Some(home) => home.join(rest),
        None => PathBuf::from(rest),
    })
}

/// Leading `~/` (or a bare `~`) means the home folder; anything else is returned unchanged.
fn home_relative(path: &str, home: &Path) -> PathBuf {
    match path {
        "~" => home.to_path_buf(),
        p => p
            .strip_prefix("~/")
            .map_or_else(|| PathBuf::from(p), |rest| home.join(rest)),
    }
}

fn parse_bind(arg: &str) -> Result<(String, String), String> {
    arg.split_once('=')
        .filter(|(c, a)| !c.is_empty() && !a.is_empty())
        .map(|(c, a)| (c.to_owned(), a.to_owned()))
        .ok_or_else(|| "expected COMMAND=ACCELERATOR, e.g. save=<Ctrl>s".to_owned())
}

/// Command-line overrides; every option that is given beats the config file.
/// Boolean options accept `--flag` (true) or `--flag=false`.
#[derive(Args, Debug, Default)]
pub struct LaunchArgs {
    /// Settings file (default: $XDG_CONFIG_HOME/sharpshot/config.json)
    #[arg(long, value_name = "FILE")]
    pub config: Option<PathBuf>,
    /// Open the editor fullscreen
    #[arg(short = 'f', long, num_args = 0..=1, require_equals = true, default_missing_value = "true", value_name = "BOOL")]
    fullscreen: Option<bool>,
    /// Hide the window titlebar and decorations
    #[arg(long, num_args = 0..=1, require_equals = true, default_missing_value = "true", value_name = "BOOL")]
    no_window_decoration: Option<bool>,
    /// Open the editor as a floating, non-resizable window (a hint for tiling compositors)
    #[arg(long, num_args = 0..=1, require_equals = true, default_missing_value = "true", value_name = "BOOL")]
    floating: Option<bool>,
    /// Which monitors --fullscreen covers: current-screen or all
    #[arg(long, value_parser = ["current-screen", "all"], value_name = "MODE")]
    fullscreen_mode: Option<String>,
    /// Initial window width in pixels
    #[arg(long, value_name = "PX")]
    width: Option<i32>,
    /// Initial window height in pixels
    #[arg(long, value_name = "PX")]
    height: Option<i32>,
    /// Window title
    #[arg(long)]
    title: Option<String>,
    /// Application id (reverse-DNS, e.g. org.example.Shots)
    #[arg(long, value_name = "ID")]
    app_id: Option<String>,
    /// Display scale of the input image; initial zoom is zoom-factor / input-scale
    #[arg(long, value_name = "SCALE")]
    input_scale: Option<f64>,
    /// Initial image zoom (1 = fit when input-scale is also 1); not a wheel step
    #[arg(long, value_name = "ZOOM")]
    zoom_factor: Option<f64>,
    /// Screen pixels moved per Alt+arrow pan
    #[arg(long, alias = "pan-step-size", value_name = "PX")]
    pan_step: Option<f64>,
    /// Document pixels a selected caption moves per arrow key (default 1)
    #[arg(long, alias = "text-move-length", value_name = "PX")]
    text_move: Option<f64>,
    /// Tool selected at launch
    #[arg(long, alias = "init-tool", value_name = "TOOL")]
    initial_tool: Option<String>,
    /// Highlighter that G (and the primary highlight button) selects: block or freehand
    #[arg(long, value_parser = ["block", "freehand"], value_name = "KIND")]
    primary_highlighter: Option<String>,
    /// Extra application stylesheet (CSS file, ~/ allowed); layered over the built-in styling
    #[arg(long, value_name = "FILE")]
    stylesheet: Option<PathBuf>,
    /// Log how long each startup phase takes to standard error; sends nothing anywhere
    #[arg(long)]
    pub(crate) profile_startup: bool,
    /// Fill new shapes by default
    #[arg(long, alias = "default-fill-shapes", num_args = 0..=1, require_equals = true, default_missing_value = "true", value_name = "BOOL")]
    fill: Option<bool>,
    /// Round line caps on new marks
    #[arg(long, num_args = 0..=1, require_equals = true, default_missing_value = "true", value_name = "BOOL")]
    round_caps: Option<bool>,
    /// Corner radius of new rectangles
    #[arg(long, alias = "corner-roundness", value_name = "PX")]
    corner_radius: Option<f64>,
    /// Multiplier for width/size of new marks
    #[arg(long, value_name = "FACTOR")]
    annotation_size_factor: Option<f64>,
    /// Freehand smoothing window in points (0 = off)
    #[arg(long, alias = "brush-smooth-history-size", value_name = "N")]
    brush_smoothing: Option<usize>,
    /// Font family for new text
    #[arg(long)]
    font_family: Option<String>,
    /// Font style for new text, e.g. Regular or "Bold Italic"
    #[arg(long)]
    font_style: Option<String>,
    /// Comma-separated fallback font families
    #[arg(long, value_delimiter = ',')]
    font_fallback: Vec<String>,
    /// Comma-separated swatches (#RRGGBB or #RRGGBBAA); first is the initial colour
    #[arg(long, value_delimiter = ',', value_name = "COLORS")]
    palette: Vec<String>,
    /// Comma-separated extra colours offered beside the palette
    #[arg(long, value_delimiter = ',', value_name = "COLORS")]
    custom_colors: Vec<String>,
    /// Start with the toolbars hidden
    #[arg(long, alias = "default-hide-toolbars", num_args = 0..=1, require_equals = true, default_missing_value = "true", value_name = "BOOL")]
    hide_toolbars: Option<bool>,
    /// Let the toolbar toggle also move focus
    #[arg(long, num_args = 0..=1, require_equals = true, default_missing_value = "true", value_name = "BOOL")]
    focus_toggle_toolbars: Option<bool>,
    /// Save destination; supports ~/ and strftime codes such as %Y-%m-%d_%H-%M-%S
    #[arg(long, value_name = "TEMPLATE")]
    output_filename: Option<String>,
    /// Copy the result to the clipboard whenever it is saved
    #[arg(long, num_args = 0..=1, require_equals = true, default_missing_value = "true", value_name = "BOOL")]
    auto_copy: Option<bool>,
    /// Also save after copying
    #[arg(long, num_args = 0..=1, require_equals = true, default_missing_value = "true", value_name = "BOOL")]
    save_after_copy: Option<bool>,
    /// Comma-separated exports after which the editor closes
    #[arg(long, value_delimiter = ',', value_enum)]
    early_exit: Vec<Trigger>,
    /// Copy program and arguments, no shell; repeat once per argument
    #[arg(long, action = clap::ArgAction::Append, num_args = 1, allow_hyphen_values = true, value_name = "ARG")]
    copy_command: Vec<String>,
    /// Comma-separated actions run on Enter
    #[arg(long, alias = "actions-on-enter", value_delimiter = ',', value_enum)]
    on_enter: Vec<Action>,
    /// Comma-separated actions run on Escape
    #[arg(long, alias = "actions-on-escape", value_delimiter = ',', value_enum)]
    on_escape: Vec<Action>,
    /// Comma-separated actions run on right click
    #[arg(
        long,
        alias = "actions-on-right-click",
        value_delimiter = ',',
        value_enum
    )]
    on_right_click: Vec<Action>,
    /// Rebind a command, e.g. --keybind save=<Ctrl><Shift>s (repeatable)
    #[arg(long, value_parser = parse_bind, value_name = "COMMAND=ACCEL")]
    keybind: Vec<(String, String)>,
    /// Suppress desktop notifications
    #[arg(long, num_args = 0..=1, require_equals = true, default_missing_value = "true", value_name = "BOOL")]
    disable_notifications: Option<bool>,
    /// Show the image thumbnail in notifications
    #[arg(long, num_args = 0..=1, require_equals = true, default_missing_value = "true", value_name = "BOOL")]
    notification_thumbnail: Option<bool>,
}

fn set<T>(target: &mut T, value: Option<T>) {
    if let Some(value) = value {
        *target = value;
    }
}

fn set_list<T>(target: &mut Vec<T>, value: Vec<T>) {
    if !value.is_empty() {
        *target = value;
    }
}

impl LaunchArgs {
    /// Config file, then these overrides, then validation of the merged result.
    pub fn resolve(self) -> Result<Settings> {
        let mut settings = Settings::load(self.config.as_deref())?;
        self.apply(&mut settings);
        settings
            .validate()
            .context("Invalid value in command-line options")?;
        Ok(settings)
    }

    fn apply(self, s: &mut Settings) {
        set(&mut s.fullscreen, self.fullscreen);
        set(&mut s.no_window_decoration, self.no_window_decoration);
        set(&mut s.floating, self.floating);
        set(&mut s.fullscreen_mode, self.fullscreen_mode);
        s.width = self.width.or(s.width);
        s.height = self.height.or(s.height);
        set(&mut s.title, self.title);
        set(&mut s.app_id, self.app_id);
        set(&mut s.input_scale, self.input_scale);
        set(&mut s.zoom_factor, self.zoom_factor);
        set(&mut s.pan_step, self.pan_step);
        set(&mut s.text_move, self.text_move);
        set(&mut s.initial_tool, self.initial_tool);
        set(&mut s.primary_highlighter, self.primary_highlighter);
        s.stylesheet = self.stylesheet.or(s.stylesheet.take());
        s.profile_startup = self.profile_startup;
        set(&mut s.default_fill, self.fill);
        set(&mut s.default_round_caps, self.round_caps);
        set(&mut s.corner_radius, self.corner_radius);
        set(&mut s.annotation_size_factor, self.annotation_size_factor);
        set(&mut s.brush_smoothing, self.brush_smoothing);
        set(&mut s.font_family, self.font_family);
        set(&mut s.font_style, self.font_style);
        set_list(&mut s.font_fallback, self.font_fallback);
        set_list(&mut s.palette, self.palette);
        set_list(&mut s.custom_colors, self.custom_colors);
        set(&mut s.hide_toolbars, self.hide_toolbars);
        set(&mut s.focus_toggle_toolbars, self.focus_toggle_toolbars);
        s.output_filename = self.output_filename.or(s.output_filename.take());
        set(&mut s.auto_copy, self.auto_copy);
        set(&mut s.save_after_copy, self.save_after_copy);
        set_list(&mut s.early_exit, self.early_exit);
        if !self.copy_command.is_empty() {
            s.copy_command = Some(self.copy_command);
        }
        set_list(&mut s.on_enter, self.on_enter);
        set_list(&mut s.on_escape, self.on_escape);
        set_list(&mut s.on_right_click, self.on_right_click);
        s.keybinds.extend(self.keybind);
        set(&mut s.disable_notifications, self.disable_notifications);
        set(&mut s.notification_thumbnail, self.notification_thumbnail);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    #[test]
    fn assembled_font_fallback_list_must_fit_the_persisted_model() {
        let mut settings = Settings::default();
        settings.font_family = "a".repeat(128);
        settings.font_fallback = vec!["b".repeat(128)];
        assert!(settings.validate().is_err());
        settings.font_fallback = vec!["Sans".into()];
        assert!(settings.validate().is_ok());
    }

    #[derive(Parser)]
    struct Cli {
        #[command(flatten)]
        launch: LaunchArgs,
    }

    fn cli(args: &[&str]) -> LaunchArgs {
        Cli::try_parse_from(std::iter::once("sharpshot").chain(args.iter().copied()))
            .unwrap()
            .launch
    }

    fn load(json: &str) -> Result<Settings> {
        Settings::from_json(json.as_bytes(), "test.json")
    }

    fn rejected(json: &str, needle: &str) {
        let error = format!("{:#}", load(json).unwrap_err());
        assert!(error.contains(needle), "{needle:?} not in {error:?}");
    }

    #[test]
    fn defaults_are_valid_and_keep_the_house_palette() {
        let s = Settings::default();
        s.validate().unwrap();
        let rgb: Vec<_> = s
            .palette
            .iter()
            .map(|c| parse_color(c).unwrap().map(|v| (v * 255.0).round() as u8))
            .collect();
        assert_eq!(
            rgb,
            [
                [245, 155, 52, 255],
                [244, 83, 96, 255],
                [121, 197, 84, 255],
                [54, 198, 202, 255],
                [169, 135, 237, 255],
                [243, 246, 247, 255]
            ]
        );
        assert_eq!(load("{}").unwrap(), s);
    }

    #[test]
    fn colours_are_strict() {
        assert_eq!(parse_color("#ff0000").unwrap(), [1.0, 0.0, 0.0, 1.0]);
        assert_eq!(parse_color("#00000080").unwrap()[3], 128.0 / 255.0);
        for bad in [
            "", "#", "#fff", "red", "ff0000", "#12345g", "#1234567", "#éé123",
        ] {
            assert!(parse_color(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn partial_config_overrides_only_what_it_names() {
        let s =
            load(r##"{"title":"Mine","on_enter":["copy","exit"],"palette":["#112233"]}"##).unwrap();
        assert_eq!(s.title, "Mine");
        assert_eq!(s.on_enter, [Action::Copy, Action::Exit]);
        assert_eq!(s.palette, ["#112233"]);
        assert_eq!(s.pan_step, Settings::default().pan_step);
    }

    #[test]
    fn corrupt_unknown_and_oversize_config_is_actionable() {
        rejected("{", "not valid SharpShot settings");
        rejected(r#"{"colour":1}"#, "unknown field `colour`");
        rejected(r#"{"on_enter":["explode"]}"#, "unknown variant `explode`");
        rejected(r#"{"early_exit":["exit"]}"#, "unknown variant `exit`");
        rejected(r#"{"title":"a","title":"b"}"#, "duplicate field");
        let big = format!(r#"{{"title":"{}"}}"#, "x".repeat(MAX_CONFIG_BYTES as usize));
        rejected(&big, "larger than 1024 KiB");
        // Reading stops at the cap instead of slurping an endless source.
        let endless = std::io::repeat(b' ');
        assert!(Settings::from_reader(endless, "endless").is_err());
    }

    #[test]
    fn out_of_range_values_name_the_setting() {
        for (json, needle) in [
            (r#"{"zoom_factor":0}"#, "zoom_factor"),
            (r#"{"input_scale":1e9}"#, "input_scale"),
            (r#"{"width":50}"#, "width"),
            (r#"{"height":100000}"#, "height"),
            (r#"{"corner_radius":-1}"#, "corner_radius"),
            (r#"{"annotation_size_factor":11}"#, "annotation_size_factor"),
            (r#"{"brush_smoothing":65}"#, "brush_smoothing"),
            (r#"{"initial_tool":"pencil"}"#, "initial_tool"),
            (r#"{"app_id":"nodots"}"#, "app_id"),
            (r#"{"title":"  "}"#, "title"),
            (r#"{"font_family":"A,B"}"#, "font_family"),
            (r#"{"palette":[]}"#, "palette"),
            (r#"{"custom_colors":["blue"]}"#, "custom_colors"),
            (r#"{"output_filename":"a\u0000b"}"#, "output_filename"),
            (r#"{"copy_command":[]}"#, "copy_command"),
            (r#"{"copy_command":[" ","x"]}"#, "copy_command"),
            (r#"{"save_after_copy":true}"#, "save_after_copy"),
        ] {
            rejected(json, needle);
        }
    }

    #[test]
    fn keybinds_reject_unknown_commands_bad_accelerators_and_duplicates() {
        load(r#"{"keybinds":{"save":"<Ctrl><Shift>s","highlight":"h"}}"#).unwrap();
        rejected(r#"{"keybinds":{"explode":"x"}}"#, "unknown command");
        rejected(r#"{"keybinds":{"save":"<Ctrl>"}}"#, "not a GTK accelerator");
        rejected(r#"{"keybinds":{"save":"Alt_L"}}"#, "modifier alone");
        rejected(
            r#"{"keybinds":{"save":"nonsensekey"}}"#,
            "not a GTK accelerator",
        );
        rejected(
            r#"{"keybinds":{"save":"<Ctrl>s","copy":"<Control>S"}}"#,
            "more than one command",
        );
        let s = load(r#"{"keybinds":{"save":"<Ctrl><Shift>s"}}"#).unwrap();
        let (key, mods) = s.accelerator("save").unwrap();
        assert_eq!(key.to_lower(), gdk::Key::s);
        assert!(mods.contains(gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::SHIFT_MASK));
        assert!(s.accelerator("copy").is_none());
    }

    #[test]
    fn output_templates_expand_home_and_dates_without_touching_disk() {
        let when = glib::DateTime::from_utc(2026, 11, 2, 3, 4, 5.0).unwrap();
        let home = Path::new("/home/a%b");
        let expand = |t| expand_output(t, &when, home).unwrap();
        assert_eq!(
            expand("~/Shots/s-%Y-%m-%d_%H-%M-%S.png"),
            Path::new("/home/a%b/Shots/s-2026-11-02_03-04-05.png")
        );
        assert_eq!(expand("~"), home);
        assert_eq!(expand("rel/100%%.png"), Path::new("rel/100%.png"));
        assert_eq!(expand("/tmp/x.png"), Path::new("/tmp/x.png"));
        // Only a bare `~` or `~/` means home; `~other` is an ordinary relative name.
        assert_eq!(expand("~other/x.png"), Path::new("~other/x.png"));
        assert!(expand_output("", &when, home).is_err());
        assert_eq!(Settings::default().output_path().unwrap(), None);
    }

    #[test]
    fn command_line_beats_config_including_explicit_false() {
        let mut s = load(
            r##"{"fullscreen":true,"title":"cfg","on_enter":["save"],"width":900,"keybinds":{"save":"<Ctrl>s"},"palette":["#000000"]}"##,
        )
        .unwrap();
        cli(&[
            "--fullscreen=false",
            "--title",
            "cli",
            "--on-enter",
            "copy,exit",
            "--height",
            "700",
            "--keybind",
            "save=<Ctrl><Shift>s",
            "--keybind",
            "copy=<Ctrl>c",
            "--palette",
            "#FFFFFF,#ff000080",
            "--copy-command",
            "wl-copy",
            "--copy-command",
            "--type=image/png",
            "--early-exit",
            "save,save-as",
        ])
        .apply(&mut s);
        s.validate().unwrap();
        assert!(!s.fullscreen);
        assert_eq!(
            (s.title.as_str(), s.width, s.height),
            ("cli", Some(900), Some(700))
        );
        assert_eq!(s.on_enter, [Action::Copy, Action::Exit]);
        assert_eq!(s.keybinds["save"], "<Ctrl><Shift>s");
        assert_eq!(s.palette, ["#FFFFFF", "#ff000080"]);
        assert_eq!(
            s.copy_command.as_deref().unwrap(),
            ["wl-copy", "--type=image/png"]
        );
        assert_eq!(s.early_exit, [Trigger::Save, Trigger::SaveAs]);
    }

    #[test]
    fn bare_flags_mean_true_and_absent_flags_change_nothing() {
        let mut s = Settings::default();
        cli(&["--hide-toolbars", "--fill", "--disable-notifications=false"]).apply(&mut s);
        assert!(s.hide_toolbars && s.default_fill && !s.disable_notifications);
        assert!(s.default_round_caps && !s.fullscreen);
        assert!(Cli::try_parse_from(["x", "--keybind", "nonsense"]).is_err());
        assert!(Cli::try_parse_from(["x", "--on-enter", "explode"]).is_err());
    }

    #[test]
    fn example_config_lists_every_setting_and_is_valid() {
        let example = include_str!("../config.example.json");
        let parsed = load(example).unwrap();
        let keys = |v: serde_json::Value| -> Vec<String> {
            v.as_object().unwrap().keys().cloned().collect()
        };
        let mut have = keys(serde_json::from_str(example).unwrap());
        let mut want = keys(serde_json::to_value(Settings::default()).unwrap());
        have.sort();
        want.sort();
        assert_eq!(have, want);
        assert_eq!(parsed.zoom_factor, 1.0);
    }

    #[test]
    fn shipped_controls_default_quiet_and_validate() {
        let s = Settings::default();
        assert!(!s.floating && !s.profile_startup);
        assert_eq!(s.fullscreen_mode, "current-screen");
        assert_eq!(s.primary_highlighter, "block");
        assert_eq!(s.stylesheet, None);
        assert!(s.disable_notifications && !s.notification_thumbnail);
        let s = load(r#"{"floating":true,"fullscreen_mode":"all","primary_highlighter":"freehand","stylesheet":"~/x.css"}"#).unwrap();
        assert!(s.floating && s.fullscreen_mode == "all" && s.primary_highlighter == "freehand");
        for (json, needle) in [
            (r#"{"fullscreen_mode":"both"}"#, "fullscreen_mode"),
            (r#"{"primary_highlighter":"pen"}"#, "primary_highlighter"),
            (r#"{"stylesheet":""}"#, "stylesheet"),
            (r#"{"stylesheet":"a\u0000b.css"}"#, "stylesheet"),
            (r#"{"profile_startup":true}"#, "unknown field"),
        ] {
            rejected(json, needle);
        }
    }

    #[test]
    fn digit_colour_commands_replace_unsupported_overlay_commands() {
        let s = load(r#"{"keybinds":{"color-1":"<Alt>1","color-10":"<Alt>0"}}"#).unwrap();
        assert!(s.accelerator("color-10").is_some());
        for bad in ["color-0", "color-11", "paste", "import-image"] {
            rejected(
                &format!(r#"{{"keybinds":{{"{bad}":"x"}}}}"#),
                "unknown command",
            );
        }
    }

    #[test]
    fn new_flags_override_config_and_profile_flag_is_cli_only() {
        let mut s = load(r#"{"primary_highlighter":"freehand","fullscreen_mode":"all"}"#).unwrap();
        cli(&[
            "--floating",
            "--primary-highlighter",
            "block",
            "--fullscreen-mode",
            "current-screen",
            "--stylesheet",
            "/tmp/a.css",
            "--profile-startup",
        ])
        .apply(&mut s);
        assert!(s.floating && s.profile_startup);
        assert_eq!(
            (s.primary_highlighter.as_str(), s.fullscreen_mode.as_str()),
            ("block", "current-screen")
        );
        assert_eq!(s.stylesheet.as_deref(), Some(Path::new("/tmp/a.css")));
        assert!(Cli::try_parse_from(["x", "--fullscreen-mode", "both"]).is_err());
        assert!(Cli::try_parse_from(["x", "--primary-highlighter", "pen"]).is_err());
        cli(&[
            "--default-hide-toolbars",
            "--default-fill-shapes",
            "--corner-roundness",
            "6",
            "--brush-smooth-history-size",
            "3",
        ])
        .apply(&mut s);
        assert!(
            s.hide_toolbars && s.default_fill && s.corner_radius == 6.0 && s.brush_smoothing == 3
        );
    }

    #[test]
    fn stylesheet_is_bounded_regular_utf8_and_home_relative() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.css");
        std::fs::write(&path, "window { background: red; }").unwrap();
        let mut s = Settings {
            stylesheet: Some(path.clone()),
            ..Settings::default()
        };
        assert_eq!(
            s.read_stylesheet().unwrap().unwrap(),
            "window { background: red; }"
        );
        std::fs::write(&path, vec![b'a'; MAX_STYLESHEET_BYTES as usize + 1]).unwrap();
        assert!(format!("{:#}", s.read_stylesheet().unwrap_err()).contains("larger than"));
        std::fs::write(&path, [0xff, 0xfe]).unwrap();
        assert!(s.read_stylesheet().is_err());
        s.stylesheet = Some(dir.path().to_path_buf());
        assert!(s.read_stylesheet().is_err());
        s.stylesheet = Some(dir.path().join("missing.css"));
        assert!(format!("{:#}", s.read_stylesheet().unwrap_err()).contains("missing.css"));
        assert_eq!(
            home_relative("~/a.css", Path::new("/h")),
            Path::new("/h/a.css")
        );
        assert_eq!(
            home_relative("~x/a.css", Path::new("/h")),
            Path::new("~x/a.css")
        );
        assert_eq!(Settings::default().read_stylesheet().unwrap(), None);
    }
}
