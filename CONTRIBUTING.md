# Contributing

Report bugs and propose changes at [Osyna/Screnerrr](https://github.com/Osyna/Screnerrr/issues). Open an issue before a substantial feature or architectural change.

## Development

SharpShot is the application and Rust package name; Screnerrr is the repository name. Follow the Linux and native-library requirements in the [README](README.md).

```sh
git clone https://github.com/Osyna/Screnerrr.git
cd Screnerrr
cargo build --locked
cargo fmt --check
cargo test --locked
```

## Changes and pull requests

Branch from `main`, keep changes focused and follow the surrounding code. Describe the user-visible behavior and the checks you ran. Include an actual editor screenshot for UI changes. Exercise capture, clipboard and compositor behavior in an isolated desktop; unit tests do not cover those paths.

Add regression coverage for consumer-visible failures. Update usage documentation when behavior changes. Avoid adding dependencies for code already available in the project or standard library.

## Safe reports

Use a synthetic screenshot or redact private information before attaching images, logs or `.sharp` projects. Editable projects can contain original pixels underneath privacy masks. Follow [SECURITY.md](SECURITY.md) for vulnerabilities and the [Code of Conduct](CODE_OF_CONDUCT.md) when participating.

## Licensing

Original contributions use MIT. Do not relicense or remove notices from third-party or reference-derived code. Read [LICENSING.md](LICENSING.md) before modifying those components or redistributing models.
