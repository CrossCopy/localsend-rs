//! Safe GitHub release discovery, verification, staging, and confirmed macOS app replacement.
//!
//! This module never reads a trust identity from the running process environment. Publisher
//! trust is embedded at build time through `LOCALSEND_GPUI_TRUSTED_TEAM_ID`; builds without
//! that setting may inspect releases, but cannot stage or install one.
//!
//! The macOS verifier requires a pinned Developer ID Application certificate chain and a
//! successful Gatekeeper execution assessment. Gatekeeper may accept a valid stapled ticket or
//! Apple's online notarization record; this implementation does not require a staple itself.
//! The release's macOS tar utility restores AppleDouble/PAX metadata; `ditto` preserves it
//! during staging. Signature and Gatekeeper checks fail closed if restored content is invalid.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use flate2::read::MultiGzDecoder;
use reqwest::Url;
use semver::Version;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tar::EntryType;
use tempfile::TempDir;
use thiserror::Error;

use crate::bundle::{BUNDLE_ID, BUNDLE_NAME, EXECUTABLE};

const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");
const GITHUB_API_BASE: &str = "https://api.github.com/";
const RELEASE_ARCHIVE_MAX_BYTES: u64 = 1024 * 1024 * 1024;
const CHECKSUMS_MAX_BYTES: u64 = 64 * 1024;
const RELEASE_METADATA_MAX_BYTES: u64 = 8 * 1024 * 1024;
const MAX_ARCHIVE_ENTRIES: usize = 20_000;
const MAX_TAR_TRAILING_PADDING_BYTES: u64 = 1024 * 1024;
const MAX_PAX_VALUE_BYTES: usize = 64 * 1024;
const MAX_PAX_METADATA_BYTES_PER_ENTRY: usize = 256 * 1024;
const TRUSTED_PUBLISHER_TEAM_ID: Option<&str> = option_env!("LOCALSEND_GPUI_TRUSTED_TEAM_ID");

/// Errors from release discovery, validation, staging, and installation.
#[derive(Debug, Error)]
pub enum UpdateError {
    /// The release service could not be reached or returned an unsuccessful response.
    #[error("the GitHub release service is unavailable")]
    ReleaseServiceUnavailable,
    /// The HTTP client failed while communicating with GitHub.
    #[error("the update request failed: {0}")]
    Http(#[from] reqwest::Error),
    /// The GitHub release response was malformed or inconsistent.
    #[error("GitHub returned invalid release metadata")]
    InvalidReleaseMetadata,
    /// The archive or checksum asset exceeded its configured bound.
    #[error("the update asset exceeds the permitted size")]
    AssetTooLarge,
    /// A download URL did not point to an expected GitHub HTTPS host.
    #[error("the update asset URL is not a trusted GitHub HTTPS URL")]
    UntrustedAssetUrl,
    /// The release does not include the target archive and checksum file.
    #[error("the release does not contain the required archive and SHA256SUMS assets")]
    MissingReleaseAssets,
    /// The archive digest did not match its SHA256SUMS entry.
    #[error("the update archive checksum does not match SHA256SUMS")]
    ChecksumMismatch,
    /// The checksum file was malformed or omitted the selected archive.
    #[error("SHA256SUMS is malformed or does not name the selected archive")]
    InvalidChecksums,
    /// The archive contained a path outside the expected application bundle.
    #[error("the update archive contains an unsafe path")]
    UnsafeArchivePath,
    /// The archive contained a symlink, hard link, or special file.
    #[error("the update archive contains an unsupported or unsafe entry")]
    UnsafeArchiveEntry,
    /// The archive contains sparse or unsupported PAX metadata.
    #[error("the update archive contains unsupported or unsafe PAX metadata")]
    UnsafeArchiveMetadata,
    /// The archive exceeded supported entry-count or extracted-size limits.
    #[error("the update archive exceeds supported extraction limits")]
    ArchiveTooLarge,
    /// The macOS archive utility could not restore the validated archive.
    #[error("the validated update archive could not be safely extracted")]
    ArchiveExtractionFailed,
    /// The archive did not contain the expected complete application bundle.
    #[error("the update archive does not contain a complete LocalSend RS.app bundle")]
    InvalidBundle,
    /// The archive's app identifier differs from this application.
    #[error("the update bundle identifier does not match this application")]
    WrongBundleIdentifier,
    /// The archive's version does not match the selected GitHub release.
    #[error("the update bundle version does not match its GitHub release")]
    WrongBundleVersion,
    /// The selected release is not newer than the running app.
    #[error("the selected release is not newer than the installed version")]
    NotNewerVersion,
    /// The release is for another OS or CPU architecture.
    #[error("the release targets {release}, not this {current} Mac")]
    WrongArchitecture {
        /// Architecture selected by release metadata.
        release: String,
        /// Architecture this updater was asked to stage.
        current: String,
    },
    /// A build has not pinned a publisher Team ID.
    #[error("no trusted publisher Team ID is configured in this build")]
    TrustedPublisherNotConfigured,
    /// The configured publisher Team ID is malformed.
    #[error("the build-time trusted publisher Team ID is invalid")]
    InvalidTrustedPublisher,
    /// The downloaded application is not signed by the configured publisher.
    #[error("the update bundle's publisher signature is invalid")]
    PublisherSignatureInvalid,
    /// macOS Gatekeeper did not accept the app's notarization/signing status.
    #[error("macOS Gatekeeper rejected the update bundle")]
    NotarizationAssessmentFailed,
    /// The current host is not macOS.
    #[error("macOS app-bundle updates are only supported on macOS")]
    UnsupportedPlatform,
    /// The user has not supplied the explicit confirmation token.
    #[error("installing an update requires explicit user confirmation")]
    ConfirmationRequired,
    /// The destination is not a real, expected app bundle.
    #[error("the installation destination is not a safe LocalSend RS.app bundle")]
    InvalidInstallDestination,
    /// The application could not be replaced transactionally.
    #[error("the app bundle could not be replaced safely: {0}")]
    InstallIo(#[source] io::Error),
    /// The filesystem did not support the required atomic directory swap.
    #[error("the filesystem could not atomically swap the app bundle")]
    AtomicSwapUnavailable(#[source] io::Error),
    /// A failed install could not restore the previous bundle.
    #[error(
        "the update failed and automatic rollback failed; the previous bundle remains at {retained_bundle}"
    )]
    RollbackFailed {
        /// The retained old bundle path to use for manual recovery.
        retained_bundle: PathBuf,
    },
    /// The newly installed bundle could not be launched.
    #[error("the updated application could not be launched; the previous bundle was restored")]
    RelaunchFailed,
    /// An archive or bundle filesystem operation failed validation or access.
    #[error("the update bundle could not be read or written: {0}")]
    Io(#[source] io::Error),
    /// The update plist could not be decoded.
    #[error("the update bundle Info.plist is invalid")]
    InvalidInfoPlist,
}

/// A supported macOS release architecture and its corresponding Rust target triple.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum UpdateTarget {
    /// Apple Silicon (`aarch64-apple-darwin`).
    Aarch64AppleDarwin,
    /// Intel macOS (`x86_64-apple-darwin`).
    X86_64AppleDarwin,
}

impl UpdateTarget {
    /// Parse one of the two supported macOS Rust target triples.
    pub fn new(target: &str) -> Result<Self, UpdateError> {
        match target {
            "aarch64-apple-darwin" => Ok(Self::Aarch64AppleDarwin),
            "x86_64-apple-darwin" => Ok(Self::X86_64AppleDarwin),
            _ => Err(UpdateError::WrongArchitecture {
                release: target.to_owned(),
                current: "supported macOS".to_owned(),
            }),
        }
    }

    /// Return the target triple used in the release archive name.
    pub const fn triple(self) -> &'static str {
        match self {
            Self::Aarch64AppleDarwin => "aarch64-apple-darwin",
            Self::X86_64AppleDarwin => "x86_64-apple-darwin",
        }
    }

    /// Detect the current macOS architecture, rejecting unsupported hosts.
    pub fn current() -> Result<Self, UpdateError> {
        if std::env::consts::OS != "macos" {
            return Err(UpdateError::UnsupportedPlatform);
        }
        match std::env::consts::ARCH {
            "aarch64" => Ok(Self::Aarch64AppleDarwin),
            "x86_64" => Ok(Self::X86_64AppleDarwin),
            _ => Err(UpdateError::UnsupportedPlatform),
        }
    }
}

/// The fixed public GitHub origin for this application.
#[derive(Clone, Debug, Eq, PartialEq)]
struct GitHubRepository {
    owner: String,
    name: String,
}

impl GitHubRepository {
    /// Create a validated GitHub repository coordinate.
    fn new(owner: &str, name: &str) -> Result<Self, UpdateError> {
        fn safe_segment(value: &str) -> bool {
            !value.is_empty()
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        }
        if !safe_segment(owner) || !safe_segment(name) {
            return Err(UpdateError::InvalidReleaseMetadata);
        }
        Ok(Self {
            owner: owner.to_owned(),
            name: name.to_owned(),
        })
    }

    fn api_path(&self) -> String {
        format!("repos/{}/{}/releases?per_page=100", self.owner, self.name)
    }

    fn asset_path_prefix(&self) -> String {
        format!("/{}/{}/releases/download/", self.owner, self.name)
    }
}

/// A parsed release that contains the exact archive/checksum pair for one Mac target.
#[derive(Clone, Debug)]
pub struct ReleaseInfo {
    version: Version,
    tag: String,
    target: UpdateTarget,
    archive_name: String,
    archive_url: Option<Url>,
    checksums_url: Option<Url>,
    archive_path: Option<PathBuf>,
    checksums_path: Option<PathBuf>,
    #[cfg(test)]
    checksum_bytes: Vec<u8>,
    notes: Option<String>,
}

impl ReleaseInfo {
    /// Return the semantic version of this release.
    pub fn version(&self) -> &Version {
        &self.version
    }

    /// Return the Git tag as published by GitHub.
    pub fn tag(&self) -> &str {
        &self.tag
    }

    /// Return the release architecture.
    pub const fn target(&self) -> UpdateTarget {
        self.target
    }

    /// Return the release notes, when GitHub supplied them.
    pub fn notes(&self) -> Option<&str> {
        self.notes.as_deref()
    }

    #[cfg(test)]
    fn metadata_for_test(tag: &str, target: UpdateTarget) -> Self {
        Self {
            version: parse_version_tag(tag).unwrap(),
            tag: tag.to_owned(),
            target,
            archive_name: archive_asset_name(target, &parse_version_tag(tag).unwrap().to_string()),
            archive_url: None,
            checksums_url: None,
            archive_path: None,
            checksums_path: None,
            checksum_bytes: Vec::new(),
            notes: None,
        }
    }

    #[cfg(test)]
    fn from_test_assets(
        version: &str,
        target: UpdateTarget,
        _archive: &[u8],
        checksum_bytes: Vec<u8>,
    ) -> Result<Self, UpdateError> {
        let version = Version::parse(version).map_err(|_| UpdateError::InvalidReleaseMetadata)?;
        Ok(Self {
            tag: format!("v{version}"),
            archive_name: archive_asset_name(target, &version.to_string()),
            version,
            target,
            archive_url: None,
            checksums_url: None,
            archive_path: None,
            checksums_path: None,
            checksum_bytes,
            notes: None,
        })
    }
}

/// The staged, fully verified application bundle.
///
/// Its private temporary directory is removed when this value is dropped. The installed app
/// and its associated settings/data are never stored beneath this staging directory.
#[derive(Debug)]
pub struct StagedUpdate {
    _workspace: TempDir,
    bundle_path: PathBuf,
    version: Version,
    target: UpdateTarget,
    publisher: PublisherIdentity,
}

impl StagedUpdate {
    /// Return the fully extracted, signature-verified bundle path.
    pub fn bundle_path(&self) -> &Path {
        &self.bundle_path
    }

    /// Return the verified release version.
    pub fn version(&self) -> &Version {
        &self.version
    }

    /// Return the verified architecture.
    pub const fn target(&self) -> UpdateTarget {
        self.target
    }
}

/// An explicit affirmative action from the update-confirmation UI.
///
/// The updater does not display UI itself. Call `user_confirmed` only from the affirmative
/// branch of a confirmation dialog; there is intentionally no implicit confirmation default.
#[derive(Debug)]
pub struct InstallConfirmation {
    _confirmed: (),
}

impl InstallConfirmation {
    /// Create a confirmation token after the user accepts an install prompt.
    pub fn user_confirmed() -> Self {
        Self { _confirmed: () }
    }
}

/// The result of a confirmed atomic bundle swap.
#[derive(Debug)]
pub struct InstallResult {
    /// The updated bundle path.
    pub installed_bundle: PathBuf,
    /// The retained previous bundle, available for manual rollback or removal.
    pub previous_bundle: PathBuf,
    /// The installed semantic version.
    pub version: Version,
    /// `open -n` accepted the relaunch request; this does not prove the app finished startup.
    pub relaunch_requested: bool,
}

/// The update service for CrossCopy/localsend-rs release assets.
pub struct UpdateService {
    repository: GitHubRepository,
    client: reqwest::Client,
    api_base: Url,
    trusted_publisher: Option<PublisherIdentity>,
    verifier: Arc<dyn BundleSignatureVerifier>,
    launcher: Arc<dyn AppLauncher>,
    swapper: Arc<dyn BundleSwapper>,
}

impl UpdateService {
    /// Construct the production updater.
    ///
    /// `LOCALSEND_GPUI_TRUSTED_TEAM_ID` is read only by the Rust compiler via `option_env!`.
    /// A process environment variable cannot change which publisher this binary trusts.
    pub fn new() -> Result<Self, UpdateError> {
        let publisher = TRUSTED_PUBLISHER_TEAM_ID
            .map(PublisherIdentity::new)
            .transpose()?;
        Self::build(
            GitHubRepository::new("CrossCopy", "localsend-rs")?,
            publisher,
            Arc::new(MacOsBundleVerifier),
            Arc::new(OpenAppLauncher),
            Arc::new(MacOsAtomicSwapper),
            Url::parse(GITHUB_API_BASE).map_err(|_| UpdateError::InvalidReleaseMetadata)?,
        )
    }

    fn build(
        repository: GitHubRepository,
        trusted_publisher: Option<PublisherIdentity>,
        verifier: Arc<dyn BundleSignatureVerifier>,
        launcher: Arc<dyn AppLauncher>,
        swapper: Arc<dyn BundleSwapper>,
        api_base: Url,
    ) -> Result<Self, UpdateError> {
        let client = reqwest::Client::builder()
            .user_agent(concat!("localsend-gpui/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::custom(|attempt| {
                if attempt.previous().len() < 10 && is_allowed_redirect_target(attempt.url()) {
                    attempt.follow()
                } else {
                    attempt.stop()
                }
            }))
            .build()?;
        Ok(Self {
            repository,
            client,
            api_base,
            trusted_publisher,
            verifier,
            launcher,
            swapper,
        })
    }

    /// Find the newest stable release newer than this build and compatible with this Mac.
    pub async fn discover_update(&self) -> Result<Option<ReleaseInfo>, UpdateError> {
        self.discover_update_for_target(UpdateTarget::current()?)
            .await
    }

    /// Find the newest stable release newer than the supplied supported Mac target.
    ///
    /// This explicit-target form is useful to frontends that display both Mac builds.
    pub async fn discover_update_for_target(
        &self,
        target: UpdateTarget,
    ) -> Result<Option<ReleaseInfo>, UpdateError> {
        let current =
            Version::parse(CURRENT_VERSION).map_err(|_| UpdateError::InvalidReleaseMetadata)?;
        let url = self
            .api_base
            .join(&self.repository.api_path())
            .map_err(|_| UpdateError::InvalidReleaseMetadata)?;
        let response = self
            .client
            .get(url)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .map_err(|_| UpdateError::ReleaseServiceUnavailable)?;
        if !response.status().is_success() {
            return Err(UpdateError::ReleaseServiceUnavailable);
        }
        let metadata = read_response_limited(response, RELEASE_METADATA_MAX_BYTES).await?;
        let raw: Vec<GitHubRelease> =
            serde_json::from_slice(&metadata).map_err(|_| UpdateError::InvalidReleaseMetadata)?;
        let mut releases = Vec::new();
        for release in raw {
            releases.extend(release_to_candidates(release, &self.repository)?);
        }
        Ok(select_release(&releases, target, current))
    }

    /// Download, checksum, safely extract, inspect, and signature-verify a release.
    pub async fn stage_release(
        &self,
        release: &ReleaseInfo,
        staging_parent: &Path,
    ) -> Result<StagedUpdate, UpdateError> {
        self.stage_release_for_target(release, UpdateTarget::current()?, staging_parent)
            .await
    }

    /// Stage a release for an explicit Mac target, without changing the installed application.
    pub async fn stage_release_for_target(
        &self,
        release: &ReleaseInfo,
        target: UpdateTarget,
        staging_parent: &Path,
    ) -> Result<StagedUpdate, UpdateError> {
        let publisher = self.ensure_trusted_publisher()?.clone();
        if release.target != target {
            return Err(UpdateError::WrongArchitecture {
                release: release.target.triple().to_owned(),
                current: target.triple().to_owned(),
            });
        }
        let current =
            Version::parse(CURRENT_VERSION).map_err(|_| UpdateError::InvalidReleaseMetadata)?;
        if release.version <= current {
            return Err(UpdateError::NotNewerVersion);
        }
        let workspace = make_private_workspace(staging_parent)?;
        let (archive_path, checksums) =
            if let (Some(archive), Some(sums)) = (&release.archive_path, &release.checksums_path) {
                (archive.clone(), fs::read(sums).map_err(UpdateError::Io)?)
            } else {
                self.download_release_assets(release, workspace.path())
                    .await?
            };
        let expected = parse_expected_checksum(&checksums, &release.archive_name)?;
        let actual = sha256_file(&archive_path)?;
        if actual != expected {
            return Err(UpdateError::ChecksumMismatch);
        }

        let bundle_path = extract_bundle(&archive_path, workspace.path())?;
        inspect_bundle(&bundle_path, &release.version)?;
        self.verifier.verify(&bundle_path, &publisher, target)?;

        Ok(StagedUpdate {
            _workspace: workspace,
            bundle_path,
            version: release.version.clone(),
            target,
            publisher,
        })
    }

    /// Replace an installed app bundle and relaunch it after an explicit confirmation.
    ///
    /// Replacement uses sibling-directory renames on the same volume. The prior `.app` is
    /// retained at `previous_bundle`; no settings, receive files, keychain entries, or other
    /// user data are touched. If replacement or relaunch fails, the previous bundle is restored.
    /// The private staging directory is reverified immediately before the atomic swap and once
    /// afterward. Same-user processes are not sandboxed from the account and can still race or
    /// mutate paths; this is not protection against a hostile process running as the same user.
    /// A successful `/usr/bin/open` result means only that relaunch was requested, not that startup
    /// completed; the previous bundle remains available for manual recovery.
    pub fn install_after_confirmation(
        &self,
        staged: StagedUpdate,
        installed_bundle: &Path,
        _confirmation: InstallConfirmation,
    ) -> Result<InstallResult, UpdateError> {
        let trusted = self.ensure_trusted_publisher()?;
        if trusted != &staged.publisher {
            return Err(UpdateError::PublisherSignatureInvalid);
        }
        if staged.target != UpdateTarget::current()? {
            return Err(UpdateError::WrongArchitecture {
                release: staged.target.triple().to_owned(),
                current: UpdateTarget::current()?.triple().to_owned(),
            });
        }
        let destination = validate_install_destination(installed_bundle)?;
        let installed_version = bundle_version(&destination)?;
        if staged.version <= installed_version {
            return Err(UpdateError::NotNewerVersion);
        }
        self.verifier
            .verify(&staged.bundle_path, trusted, staged.target)?;

        let parent = destination
            .parent()
            .ok_or(UpdateError::InvalidInstallDestination)?;
        let transaction = tempfile::Builder::new()
            .prefix(".localsend-gpui-update-")
            .tempdir_in(parent)
            .map_err(UpdateError::Io)?;
        let candidate = transaction.path().join(BUNDLE_NAME);
        copy_bundle_preserving_macos_metadata(&staged.bundle_path, &candidate)?;
        inspect_bundle(&candidate, &staged.version)?;
        // This verification is intentionally as close as possible to the namespace swap.
        self.verifier.verify(&candidate, trusted, staged.target)?;

        // The macOS RENAME_SWAP operation keeps the installed path present at every point.
        // There is deliberately no two-rename fallback: unsupported filesystems fail closed.
        self.swapper.swap(&destination, &candidate)?;
        if let Err(error) = self.verifier.verify(&destination, trusted, staged.target) {
            if self.swapper.swap(&destination, &candidate).is_err() {
                let _ = transaction.keep();
                return Err(UpdateError::RollbackFailed {
                    retained_bundle: candidate,
                });
            }
            return Err(error);
        }
        if self.launcher.launch(&destination).is_err() {
            if self.swapper.swap(&destination, &candidate).is_err() {
                let _ = transaction.keep();
                return Err(UpdateError::RollbackFailed {
                    retained_bundle: candidate,
                });
            }
            let _ = self.launcher.launch(&destination);
            return Err(UpdateError::RelaunchFailed);
        }

        let previous = candidate;
        let _ = transaction.keep();
        Ok(InstallResult {
            installed_bundle: destination,
            previous_bundle: previous,
            version: staged.version.clone(),
            relaunch_requested: true,
        })
    }

    fn ensure_trusted_publisher(&self) -> Result<&PublisherIdentity, UpdateError> {
        self.trusted_publisher
            .as_ref()
            .ok_or(UpdateError::TrustedPublisherNotConfigured)
    }

    async fn download_release_assets(
        &self,
        release: &ReleaseInfo,
        directory: &Path,
    ) -> Result<(PathBuf, Vec<u8>), UpdateError> {
        let archive_url = release
            .archive_url
            .as_ref()
            .ok_or(UpdateError::MissingReleaseAssets)?;
        let checksums_url = release
            .checksums_url
            .as_ref()
            .ok_or(UpdateError::MissingReleaseAssets)?;
        let archive_path = directory.join(&release.archive_name);
        download_file(
            &self.client,
            archive_url,
            &archive_path,
            RELEASE_ARCHIVE_MAX_BYTES,
        )
        .await?;
        let checksums_path = directory.join("SHA256SUMS");
        download_file(
            &self.client,
            checksums_url,
            &checksums_path,
            CHECKSUMS_MAX_BYTES,
        )
        .await?;
        let checksums = fs::read(checksums_path).map_err(UpdateError::Io)?;
        Ok((archive_path, checksums))
    }

    #[cfg(test)]
    fn with_parts(
        repository: GitHubRepository,
        trusted_publisher: Option<PublisherIdentity>,
        verifier: Arc<dyn BundleSignatureVerifier>,
        api_base: String,
    ) -> Self {
        let api_base = Url::parse(&api_base).unwrap();
        Self::build(
            repository,
            trusted_publisher,
            verifier,
            Arc::new(TestLauncher { fail: false }),
            Arc::new(TestSwapper::succeed()),
            api_base,
        )
        .unwrap()
    }

    #[cfg(test)]
    fn with_test_launch_failure(
        repository: GitHubRepository,
        trusted_publisher: Option<PublisherIdentity>,
        verifier: Arc<dyn BundleSignatureVerifier>,
        api_base: String,
    ) -> Self {
        Self::build(
            repository,
            trusted_publisher,
            verifier,
            Arc::new(TestLauncher { fail: true }),
            Arc::new(TestSwapper::succeed()),
            Url::parse(&api_base).unwrap(),
        )
        .unwrap()
    }

    #[cfg(test)]
    fn with_test_swap_failure(
        repository: GitHubRepository,
        trusted_publisher: Option<PublisherIdentity>,
        verifier: Arc<dyn BundleSignatureVerifier>,
        api_base: String,
    ) -> Self {
        Self::build(
            repository,
            trusted_publisher,
            verifier,
            Arc::new(TestLauncher { fail: false }),
            Arc::new(TestSwapper::fail_before_swap()),
            Url::parse(&api_base).unwrap(),
        )
        .unwrap()
    }

    #[cfg(test)]
    fn with_test_rollback_failure(
        repository: GitHubRepository,
        trusted_publisher: Option<PublisherIdentity>,
        verifier: Arc<dyn BundleSignatureVerifier>,
        api_base: String,
    ) -> Self {
        Self::build(
            repository,
            trusted_publisher,
            verifier,
            Arc::new(TestLauncher { fail: true }),
            Arc::new(TestSwapper::fail_on_rollback()),
            Url::parse(&api_base).unwrap(),
        )
        .unwrap()
    }

    #[cfg(test)]
    async fn stage_release_from_files(
        &self,
        release: &ReleaseInfo,
        staging_parent: &Path,
    ) -> Result<StagedUpdate, UpdateError> {
        self.stage_release_for_target(release, release.target, staging_parent)
            .await
    }

    #[cfg(test)]
    async fn stage_release_from_files_for_target(
        &self,
        release: &ReleaseInfo,
        target: UpdateTarget,
        staging_parent: &Path,
    ) -> Result<StagedUpdate, UpdateError> {
        self.stage_release_for_target(release, target, staging_parent)
            .await
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PublisherIdentity(String);

impl PublisherIdentity {
    fn new(team_id: &str) -> Result<Self, UpdateError> {
        let valid = team_id.len() == 10
            && team_id
                .bytes()
                .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit());
        if !valid {
            return Err(UpdateError::InvalidTrustedPublisher);
        }
        Ok(Self(team_id.to_owned()))
    }
}

trait BundleSignatureVerifier: Send + Sync {
    fn verify(
        &self,
        bundle: &Path,
        publisher: &PublisherIdentity,
        target: UpdateTarget,
    ) -> Result<(), UpdateError>;
}

struct MacOsBundleVerifier;

impl BundleSignatureVerifier for MacOsBundleVerifier {
    fn verify(
        &self,
        bundle: &Path,
        publisher: &PublisherIdentity,
        target: UpdateTarget,
    ) -> Result<(), UpdateError> {
        if std::env::consts::OS != "macos" {
            return Err(UpdateError::UnsupportedPlatform);
        }
        let status = Command::new("/usr/bin/codesign")
            .arg("--verify")
            .arg("--deep")
            .arg("--strict")
            .arg("--verbose=2")
            .arg(bundle)
            .status()
            .map_err(UpdateError::Io)?;
        if !status.success() {
            return Err(UpdateError::PublisherSignatureInvalid);
        }
        let status = Command::new("/usr/bin/codesign")
            .arg("--verify")
            .arg("--strict")
            .arg("--test-requirement")
            .arg(publisher_requirement(&publisher.0))
            .arg(bundle)
            .status()
            .map_err(UpdateError::Io)?;
        if !status.success() {
            return Err(UpdateError::PublisherSignatureInvalid);
        }
        let executable = bundle.join("Contents/MacOS").join(EXECUTABLE);
        if !macho_has_architecture(&executable, target)? {
            return Err(UpdateError::WrongArchitecture {
                release: "the executable's Mach-O header".to_owned(),
                current: target.triple().to_owned(),
            });
        }
        let status = Command::new("/usr/sbin/spctl")
            .arg("--assess")
            .arg("--type")
            .arg("execute")
            .arg("--verbose=4")
            .arg(bundle)
            .status()
            .map_err(UpdateError::Io)?;
        if !status.success() {
            return Err(UpdateError::NotarizationAssessmentFailed);
        }
        Ok(())
    }
}

fn publisher_requirement(team_id: &str) -> String {
    // The leaf OID excludes Apple Development certificates; the intermediate OID and Apple
    // anchor require the Developer ID certificate chain in addition to the pinned Team ID.
    format!(
        "identifier \"{BUNDLE_ID}\" and anchor apple generic and certificate 1[field.1.2.840.113635.100.6.2.6] exists and certificate leaf[field.1.2.840.113635.100.6.1.13] exists and certificate leaf[subject.OU] = \"{team_id}\""
    )
}

fn macho_has_architecture(path: &Path, target: UpdateTarget) -> Result<bool, UpdateError> {
    #[derive(Clone, Copy)]
    enum Endian {
        Big,
        Little,
    }

    fn read_u32(bytes: &[u8], endian: Endian) -> u32 {
        let bytes: [u8; 4] = bytes[..4].try_into().unwrap();
        match endian {
            Endian::Big => u32::from_be_bytes(bytes),
            Endian::Little => u32::from_le_bytes(bytes),
        }
    }

    fn matches_target(cpu_type: u32, target: UpdateTarget) -> bool {
        let expected = match target {
            UpdateTarget::Aarch64AppleDarwin => 0x0100_000c,
            UpdateTarget::X86_64AppleDarwin => 0x0100_0007,
        };
        cpu_type == expected
    }

    let mut file = File::open(path).map_err(UpdateError::Io)?;
    let mut magic = [0_u8; 4];
    file.read_exact(&mut magic).map_err(UpdateError::Io)?;
    let (endian, fat_entry_size) = match magic {
        [0xce, 0xfa, 0xed, 0xfe] | [0xcf, 0xfa, 0xed, 0xfe] => (Endian::Little, None),
        [0xfe, 0xed, 0xfa, 0xce] | [0xfe, 0xed, 0xfa, 0xcf] => (Endian::Big, None),
        [0xca, 0xfe, 0xba, 0xbe] => (Endian::Big, Some(20_usize)),
        [0xbe, 0xba, 0xfe, 0xca] => (Endian::Little, Some(20_usize)),
        [0xca, 0xfe, 0xba, 0xbf] => (Endian::Big, Some(32_usize)),
        [0xbf, 0xba, 0xfe, 0xca] => (Endian::Little, Some(32_usize)),
        _ => return Err(UpdateError::InvalidBundle),
    };
    let mut count_bytes = [0_u8; 4];
    file.read_exact(&mut count_bytes).map_err(UpdateError::Io)?;
    if let Some(entry_size) = fat_entry_size {
        let count = read_u32(&count_bytes, endian);
        if count == 0 || count > 128 {
            return Err(UpdateError::InvalidBundle);
        }
        let mut entry = vec![0_u8; entry_size];
        let mut found = false;
        for _ in 0..count {
            file.read_exact(&mut entry).map_err(UpdateError::Io)?;
            found |= matches_target(read_u32(&entry, endian), target);
        }
        Ok(found)
    } else {
        Ok(matches_target(read_u32(&count_bytes, endian), target))
    }
}

trait AppLauncher: Send + Sync {
    fn launch(&self, bundle: &Path) -> Result<(), UpdateError>;
}

struct OpenAppLauncher;

impl AppLauncher for OpenAppLauncher {
    fn launch(&self, bundle: &Path) -> Result<(), UpdateError> {
        let status = Command::new("/usr/bin/open")
            .arg("-n")
            .arg(bundle)
            .status()
            .map_err(UpdateError::Io)?;
        if status.success() {
            Ok(())
        } else {
            Err(UpdateError::RelaunchFailed)
        }
    }
}

trait BundleSwapper: Send + Sync {
    fn swap(&self, installed: &Path, candidate: &Path) -> Result<(), UpdateError>;
}

struct MacOsAtomicSwapper;

impl BundleSwapper for MacOsAtomicSwapper {
    fn swap(&self, installed: &Path, candidate: &Path) -> Result<(), UpdateError> {
        atomic_swap_paths(installed, candidate)
    }
}

#[cfg(test)]
struct TestSwapper {
    fail_on_call: Option<usize>,
    calls: std::sync::atomic::AtomicUsize,
}

#[cfg(test)]
impl TestSwapper {
    fn succeed() -> Self {
        Self {
            fail_on_call: None,
            calls: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    fn fail_before_swap() -> Self {
        Self {
            fail_on_call: Some(1),
            calls: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    fn fail_on_rollback() -> Self {
        Self {
            fail_on_call: Some(2),
            calls: std::sync::atomic::AtomicUsize::new(0),
        }
    }
}

#[cfg(test)]
impl BundleSwapper for TestSwapper {
    fn swap(&self, installed: &Path, candidate: &Path) -> Result<(), UpdateError> {
        let call = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        if self.fail_on_call == Some(call) {
            return Err(UpdateError::AtomicSwapUnavailable(io::Error::other(
                "injected atomic swap failure",
            )));
        }
        atomic_swap_paths(installed, candidate)
    }
}

fn atomic_swap_paths(installed: &Path, candidate: &Path) -> Result<(), UpdateError> {
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::ffi::OsStrExt as _;

        let installed = std::ffi::CString::new(installed.as_os_str().as_bytes())
            .map_err(|error| UpdateError::AtomicSwapUnavailable(io::Error::other(error)))?;
        let candidate = std::ffi::CString::new(candidate.as_os_str().as_bytes())
            .map_err(|error| UpdateError::AtomicSwapUnavailable(io::Error::other(error)))?;
        // SAFETY: both NUL-terminated paths are valid for the duration of the call, use
        // AT_FDCWD for absolute paths, and RENAME_SWAP only exchanges the two directory entries.
        let result = unsafe {
            libc::renameatx_np(
                libc::AT_FDCWD,
                installed.as_ptr(),
                libc::AT_FDCWD,
                candidate.as_ptr(),
                libc::RENAME_SWAP,
            )
        };
        if result == 0 {
            Ok(())
        } else {
            Err(UpdateError::AtomicSwapUnavailable(
                io::Error::last_os_error(),
            ))
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (installed, candidate);
        Err(UpdateError::AtomicSwapUnavailable(io::Error::new(
            io::ErrorKind::Unsupported,
            "atomic app-bundle swaps are only implemented on macOS",
        )))
    }
}

#[cfg(test)]
struct TestLauncher {
    fail: bool,
}

#[cfg(test)]
impl AppLauncher for TestLauncher {
    fn launch(&self, _bundle: &Path) -> Result<(), UpdateError> {
        if self.fail {
            Err(UpdateError::RelaunchFailed)
        } else {
            Ok(())
        }
    }
}

#[derive(Deserialize)]
struct GitHubRelease {
    tag_name: String,
    draft: bool,
    prerelease: bool,
    body: Option<String>,
    assets: Vec<GitHubAsset>,
}

#[derive(Deserialize)]
struct GitHubAsset {
    name: String,
    browser_download_url: String,
}

fn release_to_candidates(
    release: GitHubRelease,
    repository: &GitHubRepository,
) -> Result<Vec<ReleaseInfo>, UpdateError> {
    if release.draft || release.prerelease {
        return Ok(Vec::new());
    }
    let version = match parse_version_tag(&release.tag_name) {
        Some(version) if version.pre.is_empty() => version,
        _ => return Ok(Vec::new()),
    };
    let mut candidates = Vec::new();
    for target in [
        UpdateTarget::Aarch64AppleDarwin,
        UpdateTarget::X86_64AppleDarwin,
    ] {
        let archive_name = archive_asset_name(target, &version.to_string());
        let mut archive_assets = release
            .assets
            .iter()
            .filter(|asset| asset.name == archive_name);
        let Some(archive) = archive_assets.next() else {
            continue;
        };
        if archive_assets.next().is_some() {
            return Err(UpdateError::InvalidReleaseMetadata);
        }
        let mut checksum_assets = release
            .assets
            .iter()
            .filter(|asset| asset.name == "SHA256SUMS");
        let Some(checksums) = checksum_assets.next() else {
            continue;
        };
        if checksum_assets.next().is_some() {
            return Err(UpdateError::InvalidReleaseMetadata);
        }
        let archive_url = Url::parse(&archive.browser_download_url)
            .map_err(|_| UpdateError::InvalidReleaseMetadata)?;
        let checksums_url = Url::parse(&checksums.browser_download_url)
            .map_err(|_| UpdateError::InvalidReleaseMetadata)?;
        if !valid_release_download_url(&archive_url, repository, &release.tag_name, &archive_name)
            || !valid_release_download_url(
                &checksums_url,
                repository,
                &release.tag_name,
                "SHA256SUMS",
            )
        {
            return Err(UpdateError::UntrustedAssetUrl);
        }
        candidates.push(ReleaseInfo {
            version: version.clone(),
            tag: release.tag_name.clone(),
            target,
            archive_name,
            archive_url: Some(archive_url),
            checksums_url: Some(checksums_url),
            archive_path: None,
            checksums_path: None,
            #[cfg(test)]
            checksum_bytes: Vec::new(),
            notes: release.body.clone(),
        });
    }
    Ok(candidates)
}

fn valid_release_download_url(
    url: &Url,
    repository: &GitHubRepository,
    tag: &str,
    asset_name: &str,
) -> bool {
    url.scheme() == "https"
        && url.host_str() == Some("github.com")
        && url.username().is_empty()
        && url.password().is_none()
        && url.port().is_none_or(|port| port == 443)
        && url.query().is_none()
        && url.fragment().is_none()
        && url.path() == format!("{}{tag}/{asset_name}", repository.asset_path_prefix())
}

fn parse_version_tag(tag: &str) -> Option<Version> {
    Version::parse(tag.strip_prefix('v').unwrap_or(tag)).ok()
}

fn select_release(
    releases: &[ReleaseInfo],
    target: UpdateTarget,
    current: Version,
) -> Option<ReleaseInfo> {
    releases
        .iter()
        .filter(|release| {
            release.target == target && release.version > current && release.version.pre.is_empty()
        })
        .max_by(|left, right| left.version.cmp(&right.version))
        .cloned()
}

fn archive_asset_name(target: UpdateTarget, version: &str) -> String {
    format!("localsend-gpui-{}-v{version}.tar.gz", target.triple())
}

fn make_private_workspace(parent: &Path) -> Result<TempDir, UpdateError> {
    fs::create_dir_all(parent).map_err(UpdateError::Io)?;
    let canonical_parent = fs::canonicalize(parent).map_err(UpdateError::Io)?;
    tempfile::Builder::new()
        .prefix("localsend-gpui-update-")
        .tempdir_in(canonical_parent)
        .map_err(UpdateError::Io)
}

async fn download_file(
    client: &reqwest::Client,
    url: &Url,
    destination: &Path,
    max_bytes: u64,
) -> Result<(), UpdateError> {
    if url.scheme() != "https" || !is_allowed_download_host(url.host_str()) {
        return Err(UpdateError::UntrustedAssetUrl);
    }
    let mut response = client
        .get(url.clone())
        .timeout(Duration::from_secs(20 * 60))
        .send()
        .await
        .map_err(|_| UpdateError::ReleaseServiceUnavailable)?;
    if !response.status().is_success() {
        if response.status().is_redirection() {
            return Err(UpdateError::UntrustedAssetUrl);
        }
        return Err(UpdateError::ReleaseServiceUnavailable);
    }
    if response.url().scheme() != "https" || !is_allowed_download_host(response.url().host_str()) {
        return Err(UpdateError::UntrustedAssetUrl);
    }
    if response
        .content_length()
        .is_some_and(|length| length > max_bytes)
    {
        return Err(UpdateError::AssetTooLarge);
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)
        .map_err(UpdateError::Io)?;
    let mut written = 0_u64;
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| UpdateError::ReleaseServiceUnavailable)?
    {
        written = written
            .checked_add(chunk.len() as u64)
            .ok_or(UpdateError::AssetTooLarge)?;
        if written > max_bytes {
            return Err(UpdateError::AssetTooLarge);
        }
        file.write_all(&chunk).map_err(UpdateError::Io)?;
    }
    file.sync_all().map_err(UpdateError::Io)?;
    Ok(())
}

async fn read_response_limited(
    mut response: reqwest::Response,
    max_bytes: u64,
) -> Result<Vec<u8>, UpdateError> {
    if response
        .content_length()
        .is_some_and(|length| length > max_bytes)
    {
        return Err(UpdateError::AssetTooLarge);
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| UpdateError::ReleaseServiceUnavailable)?
    {
        let length = (body.len() as u64)
            .checked_add(chunk.len() as u64)
            .ok_or(UpdateError::AssetTooLarge)?;
        if length > max_bytes {
            return Err(UpdateError::AssetTooLarge);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn is_allowed_download_host(host: Option<&str>) -> bool {
    matches!(
        host,
        Some(
            "github.com"
                | "release-assets.githubusercontent.com"
                | "objects.githubusercontent.com"
                | "github-releases.githubusercontent.com"
        )
    )
}

fn is_allowed_redirect_target(url: &Url) -> bool {
    url.scheme() == "https"
        && url.username().is_empty()
        && url.password().is_none()
        && url.port().is_none_or(|port| port == 443)
        && matches!(
            url.host_str(),
            Some(
                "api.github.com"
                    | "github.com"
                    | "release-assets.githubusercontent.com"
                    | "objects.githubusercontent.com"
                    | "github-releases.githubusercontent.com"
            )
        )
}

fn parse_expected_checksum(checksums: &[u8], expected_name: &str) -> Result<String, UpdateError> {
    let text = std::str::from_utf8(checksums).map_err(|_| UpdateError::InvalidChecksums)?;
    let mut expected = None;
    let mut seen = HashSet::new();
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }
        let (digest, filename) = line
            .split_once("  ")
            .or_else(|| line.split_once(" *"))
            .ok_or(UpdateError::InvalidChecksums)?;
        if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(UpdateError::InvalidChecksums);
        }
        if filename.is_empty() || filename.contains('/') || filename.contains('\\') {
            return Err(UpdateError::InvalidChecksums);
        }
        if !seen.insert(filename.to_owned()) {
            return Err(UpdateError::InvalidChecksums);
        }
        if filename == expected_name {
            expected = Some(digest.to_ascii_lowercase());
        }
    }
    expected.ok_or(UpdateError::InvalidChecksums)
}

fn sha256_file(path: &Path) -> Result<String, UpdateError> {
    let mut file = File::open(path).map_err(UpdateError::Io)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer).map_err(UpdateError::Io)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn extract_bundle(archive_path: &Path, workspace: &Path) -> Result<PathBuf, UpdateError> {
    let archive_file = File::open(archive_path).map_err(UpdateError::Io)?;
    let decoder = MultiGzDecoder::new(archive_file);
    let mut archive = tar::Archive::new(decoder);
    let root = PathBuf::from(BUNDLE_NAME);
    {
        let mut entries = archive.entries().map_err(UpdateError::Io)?;
        let mut seen_entries = HashSet::new();
        let mut bundle_entries = HashSet::new();
        let mut appledouble_targets = Vec::new();
        let mut root_directory_seen = false;
        let mut count = 0_usize;
        let mut total_size = 0_u64;

        for entry in entries.by_ref() {
            let mut entry = entry.map_err(UpdateError::Io)?;
            count += 1;
            if count > MAX_ARCHIVE_ENTRIES {
                return Err(UpdateError::ArchiveTooLarge);
            }
            validate_archive_pax_metadata(&mut entry)?;
            let path = entry.path().map_err(UpdateError::Io)?.into_owned();
            let header_path = entry.header().path().map_err(UpdateError::Io)?.into_owned();
            if header_path != path {
                return Err(UpdateError::UnsafeArchivePath);
            }
            if !seen_entries.insert(path.clone()) {
                return Err(UpdateError::UnsafeArchiveEntry);
            }
            let entry_type = entry.header().entry_type();
            let sidecar_target = appledouble_target(&path);
            let target_path = sidecar_target.as_ref().unwrap_or(&path);
            validate_archive_path(target_path, &root)?;
            if let Some(target) = sidecar_target {
                if !entry_type.is_file() {
                    return Err(UpdateError::UnsafeArchiveEntry);
                }
                appledouble_targets.push(target);
            } else {
                if entry_type != EntryType::Directory && !entry_type.is_file() {
                    return Err(UpdateError::UnsafeArchiveEntry);
                }
                if !bundle_entries.insert(path.clone()) {
                    return Err(UpdateError::UnsafeArchiveEntry);
                }
            }
            let size = entry.size();
            total_size = total_size
                .checked_add(size)
                .ok_or(UpdateError::ArchiveTooLarge)?;
            if total_size > RELEASE_ARCHIVE_MAX_BYTES {
                return Err(UpdateError::ArchiveTooLarge);
            }
            if entry.header().mode().unwrap_or_default() & 0o7000 != 0 {
                return Err(UpdateError::UnsafeArchiveEntry);
            }
            if entry_type == EntryType::Directory && path == root {
                root_directory_seen = true;
            }
        }
        if !root_directory_seen
            || appledouble_targets
                .iter()
                .any(|target| !bundle_entries.contains(target))
        {
            return Err(UpdateError::InvalidBundle);
        }
    }
    let mut decoder = archive.into_inner();
    let mut trailing_bytes = 0_u64;
    let mut saw_trailing_padding = false;
    let mut buffer = [0_u8; 8192];
    loop {
        let count = decoder.read(&mut buffer).map_err(UpdateError::Io)?;
        if count == 0 {
            break;
        }
        trailing_bytes = trailing_bytes
            .checked_add(count as u64)
            .ok_or(UpdateError::ArchiveTooLarge)?;
        if trailing_bytes > MAX_TAR_TRAILING_PADDING_BYTES {
            return Err(UpdateError::ArchiveTooLarge);
        }
        if buffer[..count].iter().any(|byte| *byte != 0) {
            return Err(UpdateError::UnsafeArchiveEntry);
        }
        saw_trailing_padding = true;
    }
    if !saw_trailing_padding {
        return Err(UpdateError::InvalidBundle);
    }
    if std::env::consts::OS != "macos" {
        return Err(UpdateError::UnsupportedPlatform);
    }
    let status = Command::new("/usr/bin/tar")
        .arg("-xzf")
        .arg(archive_path)
        .arg("-C")
        .arg(workspace)
        .status()
        .map_err(UpdateError::Io)?;
    if !status.success() {
        return Err(UpdateError::ArchiveExtractionFailed);
    }
    Ok(workspace.join(BUNDLE_NAME))
}

fn validate_archive_pax_metadata<R: Read>(
    entry: &mut tar::Entry<'_, R>,
) -> Result<(), UpdateError> {
    let Some(extensions) = entry.pax_extensions().map_err(UpdateError::Io)? else {
        return Ok(());
    };
    let mut total_value_bytes = 0_usize;
    for extension in extensions {
        let extension = extension.map_err(|_| UpdateError::UnsafeArchiveMetadata)?;
        let key = extension
            .key()
            .map_err(|_| UpdateError::UnsafeArchiveMetadata)?;
        let value = extension.value_bytes();
        if value.len() > MAX_PAX_VALUE_BYTES {
            return Err(UpdateError::UnsafeArchiveMetadata);
        }
        total_value_bytes = total_value_bytes
            .checked_add(value.len())
            .ok_or(UpdateError::UnsafeArchiveMetadata)?;
        if total_value_bytes > MAX_PAX_METADATA_BYTES_PER_ENTRY {
            return Err(UpdateError::UnsafeArchiveMetadata);
        }
        if matches!(key, "path" | "linkpath") {
            return Err(UpdateError::UnsafeArchivePath);
        }
        let is_timestamp = matches!(key, "atime" | "ctime" | "mtime");
        let is_restorable_xattr =
            key.starts_with("LIBARCHIVE.xattr.") || key.starts_with("SCHILY.xattr.");
        // An allowlist makes GNU/SCHILY sparse, size overrides, ACLs, and future
        // path-affecting extensions fail closed instead of reaching native tar.
        if !is_timestamp && !is_restorable_xattr {
            return Err(UpdateError::UnsafeArchiveMetadata);
        }
    }
    Ok(())
}

fn appledouble_target(path: &Path) -> Option<PathBuf> {
    let mut components = path.components().collect::<Vec<_>>();
    let Component::Normal(name) = components.pop()? else {
        return None;
    };
    let target_name = name.to_str()?.strip_prefix("._")?;
    if target_name.is_empty() {
        return None;
    }
    let mut target = components
        .into_iter()
        .fold(PathBuf::new(), |path, component| path.join(component));
    target.push(target_name);
    Some(target)
}

fn validate_archive_path(path: &Path, expected_root: &Path) -> Result<(), UpdateError> {
    let mut components = path.components();
    if !matches!(components.next(), Some(Component::Normal(name)) if name == expected_root.as_os_str())
        || components.any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(UpdateError::UnsafeArchivePath);
    }
    Ok(())
}

fn inspect_bundle(bundle: &Path, release_version: &Version) -> Result<(), UpdateError> {
    let metadata = fs::symlink_metadata(bundle).map_err(UpdateError::Io)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(UpdateError::InvalidBundle);
    }
    let dictionary = read_bundle_plist(bundle)?;
    let bundle_id = dictionary
        .get("CFBundleIdentifier")
        .and_then(plist::Value::as_string)
        .ok_or(UpdateError::InvalidInfoPlist)?;
    if bundle_id != BUNDLE_ID {
        return Err(UpdateError::WrongBundleIdentifier);
    }
    let version = dictionary
        .get("CFBundleShortVersionString")
        .and_then(plist::Value::as_string)
        .ok_or(UpdateError::InvalidInfoPlist)?;
    if Version::parse(version).ok().as_ref() != Some(release_version) {
        return Err(UpdateError::WrongBundleVersion);
    }
    let build_version = dictionary
        .get("CFBundleVersion")
        .and_then(plist::Value::as_string)
        .ok_or(UpdateError::InvalidInfoPlist)?;
    if Version::parse(build_version).ok().as_ref() != Some(release_version) {
        return Err(UpdateError::WrongBundleVersion);
    }
    if dictionary
        .get("CFBundleExecutable")
        .and_then(plist::Value::as_string)
        != Some(EXECUTABLE)
        || dictionary
            .get("CFBundlePackageType")
            .and_then(plist::Value::as_string)
            != Some("APPL")
    {
        return Err(UpdateError::InvalidBundle);
    }
    let executable = bundle.join("Contents/MacOS").join(EXECUTABLE);
    let executable_metadata = fs::symlink_metadata(executable).map_err(UpdateError::Io)?;
    if !executable_metadata.is_file() || executable_metadata.file_type().is_symlink() {
        return Err(UpdateError::InvalidBundle);
    }
    Ok(())
}

fn read_bundle_plist(bundle: &Path) -> Result<plist::Dictionary, UpdateError> {
    let plist = plist::Value::from_file(bundle.join("Contents/Info.plist"))
        .map_err(|_| UpdateError::InvalidInfoPlist)?;
    plist
        .as_dictionary()
        .cloned()
        .ok_or(UpdateError::InvalidInfoPlist)
}

fn bundle_version(bundle: &Path) -> Result<Version, UpdateError> {
    let dictionary = read_bundle_plist(bundle)?;
    let version = dictionary
        .get("CFBundleShortVersionString")
        .and_then(plist::Value::as_string)
        .ok_or(UpdateError::InvalidInfoPlist)?;
    Version::parse(version).map_err(|_| UpdateError::InvalidInfoPlist)
}

fn validate_install_destination(bundle: &Path) -> Result<PathBuf, UpdateError> {
    if bundle.file_name() != Some(OsStr::new(BUNDLE_NAME)) {
        return Err(UpdateError::InvalidInstallDestination);
    }
    let metadata =
        fs::symlink_metadata(bundle).map_err(|_| UpdateError::InvalidInstallDestination)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(UpdateError::InvalidInstallDestination);
    }
    let parent = bundle
        .parent()
        .ok_or(UpdateError::InvalidInstallDestination)?;
    let parent = fs::canonicalize(parent).map_err(|_| UpdateError::InvalidInstallDestination)?;
    let destination = parent.join(BUNDLE_NAME);
    let dictionary = read_bundle_plist(&destination)?;
    if dictionary
        .get("CFBundleIdentifier")
        .and_then(plist::Value::as_string)
        != Some(BUNDLE_ID)
    {
        return Err(UpdateError::InvalidInstallDestination);
    }
    Ok(destination)
}

fn copy_bundle_preserving_macos_metadata(
    source: &Path,
    destination: &Path,
) -> Result<(), UpdateError> {
    if std::env::consts::OS != "macos" {
        return Err(UpdateError::UnsupportedPlatform);
    }
    let status = Command::new("/usr/bin/ditto")
        .arg("--rsrc")
        .arg("--extattr")
        .arg("--acl")
        .arg(source)
        .arg(destination)
        .status()
        .map_err(UpdateError::Io)?;
    if status.success() {
        Ok(())
    } else {
        Err(UpdateError::InstallIo(io::Error::other(
            "ditto could not preserve app-bundle metadata",
        )))
    }
}

#[cfg(test)]
mod tests;
