---
name: release
description: Release a new ufsmount version (setup and exe on GitHub Releases). Use only when the user explicitly asks for a release.
---

# Release ufsmount

Only on the user's explicit request. Each push and the tag need their OK.

1. Set `version` in `Cargo.toml`. A `cargo build` updates `Cargo.lock`.
2. Run fmt, clippy and the tests (see `.claude/rules/repo.md`). Commit.
3. Push main. Wait until CI on main is green (build and smoke).
4. Check `git -C <repo> remote -v`. Create an annotated tag `vX.Y.Z` on that commit and push the tag.
5. The tag run builds and tests again. Its release job publishes `ufsmount.exe`, the setup, `LICENSE.txt`,
   `THIRD-PARTY-NOTICES.txt`, `licenses/*.txt` and `SHA256SUMS`. The notes start with direct
   download links (setup, `ufsmount.exe`), then GitHub's generated notes.
6. A failed tag run: find the cause before a re-run. Never move or reuse a pushed tag.
7. Check the release page lists all assets.
8. Delete the local `temp/installer` and `temp/release`. They hold test builds only; releases come from CI.
