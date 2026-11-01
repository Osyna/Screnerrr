---
license: other
license_name: desert-ant-labs-source-available-1.0
license_link: https://license.desertant.com/1.0
tags:
- strokes
- shapes
- sketches
- pen-input
- on-device
- core-ml
- litert
- tflite
- pencilkit
pipeline_tag: image-classification
---

<!-- card-header:start (generated from manifest.json, edit below this block) -->
# Shapes

Rough sketch. Perfect shape.

On-device single-stroke shape recognition.

- **SDKs, install and examples:** https://desertant.com/docs/shapes/
- **Website:** https://desertant.com/models/shapes/

<!-- card-header:end -->

Turn a rough circle drawing into a clean shape, with fitted vector geometry ready to
snap to. Built
for PencilKit-style "smart shapes": draw, pause, and the rough stroke becomes a
crisp shape. The model is tiny (**200KB** Core ML, **1.3MB** LiteRT) and
runs in a few milliseconds on device.

> ✏️ ➜ ▭  ·  ✏️ ➜ △  ·  ✏️ ➜ ◯  ·  ✏️ ➜ ★

## Try it

- **Live demo:** [desert-ant-labs/shapes-demo](https://huggingface.co/spaces/desert-ant-labs/shapes-demo): draw one stroke, get a fitted shape, fully in your browser.

<!-- card-install:start (generated from manifest.json, edit below this block) -->
| | |
| --- | --- |
| **Platforms** | iOS, macOS, tvOS, visionOS, Android, Linux, Windows, Browser, Node |
| **Weights** | [v1.0.0](https://huggingface.co/desert-ant-labs/shapes) |

## Install

**Swift** ([requirements](https://github.com/Desert-Ant-Labs/desert-ant-core/blob/main/README.md#swift))

```swift
.package(url: "https://github.com/Desert-Ant-Labs/desert-ant-core.git", from: "4.2.1")
```

Then add the `Shapes` product to your target.

**Kotlin** ([requirements](https://github.com/Desert-Ant-Labs/desert-ant-core/blob/main/README.md#android))

```kotlin
implementation("ai.desertant:shapes:4.2.1")
```

**JavaScript** ([requirements](https://github.com/Desert-Ant-Labs/desert-ant-core/blob/main/README.md#javascript-and-typescript))

```bash
npm i @desert-ant-labs/shapes @litertjs/core   # browser
npm i @desert-ant-labs/shapes                  # Node, prebuilt native core
```
<!-- card-install:end -->

## Files

| File | Format | Size | Contents |
|---|---|---:|---|
| `shapes.tflite` | LiteRT / TFLite (fp32) | 1.3MB | Fixed `[1,256,3]` features + `[1,256]` mask window; runs on Android, Linux, Node, and the web (bundled by default in the Kotlin SDK; downloaded on demand by the JavaScript SDK) |
| `shapes.mlmodelc` | Compiled Core ML | 200KB | 4-bit-palettized classifier, ready to load on Apple platforms (used by the Swift SDK) |
| `shapes_meta.json` | JSON | tiny | classes, preprocessing constants, model dims, and snap gates |
| `shapes.safetensors` | safetensors | 200KB | Packed weights for SDK versions before the LiteRT migration (tag `v0.1.0`) |

## Inputs and outputs

- **Input:** an ordered list of stroke points in canvas coordinates. Single stroke.
- **Output:** a shape class plus fitted geometry, or nothing if the stroke is rejected.

## Classes

`line`, `rectangle`, `triangle`, `ellipse`, `star`, plus `none` (the reject class:
scribbles, partial shapes, and other non-shape strokes). Squares and circles are
covered by `rectangle` and `ellipse` (snapped when near-regular).

## Limitations

- Single stroke only; multi-stroke shapes aren't recognized.
- Tuned for deliberate shapes; very rough or ambiguous strokes are rejected by design.

<!-- card-footer:start (generated from manifest.json, edit above this block) -->
## License

[Desert Ant Labs Source-Available License](https://license.desertant.com/1.0). Free for most
apps, and a commercial license is required at scale. Full terms are at the link.
Licensing: <licensing@desertant.com>.

See [`THIRD_PARTY_NOTICES.md`](THIRD_PARTY_NOTICES.md).

## Citation

```bibtex
@software{shapes_2026,
  title  = {Shapes: On-device single-stroke shape recognition},
  author = {Desert Ant Labs},
  year   = {2026},
  url    = {https://huggingface.co/desert-ant-labs/shapes},
}
```

---

© 2026 Desert Ant Labs · <https://desertant.com>
<!-- card-footer:end -->
