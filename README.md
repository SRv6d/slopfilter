<h1 align="center"><code>slopfilter</code></h1>

`slopfilter` runs saved [Matter] articles and Atom/RSS feed entries through [Pangram], rating them by their likelyhood of being LLM authored. Somewhat ironically, it was conceived in a single weekend and is fully LLM written. It is not pretty code by any means (and my own standards) but that does not make it any less useful in it's current form.

## Usage

```console
$ slopfilter --help
Inspect and score text for likely AI authorship

Usage: slopfilter <COMMAND>

Commands:
  matter  Score Matter articles
  feed    Score entries of an Atom or RSS feed
  file    Score a UTF-8 text or Markdown file
  stdin   Score UTF-8 text piped on standard input
  help    Print this message or the help of the given subcommand(s)

Options:
  -h, --help  Print help
```

Each subcommand documents its own options:

```console
$ slopfilter matter --help
Score Matter articles

Usage: slopfilter matter [OPTIONS] --matter-api-token <TOKEN>

Options:
      --status <STATUS>           Filter items by status [default: queue] [possible values: inbox, queue, archive]
      --tag <TAG>                 Filter items by their tag
      --limit <LIMIT>             The maximum number of items to score
      --matter-api-token <TOKEN>  The Matter API token [env: MATTER_API_TOKEN]
      --max-words <MAX_WORDS>     The maximum total number of words authorized for this invocation [default: 2000]
      --dry-run                   Inspect inputs without submitting to Pangram
      --pangram-api-key <KEY>     The Pangram API key [env: PANGRAM_API_KEY]
  -h, --help                      Print help
```
## Processing model

`matter` and `feed` use Pangram's bulk API. Each run has three sequential
phases:

1. Discover every source item and display it as `Pending`.
2. Select and prepare documents in source order under `--max-words`.
3. Partition prepared documents and submit one Pangram bulk job at a time.

For Matter, the list response's estimated `word_count` controls initial
admission. Markdown is requested only for admitted items, with up to four
Matter requests in flight. The downloaded Markdown is counted again; an item
whose actual content no longer fits is skipped without preventing later
admitted items from being considered. Feed entries use the word count of their
inline full-text content directly.

`--max-words` limits source words, not Pangram billing. A bulk item consumes at
least one unit and one additional unit for every started 100-word block. Jobs
are split at 1,000 units. Results and item-level failures are applied in source
order even if Pangram returns them in another order.

`--dry-run` performs discovery, selection, Matter Markdown fetching, and bulk
partitioning without requiring `PANGRAM_API_KEY` or contacting Pangram.
`file` and `stdin` continue to use Pangram's realtime task API.


[Matter]: https://www.getmatter.com
[Pangram]: https://www.pangram.com
