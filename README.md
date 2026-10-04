<h1 align="center"><code>slopfilter</code></h1>

`slopfilter` runs saved [Matter] articles through [Pangram], rating them by their likelyhood of being LLM authored. Somewhat ironically, it was conceived in a single weekend and is fully LLM written. It is not pretty code by any means (and my own standards) but that does not make it any less useful in it's current form.

## Usage

Set `MATTER_API_TOKEN`, then inspect queued articles and their total word count without contacting Pangram:

```console
$ slopfilter list matter --limit 20
```

Scoring is an explicit, billable operation that accepts one Matter item ID:

```console
$ slopfilter score matter itm_Su4
```

`score matter` requires `PANGRAM_API_KEY` and refuses articles over 2,000 words by
default. Review the word count from `list matter`, then explicitly authorize a
larger submission when intended:

```console
$ slopfilter score matter itm_Su4 --max-words 5000
```

[Matter]: https://www.getmatter.com
[Pangram]: https://www.pangram.com
