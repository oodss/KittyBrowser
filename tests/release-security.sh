#!/bin/sh
# Regression checks for the release checkout and install trust policy.
set -eu

root="$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)"
test_tmp="$(mktemp -d)"
trap 'rm -rf "$test_tmp"' EXIT

fail_test() {
  printf 'release security test: %s\n' "$*" >&2
  exit 1
}

contains() {
  grep -F "$2" "$1" >/dev/null || fail_test "$1 is missing: $2"
}

workflow="$root/.github/workflows/release.yml"
contains "$workflow" 'git fetch --no-tags --depth=1 origin "$SOURCE_SHA"'
contains "$workflow" 'test "$(git rev-parse HEAD)" = "$SOURCE_SHA"'
contains "$workflow" "if: github.event_name == 'push' && startsWith(github.ref, 'refs/tags/')"
contains "$workflow" 'cargo "+$RUST_VERSION" build --release --locked'
if grep -E 'toolchain (install|add).*stable|cargo \+stable|@[[:alnum:]_.-]*stable' "$workflow" >/dev/null; then
  fail_test "release workflow contains a floating stable toolchain"
fi
bad_uses="$(sed -n 's/.*uses: [^@]*@\([^ #]*\).*/\1/p' "$workflow" | grep -Ev '^[0-9a-f]{40}$' || true)"
[ -z "$bad_uses" ] || fail_test "release workflow contains an action that is not commit-pinned: $bad_uses"

# Exercise the same exact-SHA fetch for an unchanged branch, a branch that
# advances after the event, and an annotated release tag.
checkout_event() {
  remote=$1
  event_sha=$2
  destination=$3
  mkdir "$destination"
  git -C "$destination" init -q
  git -C "$destination" remote add origin "$remote"
  git -C "$destination" fetch -q --no-tags --depth=1 origin "$event_sha"
  git -C "$destination" checkout -q --detach FETCH_HEAD
  [ "$(git -C "$destination" rev-parse HEAD)" = "$event_sha" ] ||
    fail_test "exact event commit was not checked out"
}

source_repo="$test_tmp/source"
remote_repo="$test_tmp/remote.git"
git init -q "$source_repo"
git -C "$source_repo" config user.name "Release test"
git -C "$source_repo" config user.email "release-test@example.invalid"
printf 'first\n' > "$source_repo/content"
git -C "$source_repo" add content
git -C "$source_repo" commit -q -m first
git -C "$source_repo" branch -M main
original_sha="$(git -C "$source_repo" rev-parse HEAD)"
git init -q --bare "$remote_repo"
git -C "$source_repo" remote add origin "$remote_repo"
git -C "$source_repo" push -q origin main

checkout_event "$remote_repo" "$original_sha" "$test_tmp/unchanged"
printf 'second\n' >> "$source_repo/content"
git -C "$source_repo" commit -q -am second
git -C "$source_repo" push -q origin main
checkout_event "$remote_repo" "$original_sha" "$test_tmp/advanced"
git -C "$source_repo" tag -a v1.2.3 "$original_sha" -m v1.2.3
git -C "$source_repo" push -q origin refs/tags/v1.2.3
tag_commit="$(git -C "$source_repo" rev-list -n 1 v1.2.3)"
checkout_event "$remote_repo" "$tag_commit" "$test_tmp/tag"

# Source the installer functions without invoking main. The gh function below
# models the verifier boundary so every installer policy input is observable.
sed '/^main "\$@"$/d' "$root/install.sh" > "$test_tmp/install-lib.sh"
. "$test_tmp/install-lib.sh"

gh() {
  if [ "$1" = attestation ] && [ "$2" = verify ] && [ "$3" = --help ]; then
    printf '%s\n' '--repo --signer-workflow --signer-digest --source-ref --source-digest --cert-oidc-issuer --deny-self-hosted-runners'
    return
  fi
  if [ "$1" = api ]; then
    case "$2" in
      repos/*/releases/latest) printf '%s\n' "$api_tag" ;;
      repos/*/commits/*) printf '%s\n' "$api_source_sha" ;;
      *) return 1 ;;
    esac
    return
  fi
  [ "$1" = attestation ] && [ "$2" = verify ] || return 1
  verified_archive=$3
  shift 3
  policy_repo=
  policy_workflow=
  policy_signer_digest=
  policy_ref=
  policy_source=
  policy_issuer=
  deny_self_hosted=0
  while [ "$#" -gt 0 ]; do
    case "$1" in
      --repo) policy_repo=$2; shift 2 ;;
      --signer-workflow) policy_workflow=$2; shift 2 ;;
      --signer-digest) policy_signer_digest=$2; shift 2 ;;
      --source-ref) policy_ref=$2; shift 2 ;;
      --source-digest) policy_source=$2; shift 2 ;;
      --cert-oidc-issuer) policy_issuer=$2; shift 2 ;;
      --deny-self-hosted-runners) deny_self_hosted=1; shift ;;
      *) return 1 ;;
    esac
  done
  [ "${evidence_present:-0}" = 1 ] &&
    [ "$evidence_digest" = "$(hash_file "$verified_archive")" ] &&
    [ "$evidence_repo" = "$policy_repo" ] &&
    [ "$evidence_workflow" = "$policy_workflow" ] &&
    [ "$evidence_signer_digest" = "$policy_signer_digest" ] &&
    [ "$evidence_ref" = "$policy_ref" ] &&
    [ "$evidence_source" = "$policy_source" ] &&
    [ "$evidence_issuer" = "$policy_issuer" ] &&
    [ "$evidence_runner" = github-hosted ] &&
    [ "$deny_self_hosted" = 1 ]
}

curl() {
  output=
  requested_url=
  while [ "$#" -gt 0 ]; do
    case "$1" in
      -o) output=$2; shift 2 ;;
      -*) shift ;;
      *) requested_url=$1; shift ;;
    esac
  done
  case "$requested_url" in
    *.tar.gz.sha256) cp "$fixture_checksum" "$output" ;;
    *.tar.gz) cp "$fixture_archive" "$output" ;;
    *) return 1 ;;
  esac
}

api_tag=v1.2.3
api_source_sha=0123456789abcdef0123456789abcdef01234567
repo=oodss/KittyBrowser
trusted_repo=$repo
target=x86_64-unknown-linux-gnu
payload="$test_tmp/payload/kb-$target"
mkdir -p "$payload"
printf '#!/bin/sh\nprintf "kb fixture\\n"\n' > "$payload/kb"
chmod 755 "$payload/kb"
fixture_archive="$test_tmp/kb-$target.tar.gz"
(cd "$test_tmp/payload" && tar czf "$fixture_archive" "kb-$target")
fixture_checksum="$fixture_archive.sha256"
printf '%s  %s\n' "$(hash_file "$fixture_archive")" "kb-$target.tar.gz" > "$fixture_checksum"

set_valid_evidence() {
  evidence_present=1
  evidence_digest="$(hash_file "$fixture_archive")"
  evidence_repo=$trusted_repo
  evidence_workflow=$trusted_repo/.github/workflows/release.yml
  evidence_signer_digest=$api_source_sha
  evidence_ref=refs/tags/$api_tag
  evidence_source=$api_source_sha
  evidence_issuer=https://token.actions.githubusercontent.com
  evidence_runner=github-hosted
  tag=$api_tag
  source_sha=$api_source_sha
}

expect_rejected() {
  name=$1
  if (verify_release "$fixture_archive") >/dev/null 2>&1; then
    fail_test "$name evidence was accepted"
  fi
}

set_valid_evidence
cp "$fixture_archive" "$test_tmp/untampered.tar.gz"
printf 'tampered\n' >> "$fixture_archive"
expect_rejected tampered
mv "$test_tmp/untampered.tar.gz" "$fixture_archive"

set_valid_evidence
evidence_digest=ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff
expect_rejected "mismatched digest"
set_valid_evidence
evidence_repo=attacker/example
expect_rejected "wrong repository"
set_valid_evidence
evidence_workflow=oodss/KittyBrowser/.github/workflows/other.yml
expect_rejected "wrong workflow"
set_valid_evidence
evidence_signer_digest=ffffffffffffffffffffffffffffffffffffffff
expect_rejected "wrong workflow revision"
set_valid_evidence
evidence_issuer=https://issuer.example.invalid
expect_rejected "wrong signer"
set_valid_evidence
evidence_runner=self-hosted
expect_rejected "self-hosted signer"
set_valid_evidence
evidence_ref=refs/tags/v9.9.9
expect_rejected "wrong source tag"
set_valid_evidence
evidence_source=ffffffffffffffffffffffffffffffffffffffff
expect_rejected "wrong source commit"
set_valid_evidence
evidence_present=0
expect_rejected missing

repo=some-owner/some-fork
unset KB_TRUSTED_REPO
if (choose_trusted_repo) >/dev/null 2>&1; then
  fail_test "custom KB_REPO was trusted without explicit consent"
fi
KB_TRUSTED_REPO=$repo
choose_trusted_repo
unset KB_TRUSTED_REPO

repo=oodss/KittyBrowser
trusted_repo=$repo
set_valid_evidence
dir="$test_tmp/installed"
download
download_tmp=$tmp
trap 'rm -rf "$test_tmp" "$download_tmp"' EXIT
cmp "$payload/kb" "$dir/kb" >/dev/null || fail_test "valid attested archive was not installed"

printf 'release security tests passed\n'
