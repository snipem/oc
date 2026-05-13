# oc — project notes for Claude

## Versioning

- The version in `Cargo.toml` and the git tag **must always match** (e.g. `version = "0.4.1"` ↔ tag `v0.4.1`).
- Use **`0.0.x`** (patch) for small/minor changes, **`0.x.0`** (minor) for larger feature sets.
- After bumping `Cargo.toml`, commit the change and move the tag to HEAD with `git tag -f vX.Y.Z HEAD`.
