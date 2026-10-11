<h1 align="center"><code>slopfilter</code></h1>

`slopfilter` runs saved [Matter] articles through [Pangram], rating them by their likelyhood of being LLM authored. Somewhat ironically, it was conceived in a single weekend and is fully LLM written. It is not pretty code by any means (and my own standards) but that does not make it any less useful in it's current form.

## Usage

Set `MATTER_API_TOKEN`, then inspect queued articles without contacting Pangram:

```console
$ slopfilter matter --limit 20 --dry-run
$ slopfilter matter --status inbox --limit 20 --dry-run
$ slopfilter matter --status inbox --status queue --limit 20 --dry-run
$ slopfilter matter --tag tag_n5j2x --limit 20 --dry-run
```

`--status` accepts `inbox`, `queue`, and `archive`; repeat the option to select
multiple statuses. It defaults to `queue`.
`--tag` accepts Matter tag IDs; repeat the option to match any selected tag.
`--limit` caps the total items across pages; omit it to scan every match.

Dry runs follow the same cumulative word budget without contacting Pangram and
do not require `PANGRAM_API_KEY`. They also work for files and standard input:

```console
$ slopfilter file article.md --dry-run
$ cat article.md | slopfilter stdin --dry-run
```

Without `--dry-run`, scoring is billable and requires `PANGRAM_API_KEY`.
Matter streams matching articles, hydrates up to four Markdown bodies
concurrently while respecting its burst limit and `Retry-After`, and preserves
the listed order for scoring. Each eligible article is submitted through
Pangram's realtime task API.

```console
$ slopfilter matter --limit 20 --max-words 10000
```

[Matter]: https://www.getmatter.com
[Pangram]: https://www.pangram.com
