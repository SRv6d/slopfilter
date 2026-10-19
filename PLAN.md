# Slopfilter implementation plan

## Current state

The project is a Rust CLI with a Nix-managed toolchain and devcontainer.

Implemented:

- `matter` reads `MATTER_API_TOKEN`, paginates every selected article, and
  supports repeated status and tag filters plus an optional total item limit.
- Matter fetches up to four Markdown bodies concurrently at Matter's burst
  ceiling, pauses all requests on `429 Retry-After`, and preserves list order.
- `feed <URL>` fetches an Atom or RSS feed once and scores entries in feed
  order.
- Feed entries preserve source order, resolve relative links, and convert
  embedded HTML to scoreable text. Only full inline content (Atom `content`,
  RSS `content:encoded`) is scored; summary- or description-only entries are
  item-level unavailable errors rather than invalidating the whole feed.
- Feed and Matter use module-local `Error` types for source-wide failures and
  `ItemError` types for failures that leave sibling items processable.
- Feed and Matter validate item URLs at adaptation time and retain malformed
  URLs as absent optional metadata rather than failing scoreable items.
- Matter and feed requests time out after 30 seconds, including the response
  body; Pangram requests time out after 10 seconds.
- Matter, feed, file, and standard-input documents use Pangram's realtime task
  API.
- Matter and feed adapters both yield `(Item, Result<Document, ItemError>)`.
  `ItemScoring` in `main.rs` applies one word budget, reports `Unavailable` and
  `Failed` items identically, and prints the run total.
- Pangram responses model terminal results, pending tasks, detailed
  predictions, fractions, segments, and windows.
- Dry runs execute source discovery and the cumulative word budget without
  contacting Pangram or requiring `PANGRAM_API_KEY`.
- Output leads with the article title, file path, or standard-input label.
  Matter and feed entry IDs remain diagnostic metadata. Terminal titles are
  bold; piped output and `NO_COLOR` output remain plain.
- Nix provides Rust, Clippy, Rustfmt, Rust Analyzer, and `rust-src`. The
  devcontainer installs the exported Rust toolchain into the Nix profile for
  the VS Code extension host.

### Matter request model

- Listing, Markdown fetches, and Pangram scoring run in one task. Up to four
  Markdown requests run concurrently, capped at Matter's 5/s burst ceiling, and
  results are yielded in list order.
- Requests go as fast as that allows until Matter answers `429`; every request
  then waits out the server's `Retry-After` before continuing. The client does
  not pre-space requests to the 20/min Markdown quota.
- While an item is scored through the realtime task API the stream is not
  polled, so pending fetches pause. Bulk scoring (milestone 1) removes that
  wait.
- Stopping at the word budget drops the stream and cancels at most four
  in-flight fetches.

Not implemented:

- No Pangram bulk scoring; Matter Markdown is fetched for each listed item
  before word-budget admission.
- No SQLite state, migrations, or Pangram result persistence.
- No incremental Matter sync, body hash, or persisted resume support.
- No report, filtering, or Matter write-back.
- Matter requests do not pause proactively when `X-RateLimit-Remaining`
  reaches `0`; requests in flight at the end of a window receive `429`. Pause
  until `X-RateLimit-Reset` once a real token confirms which quota Markdown
  responses report.

## Next milestones

### 1. Sequential Pangram bulk scoring

Score Matter and feed selections through Pangram's bulk API instead of creating
one realtime task per article.

Processing phases:

1. Discover every candidate and display it as pending before fetching Matter
   Markdown or contacting Pangram.
2. Prepare the selected documents in source order. Matter uses listing
   estimates for initial admission and then re-checks actual Markdown; feeds
   already contain the text used for admission.
3. Partition all prepared documents at Pangram's request limit, then submit and
   poll one bulk job at a time. Update each displayed item from its individual
   result before continuing to the next job.

Requirements:

- Select Matter items against `--max-words` using the listing's estimated
  `word_count`, so Markdown requests are spent only on items expected to fit.
- Re-check `--max-words` against fetched Markdown and skip items whose actual
  text no longer fits.
- Select feed entries using their actual word count.
- Keep `--max-words` as a source-text word budget. Compute Pangram billable
  units separately—one per started 100-word block per item, minimum one—and
  split jobs at the 1,000-unit request limit.
- Submit with `POST /bulk` using source item IDs as bulk item IDs, poll
  `GET /bulk/{bulk_id}`, and report per-item results in source order.
- Complete document preparation before contacting Pangram. The initial bulk
  implementation does not overlap Matter requests with Pangram jobs and does
  not keep multiple Pangram jobs in flight.
- Keep `file` and `stdin` on the realtime task API.
- Replace `Scorer::score`'s nested
  `Result<Result<Option<ScoreOutcome>, BudgetExhausted>>` with a flat outcome
  type as admission moves to bulk selection.

Acceptance: a Matter run displays every candidate before preparation, requests
Markdown only for selected items, completes preparation before contacting
Pangram, splits oversized selections into sequential bulk jobs, and updates
each item from its own result. A feed run follows the same discovery and
sequential bulk-job phases. Dry runs report selection and job partitioning
without contacting Pangram.

### 2. Pipelined bulk processing

Optimize the sequential workflow only if measurements show that waiting for
Matter preparation before Pangram submission materially increases run time.

Possible work:

- Submit prepared jobs while later Matter documents are still being fetched,
  including at Matter quota-window pauses.
- Bound concurrently submitted and polled Pangram jobs.
- Preserve the sequential workflow's budget decisions, per-item states, source
  order, failure isolation, and cancellation behavior when results complete
  out of order.

Acceptance: the pipelined workflow produces the same selected items and
per-item outcomes as the sequential workflow while reducing measured wall-clock
time on a rate-limited Matter run.


### 3. SQLite state

Implement `store/sqlite.rs` behind `store.rs` using the now-known Pangram
request and task-result contracts.

Requirements:

- Create and migrate a local database.
- Persist source identity, canonical URL, title, content hash, and fetch
  timestamp.
- Persist selected Pangram model/version, bulk job ID, per-item task ID,
  lifecycle state, and terminal result.
- Persist work state so interrupted scans resume rather than repeat completed
  work.
- Do not retain full document text unless a concrete feature requires it.

Acceptance: scoring the same unchanged source document twice does not create
duplicate work, and a submitted Pangram bulk job resumes polling after
interruption within Pangram's 48-hour result retention.

### 4. Persistent scoring workflow

Extend the explicit scoring command into the persisted end-to-end workflow:

```text
Source discovery → content fetch → content hash → SQLite deduplication → Pangram → SQLite result
```

Requirements:

- Skip only when stored content hash and selected model already have a terminal result.
- Use Matter's incremental sync cursor after the initial backfill.
- Preserve Matter's burst-until-`429` request pacing and list-time budget
  selection.

Acceptance: scoring an unchanged, already-completed article does no Pangram work.

### 5. Reports

Add a read-only report command backed solely by SQLite.

Initial behavior:

- Sort by AI-generated or AI-assisted fraction.
- Show title, URL, Matter ID, model/version, score, and confidence.
- Filter by score threshold and processing outcome.

Matter tagging or archive/write-back remains out of scope until the local report is useful.

## Deliberate constraints

- Keep source adapters concrete. Shared orchestration stays in the CLI; do not
  add a generic source-provider trait.
- Keep secrets out of source control and out of local state. Use environment variables through Clap.
- Treat AI-detection output as a ranking signal, not proof of authorship.
