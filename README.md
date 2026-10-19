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

[Matter]: https://www.getmatter.com
[Pangram]: https://www.pangram.com
