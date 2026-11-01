# OCR models: provenance, licences, checksums

SharpShot's text layer runs **PP-OCRv5 mobile** from PaddleOCR (Baidu / PaddlePaddle authors), unmodified, on
CPU through ONNX Runtime. Everything is bundled in the executable (`include_bytes!` in `src/ocr.rs`); nothing is
downloaded or uploaded at run time.

| File | What | Source | SHA256 |
|---|---|---|---|
| `ppocrv5_det_mobile.onnx` | DB text detector (`ch_PP-OCRv5_det_mobile`), 4.8 MB, dynamic input `x` = `[N,3,H,W]` | RapidOCR model repository, ONNX export of PaddleOCR's PP-OCRv5 mobile detector: `https://www.modelscope.cn/models/RapidAI/RapidOCR/resolve/v3.10.0/onnx/PP-OCRv5/det/ch_PP-OCRv5_det_mobile.onnx` | `4d97c44a20d30a81aad087d6a396b08f786c4635742afc391f6621f5c6ae78ae` (matches `RapidOCR/python/rapidocr/default_models.yaml`) |
| `ppocrv5_rec_mobile.onnx` | CTC recogniser (`ch_PP-OCRv5_rec_mobile`), 16.6 MB, input `[N,3,48,W]`, output `[N,T,18385]`; reads simplified and traditional Chinese, English, Japanese and digits/symbols | `https://www.modelscope.cn/models/RapidAI/RapidOCR/resolve/v3.10.0/onnx/PP-OCRv5/rec/ch_PP-OCRv5_rec_mobile.onnx` | `5825fc7ebf84ae7a412be049820b4d86d77620f204a041697b0494669b1742c5` (matches `default_models.yaml`) |
| `ppocrv5_dict.txt` | Recogniser dictionary, 18383 characters, one per line (CTC classes are blank + these + a trailing space = 18385) | `https://www.modelscope.cn/models/RapidAI/RapidOCR/resolve/v3.10.0/paddle/PP-OCRv5/rec/ch_PP-OCRv5_rec_mobile/ppocrv5_dict.txt` | `d1979e9f794c464c0d2e0b70a7fe14dd978e9dc644c0e71f14158cdf8342af1b` |
| `sample.png` | Test fixture rendered by the author with Noto Sans (SIL OFL 1.1); no third-party content | generated | see `SHA256SUMS` |

`SHA256SUMS` lists every file in this directory (`sha256sum -c SHA256SUMS`).

## Licences

* PaddleOCR models and dictionary: Apache License 2.0, Copyright PaddlePaddle Authors (`LICENSE-PaddleOCR.txt`).
* RapidOCR (the ONNX conversions and the reference pipeline `src/ocr.rs` follows): Apache License 2.0
  (`LICENSE-RapidOCR.txt`).
* ONNX Runtime (statically linked into the executable): MIT, Copyright Microsoft Corporation
  (`LICENSE-ONNXRuntime.txt`); its bundled third-party notices are in `ThirdPartyNotices-ONNXRuntime.txt`
  (taken from upstream `main` at the time of writing; the prebuilt library comes from pyke.io's `ort-sys`
  `2.0.0-rc.14` download, which `ort-sys` SHA-verifies at build time).
* `ort` crate (Rust bindings): MIT OR Apache-2.0 (`LICENSE-ort-MIT.txt`, `LICENSE-ort-APACHE.txt`).

Distributing a SharpShot build that contains these models and the ONNX Runtime requires shipping these notices
alongside it.

## Processing contract (what `src/ocr.rs` implements)

* Detector: BGR planes, `(x/255 - mean) / std` with mean `[0.485, 0.456, 0.406]`, std `[0.229, 0.224, 0.225]`,
  sides rounded to multiples of 32, longest side at most 2560 px. Probability map: threshold 0.3, 8-connected
  components, box score >= 0.6, minimum-area rectangle, unclip ratio 1.5.
* Recogniser: each box is warped to a 48 px tall strip, BGR planes, `(x - 127.5) / 127.5`; greedy CTC decoding;
  lines scoring below 0.5 are dropped. Per-character geometry comes from the decoder step that fired
  (`step / T` of the strip width), not from equal splitting.
* No text-angle classifier: vertical or upside-down text is not recognised.
