use super::*;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use flate2::write::GzEncoder;
use flate2::Compression;
use sha2::{Digest, Sha256};
use tar::{Builder, Header};

struct TestVerifier {
    accepted: bool,
}

impl BundleSignatureVerifier for TestVerifier {
    fn verify(
        &self,
        _bundle: &Path,
        _publisher: &PublisherIdentity,
        _target: UpdateTarget,
    ) -> Result<(), UpdateError> {
        if self.accepted {
            Ok(())
        } else {
            Err(UpdateError::PublisherSignatureInvalid)
        }
    }
}

fn service(_root: &Path, accepted: bool) -> UpdateService {
    UpdateService::with_parts(
        GitHubRepository::new("CrossCopy", "localsend-rs").unwrap(),
        Some(PublisherIdentity::new("ABCDEFGHIJ").unwrap()),
        Arc::new(TestVerifier { accepted }),
        "http://127.0.0.1:1/".to_owned(),
    )
}

fn service_that_cannot_relaunch(accepted: bool) -> UpdateService {
    UpdateService::with_test_launch_failure(
        GitHubRepository::new("CrossCopy", "localsend-rs").unwrap(),
        Some(PublisherIdentity::new("ABCDEFGHIJ").unwrap()),
        Arc::new(TestVerifier { accepted }),
        "http://127.0.0.1:1/".to_owned(),
    )
}

fn service_with_swap_failure() -> UpdateService {
    UpdateService::with_test_swap_failure(
        GitHubRepository::new("CrossCopy", "localsend-rs").unwrap(),
        Some(PublisherIdentity::new("ABCDEFGHIJ").unwrap()),
        Arc::new(TestVerifier { accepted: true }),
        "http://127.0.0.1:1/".to_owned(),
    )
}

fn service_that_cannot_rollback() -> UpdateService {
    UpdateService::with_test_rollback_failure(
        GitHubRepository::new("CrossCopy", "localsend-rs").unwrap(),
        Some(PublisherIdentity::new("ABCDEFGHIJ").unwrap()),
        Arc::new(TestVerifier { accepted: true }),
        "http://127.0.0.1:1/".to_owned(),
    )
}

fn add_file(builder: &mut Builder<GzEncoder<Vec<u8>>>, path: &str, bytes: &[u8]) {
    let mut header = Header::new_gnu();
    header.set_size(bytes.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    builder.append_data(&mut header, path, bytes).unwrap();
}

fn archive(version: &str) -> Vec<u8> {
    let mut builder = Builder::new(GzEncoder::new(Vec::new(), Compression::default()));
    let mut root = Header::new_gnu();
    root.set_entry_type(tar::EntryType::Directory);
    root.set_size(0);
    root.set_mode(0o755);
    root.set_cksum();
    builder
        .append_data(&mut root, "LocalSend RS.app/", &[][..])
        .unwrap();
    add_file(
        &mut builder,
        "LocalSend RS.app/Contents/Info.plist",
        &bundle_plist(version),
    );
    add_file(
        &mut builder,
        "LocalSend RS.app/Contents/MacOS/localsend-gpui",
        b"test executable",
    );
    builder.into_inner().unwrap().finish().unwrap()
}

fn bundle_plist(version: &str) -> Vec<u8> {
    format!(
        r#"<?xml version="1.0"?><plist version="1.0"><dict><key>CFBundleIdentifier</key><string>{BUNDLE_ID}</string><key>CFBundleExecutable</key><string>{EXECUTABLE}</string><key>CFBundlePackageType</key><string>APPL</string><key>CFBundleShortVersionString</key><string>{version}</string><key>CFBundleVersion</key><string>{version}</string></dict></plist>"#
    )
    .into_bytes()
}

fn archive_with_traversal() -> Vec<u8> {
    let mut builder = Builder::new(GzEncoder::new(Vec::new(), Compression::default()));
    let mut header = Header::new_gnu();
    let traversal_path = b"../outside";
    header.as_mut_bytes()[..traversal_path.len()].copy_from_slice(traversal_path);
    header.set_size(7);
    header.set_mode(0o644);
    header.set_cksum();
    builder.append(&header, &b"outside"[..]).unwrap();
    builder.into_inner().unwrap().finish().unwrap()
}

fn archive_with_concatenated_traversal() -> Vec<u8> {
    let mut archive = archive("0.3.0");
    archive.extend(archive_with_traversal());
    archive
}

fn archive_with_sparse_pax(key: &str) -> Vec<u8> {
    let mut builder = Builder::new(GzEncoder::new(Vec::new(), Compression::default()));
    let mut root = Header::new_gnu();
    root.set_entry_type(tar::EntryType::Directory);
    root.set_size(0);
    root.set_mode(0o755);
    root.set_cksum();
    builder
        .append_data(&mut root, "LocalSend RS.app/", &[][..])
        .unwrap();
    builder
        .append_pax_extensions([(key, b"999999999999".as_slice())])
        .unwrap();
    add_file(
        &mut builder,
        "LocalSend RS.app/Contents/huge",
        b"small payload",
    );
    builder.into_inner().unwrap().finish().unwrap()
}

fn archive_with_symlink() -> Vec<u8> {
    let mut builder = Builder::new(GzEncoder::new(Vec::new(), Compression::default()));
    let mut header = Header::new_gnu();
    header.set_entry_type(tar::EntryType::Symlink);
    header.set_size(0);
    header.set_link_name("../../outside").unwrap();
    header.set_mode(0o777);
    header.set_cksum();
    builder
        .append_data(&mut header, "LocalSend RS.app/Contents/escape", &[][..])
        .unwrap();
    builder.into_inner().unwrap().finish().unwrap()
}

fn archive_with_xattr_pax_header(version: &str) -> Vec<u8> {
    let mut builder = Builder::new(GzEncoder::new(Vec::new(), Compression::default()));
    let mut root = Header::new_gnu();
    root.set_entry_type(tar::EntryType::Directory);
    root.set_size(0);
    root.set_mode(0o755);
    root.set_cksum();
    builder
        .append_data(&mut root, "LocalSend RS.app/", &[][..])
        .unwrap();
    add_file(
        &mut builder,
        "LocalSend RS.app/Contents/Info.plist",
        &bundle_plist(version),
    );
    builder
        .append_pax_extensions([(
            "LIBARCHIVE.xattr.com.apple.provenance",
            b"AQIAgRWBcfpQNHE".as_slice(),
        )])
        .unwrap();
    add_file(
        &mut builder,
        "LocalSend RS.app/Contents/MacOS/localsend-gpui",
        b"test executable",
    );
    builder.into_inner().unwrap().finish().unwrap()
}

fn make_release(version: &str, target: UpdateTarget, archive: &[u8]) -> ReleaseInfo {
    ReleaseInfo::from_test_assets(
        version,
        target,
        archive,
        checksum_file(archive, version, target),
    )
    .unwrap()
}

fn checksum_file(archive: &[u8], version: &str, target: UpdateTarget) -> Vec<u8> {
    let name = archive_asset_name(target, version);
    let checksum = format!("{:x}", Sha256::digest(archive));
    format!("{checksum}  {name}\n").into_bytes()
}

fn put_bytes(root: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    fs::create_dir_all(root).unwrap();
    let path = root.join(name);
    fs::write(&path, bytes).unwrap();
    path
}

#[test]
fn release_selection_is_semver_ordered_and_requires_the_current_macos_architecture() {
    let arm = UpdateTarget::new("aarch64-apple-darwin").unwrap();
    let intel = UpdateTarget::new("x86_64-apple-darwin").unwrap();
    let releases = vec![
        ReleaseInfo::metadata_for_test("v0.9.0", intel),
        ReleaseInfo::metadata_for_test("v0.8.0", arm),
        ReleaseInfo::metadata_for_test("v0.7.0", arm),
    ];

    let selected = select_release(&releases, arm, Version::parse("0.6.0").unwrap()).unwrap();

    assert_eq!(selected.version(), &Version::parse("0.8.0").unwrap());
    assert!(select_release(&releases, intel, Version::parse("0.9.0").unwrap()).is_none());
}

#[test]
fn release_selection_never_offers_an_equal_or_downgrade_version() {
    let target = UpdateTarget::new("aarch64-apple-darwin").unwrap();
    let releases = vec![
        ReleaseInfo::metadata_for_test("v1.2.0", target),
        ReleaseInfo::metadata_for_test("v1.1.0", target),
    ];

    assert!(select_release(&releases, target, Version::parse("1.2.0").unwrap()).is_none());
}

#[test]
fn release_metadata_yields_both_architecture_builds_from_one_github_release() {
    let version = "0.3.0";
    let tag = format!("v{version}");
    let assets = [
        UpdateTarget::Aarch64AppleDarwin,
        UpdateTarget::X86_64AppleDarwin,
    ]
    .into_iter()
    .map(|target| {
        let name = archive_asset_name(target, version);
        GitHubAsset {
            browser_download_url: format!(
                "https://github.com/CrossCopy/localsend-rs/releases/download/{tag}/{name}"
            ),
            name,
        }
    })
    .chain(std::iter::once(GitHubAsset {
        name: "SHA256SUMS".to_owned(),
        browser_download_url: format!(
            "https://github.com/CrossCopy/localsend-rs/releases/download/{tag}/SHA256SUMS"
        ),
    }))
    .collect();
    let repository = GitHubRepository::new("CrossCopy", "localsend-rs").unwrap();

    let candidates = release_to_candidates(
        GitHubRelease {
            tag_name: tag,
            draft: false,
            prerelease: false,
            body: None,
            assets,
        },
        &repository,
    )
    .unwrap();

    assert_eq!(candidates.len(), 2);
    assert!(candidates
        .iter()
        .any(|release| { release.target() == UpdateTarget::Aarch64AppleDarwin }));
    assert!(candidates
        .iter()
        .any(|release| { release.target() == UpdateTarget::X86_64AppleDarwin }));
}

#[test]
fn release_metadata_cannot_point_assets_at_an_untrusted_host() {
    let target = UpdateTarget::Aarch64AppleDarwin;
    let version = "0.3.0";
    let tag = format!("v{version}");
    let archive_name = archive_asset_name(target, version);
    let repository = GitHubRepository::new("CrossCopy", "localsend-rs").unwrap();
    let release = GitHubRelease {
        tag_name: tag.clone(),
        draft: false,
        prerelease: false,
        body: None,
        assets: vec![
            GitHubAsset {
                name: archive_name,
                browser_download_url: format!("https://attacker.example/{tag}/archive.tar.gz"),
            },
            GitHubAsset {
                name: "SHA256SUMS".to_owned(),
                browser_download_url: format!(
                    "https://github.com/CrossCopy/localsend-rs/releases/download/{tag}/SHA256SUMS"
                ),
            },
        ],
    };

    assert!(matches!(
        release_to_candidates(release, &repository),
        Err(UpdateError::UntrustedAssetUrl)
    ));
}

#[test]
fn mach_o_header_architecture_is_checked_without_external_tools() {
    let temp = tempfile::tempdir().unwrap();
    let executable = temp.path().join("localsend-gpui");
    let mut bytes = vec![0_u8; 32];
    bytes[..4].copy_from_slice(&[0xcf, 0xfa, 0xed, 0xfe]);
    bytes[4..8].copy_from_slice(&0x0100_0007_u32.to_le_bytes());
    fs::write(&executable, bytes).unwrap();

    assert!(macho_has_architecture(&executable, UpdateTarget::X86_64AppleDarwin).unwrap());
    assert!(!macho_has_architecture(&executable, UpdateTarget::Aarch64AppleDarwin).unwrap());
}

#[tokio::test]
async fn staging_rejects_a_bad_archive_checksum_before_unpacking() {
    let target = UpdateTarget::new("aarch64-apple-darwin").unwrap();
    let payload = archive("0.3.0");
    let mut release = make_release("0.3.0", target, &payload);
    release.checksum_bytes = b"0000000000000000000000000000000000000000000000000000000000000000  localsend-gpui-aarch64-apple-darwin-v0.3.0.tar.gz\n".to_vec();
    let temp = tempfile::tempdir().unwrap();
    let archive_path = put_bytes(temp.path(), "release.tar.gz", &payload);
    let checksums_path = put_bytes(temp.path(), "SHA256SUMS", &release.checksum_bytes);
    release.archive_path = Some(archive_path);
    release.checksums_path = Some(checksums_path);

    let result = service(temp.path(), true)
        .stage_release_from_files(&release, temp.path())
        .await;

    assert!(matches!(result, Err(UpdateError::ChecksumMismatch)));
}

#[tokio::test]
async fn staging_rejects_an_invalid_publisher_signature() {
    let target = UpdateTarget::new("aarch64-apple-darwin").unwrap();
    let payload = archive("0.3.0");
    let mut release = make_release("0.3.0", target, &payload);
    let temp = tempfile::tempdir().unwrap();
    release.archive_path = Some(put_bytes(temp.path(), "release.tar.gz", &payload));
    release.checksums_path = Some(put_bytes(
        temp.path(),
        "SHA256SUMS",
        &release.checksum_bytes,
    ));

    let result = service(temp.path(), false)
        .stage_release_from_files(&release, temp.path())
        .await;

    assert!(
        matches!(result, Err(UpdateError::PublisherSignatureInvalid)),
        "{result:?}"
    );
}

#[tokio::test]
async fn staging_extracts_and_validates_a_whole_signed_bundle() {
    let target = UpdateTarget::new("aarch64-apple-darwin").unwrap();
    let payload = archive("0.3.0");
    let mut release = make_release("0.3.0", target, &payload);
    let temp = tempfile::tempdir().unwrap();
    release.archive_path = Some(put_bytes(temp.path(), "release.tar.gz", &payload));
    release.checksums_path = Some(put_bytes(
        temp.path(),
        "SHA256SUMS",
        &release.checksum_bytes,
    ));

    let staged = service(temp.path(), true)
        .stage_release_from_files(&release, temp.path())
        .await
        .unwrap();

    assert_eq!(staged.version(), &Version::parse("0.3.0").unwrap());
    assert_eq!(
        fs::read(staged.bundle_path().join("Contents/MacOS/localsend-gpui")).unwrap(),
        b"test executable"
    );
}

#[tokio::test]
async fn staging_rejects_an_archive_for_a_different_architecture() {
    let target = UpdateTarget::new("aarch64-apple-darwin").unwrap();
    let wrong_target = UpdateTarget::new("x86_64-apple-darwin").unwrap();
    let payload = archive("0.3.0");
    let mut release = make_release("0.3.0", wrong_target, &payload);
    let temp = tempfile::tempdir().unwrap();
    release.archive_path = Some(put_bytes(temp.path(), "release.tar.gz", &payload));
    release.checksums_path = Some(put_bytes(
        temp.path(),
        "SHA256SUMS",
        &release.checksum_bytes,
    ));

    let result = service(temp.path(), true)
        .stage_release_from_files_for_target(&release, target, temp.path())
        .await;

    assert!(matches!(result, Err(UpdateError::WrongArchitecture { .. })));
}

#[tokio::test]
async fn staging_refuses_equal_or_older_release_versions() {
    let target = UpdateTarget::new("aarch64-apple-darwin").unwrap();
    let payload = archive("0.1.0");
    let mut release = make_release("0.1.0", target, &payload);
    let temp = tempfile::tempdir().unwrap();
    release.archive_path = Some(put_bytes(temp.path(), "release.tar.gz", &payload));
    release.checksums_path = Some(put_bytes(
        temp.path(),
        "SHA256SUMS",
        &release.checksum_bytes,
    ));

    let result = service(temp.path(), true)
        .stage_release_from_files(&release, temp.path())
        .await;

    assert!(matches!(result, Err(UpdateError::NotNewerVersion)));
}

#[tokio::test]
async fn staging_rejects_a_bundle_version_that_disagrees_with_the_release_tag() {
    let target = UpdateTarget::new("aarch64-apple-darwin").unwrap();
    let payload = archive("0.4.0");
    let mut release = make_release("0.3.0", target, &payload);
    let temp = tempfile::tempdir().unwrap();
    release.archive_path = Some(put_bytes(temp.path(), "release.tar.gz", &payload));
    release.checksums_path = Some(put_bytes(
        temp.path(),
        "SHA256SUMS",
        &release.checksum_bytes,
    ));

    let result = service(temp.path(), true)
        .stage_release_from_files(&release, temp.path())
        .await;

    assert!(matches!(result, Err(UpdateError::WrongBundleVersion)));
}

#[tokio::test]
async fn staging_rejects_archive_path_traversal() {
    let target = UpdateTarget::new("aarch64-apple-darwin").unwrap();
    let payload = archive_with_traversal();
    let mut release = make_release("0.3.0", target, &payload);
    let temp = tempfile::tempdir().unwrap();
    release.archive_path = Some(put_bytes(temp.path(), "release.tar.gz", &payload));
    release.checksums_path = Some(put_bytes(
        temp.path(),
        "SHA256SUMS",
        &release.checksum_bytes,
    ));

    let result = service(temp.path(), true)
        .stage_release_from_files(&release, temp.path())
        .await;

    assert!(matches!(result, Err(UpdateError::UnsafeArchivePath)));
    assert!(!temp.path().join("outside.txt").exists());
}

#[tokio::test]
async fn staging_rejects_a_concatenated_tar_stream_after_the_end_marker() {
    let target = UpdateTarget::new("aarch64-apple-darwin").unwrap();
    let payload = archive_with_concatenated_traversal();
    let mut release = make_release("0.3.0", target, &payload);
    let temp = tempfile::tempdir().unwrap();
    release.archive_path = Some(put_bytes(temp.path(), "release.tar.gz", &payload));
    release.checksums_path = Some(put_bytes(
        temp.path(),
        "SHA256SUMS",
        &release.checksum_bytes,
    ));

    let result = service(temp.path(), true)
        .stage_release_from_files(&release, temp.path())
        .await;

    assert!(matches!(result, Err(UpdateError::UnsafeArchiveEntry)));
    assert!(!temp.path().join("outside.txt").exists());
}

#[test]
fn sparse_pax_metadata_is_rejected_before_native_tar_extraction() {
    for key in [
        "GNU.sparse.realsize",
        "GNU.sparse.map",
        "SCHILY.realsize",
        "SCHILY.sparse.size",
    ] {
        let temp = tempfile::tempdir().unwrap();
        let archive = archive_with_sparse_pax(key);
        let archive_path = put_bytes(temp.path(), "sparse.tar.gz", &archive);
        let workspace = temp.path().join("workspace");
        fs::create_dir(&workspace).unwrap();

        let result = extract_bundle(&archive_path, &workspace);

        assert!(
            matches!(result, Err(UpdateError::UnsafeArchiveMetadata)),
            "{key} must be rejected: {result:?}"
        );
        assert!(
            fs::read_dir(workspace).unwrap().next().is_none(),
            "{key} must be rejected before the system tar extracts any entry"
        );
    }
}

#[tokio::test]
async fn staging_rejects_archive_symlinks() {
    let target = UpdateTarget::new("aarch64-apple-darwin").unwrap();
    let payload = archive_with_symlink();
    let mut release = make_release("0.3.0", target, &payload);
    let temp = tempfile::tempdir().unwrap();
    release.archive_path = Some(put_bytes(temp.path(), "release.tar.gz", &payload));
    release.checksums_path = Some(put_bytes(
        temp.path(),
        "SHA256SUMS",
        &release.checksum_bytes,
    ));

    let result = service(temp.path(), true)
        .stage_release_from_files(&release, temp.path())
        .await;

    assert!(matches!(result, Err(UpdateError::UnsafeArchiveEntry)));
}

#[tokio::test]
async fn release_discovery_returns_a_typed_error_when_github_is_unavailable() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = [0_u8; 2048];
        let _ = socket.read(&mut request).await.unwrap();
        socket
            .write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
    });
    let service = UpdateService::with_parts(
        GitHubRepository::new("CrossCopy", "localsend-rs").unwrap(),
        Some(PublisherIdentity::new("ABCDEFGHIJ").unwrap()),
        Arc::new(TestVerifier { accepted: true }),
        format!("http://{address}/"),
    );

    let result = service.discover_update().await;

    assert!(matches!(
        result,
        Err(UpdateError::ReleaseServiceUnavailable)
    ));
    server.await.unwrap();
}

#[tokio::test]
async fn oversized_release_metadata_is_rejected_before_json_decoding() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = [0_u8; 2048];
        let _ = socket.read(&mut request).await.unwrap();
        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 8388609\r\nConnection: close\r\n\r\n",
            )
            .await
            .unwrap();
    });
    let service = UpdateService::with_parts(
        GitHubRepository::new("CrossCopy", "localsend-rs").unwrap(),
        Some(PublisherIdentity::new("ABCDEFGHIJ").unwrap()),
        Arc::new(TestVerifier { accepted: true }),
        format!("http://{address}/"),
    );

    let result = service
        .discover_update_for_target(UpdateTarget::Aarch64AppleDarwin)
        .await;

    assert!(matches!(result, Err(UpdateError::AssetTooLarge)));
    server.await.unwrap();
}

#[tokio::test]
async fn pax_xattrs_are_restored_by_the_macos_archive_utility() {
    let target = UpdateTarget::new("aarch64-apple-darwin").unwrap();
    let payload = archive_with_xattr_pax_header("0.3.0");
    let mut release = make_release("0.3.0", target, &payload);
    let temp = tempfile::tempdir().unwrap();
    release.archive_path = Some(put_bytes(temp.path(), "release.tar.gz", &payload));
    release.checksums_path = Some(put_bytes(
        temp.path(),
        "SHA256SUMS",
        &release.checksum_bytes,
    ));

    let updater = service(temp.path(), true);
    let staged = updater
        .stage_release_from_files(&release, temp.path())
        .await
        .unwrap();

    let staged_executable = staged.bundle_path().join("Contents/MacOS").join(EXECUTABLE);
    assert_eq!(fs::read(&staged_executable).unwrap(), b"test executable");
    assert!(has_provenance_xattr(&staged_executable));

    let installed = temp.path().join(BUNDLE_NAME);
    fs::create_dir_all(installed.join("Contents/MacOS")).unwrap();
    fs::write(installed.join("Contents/Info.plist"), bundle_plist("0.2.0")).unwrap();
    fs::write(
        installed.join("Contents/MacOS").join(EXECUTABLE),
        b"old app",
    )
    .unwrap();
    updater
        .install_after_confirmation(staged, &installed, InstallConfirmation::user_confirmed())
        .unwrap();
    assert!(has_provenance_xattr(
        &installed.join("Contents/MacOS").join(EXECUTABLE)
    ));
}

fn has_provenance_xattr(path: &Path) -> bool {
    std::process::Command::new("/usr/bin/xattr")
        .arg("-p")
        .arg("com.apple.provenance")
        .arg(path)
        .output()
        .is_ok_and(|result| result.status.success())
}

#[test]
fn redirect_policy_allows_only_https_github_and_release_asset_hosts() {
    for accepted in [
        "https://github.com/CrossCopy/localsend-rs/releases/download/v0.3.0/app.tar.gz",
        "https://release-assets.githubusercontent.com/object",
        "https://objects.githubusercontent.com/object",
        "https://api.github.com/repos/CrossCopy/localsend-rs/releases",
    ] {
        assert!(
            is_allowed_redirect_target(&Url::parse(accepted).unwrap()),
            "{accepted} should be permitted"
        );
    }
    for rejected in [
        "http://github.com/CrossCopy/localsend-rs/releases/download/app.tar.gz",
        "https://attacker.example/app.tar.gz",
        "https://github.com.attacker.example/app.tar.gz",
        "https://user:password@github.com/app.tar.gz",
        "https://github.com:444/app.tar.gz",
    ] {
        assert!(
            !is_allowed_redirect_target(&Url::parse(rejected).unwrap()),
            "{rejected} should be refused"
        );
    }
}

#[tokio::test]
async fn http_redirect_is_stopped_before_the_unapproved_intermediate_is_contacted() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let blocked = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    blocked.set_nonblocking(true).unwrap();
    let blocked_address = blocked.local_addr().unwrap();
    let origin = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin_address = origin.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = origin.accept().await.unwrap();
        let mut request = [0_u8; 2048];
        let _ = socket.read(&mut request).await.unwrap();
        let response = format!(
            "HTTP/1.1 302 Found\r\nLocation: http://{blocked_address}/steal\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        );
        socket.write_all(response.as_bytes()).await.unwrap();
    });
    let service = UpdateService::with_parts(
        GitHubRepository::new("CrossCopy", "localsend-rs").unwrap(),
        Some(PublisherIdentity::new("ABCDEFGHIJ").unwrap()),
        Arc::new(TestVerifier { accepted: true }),
        format!("http://{origin_address}/"),
    );

    let response = service
        .client
        .get(format!("http://{origin_address}/redirect"))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), reqwest::StatusCode::FOUND);
    assert!(matches!(
        blocked.accept(),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock
    ));
    server.await.unwrap();
}

#[tokio::test]
async fn confirmed_install_replaces_only_the_bundle_and_keeps_the_previous_copy() {
    let target = UpdateTarget::current().unwrap();
    let payload = archive("0.3.0");
    let mut release = make_release("0.3.0", target, &payload);
    let temp = tempfile::tempdir().unwrap();
    release.archive_path = Some(put_bytes(temp.path(), "release.tar.gz", &payload));
    release.checksums_path = Some(put_bytes(
        temp.path(),
        "SHA256SUMS",
        &release.checksum_bytes,
    ));

    let installed = temp.path().join(BUNDLE_NAME);
    fs::create_dir_all(installed.join("Contents/MacOS")).unwrap();
    fs::write(installed.join("Contents/Info.plist"), bundle_plist("0.2.0")).unwrap();
    fs::write(
        installed.join("Contents/MacOS").join(EXECUTABLE),
        b"old app",
    )
    .unwrap();
    let user_data = temp.path().join("settings.json");
    fs::write(&user_data, b"preserve me").unwrap();

    let updater = service(temp.path(), true);
    let staged = updater
        .stage_release_from_files(&release, temp.path())
        .await
        .unwrap();
    let result = updater
        .install_after_confirmation(staged, &installed, InstallConfirmation::user_confirmed())
        .unwrap();

    assert_eq!(result.version, Version::parse("0.3.0").unwrap());
    assert!(result.relaunch_requested);
    assert_eq!(
        fs::read(installed.join("Contents/MacOS").join(EXECUTABLE)).unwrap(),
        b"test executable"
    );
    assert_eq!(
        fs::read(
            result
                .previous_bundle
                .join("Contents/MacOS")
                .join(EXECUTABLE)
        )
        .unwrap(),
        b"old app"
    );
    assert_eq!(fs::read(user_data).unwrap(), b"preserve me");
}

#[tokio::test]
async fn failed_relaunch_restores_the_previous_bundle() {
    let target = UpdateTarget::current().unwrap();
    let payload = archive("0.3.0");
    let mut release = make_release("0.3.0", target, &payload);
    let temp = tempfile::tempdir().unwrap();
    release.archive_path = Some(put_bytes(temp.path(), "release.tar.gz", &payload));
    release.checksums_path = Some(put_bytes(
        temp.path(),
        "SHA256SUMS",
        &release.checksum_bytes,
    ));

    let installed = temp.path().join(BUNDLE_NAME);
    fs::create_dir_all(installed.join("Contents/MacOS")).unwrap();
    fs::write(installed.join("Contents/Info.plist"), bundle_plist("0.2.0")).unwrap();
    fs::write(
        installed.join("Contents/MacOS").join(EXECUTABLE),
        b"old app",
    )
    .unwrap();

    let stage_service = service(temp.path(), true);
    let staged = stage_service
        .stage_release_from_files(&release, temp.path())
        .await
        .unwrap();
    let installer = service_that_cannot_relaunch(true);

    let result = installer.install_after_confirmation(
        staged,
        &installed,
        InstallConfirmation::user_confirmed(),
    );

    assert!(matches!(result, Err(UpdateError::RelaunchFailed)));
    assert_eq!(
        fs::read(installed.join("Contents/MacOS").join(EXECUTABLE)).unwrap(),
        b"old app"
    );
}

#[tokio::test]
async fn failed_rollback_reports_the_retained_previous_bundle_path() {
    let target = UpdateTarget::current().unwrap();
    let payload = archive("0.3.0");
    let mut release = make_release("0.3.0", target, &payload);
    let temp = tempfile::tempdir().unwrap();
    release.archive_path = Some(put_bytes(temp.path(), "release.tar.gz", &payload));
    release.checksums_path = Some(put_bytes(
        temp.path(),
        "SHA256SUMS",
        &release.checksum_bytes,
    ));
    let installed = temp.path().join(BUNDLE_NAME);
    fs::create_dir_all(installed.join("Contents/MacOS")).unwrap();
    fs::write(installed.join("Contents/Info.plist"), bundle_plist("0.2.0")).unwrap();
    fs::write(
        installed.join("Contents/MacOS").join(EXECUTABLE),
        b"old app",
    )
    .unwrap();
    let staged = service(temp.path(), true)
        .stage_release_from_files(&release, temp.path())
        .await
        .unwrap();

    let result = service_that_cannot_rollback().install_after_confirmation(
        staged,
        &installed,
        InstallConfirmation::user_confirmed(),
    );

    let retained_bundle = match result {
        Err(UpdateError::RollbackFailed { retained_bundle }) => retained_bundle,
        other => panic!("expected the retained previous bundle path, got {other:?}"),
    };
    assert_eq!(
        fs::read(retained_bundle.join("Contents/MacOS").join(EXECUTABLE)).unwrap(),
        b"old app"
    );
    assert_eq!(
        fs::read(installed.join("Contents/MacOS").join(EXECUTABLE)).unwrap(),
        b"test executable"
    );
}

#[tokio::test]
async fn failed_atomic_swap_leaves_the_installed_bundle_unchanged() {
    let target = UpdateTarget::current().unwrap();
    let payload = archive("0.3.0");
    let mut release = make_release("0.3.0", target, &payload);
    let temp = tempfile::tempdir().unwrap();
    release.archive_path = Some(put_bytes(temp.path(), "release.tar.gz", &payload));
    release.checksums_path = Some(put_bytes(
        temp.path(),
        "SHA256SUMS",
        &release.checksum_bytes,
    ));
    let installed = temp.path().join(BUNDLE_NAME);
    fs::create_dir_all(installed.join("Contents/MacOS")).unwrap();
    fs::write(installed.join("Contents/Info.plist"), bundle_plist("0.2.0")).unwrap();
    fs::write(
        installed.join("Contents/MacOS").join(EXECUTABLE),
        b"old app",
    )
    .unwrap();

    let stage_service = service(temp.path(), true);
    let staged = stage_service
        .stage_release_from_files(&release, temp.path())
        .await
        .unwrap();
    let installer = service_with_swap_failure();
    let result = installer.install_after_confirmation(
        staged,
        &installed,
        InstallConfirmation::user_confirmed(),
    );

    assert!(matches!(result, Err(UpdateError::AtomicSwapUnavailable(_))));
    assert_eq!(
        fs::read(installed.join("Contents/MacOS").join(EXECUTABLE)).unwrap(),
        b"old app"
    );
}

#[test]
fn signing_requirement_pins_the_bundle_id_and_team_identifier() {
    assert_eq!(
        publisher_requirement("ABCDEFGHIJ"),
        format!(
            "identifier \"{BUNDLE_ID}\" and anchor apple generic and certificate 1[field.1.2.840.113635.100.6.2.6] exists and certificate leaf[field.1.2.840.113635.100.6.1.13] exists and certificate leaf[subject.OU] = \"ABCDEFGHIJ\""
        )
    );
}

#[cfg(target_os = "macos")]
#[test]
fn macos_release_script_archive_of_unsigned_bundle_has_supported_layout() {
    let temp = tempfile::tempdir().unwrap();
    let executable = temp.path().join(EXECUTABLE);
    fs::write(&executable, b"unsigned packaging fixture").unwrap();
    let package_dir = temp.path().join("package");
    fs::create_dir_all(&package_dir).unwrap();
    let app = crate::bundle::bundle(&package_dir, &executable).unwrap();
    let archive_path = temp.path().join("release.tar.gz");
    let status = std::process::Command::new("/usr/bin/tar")
        .arg("-czf")
        .arg(&archive_path)
        .arg("-C")
        .arg(&package_dir)
        .arg(BUNDLE_NAME)
        .status()
        .unwrap();
    assert!(status.success());
    let extraction_dir = temp.path().join("extract");
    fs::create_dir(&extraction_dir).unwrap();

    let extracted = extract_bundle(&archive_path, &extraction_dir).unwrap();
    inspect_bundle(&extracted, &Version::parse(CURRENT_VERSION).unwrap()).unwrap();

    assert_eq!(
        fs::read(extracted.join("Contents/Resources/AppIcon.icns")).unwrap(),
        fs::read(app.join("Contents/Resources/AppIcon.icns")).unwrap()
    );
}

#[cfg(target_os = "macos")]
#[test]
fn macos_atomic_swap_exchanges_bundles_without_a_missing_path_window() {
    let temp = tempfile::tempdir().unwrap();
    let installed = temp.path().join("installed.app");
    let candidate = temp.path().join("candidate.app");
    fs::create_dir(&installed).unwrap();
    fs::create_dir(&candidate).unwrap();
    fs::write(installed.join("marker"), b"old").unwrap();
    fs::write(candidate.join("marker"), b"new").unwrap();

    atomic_swap_paths(&installed, &candidate).unwrap();

    assert!(installed.is_dir());
    assert!(candidate.is_dir());
    assert_eq!(fs::read(installed.join("marker")).unwrap(), b"new");
    assert_eq!(fs::read(candidate.join("marker")).unwrap(), b"old");
}

#[test]
fn missing_trusted_publisher_fails_closed() {
    let service = UpdateService::with_parts(
        GitHubRepository::new("crosscopy", "localsend-gpui").unwrap(),
        None,
        Arc::new(TestVerifier { accepted: true }),
        "https://api.github.com/".to_owned(),
    );

    assert!(matches!(
        service.ensure_trusted_publisher(),
        Err(UpdateError::TrustedPublisherNotConfigured)
    ));
}
