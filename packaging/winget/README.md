# winget packaging

`scripts/make_winget.sh vX.Y.Z` generates the manifest trio
(version/installer/defaultLocale) for a published release into
`packaging/winget/<version>/`, checksummed from the release's
`checksums.txt`.

Submission to [microsoft/winget-pkgs](https://github.com/microsoft/winget-pkgs)
is a PR per version. Two routes:

- `wingetcreate submit --token <GITHUB_PAT> packaging/winget/<version>` —
  needs a classic PAT with `public_repo`. The release workflow runs this
  automatically when the `WINGET_TOKEN` secret exists (no-op otherwise,
  same pattern as the homebrew tap and marketplace jobs).
- Manual: fork winget-pkgs, copy the trio to
  `manifests/e/exYze/rift/<version>/`, open the PR.

First-time note: the initial submission of a new package goes through
winget-pkgs moderation; subsequent version bumps are fast-tracked once the
package is established. Users then install with `winget install exYze.rift`.
