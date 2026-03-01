#!/bin/bash
set -euo pipefail

usage() {
    echo "Usage: $0 <major|minor|patch>"
    exit 1
}

[[ $# -ne 1 ]] && usage

BUMP="$1"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CARGO="$ROOT/Cargo.toml"
PLIST="$ROOT/Info.plist"

# Read current version from Cargo.toml
CURRENT=$(grep '^version = ' "$CARGO" | head -1 | sed 's/version = "\(.*\)"/\1/')
IFS='.' read -r MAJOR MINOR PATCH <<< "$CURRENT"

case "$BUMP" in
    major) MAJOR=$((MAJOR + 1)); MINOR=0; PATCH=0 ;;
    minor) MINOR=$((MINOR + 1)); PATCH=0 ;;
    patch) PATCH=$((PATCH + 1)) ;;
    *) usage ;;
esac

NEW="$MAJOR.$MINOR.$PATCH"
TAG="v$NEW"

echo "Bumping $CURRENT -> $NEW"

# Check no uncommitted changes
if ! git -C "$ROOT" diff --quiet || ! git -C "$ROOT" diff --cached --quiet; then
    echo "Error: uncommitted changes. Commit or stash first."
    exit 1
fi

# Check tag doesn't exist
if git -C "$ROOT" tag -l "$TAG" | grep -q "$TAG"; then
    echo "Error: tag $TAG already exists."
    exit 1
fi

# Update Cargo.toml
sed -i '' "0,/^version = \".*\"/s//version = \"$NEW\"/" "$CARGO"

# Update Info.plist (both CFBundleVersion and CFBundleShortVersionString)
sed -i '' "/<key>CFBundleVersion<\/key>/{n;s/<string>.*<\/string>/<string>$NEW<\/string>/;}" "$PLIST"
sed -i '' "/<key>CFBundleShortVersionString<\/key>/{n;s/<string>.*<\/string>/<string>$NEW<\/string>/;}" "$PLIST"

# Commit, tag, push
git -C "$ROOT" add "$CARGO" "$PLIST"
git -C "$ROOT" commit -m "release: $TAG"
git -C "$ROOT" tag "$TAG"
git -C "$ROOT" push
git -C "$ROOT" push origin "$TAG"

echo "Released $TAG"
