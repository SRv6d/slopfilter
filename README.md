<h1 align="center"><code>slopfilter</code></h1>
<div align="center">
  <a href="https://github.com/SRv6d/slopfilter/actions/workflows/test.yml">
    <img src="https://github.com/SRv6d/slopfilter/actions/workflows/test.yml/badge.svg?branch=main" alt="Test status" />
  </a>
  <a href="https://crates.io/crates/slopfilter">
    <img src="https://img.shields.io/crates/v/slopfilter.svg?logo=rust" alt="Cargo version" />
  </a>
</div>
<br />

`slopfilter` scores saved [Matter] articles, full-content Atom and RSS entries, files, and standard input for likely AI authorship using [Pangram].

## Installation

### Using pre-built binaries

Static Linux binaries and macOS binaries for x86-64 and ARM64 are attached to every [release]. Each release also includes SHA-256 checksums and build provenance attestations.

Release archives can be verified with the GitHub CLI:

```sh
gh attestation verify slopfilter-v0.1.0-aarch64-apple-darwin.tar.gz \
  --repo SRv6d/slopfilter
```

### Using Cargo

Install `slopfilter` from [crates.io] using Cargo:

```sh
cargo install --locked slopfilter
```

## Usage

Set a Pangram API key, then choose an input source:

```sh
export PANGRAM_API_KEY="..."

slopfilter matter --matter-api-token "$MATTER_API_TOKEN" --status queue
slopfilter feed "https://example.com/feed.xml"
slopfilter file article.md
cat article.md | slopfilter stdin
```

`matter` and `feed` use Pangram's bulk API. `file` and `stdin` use its realtime API. Input text is sent to Pangram unless `--dry-run` is set.

Each subcommand accepts a total source-text budget through `--max-words` (default: `2000`). Once the next item would exceed the remaining budget, that item is skipped and later items are still considered.

Matter credentials can be supplied through `MATTER_API_TOKEN` or `--matter-api-token`. The Pangram key can be supplied through `PANGRAM_API_KEY` or `--pangram-api-key`; it is not required for a dry run.

```sh
slopfilter feed "https://example.com/feed.xml" --dry-run --max-words 10000
```

Feeds must provide full inline content. Summary-only entries and entries whose content is only available through a link are reported as unavailable rather than sent to Pangram.

Run `slopfilter --help` or `slopfilter <COMMAND> --help` for the complete command-line reference.

## Contributing

Contributions are welcome. See the [contribution guidelines](.github/CONTRIBUTING.md) for development and release commands.

## License

This project is licensed under the [MIT License](LICENSE).

[crates.io]: https://crates.io/crates/slopfilter
[Matter]: https://www.getmatter.com
[Pangram]: https://www.pangram.com
[release]: https://github.com/SRv6d/slopfilter/releases
