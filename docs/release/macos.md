# macOS desktop releases

Each `v<version>` release builds two standalone desktop app archives:

- `localsend-gpui-aarch64-apple-darwin-v<version>.tar.gz`
- `localsend-gpui-x86_64-apple-darwin-v<version>.tar.gz`

Each archive contains a complete `LocalSend RS.app` bundle with its executable,
`Info.plist`, `.icns` icon, and compiled light/dark `Assets.car`. The release
workflow requires the tag to equal both the `localsend-gpui` and `localsend-rs`
Cargo package versions;
the generated plist's `CFBundleShortVersionString` and `CFBundleVersion` are
also checked against that version. The stable bundle identifier is
`app.crosscopy.localsend-gpui`.

## Architecture and packaging

The arm64 artifact is built on an Apple silicon runner and the x86_64 artifact
on an Intel runner. `scripts/release.py` checks the runner architecture before
building, inspects the built Mach-O executable with `lipo`, and only then runs
that same native executable's `bundle` command. It checks the executable inside
the finished app again before archiving. A target binary is never executed on a
host of another architecture. Release tags must match both the desktop package
version and the root `localsend-rs` package version.

`SHA256SUMS` is attached to the GitHub release and covers all release archives.
From a downloaded release directory, verify archive integrity with:

```bash
shasum -a 256 -c SHA256SUMS
```

This detects corruption only relative to the manifest. It is not a publisher
signature, and a checksum downloaded from the same untrusted source does not
authenticate the publisher.

## Manual signing check without a release

After this workflow and the desktop app have been committed and pushed, run
the **Release** workflow manually from GitHub Actions, or use
`gh workflow run release.yml --ref <branch>`. GitHub requires the
`workflow_dispatch` version of the workflow to exist on the default branch
before it can be run manually.

The manual run derives the version from the checked-out Cargo manifests and
builds, signs, submits the app to Apple for notarization, staples its ticket,
and checks Gatekeeper on both native macOS runner architectures. It requires
the complete signing configuration below; missing credentials fail the run
rather than producing an unsigned result. Successful archives are retained as
GitHub Actions artifacts, **not published as a GitHub release**. The
cross-platform CLI builds and release-publishing job only run on tag pushes.
The check does not verify automatic update installation end to end.

## Signing and notarization

Signing/notarization is enabled only when the repository has all six of these
GitHub Actions secrets and the pinned Team ID variable configured:

| Secret | Contents |
|---|---|
| `APPLE_DEVELOPER_ID_CERTIFICATE_P12_BASE64` | Base64-encoded exported Developer ID Application `.p12` certificate |
| `APPLE_DEVELOPER_ID_CERTIFICATE_PASSWORD` | Password for that `.p12` |
| `APPLE_DEVELOPER_ID_SIGNING_IDENTITY` | Exact Developer ID Application identity name in the certificate |
| `APPLE_NOTARY_API_KEY_P8_BASE64` | Base64-encoded App Store Connect API `.p8` key |
| `APPLE_NOTARY_KEY_ID` | App Store Connect API key ID |
| `APPLE_NOTARY_ISSUER_ID` | App Store Connect issuer ID |
| `APPLE_DEVELOPER_TEAM_ID` (Actions variable) | The 10-character Apple Developer Team ID trusted by the updater |

The certificate must be valid for Developer ID distribution, and the API key
must be authorized to submit software for notarization. The job imports the
certificate into a temporary keychain, signs with hardened runtime and a secure
timestamp, submits the app for notarization, staples Apple's ticket, and removes
the temporary key material/keychain on exit.

`APPLE_DEVELOPER_TEAM_ID` is a public identifier, not a signing secret. Pin it
as the repository's trusted build input and treat changes as trust-root
rotations requiring review. For signed builds, CI passes it to Cargo as
`LOCALSEND_GPUI_TRUSTED_TEAM_ID`; Rust embeds it at compile time for the updater.
That pin is immutable in each compiled app; changing the Actions variable only
affects future builds.
The signed bundle's Team ID is checked against that same pinned value before
notarization. The updater validates the downloaded app's Apple signature against
its build-time pin; it never learns or trusts a Team ID from the downloaded
bundle or release metadata.

If none of the six secrets is present, the workflow deliberately produces an
**unsigned and not notarized** archive and emits a release-run notice. macOS may
show a Gatekeeper warning or refuse to open it. It has no verified Apple
publisher identity. A partially configured set is an error and stops the
release rather than silently downgrading to unsigned. Do not describe an
unsigned archive as trusted or Apple-verified.

The GitHub release notes and the `MACOS-ARTIFACT-STATUS.txt` release asset also
label whether both macOS archives are signed/notarized or unsigned. The updater
continues to consume only the existing target archive names and `SHA256SUMS`;
the status asset and release-note text are informational and do not affect its
verification contract. Builds without signing secrets do not embed a Team ID,
and the updater refuses to stage an update without a build-time publisher pin.

## Local checks

From `apps/desktop-gpui`:

```bash
python3 -m unittest discover -s scripts -p 'test_*.py'
python3 scripts/release.py check-version --tag v0.2.0
```

The packaging command is intended for a matching native macOS runner; it refuses
to package on a different host architecture.
