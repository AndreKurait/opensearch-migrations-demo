# Releasing ma-demo

1. Update the package version in `Cargo.toml` and `Cargo.lock`, and write
   `docs/releases/<version>.md` with behavior changes and validation limits.
   Plain source builds derive their development version from `Cargo.toml`.
2. Run the checks listed in the README and merge through passing CI and release
   validation runs. Relevant pull requests build and verify all four targets
   without publishing a release.
3. Create a new `v<version>` tag at the tested commit. Never move a published tag.
4. Watch the `release` workflow. It resolves the tag once, checks the package
   version and release notes, builds four targets, and checks native versions.
   Each build stages its archive and checksum as workflow artifacts.
5. Publication validates the complete artifact set and archive contents, then
   uploads to a draft release. Only a successful upload publishes the release.
   A failed build creates no public release; a failed upload can leave a draft.
   Inspect and remove that incomplete draft before retrying the workflow against
   the same unchanged tag. Do not replace an already published release.
6. Verify the public installer in a temporary `BIN_DIR`, using the new version
   explicitly, then check `ma-demo version` and `ma-demo help`. These smoke
   checks do not provision resources or establish live AWS compatibility.

Manual workflow dispatch accepts an existing tag and builds only that tag's
commit. It is intended for recovery, not rebuilding published assets in place.
