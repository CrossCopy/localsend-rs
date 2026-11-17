# Homebrew

The repository owns two tap files, both rendered from the archives a release
actually published:

| File | Installs |
|---|---|
| `packaging/homebrew/Casks/localsend-rs.rb` | `LocalSend RS.app` (signed and notarized) |
| `packaging/homebrew/Formula/localsend-rs-cli.rb` | the `localsend-rs` command-line binary |

They live in `HuakunShen/homebrew-tap`:

```sh
brew install --cask HuakunShen/tap/localsend-rs
brew install HuakunShen/tap/localsend-rs-cli
```

The CLI formula is named `localsend-rs-cli` so it cannot be confused with the
desktop cask (or with Homebrew core's `localsend-cli`, the official LocalSend
terminal client); the installed executable is `localsend-rs`.

## How a release updates them

The `release` workflow's `homebrew` job downloads the six archives the two files
install, then runs:

```bash
cd apps/desktop-gpui
python3 scripts/homebrew.py --version <version> --assets-dir <downloaded archives>
```

The renderer rewrites the version and every `sha256` from the downloaded bytes.
It fails when an archive is missing rather than leaving a stale checksum, and it
refuses to guess the formula's version when the two files disagree. The result
is attached to the GitHub release as `localsend-rs.rb` and
`localsend-rs-cli.rb`.

CI never pushes to the tap: the tap holds other owners' casks and formulae, and
a repository token should not be able to rewrite them. After a release, copy the
two attached files into `HuakunShen/homebrew-tap` (`Casks/` and `Formula/`) and
push that repository.

## Verification

```sh
brew audit --cask HuakunShen/tap/localsend-rs
brew audit HuakunShen/tap/localsend-rs-cli
brew fetch --cask HuakunShen/tap/localsend-rs
brew fetch HuakunShen/tap/localsend-rs-cli
```

The cask installs a Developer ID signed and notarized bundle, so
`codesign -dv --verbose=2 "/Applications/LocalSend RS.app"` must report
`TeamIdentifier=25MFJJYA8J` and `spctl --assess --type execute` must accept it.
The CLI formula installs an unsigned command-line binary; it is not subject to
Gatekeeper's app-bundle checks.

`auto_updates true` in the cask is required: the app updates itself from the
signed release feed, and `brew upgrade` must not fight it.

## Local render

```bash
# Download the archives for a release with gh, then:
cd apps/desktop-gpui
python3 scripts/homebrew.py --version 0.2.1 --assets-dir /tmp/localsend-0.2.1
python3 -m unittest scripts.test_homebrew
```
