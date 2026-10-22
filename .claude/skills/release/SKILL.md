---
name: release
description: Release a new version of Kova
argument-hint: "<major|minor|patch>"
---

# Release Kova

Argument: `$ARGUMENTS` must be one of `major`, `minor`, or `patch`. If missing or invalid, abort with a clear error message.

## Steps

1. **Validate argument** is `major`, `minor`, or `patch`.

2. **Check branch and remote state:**
   - Abort if the current branch is not `main`.
   - Run `git fetch origin main` then check that `main` is up to date with `origin/main`. Abort if behind.

3. **Check for uncommitted changes.** If there are any:
   - Review the diff (`git diff` + `git status`)
   - Group changes into atomic commits by topic (e.g. separate feature from chore)
   - Stage whole files per commit (`git add <files>`, never `git add -p` or partial staging)
   - If files mix multiple topics, commit them together — don't split within a file
   - **Show each planned commit (files + message) to the user and ask for confirmation before committing**

4. **Parse current version** from `Cargo.toml` (`version = "X.Y.Z"`).

5. **Compute new version** by bumping the appropriate component.

6. **Check the tag `vX.Y.Z` doesn't already exist** (abort if it does).

7. **Update version** in both `Cargo.toml` and `Info.plist` (CFBundleVersion + CFBundleShortVersionString).

   **Write `RELEASE_NOTES.md`**: replace its whole content with `# X.Y.Z`, a blank line, then 1-5 bullets written for a Kova user from the commits since the last tag (what changes for them, not the internals). Kova embeds this file and shows it once on the first launch of the new version, and a test fails the release if its heading does not match `Cargo.toml`.

8. **Update `Cargo.lock`** by running `cargo check`. This updates the lockfile as a side effect without regenerating it from scratch.

9. **Build and test** (warnings are errors):
   ```bash
   RUSTFLAGS="-D warnings" ./build.sh && RUSTFLAGS="-D warnings" cargo test
   ```
   Abort if either fails. Do not tag a version that doesn't compile, has warnings, or fails tests.
   `./build.sh`, not `cargo build --release`: it also installs the released binary and Info.plist
   into `/Applications/Kova.app`, so the app carries the version being tagged.

10. **Generate release commit message:** Use `git log` since the last tag to summarize changes. Format:
    ```
    release: vX.Y.Z

    - <summary of changes since last tag, including commits just created in step 3>
    ```
    Keep it concise (1-5 bullet points).

11. **Commit all modified files** — specifically `Cargo.toml`, `Cargo.lock`, `Info.plist`, and `RELEASE_NOTES.md` (plus anything else changed by previous steps). Use `git add` with explicit file names, then verify with `git status` that `Cargo.lock` is staged before committing.

12. **Tag** as `vX.Y.Z`.

13. **Ask the user for confirmation** before pushing and creating the release. Show a summary: version bump, commits included, tag name.
    The GitHub release is a public post that notifies the repo's watchers: an outbound guard
    on the machine may refuse `gh release create` even when the user asked for the release.
    So show the release notes (step 15's text) in this same message, and have the user's
    go-ahead cover that post explicitly, in the form their guard expects, before step 15. A
    refused call still means the push went through: never retry it until they answer.

14. **Push** commit and tag atomically:
    ```bash
    git push --atomic origin main vX.Y.Z
    ```

15. **Create GitHub release** with the same notes, without their heading line:
    ```bash
    tail -n +2 RELEASE_NOTES.md > /tmp/kova-notes.md
    gh release create vX.Y.Z --notes-file /tmp/kova-notes.md
    ```

16. Confirm success to the user with the tag name and release URL.
