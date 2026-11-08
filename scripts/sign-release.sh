#!/usr/bin/env bash
# sign-release.sh vX.Y.Z [--apply-formula]
#
# Run on the owner's Mac after release.yml has built a DRAFT release.
#   1. downloads the draft's tarballs + SHA256SUMS
#   2. `gh attestation verify` each tarball and SHA256SUMS (build provenance)
#   3. checks the tarballs against SHA256SUMS
#   4. minisign-signs SHA256SUMS with the OFFLINE key
#   5. uploads SHA256SUMS.minisig and publishes the draft
#   6. prints the sha256 lines for homebrew-tap/Formula/scenery.rb
#      (--apply-formula edits the formula in place; you commit it)
#
# Environment:
#   SCENERY_MINISIGN_KEY   path to the minisign secret key (required, never printed)
#   SCENERY_FORMULA        formula path (default: ../../homebrew-tap/Formula/scenery.rb)
#
# Requires: gh (authenticated), minisign, shasum or sha256sum.
# Works with bash 3.2 (macOS default).
set -euo pipefail

TAG="${1:-}"
APPLY=0
[ "${2:-}" = "--apply-formula" ] && APPLY=1
case "$TAG" in v[0-9]*.[0-9]*.[0-9]*) ;; *) echo "usage: $0 vX.Y.Z [--apply-formula]" >&2; exit 2 ;; esac
VERSION="${TAG#v}"

REPO="scenery-dev/scenery"
SIGNER_WF="${REPO}/.github/workflows/release.yml"
HERE="$(cd "$(dirname "$0")" && pwd)"
FORMULA="${SCENERY_FORMULA:-${HERE}/../../homebrew-tap/Formula/scenery.rb}"

: "${SCENERY_MINISIGN_KEY:?set SCENERY_MINISIGN_KEY to the path of the offline minisign secret key}"
[ -f "$SCENERY_MINISIGN_KEY" ] || { echo "error: minisign key file not found (SCENERY_MINISIGN_KEY)" >&2; exit 1; }
for t in gh minisign; do command -v "$t" >/dev/null || { echo "error: $t not found" >&2; exit 1; }; done

sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | awk '{print $1}'
  else shasum -a 256 "$1" | awk '{print $1}'; fi
}

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

echo "==> Checking ${TAG} is a draft"
IS_DRAFT="$(gh release view "$TAG" --repo "$REPO" --json isDraft -q .isDraft)"
[ "$IS_DRAFT" = "true" ] || { echo "error: release ${TAG} is not a draft (already published?)" >&2; exit 1; }

echo "==> Downloading assets"
gh release download "$TAG" --repo "$REPO" --dir "$WORK" --pattern '*.tar.gz' --pattern SHA256SUMS

TARBALLS="$(cd "$WORK" && ls ./*.tar.gz | sed 's|^\./||')"
[ -n "$TARBALLS" ] || { echo "error: no tarballs in draft" >&2; exit 1; }

echo "==> Verifying build provenance (gh attestation verify)"
for f in $TARBALLS SHA256SUMS; do
  gh attestation verify "$WORK/$f" --repo "$REPO" \
    --signer-workflow "$SIGNER_WF" --source-ref "refs/tags/${TAG}" >/dev/null
  echo "    ok  $f"
done

echo "==> Checking tarballs against SHA256SUMS"
for f in $TARBALLS; do
  want="$(awk -v f="$f" '$2==f || $2=="*"f {print $1}' "$WORK/SHA256SUMS")"
  [ -n "$want" ] || { echo "error: $f missing from SHA256SUMS" >&2; exit 1; }
  [ "$(sha256_of "$WORK/$f")" = "$want" ] || { echo "error: checksum mismatch for $f" >&2; exit 1; }
done
[ "$(wc -l < "$WORK/SHA256SUMS" | tr -d ' ')" = "$(echo "$TARBALLS" | wc -l | tr -d ' ')" ] \
  || { echo "error: SHA256SUMS has unexpected entries" >&2; exit 1; }

echo "==> Signing SHA256SUMS with minisign (you will be prompted for the key password)"
minisign -S -s "$SCENERY_MINISIGN_KEY" -t "scenery ${TAG}" -m "$WORK/SHA256SUMS" -x "$WORK/SHA256SUMS.minisig"

echo "==> Uploading SHA256SUMS.minisig"
gh release upload "$TAG" --repo "$REPO" "$WORK/SHA256SUMS.minisig" --clobber

echo "==> Publishing ${TAG}"
gh release edit "$TAG" --repo "$REPO" --draft=false

echo
echo "==> Formula sha256 lines for ${TAG}"
for f in $TARBALLS; do
  triple="${f#scenery-${VERSION}-}"; triple="${triple%.tar.gz}"
  printf '%-32s sha256 "%s"\n' "$triple" "$(sha256_of "$WORK/$f")"
done

if [ "$APPLY" = "1" ]; then
  [ -f "$FORMULA" ] || { echo "error: formula not found at $FORMULA" >&2; exit 1; }
  echo "==> Updating $FORMULA"
  : > "$WORK/map"
  for f in $TARBALLS; do
    triple="${f#scenery-${VERSION}-}"; triple="${triple%.tar.gz}"
    echo "$triple $(sha256_of "$WORK/$f")" >> "$WORK/map"
  done
  awk -v ver="$VERSION" -v mapf="$WORK/map" '
    BEGIN { while ((getline l < mapf) > 0) { split(l, a, " "); m[a[1]] = a[2] } }
    /^  version "/ { print "  version \"" ver "\""; next }
    /releases\/download\// {
      cur = ""
      for (t in m) if (index($0, t ".tar.gz")) cur = t
      print; next
    }
    /^ *sha256 "/ && cur != "" {
      sub(/sha256 "[0-9a-f]*"/, "sha256 \"" m[cur] "\""); cur = ""; print; next
    }
    { print }
  ' "$FORMULA" > "$WORK/formula.new"
  cp "$WORK/formula.new" "$FORMULA"
  echo "    done. Review with git diff, then commit and push homebrew-tap."
else
  echo "(re-run with --apply-formula to edit the formula, or paste the lines above)"
fi
