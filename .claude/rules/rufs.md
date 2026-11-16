# ufsmount: vendored rufs

- `vendor/fuse-ufs` is upstream fuse-ufs (github.com/realchonk/fuse-ufs), a squashed git subtree.
- Our changes there are separate commits that touch only `vendor/fuse-ufs`.
- Each such commit fits an upstream PR as is: nightly rustfmt with the upstream config,
  a `ChangeLog.md` line, tests where possible, and it passes rufs' tests and clippy.
- Export with `git format-patch --relative=vendor/fuse-ufs`. Apply with `git am` in the fork poeggi/fuse-ufs.
- Take upstream updates with `git subtree pull --prefix vendor/fuse-ufs --squash`.
- Upstream PRs come from branches of the fork. Never comment on upstream PRs or issues without the user.
- Upstream's `builds.sr.ht/openbsd` check fails on upstream main too (`fuse.h` not found in fuse2rs).
  It is not caused by our PRs.
