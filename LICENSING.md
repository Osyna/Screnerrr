# Licensing

Original SharpShot code is copyright (c) 2026 Osyna and licensed under the [MIT License](LICENSE), except for the third-party and reference-derived materials below. The application is not distributed solely under MIT.

## Shapes model and reference-derived code

The bundled Shapes model, its metadata, the reference implementation ported in `src/shapes.rs`, and the parity fixture in `tests/fixtures/shapes-parity.json` retain the [Desert Ant Labs Source-Available License 1.0](assets/shapes/LICENSE.md). They are not relicensed under MIT. See [provenance](assets/shapes/PROVENANCE.md) for pinned upstream revisions and the port's scope, and [third-party notices](assets/shapes/THIRD_PARTY_NOTICES.md).

Powered by [Desert Ant Labs](https://desertant.com). The source-available terms include attribution, usage limits and redistribution restrictions. Read the full license before redistributing or using the model at scale.

## OCR models and runtime

PaddleOCR models and dictionary and the RapidOCR conversions and reference pipeline retain their Apache-2.0 terms. The Rust OCR implementation follows that reference pipeline; MIT does not replace upstream notices or permissions. ONNX Runtime, its bundled components and the Rust bindings retain their respective licenses. Full texts, provenance and checksums are in [assets/ocr](assets/ocr/PROVENANCE.md), including [ONNX Runtime third-party notices](assets/ocr/ThirdPartyNotices-ONNXRuntime.txt).

## Native libraries

LiteRT and its Rust bindings retain their Apache-2.0 terms. GTK4 layer-shell retains its MIT/Expat terms. See [native runtime notices](assets/runtime/NOTICE.md) and the accompanying license texts.

Keep these licenses and notices when distributing source or binaries containing the corresponding components. Other Cargo dependencies retain their own licenses.
