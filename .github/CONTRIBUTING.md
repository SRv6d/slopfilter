# How To Contribute

Thank you for considering contributing to `slopfilter`!
This document intends to make contribution more accessible while aligning expectations.
Please don't hesitate to open issues and PRs if anything is unclear.

By contributing to `slopfilter`, you agree that your code will be licensed under the terms of the MIT License without any additional terms or conditions.

## Getting Started

To gain an overview of `slopfilter`, please read the [README](../README.md).

## General Guidelines

- Contributions of all sizes are welcome, including single-line grammar and typo fixes.
- For new features, documentation and tests are a requirement.
- Changes must pass CI. PRs with failing CI will be treated as drafts unless you explicitly ask for help.
- Simplicity is a core objective of `slopfilter`. Please open an issue before working on a new feature that materially changes behavior, reliability, privacy, or performance.
- Never include Matter or Pangram credentials, fetched private content, or API responses containing private content in issues, fixtures, logs, or commits.

## Development Environment

### Devcontainer

For users of IDEs with support for devcontainers, its use is recommended.

### Other

Ensure a recent version of [Nix](https://nixos.org/download/) with flakes is available. Enter the development environment with `nix develop`, or use [direnv](https://direnv.net/) and run `direnv allow`.

## Coding Standards

`slopfilter` uses [`rustfmt`](https://github.com/rust-lang/rustfmt) for uniform formatting and [`clippy`](https://github.com/rust-lang/rust-clippy) for linting and enforcement of best practices. The [`just`] `lint` recipe runs both.

```sh
just lint
```

In addition to formatting and linting, a high code coverage should be maintained. All tests need to pass before a PR can be merged.

```sh
just test
```

Changes to package metadata should also verify the crate that would be uploaded to crates.io.

```sh
just package
```

Changes to release packaging must preserve reproducibility for the Nix release artifact set.

```sh
just verify-reproducibility
```

## Releasing a new version

To release a new version of `slopfilter`, perform the following steps.

- Ensure the unreleased section of the [CHANGELOG](../CHANGELOG.md) contains all relevant changes.

- Pick a new version according to the [SemVer 2.0 spec](https://semver.org/spec/v2.0.0.html) and set it in your environment.

  ```sh
  export NEW_VERSION=0.2.0
  ```

- Check out a new branch.

  ```sh
  git switch -c bump-version-$NEW_VERSION
  ```

- Use the just recipe to bump the version. This creates the necessary commits and a pull request using the GitHub CLI.

  ```sh
  just bump-version $NEW_VERSION
  ```

- Once the branch is merged, create a draft release and start the release workflow.

  ```sh
  just release-latest-version $NEW_VERSION
  ```

- The workflow builds and attests the binary archives, publishes the crate using a short-lived OIDC token, then publishes the draft as the latest GitHub release.

[`just`]: https://github.com/casey/just
