# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Fixed

- Made release recipes portable across GNU/Linux and macOS.
- Passed the repository explicitly when publishing a GitHub release from CI.

## [0.1.0] - 2026-10-21

### Added

- Scoring for saved Matter articles, full-content Atom and RSS entries, UTF-8 text and Markdown files, and standard input.
- Pangram bulk scoring for Matter and feed inputs while preserving source order and isolating per-item failures.
- Configurable source-text word budgets and a dry-run mode that discovers and partitions inputs without submitting content to Pangram.
- Matter status, tag, and item-limit filters with paginated discovery and bounded concurrent content fetching.
- Explicit request timeouts, Matter rate-limit handling, terminal progress output, and `NO_COLOR` support.
- Reproducible pre-built Linux and macOS binaries with SHA-256 checksums and build provenance attestations.

[unreleased]: https://github.com/SRv6d/slopfilter/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/SRv6d/slopfilter/releases/tag/v0.1.0
