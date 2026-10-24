# Agent notes for Samsyn

Read this before changing anything. `docs/DEVELOPMENT.md` has the full developer
guide; this file only holds the rules that agents get wrong without being told.

## Commit messages: Conventional Commits, enforced

Every commit message must follow [Conventional Commits](https://www.conventionalcommits.org/):

```
<type>(<scope>)!: <summary>

<body: why, not what; wrap at 72>
```

- **type** (required), one of: `feat`, `fix`, `perf`, `refactor`, `docs`, `test`,
  `build`, `ci`, `chore`, `style`, `revert`.
- **scope** (optional, lowercase): `plugin`, `server`, `headless`, `sharing`,
  `storage`, `docs`, `ci`, `release`. Omit it for cross-cutting changes.
- **summary**: imperative, sentence case, no trailing period, header ≤ 72 chars.
  `feat(plugin): Add QR invites`, not `added qr invites.`
- **breaking change**: `!` after the type/scope and a `BREAKING CHANGE:` footer.
- Keep the header on the first line of `jj desc -m` / the commit. Bodies are welcome.

Why it is enforced: `.github/workflows/release.yml` uses the types to pick the
next version (`feat` → minor, everything else → patch, `!` → minor while on 0.x)
and `cliff.toml` builds the changelog and GitHub release notes from them. A
commit outside the format either breaks the version bump or lands in the wrong
changelog section. CI (`commitlint` job in `.github/workflows/ci.yml`) rejects
non-conforming commits.

Reserved: `chore(release): X.Y.Z` is written by the release PR workflow only;
`release.yml` keys on it to tag and publish. Never write it by hand.

## Version control: jj, not git

This repo is a colocated Jujutsu repo (`.jj/` + `.git/`). Use `jj` for all VCS
operations; raw `git` writes can corrupt jj state. Describe the change first
(`jj desc -m "type(scope): summary"`), then edit. Do not push or move the `main`
bookmark unless asked.

## Versioning and releases

- The plugin version lives in `/manifest.json` only. `plugin/manifest.json` is
  build output (copied by `plugin/esbuild.config.mjs`); never edit it.
- Never bump versions by hand. Stable releases come from merging the automated
  release PR; betas from the Release workflow button. Both run
  `scripts/version.mjs` and `git-cliff`; see docs/DEVELOPMENT.md "Releasing".
- `third_party/yrs` is a patched vendored copy; do not edit it (see its PATCH.md,
  which also explains the one manifest-only addition, a `[lints]` table).
