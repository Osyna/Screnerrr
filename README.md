# SharpShot

A native Wayland screenshot and annotation app written in Rust. The repository is `Osyna/Screnerrr`; the application and executable are named `sharpshot`.

![SharpShot editor with arrows, shapes and an editable caption](assets/screenshots/editor.png)

The demo images use synthetic content.

## Features

- Edit arrows, lines, shapes, numbered circles and captions as separate objects. Group them, change their stacking order and undo edits.
- Select recognised text with local PaddleOCR. Copy words, lines or multiline selections, then add highlights, underlines or strike-through marks.
- Cover sensitive areas with opaque redaction or a source-independent pixelated privacy zone. OCR menus can mask selected text directly.
- Crop and resize screenshots, then add backgrounds, spacing, rounded corners, borders and shadows in Designer.
- Zoom around the cursor and pan with the middle mouse button. Right-click for actions relevant to the selected object or current mode.
- Copy or export the rendered image, and save an editable `.sharp` project to continue later.

![OCR selection and its context menu](assets/screenshots/ocr-context.png)

## Requirements

- Linux with GTK 4.8+, Cairo 1.16+ and a C/C++ toolchain.
- Rust 1.92 or later, Cargo and pkg-config.
- A compositor supporting the wlroots screencopy protocol for desktop capture, such as Hyprland. GNOME/KDE capture portals are not implemented.
- `slurp` for interactive region selection. Existing-image editing does not need desktop capture.

The native OCR runtime needs a modern glibc/libstdc++ environment. The CI environment uses Debian 13; Debian 12 is not a supported build environment.

## Install from source

Install native dependencies on Arch Linux:

```sh
sudo pacman -S --needed rust base-devel pkgconf gtk4 openssl slurp
```

On Debian 13, install Rust separately using [rustup](https://rustup.rs), then install native dependencies:

```sh
sudo apt install build-essential pkg-config libgtk-4-dev libssl-dev ca-certificates slurp
```

Build and check the executable:

```sh
git clone https://github.com/Osyna/Screnerrr.git
cd Screnerrr
cargo build --release --locked
./target/release/sharpshot --version
./target/release/sharpshot --help
```

The first build downloads native inference runtimes. Shapes and OCR model files are included in the source tree and bundled into the executable. Keep the bundled model files when cloning or packaging the application.

For installation outside the build tree and native-library redistribution, follow the [build instructions in the manual](MANUAL.md#build). Wayland reference pins additionally need GTK4 layer-shell. Binary distributions must include the relevant native libraries and [third-party notices](LICENSING.md).

## Usage

Capture a monitor and open the editor:

```sh
./target/release/sharpshot
```

Select a desktop region:

```sh
./target/release/sharpshot --region
```

Edit an existing image or reopen a project:

```sh
./target/release/sharpshot --input image.png
./target/release/sharpshot --project screenshot.sharp
```

Save a capture without opening the editor:

```sh
./target/release/sharpshot --capture capture.png
```

Use **F8** for the OCR text layer. Right-click for object or text actions; **Menu** or **Shift+F10** opens the same contextual actions from the keyboard. Scroll to zoom and drag with the middle mouse button to pan. **Ctrl+Z** undoes an edit.

See the [manual](MANUAL.md) for annotation shortcuts, Designer, privacy masks, editable projects, pinned references, scrolling capture and compositor setup.

## Privacy

A flattened export and an editable project are different deliverables. An editable `.sharp` project can retain original screenshot pixels underneath redactions. Do not share it as a sanitised image. Inspect flattened exports before sharing; OCR can miss or misread text, and surrounding context can reveal information outside a mask.

Use synthetic input for bug reports. Follow [SECURITY.md](SECURITY.md) for vulnerabilities.

## Contributing

Read [CONTRIBUTING.md](CONTRIBUTING.md) and the [Code of Conduct](CODE_OF_CONDUCT.md). For development checks:

```sh
cargo fmt --check
cargo test --locked
```

Changes to capture, clipboard or the editor also need a real or isolated desktop smoke check.

## License

Original SharpShot code is copyright (c) 2026 Osyna under the [MIT License](LICENSE), with the exceptions in [LICENSING.md](LICENSING.md).

**Powered by [Desert Ant Labs](https://desertant.com).** The bundled Shapes model and reference-derived recognition code retain the [Desert Ant Labs Source-Available License 1.0](assets/shapes/LICENSE.md), including its usage and redistribution restrictions. They are not MIT-licensed or OSI open source.

PaddleOCR, RapidOCR, ONNX Runtime and native libraries retain their own licenses. Preserve the accompanying notices when redistributing source or binaries. See [model provenance](assets/shapes/PROVENANCE.md), [OCR provenance](assets/ocr/PROVENANCE.md) and [native runtime notices](assets/runtime/NOTICE.md).
