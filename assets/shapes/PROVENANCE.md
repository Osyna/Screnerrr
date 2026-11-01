# Shapes model provenance

Bundled, unmodified, from Desert Ant Labs "Shapes" v1.0.0 ("Weights v1.0.0"):

- Model repo: https://huggingface.co/desert-ant-labs/shapes
- Pinned revision: `f00c6932ac476d0ee80f72f803395a4d6b27ba0c`
- Product page: https://desertant.com/models/shapes/
- Files (verify with `sha256sum -c SHA256SUMS`, run in this directory):
  - `shapes.tflite` 974896 bytes, SHA256 `8274742f91341e54ae64df4c561a19e5fb4cf591d6b305290d129068f56ef6b9`
  - `shapes_meta.json` SHA256 `d570c85233d505f6e19b38b512b81ff735182728240d56e6e87782f5efe1c6a4`
  - `MODEL_CARD.md` (upstream `README.md`), `THIRD_PARTY_NOTICES.md` (training data: Quick, Draw!, CC BY 4.0, not redistributed)
  - `LICENSE.md`: Desert Ant Labs Source-Available License 1.0, https://license.desertant.com/1.0
- Tensors: input `points` [1,128,2]; outputs `probs` [1,13], `head` [1,128].
  Note: the model card's table (`[1,256,3]` features) is stale; the `.tflite` and `ShapesModel.swift`
  (`Catalog.swift`) both say [1,128,2].

Reference implementation ported to Rust in `src/shapes.rs`:

- https://github.com/Desert-Ant-Labs/desert-ant-core at commit `1a5efed67f44731b5eb5433aa9be3eebae05e269`
  - `Sources/Shapes/Preprocess.swift`, `Model.swift`, `ShapeMeta.swift`, `Fitter.swift`, `Snapping.swift`,
    `Shape.swift` (outline/arrowheads/heart template), `Geometry.swift`
- Parity fixture `tests/fixtures/shapes-parity.json` (SHA256 `4d0b6351f7d56332e9a119b5544df302b22a2feb3e910a8535409d509729e49f`),
  from `Tests/Fixtures/shapes-parity.json` at the same commit: 51 strokes with the Python model
  input, probabilities, arrowhead probabilities and fits.

Differences from upstream:

- Rotation mode is fixed to upstream's default `.snapped`; `.upright`/`.free` and the `kinds` filter are not ported.
- `scribble` (the erase gesture) is never returned; such strokes stay as ink.
- A polygon fit with fewer vertices than its kind needs (degenerate hull) is rejected, where upstream returns it.
- Inference runs the unmodified `.tflite` in Google's LiteRT CPU runtime (XNNPACK) via the Apache-2.0 `litert` 0.2.1 crate
  (https://github.com/offbit-ai/LiteRT), which loads the SHA-pinned `libLiteRt.so` from the litert-lm v0.10.2 prebuilt.
  tract-tflite (0.22/0.23) was tried and cannot run this file (no GELU, ignores `builtin_code` below 127 where the model
  stores DEQUANTIZE, cannot read the external weight buffers). Checked on all 51 fixture strokes: probabilities within
  0.018 and arrowhead outputs within 0.025 of the Python reference, about 0.8 ms per inference.
- None of the Desert Ant SDKs (and none of their usage telemetry) is used.

Licence obligations (summary, the full terms in `LICENSE.md` govern): free below 100,000 monthly active devices per platform;
credit "Powered by Desert Ant Labs" with a link to https://desertant.com where users can find it; do not use the model or its
outputs to train a competing model; do not redistribute the model standalone.
