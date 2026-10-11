use anyhow::Context as _;
use http_client::HttpClient;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The Chrome for Testing build the browser tool provisions when no Chromium
/// is already present. Pinned so a given Zed build reproduces the same browser
/// behavior.
const PINNED_CHROMIUM_VERSION: &str = "153.0.8010.47";

const CHROME_FOR_TESTING_BASE_URL: &str =
    "https://storage.googleapis.com/chrome-for-testing-public";

/// Resolve a Chromium binary for launching a browser session.
///
/// Resolution order: the configured override, the `ZED_BROWSER_CHROMIUM_PATH`
/// environment variable, an installed Chrome/Edge, then a pinned Chrome for
/// Testing download cached under the user cache directory.
///
/// The download is fetched over HTTPS from Google's Chrome for Testing bucket;
/// the pinned version plus TLS transport provide integrity, so no separate
/// checksum is verified (the Chrome for Testing manifest does not publish a
/// SHA-256 for the archive).
pub async fn resolve_chromium_binary(
    http_client: &Arc<dyn HttpClient>,
    override_path: Option<&Path>,
) -> anyhow::Result<PathBuf> {
    if let Some(path) = override_path {
        return Ok(path.to_path_buf());
    }
    if let Some(path) = discover_system_chromium() {
        return Ok(path);
    }
    ensure_pinned_chromium(http_client).await
}

fn discover_system_chromium() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("ZED_BROWSER_CHROMIUM_PATH") {
        return Some(PathBuf::from(path));
    }
    let candidates = [
        r"C:\Program Files\Google\Chrome\Application\chrome.exe",
        r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe",
        r"C:\Program Files\Microsoft\Edge\Application\msedge.exe",
    ];
    for candidate in candidates {
        let path = Path::new(candidate);
        if path.exists() {
            return Some(path.to_path_buf());
        }
    }
    None
}

struct ChromiumPlatform {
    key: &'static str,
    archive_dir: &'static str,
    binary_rel: &'static str,
}

fn current_platform() -> anyhow::Result<ChromiumPlatform> {
    if cfg!(target_os = "windows") {
        Ok(ChromiumPlatform {
            key: "win64",
            archive_dir: "chrome-win64",
            binary_rel: "chrome.exe",
        })
    } else if cfg!(target_os = "macos") {
        if cfg!(target_arch = "aarch64") {
            Ok(ChromiumPlatform {
                key: "mac-arm64",
                archive_dir: "chrome-mac-arm64",
                binary_rel: "Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing",
            })
        } else {
            Ok(ChromiumPlatform {
                key: "mac-x64",
                archive_dir: "chrome-mac-x64",
                binary_rel: "Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing",
            })
        }
    } else if cfg!(target_os = "linux") {
        Ok(ChromiumPlatform {
            key: "linux64",
            archive_dir: "chrome-linux64",
            binary_rel: "chrome",
        })
    } else {
        anyhow::bail!("automatic Chromium provisioning is not supported on this platform")
    }
}

async fn ensure_pinned_chromium(http_client: &Arc<dyn HttpClient>) -> anyhow::Result<PathBuf> {
    let platform = current_platform()?;
    let version_dir = chromium_cache_dir()?.join(PINNED_CHROMIUM_VERSION);
    let binary_path = version_dir
        .join(platform.archive_dir)
        .join(platform.binary_rel);
    if binary_path.exists() {
        return Ok(binary_path);
    }

    let url = format!(
        "{CHROME_FOR_TESTING_BASE_URL}/{PINNED_CHROMIUM_VERSION}/{}/{}.zip",
        platform.key, platform.archive_dir
    );
    download_and_extract(http_client, &url, &version_dir).await?;
    util::fs::make_file_executable(&binary_path)
        .await
        .with_context(|| format!("marking {binary_path:?} as executable"))?;
    Ok(binary_path)
}

fn chromium_cache_dir() -> anyhow::Result<PathBuf> {
    dirs::cache_dir()
        .map(|dir| dir.join("zed-browser"))
        .context("could not determine a cache directory for Chromium")
}

async fn download_and_extract(
    http_client: &Arc<dyn HttpClient>,
    url: &str,
    destination: &Path,
) -> anyhow::Result<()> {
    let destination_parent = destination
        .parent()
        .context("Chromium cache destination has no parent")?;
    async_fs::create_dir_all(destination_parent)
        .await
        .with_context(|| format!("creating Chromium cache directory {destination_parent:?}"))?;

    let staging = tempfile::Builder::new()
        .prefix(".tmp-zed-chromium-")
        .tempdir_in(destination_parent)
        .with_context(|| format!("creating Chromium staging directory in {destination_parent:?}"))?
        .keep();

    let result = extract_chromium_archive(http_client, url, &staging).await;
    if let Err(err) = result {
        let _ = async_fs::remove_dir_all(&staging).await;
        return Err(err);
    }
    if let Err(err) = finalize_download(&staging, destination).await {
        let _ = async_fs::remove_dir_all(&staging).await;
        return Err(err);
    }

    Ok(())
}

async fn extract_chromium_archive(
    http_client: &Arc<dyn HttpClient>,
    url: &str,
    destination: &Path,
) -> anyhow::Result<()> {
    log::info!("downloading pinned Chromium from {url}");
    let mut response = http_client
        .get(url, Default::default(), true)
        .await
        .with_context(|| format!("downloading Chromium from {url}"))?;
    util::archive::extract_zip(destination, response.body_mut())
        .await
        .with_context(|| format!("extracting Chromium archive into {destination:?}"))?;
    Ok(())
}

async fn finalize_download(staging_path: &Path, destination_path: &Path) -> anyhow::Result<()> {
    if let Err(err) = async_fs::remove_dir_all(destination_path).await {
        if err.kind() != std::io::ErrorKind::NotFound {
            log::warn!(
                "failed to remove existing Chromium destination {destination_path:?}: {err:?}"
            );
        }
    }
    async_fs::rename(staging_path, destination_path)
        .await
        .with_context(|| format!("renaming {staging_path:?} to {destination_path:?}"))?;
    Ok(())
}
