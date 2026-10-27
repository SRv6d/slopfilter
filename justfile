export CI := env("CI", "false")
CHANGELOG_FILE := "CHANGELOG.md"
REPO_URL := "https://github.com/SRv6d/slopfilter"
LCOV_FILE := "target/coverage/lcov.info"

default: check-lockfile lint test

# Check if the lockfile is in sync with Cargo.toml
check-lockfile:
    cargo metadata --locked --format-version=1 > /dev/null

# Lint code and check formatting
lint: lint-justfile
    cargo clippy --all-targets --all-features
    cargo fmt --all --check

lint-justfile:
    just --check --fmt --unstable

# Run all unit, integration, and example tests
test:
    cargo test --all-features --locked
    cargo test --all-features --locked --examples

# Build and verify the package that will be uploaded to crates.io
package:
    cargo package --allow-dirty --locked

# Verify that release archives build reproducibly
verify-reproducibility:
    nix build .#release-artifacts --no-update-lock-file
    nix build .#release-artifacts --rebuild --no-link --no-update-lock-file

# Generate a coverage report
coverage: _install_llvm_cov
    mkdir -p {{ parent_directory(LCOV_FILE) }}
    cargo llvm-cov --all-features --locked {{ if CI == "true" { "--lcov --output-path " + LCOV_FILE } else { "--summary-only" } }}

# Bump the crate version and open a pull request
bump-version $VERSION: _check_clean_working _check_gh_cli (_validate_semver VERSION) && (_changelog_add_version VERSION) (_bump_version_pr VERSION)
    #!/usr/bin/env bash
    set -euxo pipefail

    tmp_file=$(mktemp)
    trap 'rm -f "$tmp_file"' EXIT
    awk -v version="$VERSION" '
      $0 == "[package]" { in_package = 1 }
      in_package && /^version = / {
        print "version = \"" version "\""
        in_package = 0
        next
      }
      { print }
    ' Cargo.toml > "$tmp_file"
    cat "$tmp_file" > Cargo.toml
    cargo update -w --offline

    git add Cargo.toml Cargo.lock
    git commit -m "Bump version to v{{ VERSION }}"

# Create a draft GitHub release and start the trusted publishing workflow
release-latest-version version: _check_clean_working _check_gh_cli (_validate_release_version version) (_create_draft_release version)
    gh workflow run release.yml --ref main -f tag="v{{ version }}"

# Create a draft GitHub release containing the matching changelog entry
_create_draft_release version:
    #!/usr/bin/env bash
    set -euo pipefail

    current_release="v{{ version }}"
    previous_release=$(gh release list --json tagName,isLatest --jq '.[] | select(.isLatest) | .tagName')
    changes=$(awk -v version="{{ version }}" '
      $0 ~ "^## \\[" version "\\]" { capture = 1; next }
      capture && ($0 ~ "^## \\[" || $0 ~ "^\\[[^]]+\\]:") { exit }
      capture { print }
    ' "{{ CHANGELOG_FILE }}")

    if [[ -z "$changes" ]]; then
      echo "No changelog entry found for {{ version }}"
      exit 1
    fi

    if [[ -n "$previous_release" ]]; then
      comparison="{{ REPO_URL }}/compare/${previous_release}...${current_release}"
    else
      comparison="{{ REPO_URL }}/tree/${current_release}"
    fi

    notes=$(cat <<EOF
    ## What's Changed

    ${changes}

    **Full Changelog**: ${comparison}
    EOF
    )

    gh release create "$current_release" --draft --title "$current_release" --notes-file - <<< "$notes"

# Publish the crate from a matching version tag
publish: _validate_version_tag
    cargo publish --locked

# Check that Git has a clean working directory
_check_clean_working:
    test -z "$(git status --porcelain)" || (echo "The working directory is not clean"; exit 1)

# Check that the GitHub CLI is installed and authenticated
_check_gh_cli:
    #!/usr/bin/env bash
    set -euo pipefail

    command -v gh > /dev/null || (echo "GitHub CLI is not installed"; exit 1)
    gh auth status > /dev/null || (echo "GitHub CLI is not authenticated. Run: gh auth login"; exit 1)

# Validate that the crate version matches the release version
_validate_release_version version:
    #!/usr/bin/env bash
    set -euo pipefail

    project_version=$(sed -n '/^\[package\]$/,/^\[/ s/^version = "\([^"]*\)"$/\1/p' Cargo.toml)
    if [[ "$project_version" != "{{ version }}" ]]; then
      echo "Project version $project_version does not match release version {{ version }}"
      exit 1
    fi

# Validate that the crate version matches the Git tag
_validate_version_tag:
    #!/usr/bin/env bash
    set -euo pipefail

    project_version=$(sed -n '/^\[package\]$/,/^\[/ s/^version = "\([^"]*\)"$/\1/p' Cargo.toml)
    git_tag=$(git describe --exact-match --tags)
    if [[ "$project_version" != "${git_tag#v}" ]]; then
      echo "Project version $project_version does not match Git tag $git_tag"
      exit 1
    fi

# Validate a version against SemVer
_validate_semver version:
    #!/usr/bin/env bash
    set -euo pipefail

    if [[ ! "{{ version }}" =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-((0|[1-9][0-9]*|[0-9]*[a-zA-Z-][0-9a-zA-Z-]*)(\.(0|[1-9][0-9]*|[0-9]*[a-zA-Z-][0-9a-zA-Z-]*))*))?(\+([0-9a-zA-Z-]+(\.[0-9a-zA-Z-]+)*))?$ ]]; then
      echo "Invalid SemVer: {{ version }}"
      exit 1
    fi

# Install cargo-llvm-cov when not running in CI
_install_llvm_cov:
    #!/usr/bin/env bash
    set -euo pipefail

    if [[ "$CI" == false ]]; then
      cargo install cargo-llvm-cov --locked
    fi

# Add a new release section and link to the changelog
_changelog_add_version version filename=CHANGELOG_FILE:
    #!/usr/bin/env bash
    set -euxo pipefail

    previous_version=$(sed -n '/^\[unreleased\]:/ { n; s/^\[\([^]]*\)\].*/\1/p; }' "{{ filename }}")
    tmp_file=$(mktemp)
    trap 'rm -f "$tmp_file"' EXIT
    awk \
      -v version="{{ version }}" \
      -v date="{{ datetime("%Y-%m-%d") }}" \
      -v repo="{{ REPO_URL }}" \
      -v previous="$previous_version" '
      $0 == "## [Unreleased]" {
        print
        print ""
        print "## [" version "] - " date
        next
      }
      /^\[unreleased\]:/ {
        print "[unreleased]: " repo "/compare/v" version "...HEAD"
        print "[" version "]: " repo "/compare/v" previous "...v" version
        next
      }
      { print }
    ' "{{ filename }}" > "$tmp_file"
    cat "$tmp_file" > "{{ filename }}"

    git add "{{ filename }}"
    git commit -m "Update {{ filename }}"

_bump_version_pr version:
    gh pr create --title "Bump version to v{{ version }}" --body ""
