use scraper::{Html, Selector};
use sha2::{Digest, Sha256};
use std::{fs, io::Read, io::Write, path::PathBuf, time::Duration};

#[derive(Clone, Debug)]
pub struct FirmwareRelease {
    pub version: String,
    pub track: String,
    pub url: String,
    pub sha256: String,
}

fn cache_directory() -> PathBuf {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".cache")))
        .unwrap_or_else(std::env::temp_dir);
    base.join("axis-device-manager-for-linux/firmware")
}

fn cached_path(release: &FirmwareRelease) -> PathBuf {
    let name = release.url.rsplit('/').next().unwrap_or("axis-os.bin");
    cache_directory().join(name)
}

pub fn is_cached_verified(release: &FirmwareRelease) -> bool {
    fs::read(cached_path(release))
        .map(|bytes| format!("{:x}", Sha256::digest(&bytes)) == release.sha256)
        .unwrap_or(false)
}

fn canonical_model(value: &str) -> String {
    let mut value = value.trim().to_ascii_uppercase();
    for suffix in [
        " NETWORK CAMERA",
        " NETWORK VIDEO ENCODER",
        " VIDEO DOOR STATION",
        " CAMERA",
    ] {
        if value.ends_with(suffix) {
            value.truncate(value.len() - suffix.len());
        }
    }
    if !value.starts_with("AXIS ") {
        value = format!("AXIS {value}");
    }
    value.trim().to_owned()
}

fn text(element: scraper::ElementRef<'_>) -> String {
    element
        .text()
        .collect::<Vec<_>>()
        .join(" ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn artifact_url(href: &str) -> String {
    if let Some(path) = href.strip_prefix("/ftp/") {
        format!("https://ftp.axis.com/{path}")
    } else if href.starts_with("https://") || href.starts_with("http://") {
        href.to_owned()
    } else {
        format!("https://www.axis.com{href}")
    }
}

pub fn fetch(model: &str) -> Result<Vec<FirmwareRelease>, String> {
    let search_model = canonical_model(model);
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| e.to_string())?;
    let html = client
        .get("https://www.axis.com/support/device-software")
        .query(&[("name", &search_model)])
        .send()
        .map_err(|e| e.to_string())?
        .error_for_status()
        .map_err(|e| e.to_string())?
        .text()
        .map_err(|e| e.to_string())?;
    let document = Html::parse_document(&html);
    let row_sel = Selector::parse(".views-row").unwrap();
    let product_sel =
        Selector::parse(".firmware-listing__product .slim-link-cta__underline").unwrap();
    let version_sel = Selector::parse(".firmware-listing__version span").unwrap();
    let download_sel = Selector::parse(".firmware-listing__download a[href$='.bin']").unwrap();
    let hash_sel = Selector::parse(".support-firmware__checksum-hash").unwrap();
    let wanted = canonical_model(model);
    let mut releases = Vec::new();
    for row in document.select(&row_sel) {
        let Some(product) = row.select(&product_sel).next().map(text) else {
            continue;
        };
        if canonical_model(&product) != wanted {
            continue;
        }
        let Some(version_text) = row.select(&version_sel).next().map(text) else {
            continue;
        };
        let Some(href) = row
            .select(&download_sel)
            .next()
            .and_then(|e| e.value().attr("href"))
        else {
            continue;
        };
        let Some(sha256) = row.select(&hash_sel).next().map(text) else {
            continue;
        };
        let (version, track) = version_text
            .split_once('-')
            .map(|(v, t)| (v.trim(), t.trim()))
            .unwrap_or((version_text.trim(), "AXIS OS"));
        releases.push(FirmwareRelease {
            version: version.to_owned(),
            track: track.to_owned(),
            url: artifact_url(href),
            sha256: sha256.to_ascii_lowercase(),
        });
    }
    if releases.is_empty() {
        Err(format!("No exact catalog match for {model}"))
    } else {
        Ok(releases)
    }
}

pub fn check_download_host() -> Result<(), String> {
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(8))
        .build()
        .map_err(|error| error.to_string())?
        .head("https://ftp.axis.com/pub/axis/software/")
        .send()
        .map_err(|error| error.to_string())?
        .error_for_status()
        .map(|_| ())
        .map_err(|error| error.to_string())
}

pub fn download_with_progress(
    release: &FirmwareRelease,
    mut progress: impl FnMut(u64, Option<u64>),
) -> Result<PathBuf, String> {
    crate::logging::log(
        "INFO",
        &format!(
            "downloading AXIS OS {} from {}",
            release.version, release.url
        ),
    );
    let cache = cache_directory();
    fs::create_dir_all(&cache).map_err(|e| e.to_string())?;
    let name = release.url.rsplit('/').next().unwrap_or("axis-os.bin");
    let path = cached_path(release);
    if let Ok(existing) = fs::read(&path) {
        let actual = format!("{:x}", Sha256::digest(&existing));
        if actual == release.sha256 {
            progress(existing.len() as u64, Some(existing.len() as u64));
            crate::logging::log(
                "INFO",
                &format!("reusing verified cached AXIS OS {}", release.version),
            );
            return Ok(path);
        }
    }
    let mut response = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(600))
        .build()
        .map_err(|e| e.to_string())?
        .get(&release.url)
        .send()
        .map_err(|e| format!("Firmware download from {} failed: {e}", release.url))?
        .error_for_status()
        .map_err(|e| format!("Firmware download from {} failed: {e}", release.url))?;
    let total = response.content_length();
    let temporary = cache.join(format!(".{name}.{:016x}.part", rand::random::<u64>()));
    let result = (|| {
        let mut file = fs::File::create(&temporary).map_err(|e| e.to_string())?;
        let mut hasher = Sha256::new();
        let mut downloaded = 0_u64;
        let mut buffer = vec![0_u8; 128 * 1024];
        loop {
            let count = response.read(&mut buffer).map_err(|e| e.to_string())?;
            if count == 0 {
                break;
            }
            file.write_all(&buffer[..count])
                .map_err(|e| e.to_string())?;
            hasher.update(&buffer[..count]);
            downloaded += count as u64;
            progress(downloaded, total);
        }
        file.flush().map_err(|e| e.to_string())?;
        Ok::<String, String>(format!("{:x}", hasher.finalize()))
    })();
    let actual = match result {
        Ok(actual) => actual,
        Err(error) => {
            let _ = fs::remove_file(&temporary);
            return Err(error);
        }
    };
    if actual != release.sha256 {
        let _ = fs::remove_file(&temporary);
        return Err(format!(
            "Axis download checksum mismatch: expected {}, got {actual}",
            release.sha256
        ));
    }
    fs::rename(&temporary, &path).map_err(|error| {
        let _ = fs::remove_file(&temporary);
        format!("Could not publish verified firmware cache file: {error}")
    })?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uses_public_axis_artifact_host() {
        assert_eq!(
            artifact_url("/ftp/pub/axis/software/camera.bin"),
            "https://ftp.axis.com/pub/axis/software/camera.bin"
        );
    }

    #[test]
    #[ignore = "uses the live official Axis artifact host"]
    fn reaches_axis_artifact_host() {
        check_download_host().unwrap();
    }

    #[test]
    #[ignore = "uses the live official Axis catalog"]
    fn finds_exact_axis_model() {
        let releases = fetch("AXIS P3245-LVE").unwrap();
        assert!(!releases.is_empty());
        assert!(releases.iter().all(|release| release.url.ends_with(".bin")));
        assert!(releases.iter().all(|release| release.sha256.len() == 64));
    }

    #[test]
    #[ignore = "uses the live official Axis catalog"]
    fn finds_catalog_from_product_number() {
        assert!(!fetch("P3245-LVE").unwrap().is_empty());
    }
}
