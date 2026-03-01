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

8. **Update `Cargo.lock`** by running `cargo check`. This updates the lockfile as a side effect without regenerating it from scratch.

9. **Build and test:**
   ```bash
   cargo build --release && cargo test
   ```
   Abort if either fails. Do not tag a version that doesn't compile or pass tests.

10. **Generate release commit message:** Use `git log` since the last tag to summarize changes. Format:
    ```
    release: vX.Y.Z

    - <summary of changes since last tag, including commits just created in step 3>
    ```
    Keep it concise (1-5 bullet points).

11. **Commit all modified files** (not just version files — include everything changed by previous steps).

12. **Tag** as `vX.Y.Z`.

13. **Ask the user for confirmation** before pushing and creating the release. Show a summary: version bump, commits included, tag name.

14. **Push** commit and tag atomically:
    ```bash
    git push --atomic origin main vX.Y.Z
    ```

15. **Create GitHub release** with auto-generated notes:
    ```bash
    gh release create vX.Y.Z --generate-notes
    ```

16. Confirm success to the user with the tag name and release URL.
