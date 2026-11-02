# SharpShot

A native Rust screenshot editor for Linux, with wlroots Wayland capture, local PaddleOCR and an offline shape helper powered by [Desert Ant Labs Shapes](https://desertant.com/models/shapes/).

Capture a monitor or desktop region, annotate editable vector objects, and copy a PNG or save an editable project. Smart strokes fit to shapes. The inspector edits geometry and mixed selections; text edits directly on the canvas without stretched glyphs. Local PaddleOCR supplies selectable text, markup and reviewed smart redaction. Saved styles, screenshot framing, pinned reference images, scrolling capture and opt-in crash recovery are included.

## Build

Requires Linux, Rust 1.92+, a C toolchain, pkg-config, GTK 4.8+ and Cairo 1.16+ development libraries. The prebuilt ONNX Runtime used for OCR also requires a modern glibc/libstdc++ toolchain; Arch Linux and Debian 13 are supported build environments. Debian 12's native libraries are too old for this runtime.

```sh
# Arch Linux
sudo pacman -S --needed rust base-devel pkgconf gtk4 openssl slurp ttf-dejavu

# Debian 13+: install Rust separately with rustup
sudo apt install build-essential pkg-config libgtk-4-dev libssl-dev ca-certificates slurp fonts-dejavu-core

cargo build --release --locked
./target/release/sharpshot
```

Or install the executable into Cargo's bin directory:

```sh
cargo install --path . --locked
```

The Shapes and PP-OCRv5 mobile models are bundled in the executable. No model download, API key, Python, JavaScript, or `grim` is needed at runtime. Interactive `--region` selection uses `slurp`; it returns coordinates only. Full-monitor capture and explicit `--geometry` do not require it.

Shape inference uses Google's native LiteRT CPU runtime through Rust bindings. The first build downloads a checksum-pinned `libLiteRt.so` into the `litert-sys` cache. The executable can find that cache automatically. For an offline build, set `LITERT_LIB_DIR` to a directory containing the compatible library. OCR runs PaddleOCR's detector and recogniser locally through the Rust `ort` binding to statically linked ONNX Runtime; its first build also needs the native runtime download/cache.

For distribution, or to keep an installed executable independent of that cache, place `libLiteRt.so` next to `sharpshot`. For example, with the default x86-64 cache:

```sh
cp "${XDG_CACHE_HOME:-$HOME/.cache}/litert-sys/v0.10.2/x86_64-unknown-linux-gnu/libLiteRt.so" target/release/
# After cargo install, copy it to ~/.cargo/bin/ instead.
```

Wayland reference pins also require `libgtk4-layer-shell.so.0`, either next to the executable or installed by the system (`gtk4-layer-shell` on Arch, `libgtk4-layer-shell0` on Debian). The pin helper loads it before GTK initialises; other features do not require it. X11 pins use the window manager's native always-on-top support.

Include the runtime notices from `assets/runtime/` and the OCR model/runtime licences and notices from `assets/ocr/` when distributing a build. GTK4 and the C/C++ runtime remain system dependencies. No ONNX Runtime `.so` or GPU accelerator library is needed.

## Capture

```sh
sharpshot                         # capture the first output, then annotate
sharpshot --list-outputs          # discover monitor names
sharpshot --output DP-1           # capture a particular monitor
sharpshot --region                # drag a desktop zone, then annotate; Escape cancels
sharpshot --geometry "100,80 640x480" # capture explicit logical desktop coordinates
sharpshot --cursor                # include the pointer
sharpshot --input image.png       # annotate an existing PNG, JPEG, or WebP
cat image.png | sharpshot --input - # PNG, JPEG or WebP through stdin
sharpshot --capture capture.png   # save a new PNG without opening the editor
sharpshot --region --capture zone.png # select and save without opening the editor
sharpshot --region --capture - > zone.png # PNG-only stdout; status goes to stderr
sharpshot --project shot.sharp    # reopen an editable project
sharpshot --scroll --region       # guided vertical scrolling capture, then annotate
sharpshot --scroll --geometry "100,80 640x480" # fixed scrolling-content region
sharpshot --toggle-pins           # hide/show reference pins and restore pointer input
```

Capture uses the compositor's `zwlr_screencopy_manager_v1` protocol directly in Rust. It preserves physical pixel resolution, accounts for row stride and output transforms, and has bounded waits if the compositor stops responding. The screenshot is captured **before** the editor opens, so the editor is not included.

Regions can cross monitors, including negative desktop coordinates. Mixed-DPI captures use the highest participating scale; pixels in monitor gaps or outside the desktop are transparent. Normal outputs copy only the selected intersection. Rotated/flipped outputs capture an upright frame before cropping because older wlroots versions misplace direct region requests. Desktop selection finishes before capture; cancelling never falls back to a fullscreen screenshot.

For Lua-based Hyprland configurations:

```lua
hl.bind("SUPER + ALT + S", hl.dsp.exec_cmd("sharpshot --region"))
hl.window_rule({
    name = "sharpshot",
    match = { class = "^(io\\.github\\.sharpshot\\.SharpShot|sharpshot)$" },
    float = true,
    center = true,
    max_size = { "monitor_w*0.95", "monitor_h*0.95" },
})
```

For `.conf`-based Hyprland configurations, a binding can launch it directly:

```ini
bind = SUPER ALT, S, exec, sharpshot --region
```

The executable must be in Hyprland's `PATH`; otherwise use its absolute path. There is no global shortcut grabber. Reference pins keep a private helper process alive until the last pin closes.

The Lua rule floats SharpShot above tiled windows on its current workspace and caps its size to the monitor. It deliberately does not pin the editor: pinning also makes it follow workspaces and can cover unpinned export dialogs. Other floating windows can come forward normally.

### Hyprland permissions

If `ecosystem:enforce_permissions` is enabled, a new executable must be explicitly allowed to capture. Approve Hyprland's screencopy prompt on first launch. SharpShot allows up to 60 seconds for that response, then exits with an error; it never bypasses the compositor's permission gate or runs grim under an allowed binary name.

To persist an allow rule, use the exact executable path (`realpath ./target/release/sharpshot`, or the installed path). For example, after installation into `/usr/local/bin`, Lua configuration uses:

```lua
hl.permission({
    binary = "^/usr/local/bin/sharpshot$",
    type = "screencopy",
    mode = "allow",
})
```

The equivalent `.conf` rule is `permission = ^/usr/local/bin/sharpshot$, screencopy, allow`. Use your real installation path and escape regex metacharacters in it. Hyprland permission-rule changes require a compositor restart, not just a config reload; do not restart an active session solely to run a test.


## Configuration and automated workflows

SharpShot reads `$XDG_CONFIG_HOME/sharpshot/config.json`, normally `~/.config/sharpshot/config.json`. `--config FILE` selects an explicit file. Precedence is built-in defaults, JSON, then CLI options. Unknown fields, invalid values and files larger than 1 MiB are rejected. Configuration, stylesheet and image-file inputs must be regular files; use `--input -` for a pipe.

[config.example.json](config.example.json) lists every saved setting. SharpShot uses its own JSON format, not Satty TOML. Saved annotation styles and Designer frames remain in `presets.json`; launch settings do not replace them.

| Setting group | Fields and behavior |
| --- | --- |
| Launch | `initial_tool`, `width`, `height`, `title`, `app_id`; minimum editor size is 760 × 440 |
| View | `zoom_factor / input_scale` sets initial zoom, clamped to 10% through 400%; default 1/1 keeps Fit |
| Window | `fullscreen`, `fullscreen_mode` (`current-screen` or `all`), `floating`, `no_window_decoration`; the compositor decides floating and monitor-spanning behavior |
| Toolbars | `hide_toolbars` sets initial visibility; `focus_toggle_toolbars` hides/shows on focus changes without replacing the initial choice |
| Drawing | `default_fill`, `default_round_caps`, `corner_radius`, `annotation_size_factor`, `brush_smoothing` |
| Fonts | `font_family`, `font_style`, `font_fallback`; Pango face keywords such as Heavy, Light, Italic or Condensed, with installed-font fallback |
| Colours | `palette`, `custom_colors`; `#RRGGBB` preserves object opacity, `#RRGGBBAA` explicitly replaces it |
| Actions | `on_enter`, `on_escape`, `on_right_click`, `auto_copy`, `copy_command`, `save_after_copy`, `early_exit`, `output_filename` |
| Bindings | `keybinds` maps a supported command to a GTK accelerator, such as `rectangle: u` or `toolbar: F6` |
| Appearance | `stylesheet` is an explicit house-style override, bounded to 256 KiB of UTF-8 CSS |
| Notifications | `disable_notifications` defaults true; `notification_thumbnail` uses the rendered image, not the original |

CLI names use hyphens, for example `--font-style`, `--brush-smoothing`, `--initial-tool` and `--no-window-decoration`. `--profile-startup` is CLI-only and prints local timings to stderr. `--help` lists options and aliases.

Custom shortcuts win over defaults and disable the old binding of that command. Native editable fields keep their typing and numeric-editing keys. `color-1` through `color-10` can bind bare digits; defaults use Alt+1 through Alt+0 so 0 still means Fit and 1 means actual pixels. Palette slots beyond ten have no default digit shortcut.

### Action order and clipboard lifetime

Actions are `copy`, `save`, `save-as`, `copy-path` and `exit`, run in order. Enter defaults to image copy outside caption editing. Empty Escape/right-click lists retain cancellation and context menus; assigning right-click actions replaces that pointer menu, while Menu/Shift+F10 remains available. Caption edits, active gestures and unsaved closes stay protected.

```sh
sharpshot --region \
  --output-filename "$HOME/Pictures/Shot-%Y-%m-%d_%H-%M-%S-%f.png" \
  --on-enter save,exit

sharpshot --region \
  --copy-command wl-copy \
  --output-filename "$HOME/Pictures/Shot-%Y-%m-%d_%H-%M-%S-%f.png" \
  --save-after-copy --early-exit copy
```

Output paths expand leading `~/` and GLib date/time format specifiers. Named saves create missing parent directories and refuse to overwrite. Save as always opens a chooser. Output `-` streams PNG to stdout; it does not become a saved file path. `copy-path` uses the last successful PNG filename.

`copy_command` is an argv array in JSON and repeated `--copy-command` components on the CLI. It receives PNG on stdin, with no implicit shell. For example, `--copy-command wl-copy --copy-command=--type --copy-command image/png` fixes the MIME type. Install the helper yourself. A failed or timed-out command leaves the editor open; stderr from arbitrary helpers is not copied into the interface. Run the configured program in a terminal to inspect its own diagnostics.

The timeout stops the direct configured process, not every descendant it may spawn. Use `exec` in a single-command shell wrapper; multi-process hooks must manage their own child lifetimes. Successful clipboard daemons are deliberately allowed to outlive the editor.

`auto_copy` waits for document edits and debounces them for 200 ms. Launching or moving the pointer does not replace the clipboard. `save_after_copy` follows manual image copies, not each auto-copy. `early_exit` accepts `copy`, `save` and `save-as` triggers and only follows success. Native copying refuses automatic exit if no clipboard manager can preserve the image; the editor stays open with recovery instructions. A successful external helper such as `wl-copy` owns its own clipboard data.

## Annotate

The top strip contains icon tools with delayed native GTK tooltips and accessible names. Contextual actions appear below the canvas; OCR copy and markup actions appear there only while the text layer is enabled. The bottom strip holds colour, line thickness, opacity, zoom, output dimensions and crop reset. Tool and style strips scroll in narrow windows; `?` opens the shortcut guide.

The shortcuts below are defaults. A configured binding replaces that command's default.

| Tool or action | Shortcut | Behavior |
| --- | --- | --- |
| Select / move | V | Click painted ink or a filled object; unfilled interiors pass clicks through. Shift-click adds or removes; drag empty canvas to marquee-select |
| Select all | Ctrl+A | Select every annotation |
| Resize / rotate | Drag a handle | Square handles resize; the round handle rotates. Text and groups containing text always keep glyph proportions; Shift constrains other shapes or snaps rotation to 15° |
| Group / ungroup | Ctrl+G / Ctrl+Shift+G | A group selects, moves, resizes and rotates as one object |
| Duplicate | Ctrl+D | Copy the selection 16 px away, with internal links and grouping remapped |
| Bring to front / send to back | Ctrl+Home / Ctrl+End | Reorder the selection, preserving its internal order; also available in the context strip |
| Nudge selection | Arrow keys / Shift+arrows | Move by 1 / 10 image pixels, independent of zoom |
| Delete selection | Delete / Backspace | Remove the selection; Undo restores layer order and attachments |
| Smart shape | S | Draw one stroke; release to fit, then pick another shape or Keep original stroke |
| Freehand | P | Keep the stroke exactly as drawn |
| Block highlighter | G by default | Drag a translucent rectangle; Ctrl at press swaps to freehand |
| Freehand highlighter | Shift+G by default | Paint a uniform translucent stroke; Ctrl swaps to block; hold Shift for straight 15° segments |
| Attached arrow | A | Drag an arrow; ends attach to nearby objects. Tip scale is independent of the 0.1 through 128 px line thickness |
| Line | L | Draw a connector without an arrowhead; the same thickness control applies |
| Rectangle | R | Drag opposite corners; Alt centres on the press, Shift constrains a square; fill and corner radius are editable |
| Ellipse | E | Drag a bounding box; Alt centres on the press, Shift constrains a circle |
| Text | T | Click the image to type on it; double-click an existing caption to edit |
| Privacy zone | B | Drag a block; select Black redaction or Pixelated privacy in the context strip |
| Numbered step | N | Click for default size or hold and drag to grow; Alt at press adds an extra ring; the context strip reorders numbers |
| Magnifier callout | M | Drag the source region, then click to place a zoomed inset (1–8×) |
| Spotlight | H | Drag a rounded clear zone with a white outline; dim the outside. Multiple zones remain clear together |
| Crop | C | Drag the region to include in copy/export |
| Crop / resize | Resize icon | Set crop coordinates and final output dimensions, with aspect lock enabled by default |
| Screenshot designer | Designer icon | Preview background presets or custom colours, padding, border, round/square corners and shadow; Apply is one undo step |
| Object inspector | Ctrl+I | Edit position, dimensions, rotation and appearance; align or distribute a selection |
| Command palette | Ctrl+K | Search tools and actions; arrows navigate, Enter runs, Escape closes |
| Saved styles | Projects and actions | Save named annotation defaults and designer frames; apply or delete them locally |
| Smart redaction | Projects and actions | Review OCR-detected sensitive text before applying opaque privacy masks |
| Pin screenshot | Pin icon | Keep the current rendered result above other windows |
| Autosave and recovery | Projects and actions | Enable local recovery, change retention, restore interrupted work or delete copies |
| Scrolling capture | Projects and actions | Select a scrolling-content region and review successive frame seams |
| Export SVG / PDF | Projects and actions | Save sanitised source pixels with vector annotations where safe |
| Recognise text | F8 / OCR icon | Enable the local PaddleOCR text layer; drag to select, Ctrl+A selects all visible text and Ctrl+C copies it |
| Fit image | 0 | Fit the complete screenshot in the window |
| Actual pixels | 1 | Show at 100%; use middle-button drag or the scrollbars to pan |
| Undo | Ctrl+Z | Undo drawing, movement, deletion, style changes, or cropping |
| Redo | Ctrl+Shift+Z / Ctrl+Y | Restore the undone change |
| Fill / round line ends | F / K | Change the selected compatible objects and the defaults for new ones |
| Palette colour | Alt+1 through Alt+0 | Use a configured slot or open the custom colour chooser when that slot is absent |
| Clear annotations | Ctrl+Shift+Delete | Remove every annotation, including privacy masks, in one undoable edit |
| Toolbars / fullscreen / close | Ctrl+T / F11 / Ctrl+Q | Toggle chrome or fullscreen; an unsaved close is confirmed |
| Zoom in / out | Wheel over the canvas; Ctrl+wheel; Ctrl+plus / Ctrl+minus | Zoom from 10% to 400%; wheel zoom keeps the image point under the cursor stationary, including when the image fits |
| Smooth zoom | Ctrl+touchpad or pinch | Zoom around the pointer or gesture centre; native scrolling cannot coast the image after a consumed zoom |
| Pan view | Middle-button drag, touchpad, Alt+arrows | Direct non-inertial panning without editing; the closed hand appears during middle-drag. Escape stops it; Alt+arrows use the configured step |
| Copy image | Ctrl+C outside OCR mode / Copy image icon | Copy the rendered image including crop, framing and output size; the icon always copies the image |
| Save PNG / Save as / copy path | Ctrl+S / Ctrl+Alt+S / Ctrl+Alt+C | Save to configured output or chooser, always choose a new path, or copy the last saved path |
| Cancel / deselect | Escape | Cancel an active gesture; otherwise clear selection and return to Select |
| Open project | Ctrl+O | Reopen a `.sharp` project in a new window |
| Save project | Ctrl+Shift+S | Save an editable `.sharp` project |
| Disable snapping | Alt while dragging | Ignore the magnetic guides for this gesture |

Annotations are separate vector objects above the cached screenshot. In Select mode, click the outline of an unfilled shape, near a stroke or arrow, or inside a filled marker, privacy zone, lens or caption. Unfilled interiors let you select objects underneath. The topmost matching object is selected, with resize and rotation handles. Hovering a handle shows an open hand; dragging shows a closed hand. One Undo reverses a whole drag, resize, rotation or layer-order change, and Escape cancels the preview without creating an edit. Late smart-shape recognition preserves an object's transform.

While dragging, objects snap to the edges and centers of other objects and of the image, and to equal spacing between two neighbours; teal guides show what matched. Hold Alt to drag freely.

Bring to front and Send to back operate on the whole selection, including groups, without changing its internal order. Privacy masks always remain above ordinary annotations; their layer actions only reorder them against other privacy masks.

Attached arrows store the anchor as a fraction of the target's bounds, so they keep following it through moves, resizes and rotations. Deleting a target leaves the arrow where it was last drawn rather than deleting it; Undo restores the link.

Text has its own pixel-size field, independent of stroke width. Click with T to type directly on the image or double-click a caption to edit. The live preview includes bold, italic and paragraph alignment. Width 0 fits the text while preserving manual line breaks; a positive width wraps it. Drag a resize handle to scale uniformly, including within a group. Enter or a click outside the text editor applies the whole edit in one undo step and leaves typing mode; Shift+Enter inserts a new line. Formatting controls stay inside the edit, and Ctrl+Enter also applies from those controls. Invalid input stays open to correct. Escape cancels.

Caption controls also include a searchable installed-font family picker and four effects: no effect, inverted outline, contrast outline and drop shadow. Configured Pango faces supply the initial B/I state; turning a toggle off removes that property. Tap Alt in Text mode to cycle effects; Alt chords do not cycle them. Ctrl+Alt+arrows move the uncommitted caption by `text_move` document pixels, with Shift for 10×. Apply commits position and content together; Escape leaves the original caption untouched.

Magnifier callouts sample the screenshot, not the canvas, so annotations never appear twice. Redacted pixels are blacked out inside the lens as well, whatever layer the redaction sits on.

Colour, opacity and thickness update the selection and set the defaults for new annotations. Privacy masks ignore colour and opacity. S/M/L correspond to 2/4/8 image pixels. Pango handles Unicode text shaping; text and numeric editing shortcuts stay with their focused fields.

The thickness field has decimal steps and dedicated minus/plus buttons. Wheel or arrow-key steps are 0.1 px, Shift uses 0.01 px, Ctrl and Page Up/Down use whole pixels. Line ends can be round or flat. Rounded corners apply only to rectangles, including rotated rectangles; arbitrary polygons keep their geometry. Block and freehand highlighters accumulate alpha once per stroke so self-overlap does not darken the mark.

The inspector shows mixed selection values without replacing untouched properties. Changing line width preserves each object's colour and opacity. Alignment and distribution treat groups as units; each operation is one undo step.

Cropping is nondestructive: the full screenshot stays in the session, and Reset crop or Undo restores it. Copy and export use the same Cairo renderer as the canvas, not a downscaled preview. PNG exports are flattened; they do not save an editable project.

### Context menus

Right-click the canvas for actions relevant to the current object or OCR mode. Menu or Shift+F10 opens the menu for the current selection. Up/Down, Home/End and Enter navigate and activate; Escape dismisses without clearing the selection.

| Context | Available actions |
| --- | --- |
| OCR text | Copy the selection or all visible text; select a whole line or all text; clear selection; highlight, underline, strike through, redact or pixelate selected text; review sensitive text; undo/redo; exit the text layer |
| Shape or group | Duplicate, group/ungroup, change stacking order, inspect properties and delete, with undo/redo |
| Caption, step or privacy mask | Shape actions plus Edit text, number reordering, or opaque/pixelated privacy switching as applicable |
| Empty canvas | Copy/export/pin the image, select all objects, recognise text, open Designer, crop/reset crop and change the view scale |
| Active caption edit | Apply or cancel; right-clicking inside the text field retains GTK's standard text-editing menu |

Clicking inside the existing object or OCR selection preserves the entire selection. Clicking another object or recognised word targets it; blank space clears selection. Unavailable actions are omitted, including layer moves that would not change order. Privacy masks reorder only against other privacy masks. Opening a menu does not finish a caption edit.

### Designer and output size

Open Designer or Crop and resize for a large live preview beside three tabs. Looks has 28 complete frame presets, with actual screenshot thumbnails, across Featured, Studio, Gradients, Bold and Soft. Choose a look for its background, spacing, corners, border and shadow together. Adjust exposes the individual controls and marks a hand-edited frame as Custom. Transparent and unframed looks remain available.

Canvas has Original plus Square (1080 × 1080), Portrait (1080 × 1350), Story (1080 × 1920), Landscape (1920 × 1080), Link post (1200 × 630) and Product (1600 × 1200) formats. Fixed canvases centre and uniformly fit the complete decorated screenshot without cropping or stretching it. Decoration sizes scale with that fit; small screenshots may be enlarged. Changing the look preserves the chosen canvas, output size and crop.

The custom output fields resize the complete canvas. Aspect lock keeps its proportions; switching it off explicitly permits stretching. Reopening preserves an existing unlocked size. Crop coordinates stay in original image pixels. Reset changes the preview; Apply commits the crop and design as one undo step. Cancel leaves the document untouched. The annotation canvas keeps original-image coordinates; copied and exported images include the complete design.

Saved styles store annotation defaults and frame settings in `$XDG_CONFIG_HOME/sharpshot/presets.json` (normally `~/.config/sharpshot/presets.json`). Save a named frame from Looks and reuse it on other screenshots. Applying a saved frame preserves the current canvas, output dimensions and crop. Presets contain no screenshot pixels or fixed canvas dimensions.

### Selectable OCR text and markup

F8 or the OCR icon runs PaddleOCR's PP-OCRv5 mobile detector and recogniser on the original screenshot, on a worker thread. Results are cached for that window. Click a word, click its location again to select its line, drag across characters/lines, or use Ctrl+A. Copy preserves recognised line breaks, infers spacing and indentation from character geometry, and uses tabs for wide gaps.

The contextual marker, underline and strike-through icons create editable annotations over the selected text. The right-click menu also creates opaque or source-independent pixelated privacy zones directly over selected text. Each action is one undo step. OCR text is not saved into a project; reopening and enabling OCR recognises the original image again.

Every selection, copy and markup action checks current privacy masks and crop bounds, including masks added after recognition. Hidden glyphs cannot be copied. OCR can still misread characters or infer the wrong spacing; proofread copied text. Vertical and upside-down text are not supported.

Smart redaction recognises email addresses, phone numbers, IP addresses, IBANs, payment-card numbers and account numbers. Add a literal term for names or other identifiers. Choose categories and individual matches in the review, then apply them as one undoable change. Cancel changes nothing. Matches use the current crop and masks; a document change requires a fresh review. OCR and pattern matching can miss or misread information, so inspect the result before sharing.

PNG saving runs on a worker, writes a private temporary file in the destination directory, syncs it, and installs it without overwriting an existing file. Choose a new name rather than replacing an existing screenshot. The model runs on a separate worker, and late recognition results remain compatible with undo/redo.

## Redaction and editable projects

Privacy zones replace covered pixels in the canvas, clipboard and exported PNG. Select **Black redaction** or **Pixelated privacy**. Pixelated privacy uses an opaque, source-independent grey-blue mosaic; it never averages or samples sensitive pixels. Conventional blur or pixelation can preserve recoverable information. Context outside the zone can still reveal information, so cover the entire sensitive area.

Both modes round coverage outwards to whole pixels and stay opaque when moved, resized or rotated. Magnifier callouts black out source pixels covered by either mode, regardless of layer order.

**A `.sharp` project is not a redacted artifact.** It keeps the original screenshot so the scene stays editable, which means redacted pixels are still inside the file. Share an exported PNG, SVG or PDF; keep the project for yourself. The editor says so next to Save editable project.

```sh
sharpshot --project shot.sharp   # reopen an editable project
```

The format is a self-contained file: an `SHARPPRJ` header, a bounded JSON scene, and the source screenshot as PNG. Loading validates lengths, image size, ids, links, groups and transforms before a document exists, so a corrupt or hostile file is rejected instead of being half-applied. Saving refuses to overwrite and verifies that the scene it wrote parses back bit-identically.

Version 0.2 writes project format 5, including fill, caps, rectangle corners, fonts, text effects, marker rings and highlighters. Formats 1 through 4 still load with their original appearance; earlier executables reject format 5 rather than dropping its styles.

### SVG and PDF

Export SVG and Export PDF use the current crop, frame and output dimensions. SVG dimensions are CSS pixels; PDF uses 96 dpi sizing. Source pixels under privacy masks and outside the crop are removed before embedding. Annotations remain vectors only when their ordering, crop and masks make that safe; intersecting annotations and magnifiers are rasterised. Spotlight scenes are flattened. Hidden caption text is not embedded as searchable text behind a removable mask.

### Pinned references

Pin screenshot freezes the rendered result, including redaction, crop and framing. A pin survives closing its editor. Right-click it or use its hover controls for zoom, opacity and click-through; drag to move it. Plus/minus zoom, 0 resets zoom, Ctrl+wheel changes opacity, and Escape closes the focused pin.

Click-through sends pointer input to the window below. Use Hide or show pins or `sharpshot --toggle-pins` to regain control: showing pins clears click-through. Wayland pins use the native overlay layer, including above fullscreen windows; unsupported compositors report an error instead of opening an ordinary window. X11 requires a window manager that honours always-on-top requests.

### Autosave and crash recovery

Autosave starts only after explicit consent. Recovery copies contain the original screenshot, including redacted areas, and are not encrypted. Settings are in `$XDG_CONFIG_HOME/sharpshot/recovery.json`; copies are in `$XDG_DATA_HOME/sharpshot/recovery/` (normally `~/.local/share/sharpshot/recovery/`). Directories are owner-only and snapshot files use mode 0600.

Autosave keeps changed documents, including unfinished captions, on a worker thread. The default interval is 30 seconds and retention is 3 days. Normal window closure removes its copies; interrupted sessions can be restored into a new editor. The recovery manager changes the interval and retention, disables saving, or permanently deletes copies. Delete all also cancels outstanding writes; if autosave remains enabled, the next edit may create a new copy.

### Guided scrolling capture

Use Scrolling capture or `--scroll` with `--region` or `--geometry`. Keep the source non-fullscreen and select only scrolling content, excluding fixed headers, toolbars and sidebars. Scroll down by less than one page and Capture next frame. The controller hides before each native capture.

The preview shows the stitched image and newest seam. Repeated or changing content can require a manually adjusted Scrolled distance and Accept seam before continuing. Undo removes the newest frame; Finish opens the full-resolution result in the editor. Capture is vertical and user-guided; it does not inject scrolling into other applications.

## Privacy and performance

- Built-in capture, inference, fitting and rendering run locally without vendor telemetry. User-selected copy programs and stylesheets have their own behavior.
- Direct shared-memory capture avoids a capture subprocess and an intermediate PNG encode/decode cycle. The read-only capture mapping is converted to pixels once; only interactive coordinate selection launches `slurp`.
- The screenshot's Cairo surface, committed canvas commands and a viewport-sized raster are cached separately from interaction overlays. The raster uses the current display density, not a full-document bitmap enlarged with zoom. Transform and caption previews use the full painter to preserve transparency and mask ordering. Interaction rendering has no continuous frame timer.
- Pango logical text bounds use a thread-local cache capped at 4,096 entries and 4 MiB of text. Mark lookups validate cached ID-to-position indexes against the live scene; selection geometry reuses these lookups and text measurements.
- OCR visibility is reused only while the recognised page, crop, image dimensions and individual privacy-mask hulls match. Mask geometry is checked before reuse; invalid geometry blocks text rather than exposing it. Gaps between separate masks remain selectable.
- Scrolling seam previews are retained until either of the last two frames or their shift changes. Undo and recapture invalidate by frame identity, not just frame count.
- Pixel conversion shares opaque/transparent fast paths. Rendering and framing stay in Cairo until the final RGBA conversion; resampling quality is unchanged.
- The designer bounds its cached scene and final preview independently to a 1200 × 800 pixel budget. Export dimensions do not cause full-resolution preview allocations, and small outputs keep their native preview resolution. Preview budgets do not constrain full-resolution export.
- The model's inference plan loads once and is reused for every stroke. Results wake GTK through an asynchronous channel; there is no result-polling timer.
- Copy, Pin and PNG export render immutable request-time snapshots on workers, with one image operation per editor at a time. PNG encoding and file I/O also run off GTK. Delayed native Copy results cannot replace a newer clipboard action; configured helpers control their own clipboard side effects. No GPU is required.

These are implementation choices, not a benchmark claim against Satty or grim.

### Version 0.1 optimization measurements

The v0.1 optimization run used GTK 4.18.6's CPU/Cairo renderer, a 3840 × 2160 source and the same synthetic scenes. Values are medians: 30 hover callbacks, 21 annotation-drag frames and five Copy requests. These measurements do not profile the added v0.2 controls.

| Operation | Before | After |
| --- | ---: | ---: |
| Hover hit-testing, 1,000 text objects | 10.12 ms | 0.36 ms |
| GTK frame during annotation drag, 1,000 text objects | 43.57 ms | 1.03 ms |
| Copy UI handler, framed scene with 200 text objects | 460.92 ms | 5.76 ms |

Copy figures measure GTK-thread blocking, not time until the image is ready. Document rebuilds, full-scene previews and high-quality framing still cost more than transient overlays. These are not GPU FPS or input-to-display measurements.


### Application appearance

SharpShot pins GTK's built-in dark base, its own palette, control styling and icon theme above external GTK themes and user CSS, including a `GTK_THEME` override. This applies to application-owned windows, popovers and dialogs. System font sizing remains available for accessibility. A portal file chooser runs in another process and keeps its system appearance.

An explicit `--stylesheet FILE` is a higher-priority opt-in override. Invalid, non-UTF8, oversized or non-regular stylesheets fail instead of partially changing the appearance. Disabling window decorations retains SharpShot's tools.

## Limits

- Without `--region` or `--geometry`, capture selects one output. Choose a monitor explicitly if the compositor's first output is not the one you want.
- Requires the wlroots screencopy protocol. GNOME and KDE capture portals are not implemented.
- Smart recognition is single-stroke and confidence-gated. It does not turn arbitrary handwriting or multi-stroke drawings into shapes. Freehand is always available.
- Source-derived blur/pixelation, multi-image composition and multi-page documents are not implemented.
- Projects and recovery copies retain the original screenshot, including pixels hidden by privacy masks. Share rendered exports instead.
- Recognition offers at most three alternative shapes, and only for strokes the model scores above its floor. Keep original stroke is always offered.
- Native clipboard ownership belongs to the running app unless a manager retains it. Keep the editor open until pasting, or configure an independent helper; automatic exit checks persistence.
- Raster export is 8-bit PNG; SVG and PDF embed 8-bit sanitised images. HDR color management and wide-gamut preservation are not implemented.

## Verification

```sh
cargo test --locked
```

Regression coverage includes pixel formats, stride, rotation, HiDPI geometry, annotation/crop history, recognition completing during undo, PNG rendering and overwrite protection, shape preprocessing/fitting, and the vendor's stroke fixtures. Compositor and editor smoke testing must run in a real or headless Wayland session; unit tests alone do not exercise screencopy or GTK input.

Version 0.2 passed 302 release-mode regressions, including configured font-face B/I removal and restoration, moved-draft atomic undo, highlighter alpha, filled/rounded hit geometry, marker rings, format-5 persistence and nonblocking FIFO rejection. Native X11 checks exercised stdin limits, CLI/config precedence, literal typing under custom shortcuts, Enter/outside-click caption application, named saves, overwrite refusal, auto-copy, external helper failures/deadlines and clipboard persistence after exit.

Native Wayland checks used real virtual-pointer/keyboard protocol input on Sway at 1× and 2×. Wheel and Ctrl+smooth zoom held the cursor anchor; immediate finger lifts showed no image drift between 0.1 and 1.7 seconds after disabling GTK's separate wheel-inertia flags. Plain smooth pan moved by the exact input sum, without coasting. Middle-drag, Alt+arrows, context-menu blocking, initial scale, toolbar focus changes and regular/symlink/FIFO file inputs were exercised. Six external GTK theme names produced identical application and context-menu pixels; explicit CSS overrides and invalid CSS were also checked.

Capture-to-stdout produced a complete PNG matching native Wayland frames and explicit region crops pixel-for-pixel at 1× and 2×. Save-to-stdout emitted only the expected annotated PNG, with status on stderr; invalid capture targets emitted no image bytes. The 50-second release GIF records real editor interactions through OCR redaction and Designer, and its clipboard image matched the exported PNG pixel-for-pixel.

Physical pinch input was not exercised: the headless compositor has no virtual touch/pinch injector and the container has no input device. On Sway, all-monitor fullscreen requests covered only the focused output; monitor spanning remains compositor-dependent. The GUI recording uses synthetic data and real app interactions.

The v0.1 baseline passed 256 regression tests, including real PP-OCRv5 inference, multiline copying, dynamic privacy/crop filtering, mixed-selection editing, rich-text layout, preset persistence, recovery cancellation races, scrolling overlap detection, fitted-canvas geometry, preview memory bounds, cursor-anchored zoom and vector-export privacy. Version-1/2/3 migration and version-4 round trips were covered. Shape inference runs against 51 upstream stroke fixtures.

Version 0.1 GUI checks covered inspector geometry and style with undo/redo, live bold/italic captions, saved frame presets, command-palette actions, and reviewed OCR redaction with cancellation and privacy-filtered copying. A version-3 rich-text project reopened with pixel-identical PNG output. Independent librsvg and Poppler rendering matched export size and layout; decoded embedded images contained burned-in masks, while the public PDF caption remained selectable and private text was absent.

Context-menu GUI checks exercised blank-canvas and shape menus, duplicate/group/ungroup/delete with undo, preserved multi-object selections, OCR word/line/multiline copying, selected-text highlight and direct opaque/pixelated masking with privacy-filtered copying and undo. Menu cancellation preserved an edited caption pixel-for-pixel. Keyboard opening/navigation/activation/dismissal and closing an editor with its menu open were also checked. Native Wayland at 2× confirmed popup placement, unchanged copied PNG pixels, front/back reordering, OCR word copying, opaque/pixelated mask switching and keyboard dismissal.

Optimization checks compared five decoded before/after PNG exports pixel-for-pixel, including 4K scenes with 200 rectangles, 20/200/1,000 text objects and a fitted 1600 × 1000 frame. A 4K background-only document at 400% zoom increased peak RSS by 7.3 MiB over Fit in the isolated CPU-rendered rig, without a zoom-sized scene bitmap.

Background-job GUI checks kept selection responsive during framed rendering, rejected overlapping image requests, retained request-time pixels after later edits, preserved newer clipboard contents and left an existing PNG byte-for-byte unchanged. The pin helper displayed the rendered reference. Reopening the hidden inspector refreshed geometry and line width after selection/style changes.

Live OCR checks reused the recognised page after reviewed redaction and undo, preserving multiline clipboard text while excluding masked digits. Adding markup redrew the committed scene before the text layer was toggled; exported pixels covered both detected lines, and one undo restored the original image exactly. Native scrolling checks recaptured different last-frame pixels at the same frame count and shift, adjusted the seam from 200 to 201 and back, and produced a pixel-identical 600 × 800 image.

Designer GUI checks exercised all 28 look cards with distinct screenshot previews, all seven canvas formats with an undistorted calibration square, Cancel, one-step undo/redo, crop, saved frames, unlocked-size reopening, Reset and an 820 × 600 window. A fitted version-4 project reopened with pixel-identical output. Invalid output dimensions disabled Apply and recovered through Natural size. Looks, Canvas and Adjust rendered pixel-identically under HighContrast and hostile user CSS. Independent SVG/PDF viewers matched a fitted 1080 × 1350 PNG; embedded image assets retained burned-in masks, public captions stayed selectable and hidden captions were absent from extraction.

Preview performance checks used 20 output-field changes at 4096 × 4096, and 20 frame-spacing changes on a 4K screenshot in a square social canvas, in an isolated CPU-rendered GTK session. Before/after exports at 4096 × 4096, 1080 × 1080 and 320 × 180 were pixel-identical, including privacy masks. Small-output previews were also checked visually.

Outside-click text checks covered formatting controls, invalid drafts, one-step undo/redo and clicking Copy to apply and copy in one action. Native Wayland checks also exercised click-away text and the Ocean/Square design at exactly 1080 × 1080.

Cursor-anchored wheel zoom checks covered off-centre targets, rapid repeated input, 10%/400% limits and reversals, Fit/actual pixels, and drawing/copying after zoom. Native Wayland checks covered zoom in/out with 1× and 2× output scaling; X11 checks also covered Ctrl+wheel.

Zoom prepares the new pan bounds and cursor offset before layout and painting, rather than correcting the viewport after a frame is drawn. A 120 fps X11 capture checked 961 frames through zoom-limit reversals: the anchored target remained visible in every sampled frame, with at most 2 pixels of raster-centre drift.

Middle-button panning checks covered the closed-hand cursor, release and Escape, drawing/undo after dragging, unchanged PNG exports, OCR mode, and a live caption with PRIMARY clipboard data present. Panning preserved a tall screenshot's 9% Fit scale and kept the caption editable without pasting or committing it. Native Wayland checks covered dragging and unchanged exports at 1× and 2× scaling.

Reference pins were checked with native X11 always-on-top and empty input regions, and Wayland overlay placement above a fullscreen window. Opacity, zoom, click-through, CLI hide/show recovery and helper cleanup were exercised. Native Wayland scrolling capture stitched three overlapping frames into a pixel-identical 600 × 800 image, including undo and recapture. Recovery checks enabled consent, saved an unfinished caption, killed the editor, restored it in a new window, then disabled saving and deleted copies across two live editors.

The inspector, command palette and recovery manager rendered pixel-identically with normal settings and with HighContrast plus a hostile user stylesheet changing fonts, colours, backgrounds, borders and shadows.

Editor checks on a private X11 display exercised outline click-through to an underlying numbered circle, layer order and whole-change undo through clipboard pixels, a held 70-pixel-radius numbered circle, and open-hand/closed-hand cursors over resize and rotation controls.

Release-editor checks exercised real OCR recognition, character-range selection and multiline clipboard text, highlight/underline/strike-through with individual undo, and cached recognition filtered against a newly added privacy mask. Pixelated privacy and spotlight were checked through copied pixels. Fresh arrows and plain lines accepted independent size controls; caption corner resizing kept glyph proportions and editing applied an explicit font size. Designer Cancel preserved every pixel; gradient, padding, visible border, corners, shadow, crop and resize produced a **480 × 290** PNG, with one-step undo/redo. A version-2 project reopened with pixel-identical output, and fresh OCR still excluded its saved privacy zone.

The release's header, OCR/context and style bars, and the entire designer rendered pixel-identically under normal settings and under `GTK_THEME=HighContrast` plus a user stylesheet replacing fonts, weight, colours, backgrounds, borders and shadows. Zoom-label updates run after drawing so they cannot invalidate the toolbar allocation during GTK snapshotting.

Headless Wayland capture checks matched grim pixel-for-pixel for regions on all eight output transforms, 2× scaling, cross-output transparent gaps, and partially offscreen rectangles. Fractional 1.5× capture matched the corresponding native-frame crop; mixed 1×/2× output composition preserved native 2× pixels with sub-one-level mean channel rounding differences in the upscaled portion. Real `slurp` drag selection produced an exact 304 × 208 source crop, opened the GTK editor, and copied the same pixels with Ctrl+C; Escape exited without a file or editor. The release captured a live **3840 × 2160 Hyprland** output after normal screencopy approval, and a live **240 × 160 Hyprland** region after installation.

GUI checks in a headless sway session drove the real editor with a virtual pointer and keyboard: redaction, three numbered steps, a magnifier callout, an attached arrow, an on-canvas caption, selection handles, duplicate/undo, snapped movement, and a sketched circle fitted with `ellipse`/`cloud`/`hexagon`/`rectangle` corrections plus Keep original stroke. A moved target carried its attached arrow with it; deleting the target left the arrow in place and Undo restored both. An exported **1000 × 650** PNG contained no redacted source colour, and reopening the saved `.sharp` project reproduced the scene and copied pixels identical to that export.

The save-time guard caught one real bug during these runs: without serde_json's `float_roundtrip`, palette colours reloaded one ULP off, so the feature is enabled and pinned by a bit-exact test.

## Model license and attribution

**Powered by Desert Ant Labs.** The model and the reference-derived recognition code are under the [Desert Ant Labs Source-Available License 1.0](https://license.desertant.com/1.0), not an OSI open-source license. The published terms allow free use below **100,000 monthly active devices per platform, per model**; larger deployments require a commercial license. Attribution is required, standalone model redistribution is restricted, and training a competing model from its outputs is prohibited. Read the full terms before redistribution.

Pinned revisions, checksums, the model card, and notices are in [`assets/shapes/`](assets/shapes/). The parity fixture's provenance is documented there too.

PaddleOCR PP-OCRv5 model weights and dictionary originate from [PaddleOCR](https://github.com/PaddlePaddle/PaddleOCR), with ONNX conversions from [RapidOCR](https://github.com/RapidAI/RapidOCR). They use Apache-2.0 licensing. The Rust inference binding is MIT/Apache-2.0 dual-licensed; ONNX Runtime uses MIT licensing and includes separately licensed components. Pinned assets, checksums, source URLs and the complete notices are in [`assets/ocr/`](assets/ocr/).
