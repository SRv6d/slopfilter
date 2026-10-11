# Slopfilter implementation plan

## Current state

The project is a Rust CLI with a Nix-managed toolchain and devcontainer.

Implemented:

- `matter` reads `MATTER_API_TOKEN`, paginates every selected article, and
  supports repeated status and tag filters plus an optional total item limit.
- Matter hydrates up to four Markdown bodies concurrently, rate-limits content
  requests, honors `429 Retry-After`, and preserves list order.
- `source::feed` fetches and parses Atom and RSS feeds without CLI coupling.
- Feed entries preserve source order, resolve relative links, prefer full
  content with summary fallback, and convert embedded HTML to scoreable text.
- Feed entries without inline text remain item-level errors rather than
  invalidating the whole feed.
- Feed and Matter use module-local `Error` types for source-wide failures and
  `ItemError` types for failures that leave sibling items processable.
- Feed and Matter validate item URLs at adaptation time and retain malformed
  URLs as absent optional metadata rather than failing scoreable items.
- Matter, file, and standard-input documents use Pangram's realtime task API.
- Pangram responses model terminal results, pending tasks, detailed
  predictions, fractions, segments, and windows.
- Dry runs execute source discovery and the cumulative word budget without
  contacting Pangram or requiring `PANGRAM_API_KEY`.
- Output leads with the article title, file path, or standard-input label.
  Matter IDs remain diagnostic metadata. Terminal titles are bold; piped output
  and `NO_COLOR` output remain plain.
- Nix provides Rust, Clippy, Rustfmt, Rust Analyzer, and `rust-src`. The
  devcontainer installs the exported Rust toolchain into the Nix profile for
  the VS Code extension host.

Not implemented:

- No Atom/RSS CLI command or shared source orchestration.
- No SQLite state, migrations, or Pangram result persistence.
- No incremental Matter sync, body hash, or persisted resume support.
- No report, filtering, or Matter write-back.

## Next milestones

### 1. Atom/RSS command

Wire `source::feed` into an explicit CLI command without introducing a generic
source trait.

Requirements:

- Accept a feed URL and fetch it once.
- Process valid entries in feed order using the existing cumulative word
  budget and Pangram scoring path.
- Report malformed or content-less entries without discarding valid siblings.
- Keep source acquisition separate from scoring orchestration.

Acceptance: an Atom or RSS URL can be dry-run and scored through the same
document pipeline as Matter.

### 2. SQLite state

Implement `store/sqlite.rs` behind `store.rs` using the now-known Pangram
request and task-result contracts.

Requirements:

- Create and migrate a local database.
- Persist source identity, canonical URL, title, content hash, and fetch
  timestamp.
- Persist selected Pangram model/version, submission task ID, lifecycle state,
  and terminal result.
- Persist work state so interrupted scans resume rather than repeat completed
  work.
- Do not retain full document text unless a concrete feature requires it.

Acceptance: scoring the same unchanged source document twice does not create
duplicate work, and a submitted Pangram task resumes polling after interruption.

### 3. Persistent scoring workflow

Extend the explicit scoring command into the persisted end-to-end workflow:

```text
Source discovery → content fetch → content hash → SQLite deduplication → Pangram → SQLite result
```

Requirements:

- Skip only when stored content hash and selected model already have a terminal result.
- Use Matter's incremental sync cursor after the initial backfill.
- Preserve the current rate-limit and streaming behavior.

Acceptance: scoring an unchanged, already-completed article does no Pangram work.

### 4. Reports

Add a read-only report command backed solely by SQLite.

Initial behavior:

- Sort by AI-generated or AI-assisted fraction.
- Show title, URL, Matter ID, model/version, score, and confidence.
- Filter by score threshold and processing outcome.

Matter tagging or archive/write-back remains out of scope until the local report is useful.

## Deliberate constraints

- Keep source adapters concrete. Introduce shared orchestration only when the
  Atom/RSS command demonstrates the required common contract; do not add a
  generic source-provider trait preemptively.
- Keep secrets out of source control and out of local state. Use environment variables through Clap.
- Treat AI-detection output as a ranking signal, not proof of authorship.
