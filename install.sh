#!/bin/sh
# Install kb: downloads the latest release for this machine.
#
#   curl -fsSL https://raw.githubusercontent.com/oodss/KittyBrowser/main/install.sh | sh
#
# KB_INSTALL_DIR picks where kb goes (default ~/.local/bin). The GitHub CLI
# verifies the release's keyless build attestation before anything is unpacked.
# A custom KB_REPO also needs the same value in KB_TRUSTED_REPO, as an explicit
# decision to trust that repository's release workflow.
#
# Everything is in main, called on the last line, so a download cut short
# runs nothing.
set -eu

say() { printf 'kb install: %s\n' "$*"; }
fail() {
  say "$*" >&2
  exit 1
}

valid_repo() {
  case "$1" in
    "" | /* | */ | */*/* | *[!A-Za-z0-9._/-]*) return 1 ;;
    */*) return 0 ;;
    *) return 1 ;;
  esac
}

choose_trusted_repo() {
  official=oodss/KittyBrowser
  valid_repo "$repo" || fail "KB_REPO must be one GitHub owner/repository name"
  if [ "$repo" = "$official" ]; then
    trusted_repo="${KB_TRUSTED_REPO:-$official}"
    [ "$trusted_repo" = "$official" ] ||
      fail "KB_TRUSTED_REPO must be $official for the official installer"
  else
    trusted_repo="${KB_TRUSTED_REPO:-}"
    [ "$trusted_repo" = "$repo" ] ||
      fail "a custom KB_REPO is not trusted by default; set KB_TRUSTED_REPO=$repo to trust its release workflow"
  fi
}

require_verifier() {
  command -v gh >/dev/null ||
    fail "GitHub CLI is required to verify releases; install it from https://cli.github.com and authenticate with gh auth login"
  help="$(gh attestation verify --help 2>/dev/null)" ||
    fail "GitHub CLI does not support artifact attestations; update it and run this again"
  for option in --repo --signer-workflow --signer-digest --source-ref --source-digest --cert-oidc-issuer --deny-self-hosted-runners; do
    printf '%s\n' "$help" | grep -q -- "$option" ||
      fail "GitHub CLI is too old for the required attestation policy; update it and run this again"
  done
}

find_release() {
  tag="$(gh api "repos/$repo/releases/latest" --jq .tag_name 2>/dev/null)" ||
    fail "couldn't identify the latest release; run gh auth login and try again"
  case "$tag" in
    v[0-9]*) ;;
    *) fail "the latest release tag isn't a version tag" ;;
  esac
  case "$tag" in
    *[!A-Za-z0-9._+-]*) fail "the latest release tag contains unsupported characters" ;;
  esac

  source_sha="$(gh api "repos/$repo/commits/$tag" --jq .sha 2>/dev/null)" ||
    fail "couldn't resolve release tag $tag to its source commit"
  case "$source_sha" in
    "" | *[!0-9a-f]*) fail "release tag $tag didn't resolve to a Git commit" ;;
  esac
  [ "${#source_sha}" -eq 40 ] || fail "release tag $tag didn't resolve to a Git commit"
}

hash_file() {
  if command -v shasum >/dev/null; then
    shasum -a 256 "$1" | cut -d ' ' -f 1
  elif command -v sha256sum >/dev/null; then
    sha256sum "$1" | cut -d ' ' -f 1
  else
    fail "needs shasum or sha256sum to check the download"
  fi
}

verify_release() {
  archive_path=$1
  gh attestation verify "$archive_path" \
    --repo "$trusted_repo" \
    --signer-workflow "$trusted_repo/.github/workflows/release.yml" \
    --signer-digest "$source_sha" \
    --source-ref "refs/tags/$tag" \
    --source-digest "$source_sha" \
    --cert-oidc-issuer https://token.actions.githubusercontent.com \
    --deny-self-hosted-runners >/dev/null 2>&1 ||
    fail "release provenance is missing, invalid, or from the wrong repository, workflow, source, or artifact; nothing was installed"
}

# Download the release for $target and install it in $dir. Checksums diagnose
# damaged downloads; the independently rooted attestation authenticates them.
download() {
  command -v curl >/dev/null || fail "curl isn't installed; install it and run this again"
  require_verifier
  find_release

  archive="kb-$target.tar.gz"
  url="https://github.com/$repo/releases/download/$tag/$archive"
  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' EXIT
  curl -fsSL "$url" -o "$tmp/$archive" ||
    fail "couldn't download $archive for $tag; unsigned older releases and missing targets are not installed"
  curl -fsSL "$url.sha256" -o "$tmp/$archive.sha256" ||
    fail "couldn't download the checksum; try again"
  want="$(cut -d ' ' -f 1 "$tmp/$archive.sha256")"
  case "$want" in
    "" | *[!0-9a-f]*) fail "the release checksum is malformed" ;;
  esac
  [ "${#want}" -eq 64 ] || fail "the release checksum is malformed"
  got="$(hash_file "$tmp/$archive")"
  [ "$want" = "$got" ] || fail "the download's checksum doesn't match; try again"
  verify_release "$tmp/$archive"
  tar xzf "$tmp/$archive" -C "$tmp"
  mkdir -p "$dir"
  install -m 755 "$tmp/kb-$target/kb" "$dir/kb"
  say "installed $dir/kb"
}

main() {
  repo="${KB_REPO:-oodss/KittyBrowser}"
  dir="${KB_INSTALL_DIR:-$HOME/.local/bin}"
  choose_trusted_repo

  case "$(uname -s)" in
    Darwin) os=apple-darwin ;;
    Linux) os=unknown-linux-gnu ;;
    *) fail "kb runs on macOS and Linux (on Windows, use WSL 2)" ;;
  esac
  case "$(uname -m)" in
    arm64 | aarch64) arch=aarch64 ;;
    x86_64 | amd64) arch=x86_64 ;;
    *) fail "no build for $(uname -m)" ;;
  esac
  target="$arch-$os"

  # The Linux releases need glibc; Alpine and others use musl instead.
  if [ "$os" = unknown-linux-gnu ] && ls /lib/ld-musl-* >/dev/null 2>&1; then
    fail "the prebuilt kb needs glibc, and this Linux uses musl; install from source explicitly with the Cargo command in README.md"
  else
    download
  fi

  case ":$PATH:" in
    *":$dir:"*)
      found="$(command -v kb || true)"
      if [ -n "$found" ] && [ "$found" != "$dir/kb" ]; then
        say "another kb at $found comes first on your PATH; remove it, or run $dir/kb"
      fi
      ;;
    *) say "$dir isn't on your PATH; add this line to ~/.zshrc or ~/.bashrc: export PATH=\"$dir:\$PATH\"" ;;
  esac
  say "now run: kb doctor"
}

main "$@"
