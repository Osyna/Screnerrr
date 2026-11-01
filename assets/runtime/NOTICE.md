# Native runtime notices

SharpShot runs the Shapes model (`assets/shapes/`) in Google's LiteRT CPU runtime.

| Component | What | Licence | Text |
|---|---|---|---|
| `litert` / `litert-sys` 0.2.1 | Rust bindings, https://github.com/offbit-ai/LiteRT | Apache-2.0 | `LITERT_RS_LICENSE.txt` |
| `libLiteRt.so` | LiteRT C runtime (TFLite kernels + XNNPACK), prebuilt for `x86_64-unknown-linux-gnu`, downloaded and SHA-verified by `litert-sys`' build script from the LiteRT-LM v0.10.2 release; SHA256 `e9844d634dbb69dbeb0bc51a71f7035bb7ba523e876384ff58192955b1da63e4` | Apache-2.0 (Google LLC) | `LITERT_LICENSE.txt`, `LITERT_LM_LICENSE.txt` |
| `libgtk4-layer-shell.so.0` 1.0.4 | Native Wayland overlay windows for pinned references, https://github.com/wmww/gtk4-layer-shell | MIT and Expat | `GTK4_LAYER_SHELL_COPYRIGHT.txt` |

`libLiteRt.so` is built from https://github.com/google-ai-edge/LiteRT and statically contains upstream
third-party code under its own permissive licences; the authoritative notices are in the upstream repositories
(https://github.com/google-ai-edge/LiteRT, https://github.com/google-ai-edge/LiteRT-LM). Upstream did not
publish a top-level NOTICE file at the time of writing.

Of the LiteRT libraries, only `libLiteRt.so` is shipped. The GPU/WebGPU accelerator libraries in the same download cache are not used or distributed.
