use reqwest::blocking::{Client, RequestBuilder, Response};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fs,
    io::{self, Read},
    path::Path,
    sync::Arc,
    time::Duration,
};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DeviceInfo {
    pub product_name: Option<String>,
    pub product_number: Option<String>,
    pub firmware_version: Option<String>,
    pub serial_number: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct UpgradeResult {
    pub firmware_version: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct FirmwareStatus {
    pub active_version: Option<String>,
    pub inactive_version: Option<String>,
    pub committed: Option<bool>,
    pub pending_commit: Option<String>,
    pub time_to_rollback: Option<u64>,
}

struct ProgressReader {
    bytes: Arc<Vec<u8>>,
    position: usize,
    progress: Arc<dyn Fn(u64, u64) + Send + Sync>,
}

impl Read for ProgressReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let remaining = &self.bytes[self.position..];
        let count = remaining.len().min(buffer.len());
        buffer[..count].copy_from_slice(&remaining[..count]);
        self.position += count;
        (self.progress)(self.position as u64, self.bytes.len() as u64);
        Ok(count)
    }
}

fn client() -> Result<Client, String> {
    Client::builder()
        .danger_accept_invalid_certs(true)
        .connect_timeout(Duration::from_secs(8))
        .timeout(Duration::from_secs(900))
        .build()
        .map_err(|e| e.to_string())
}

fn parse_digest(header: &str) -> HashMap<String, String> {
    let text = header.trim().strip_prefix("Digest ").unwrap_or(header);
    text.split(',')
        .filter_map(|part| {
            let (k, v) = part.trim().split_once('=')?;
            Some((
                k.trim().to_lowercase(),
                v.trim().trim_matches('"').to_owned(),
            ))
        })
        .collect()
}

fn digest_header(
    challenge: &str,
    method: &str,
    uri: &str,
    user: &str,
    pass: &str,
) -> Result<String, String> {
    let p = parse_digest(challenge);
    let realm = p.get("realm").ok_or("Digest challenge has no realm")?;
    let nonce = p.get("nonce").ok_or("Digest challenge has no nonce")?;
    let qop = p
        .get("qop")
        .and_then(|v| v.split(',').find(|q| q.trim() == "auth"))
        .map(str::trim);
    let algorithm = p.get("algorithm").map(String::as_str).unwrap_or("MD5");
    let hash = |value: &str| -> Result<String, String> {
        if algorithm.eq_ignore_ascii_case("MD5") {
            Ok(format!("{:x}", md5::compute(value)))
        } else if algorithm.eq_ignore_ascii_case("SHA-256") {
            Ok(format!("{:x}", Sha256::digest(value.as_bytes())))
        } else {
            Err(format!("Unsupported digest algorithm: {algorithm}"))
        }
    };
    if !algorithm.eq_ignore_ascii_case("MD5") && !algorithm.eq_ignore_ascii_case("SHA-256") {
        return Err(format!("Unsupported digest algorithm: {algorithm}"));
    }
    let cnonce = format!("{:016x}", rand::random::<u64>());
    let nc = "00000001";
    let ha1 = hash(&format!("{user}:{realm}:{pass}"))?;
    let ha2 = hash(&format!("{method}:{uri}"))?;
    let response = if let Some(qop) = qop {
        hash(&format!("{ha1}:{nonce}:{nc}:{cnonce}:{qop}:{ha2}"))?
    } else {
        hash(&format!("{ha1}:{nonce}:{ha2}"))?
    };
    let mut out = format!(
        r#"Digest username="{user}", realm="{realm}", nonce="{nonce}", uri="{uri}", response="{response}""#
    );
    if let Some(opaque) = p.get("opaque") {
        out.push_str(&format!(r#", opaque="{opaque}""#));
    }
    if let Some(qop) = qop {
        out.push_str(&format!(r#", qop={qop}, nc={nc}, cnonce="{cnonce}""#));
    }
    Ok(out)
}

fn send_with_auth<F>(
    _client: &Client,
    method: &str,
    url: &str,
    user: &str,
    pass: &str,
    build: F,
) -> Result<Response, String>
where
    F: Fn() -> RequestBuilder,
{
    let first = build().send().map_err(|e| {
        crate::logging::log("ERROR", &format!("{method} {url} transport failure: {e}"));
        e.to_string()
    })?;
    crate::logging::log(
        "DEBUG",
        &format!("{method} {url} initial HTTP {}", first.status()),
    );
    if first.status().as_u16() != 401 {
        return Ok(first);
    }
    let challenge = first
        .headers()
        .get(reqwest::header::WWW_AUTHENTICATE)
        .and_then(|h| h.to_str().ok())
        .ok_or("Device requested authentication without a supported challenge")?;
    let parsed = url::Url::parse(url).map_err(|e| e.to_string())?;
    let mut uri = parsed.path().to_owned();
    if let Some(q) = parsed.query() {
        uri.push('?');
        uri.push_str(q);
    }
    let auth = if challenge.trim_start().starts_with("Digest") {
        digest_header(challenge, method, &uri, user, pass)?
    } else if challenge.trim_start().starts_with("Basic") {
        return build()
            .basic_auth(user, Some(pass))
            .send()
            .map_err(|e| e.to_string());
    } else {
        return Err("Device uses an unsupported authentication scheme".into());
    };
    let response = build()
        .header(reqwest::header::AUTHORIZATION, auth)
        .send()
        .map_err(|e| e.to_string())?;
    crate::logging::log(
        "DEBUG",
        &format!("{method} {url} authenticated HTTP {}", response.status()),
    );
    Ok(response)
}

pub fn inspect(base: &str, user: &str, pass: &str) -> Result<DeviceInfo, String> {
    crate::logging::log("INFO", &format!("inventory started for {base}"));
    let client = client()?;
    let url = format!(
        "{}/axis-cgi/basicdeviceinfo.cgi",
        base.trim_end_matches('/')
    );
    let payload = r#"{"apiVersion":"1.0","context":"axis-device-manager-for-linux","method":"getAllProperties"}"#;
    let response = send_with_auth(&client, "POST", &url, user, pass, || {
        client
            .post(&url)
            .header("Content-Type", "application/json")
            .body(payload.to_owned())
    })?;
    let status = response.status();
    let text = response.text().map_err(|e| e.to_string())?;
    if !status.is_success() {
        let error = format!(
            "HTTP {status}: {}",
            text.chars().take(160).collect::<String>()
        );
        crate::logging::log(
            "WARN",
            &format!("modern inventory {base}: {error}; trying legacy VAPIX"),
        );
        return inspect_legacy(base, user, pass)
            .map_err(|legacy| format!("{error}; legacy inventory: {legacy}"));
    }
    let value: Value = match serde_json::from_str(&text) {
        Ok(value) => value,
        Err(error) => {
            crate::logging::log(
                "WARN",
                &format!(
                    "modern inventory {base} returned invalid JSON: {error}; trying legacy VAPIX"
                ),
            );
            return inspect_legacy(base, user, pass).map_err(|legacy| {
                format!("Invalid modern response: {error}; legacy inventory: {legacy}")
            });
        }
    };
    if let Some(error) = value.get("error") {
        return Err(error.to_string());
    }
    let props = value
        .pointer("/data/propertyList")
        .or_else(|| value.pointer("/data"))
        .unwrap_or(&value);
    let pick = |names: &[&str]| -> Option<String> {
        names
            .iter()
            .find_map(|n| props.get(*n).and_then(Value::as_str).map(str::to_owned))
    };
    let info = DeviceInfo {
        product_name: pick(&["ProdFullName", "ProdNbr", "ProductName"]),
        product_number: pick(&["ProdNbr"]),
        firmware_version: pick(&["Version", "FirmwareVersion"]),
        serial_number: pick(&["SerialNumber", "Serial"]),
    };
    if info.product_name.is_none() && info.product_number.is_none() {
        crate::logging::log(
            "WARN",
            &format!("modern inventory {base} returned no product identity; trying legacy VAPIX"),
        );
        return inspect_legacy(base, user, pass);
    }
    crate::logging::log(
        "INFO",
        &format!("inventory completed for {base}: {:?}", info),
    );
    Ok(info)
}

fn inspect_legacy(base: &str, user: &str, pass: &str) -> Result<DeviceInfo, String> {
    let client = client()?;
    let url = format!(
        "{}/axis-cgi/param.cgi?action=list&group=Properties.System&group=Properties.Firmware",
        base.trim_end_matches('/')
    );
    let response = send_with_auth(&client, "GET", &url, user, pass, || client.get(&url))?;
    let status = response.status();
    let text = response.text().map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(format!(
            "HTTP {status}: {}",
            text.chars().take(160).collect::<String>()
        ));
    }
    let values: HashMap<_, _> = text
        .lines()
        .filter_map(|line| line.split_once('='))
        .map(|(key, value)| (key.trim().to_owned(), value.trim().to_owned()))
        .collect();
    let find = |suffixes: &[&str]| {
        suffixes.iter().find_map(|suffix| {
            values
                .iter()
                .find(|(key, _)| key.ends_with(suffix))
                .map(|(_, value)| value.clone())
        })
    };
    let info = DeviceInfo {
        product_name: find(&[
            "Properties.System.ProdFullName",
            "Properties.System.ProductName",
            "Properties.System.ProdNbr",
        ]),
        product_number: find(&["Properties.System.ProdNbr"]),
        firmware_version: find(&[
            "Properties.Firmware.Version",
            "Properties.System.FirmwareVersion",
        ]),
        serial_number: find(&["Properties.System.SerialNumber"]),
    };
    if info.product_name.is_none() && info.product_number.is_none() {
        return Err("legacy VAPIX returned no Axis product identity".into());
    }
    crate::logging::log(
        "INFO",
        &format!("legacy inventory completed for {base}: {:?}", info),
    );
    Ok(info)
}

pub fn upgrade(
    base: &str,
    user: &str,
    pass: &str,
    path: &Path,
    progress: impl Fn(u64, u64) + Send + Sync + 'static,
) -> Result<UpgradeResult, String> {
    crate::logging::log(
        "WARN",
        &format!("safe firmware upgrade requested for {base}; settings will be preserved"),
    );
    if path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("bin"))
        != Some(true)
    {
        return Err("Firmware must be an Axis OS .bin file".into());
    }
    let metadata = fs::metadata(path).map_err(|e| e.to_string())?;
    if metadata.len() < 1024 * 1024 {
        return Err("Firmware file is unexpectedly small".into());
    }
    let bytes = Arc::new(fs::read(path).map_err(|e| e.to_string())?);
    let filename = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("firmware.bin")
        .to_owned();
    let client = client()?;
    let url = format!(
        "{}/axis-cgi/firmwaremanagement.cgi",
        base.trim_end_matches('/')
    );
    let json = upgrade_payload();
    let challenge_response = client
        .post(&url)
        .header("Content-Type", "application/json")
        .body(r#"{"apiVersion":"1.0","context":"axis-device-manager-auth","method":"status"}"#)
        .send()
        .map_err(|e| e.to_string())?;
    let challenge = if challenge_response.status().as_u16() == 401 {
        challenge_response
            .headers()
            .get(reqwest::header::WWW_AUTHENTICATE)
            .and_then(|header| header.to_str().ok())
            .map(str::to_owned)
    } else {
        None
    };
    let progress: Arc<dyn Fn(u64, u64) + Send + Sync> = Arc::new(progress);
    let reader = ProgressReader {
        bytes: bytes.clone(),
        position: 0,
        progress,
    };
    let form = reqwest::blocking::multipart::Form::new()
        .part(
            "json",
            reqwest::blocking::multipart::Part::text(json)
                .mime_str("application/json")
                .unwrap(),
        )
        .part(
            "file",
            reqwest::blocking::multipart::Part::reader_with_length(reader, bytes.len() as u64)
                .file_name(filename)
                .mime_str("application/octet-stream")
                .unwrap(),
        );
    let mut request = client.post(&url).multipart(form);
    if let Some(challenge) = challenge {
        if challenge.trim_start().starts_with("Digest") {
            let parsed = url::Url::parse(&url).map_err(|e| e.to_string())?;
            let auth = digest_header(&challenge, "POST", parsed.path(), user, pass)?;
            request = request.header(reqwest::header::AUTHORIZATION, auth);
        } else if challenge.trim_start().starts_with("Basic") {
            request = request.basic_auth(user, Some(pass));
        } else {
            return Err("Device uses an unsupported authentication scheme".into());
        }
    }
    let response = request.send().map_err(|e| e.to_string())?;
    let status = response.status();
    let text = response.text().map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(format!(
            "HTTP {status}: {}",
            text.chars().take(300).collect::<String>()
        ));
    }
    let value: Value = serde_json::from_str(&text).map_err(|e| {
        format!(
            "Invalid upgrade response: {e}; {}",
            text.chars().take(160).collect::<String>()
        )
    })?;
    if let Some(error) = value.get("error") {
        return Err(error.to_string());
    }
    Ok(UpgradeResult {
        firmware_version: value
            .pointer("/data/firmwareVersion")
            .and_then(Value::as_str)
            .map(str::to_owned),
    })
}

fn upgrade_payload() -> String {
    r#"{"apiVersion":"1.0","context":"axis-device-manager-for-linux","method":"upgrade","params":{"factoryDefaultMode":"none","autoCommit":"default","autoRollback":"default"}}"#.to_owned()
}

pub fn firmware_status(base: &str, user: &str, pass: &str) -> Result<FirmwareStatus, String> {
    let client = client()?;
    let url = format!(
        "{}/axis-cgi/firmwaremanagement.cgi",
        base.trim_end_matches('/')
    );
    let payload =
        r#"{"apiVersion":"1.0","context":"axis-device-manager-preflight","method":"status"}"#;
    let response = send_with_auth(&client, "POST", &url, user, pass, || {
        client
            .post(&url)
            .header("Content-Type", "application/json")
            .body(payload.to_owned())
    })?;
    let status = response.status();
    let text = response.text().map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(format!(
            "Firmware safety preflight failed with HTTP {status}: {}",
            text.chars().take(300).collect::<String>()
        ));
    }
    let value: Value = serde_json::from_str(&text)
        .map_err(|e| format!("Invalid firmware status response: {e}"))?;
    if let Some(error) = value.get("error") {
        return Err(format!("Firmware safety preflight rejected: {error}"));
    }
    let committed = value.pointer("/data/isCommited").and_then(Value::as_bool);
    Ok(FirmwareStatus {
        active_version: value
            .pointer("/data/activeFirmwareVersion")
            .and_then(Value::as_str)
            .map(str::to_owned),
        inactive_version: value
            .pointer("/data/inactiveFirmwareVersion")
            .and_then(Value::as_str)
            .filter(|version| !version.eq_ignore_ascii_case("UNKNOWN"))
            .map(str::to_owned),
        committed,
        pending_commit: value
            .pointer("/data/pendingCommit")
            .and_then(Value::as_str)
            .map(str::to_owned),
        time_to_rollback: value
            .pointer("/data/timeToRollback")
            .and_then(Value::as_u64),
    })
}

pub fn rollback(base: &str, user: &str, pass: &str) -> Result<(), String> {
    let client = client()?;
    let url = format!(
        "{}/axis-cgi/firmwaremanagement.cgi",
        base.trim_end_matches('/')
    );
    let payload = rollback_payload();
    crate::logging::log("WARN", &format!("firmware rollback requested for {base}"));
    let response = send_with_auth(&client, "POST", &url, user, pass, || {
        client
            .post(&url)
            .header("Content-Type", "application/json")
            .body(payload.to_owned())
    })?;
    let status = response.status();
    let text = response.text().map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(format!(
            "Rollback failed with HTTP {status}: {}",
            text.chars().take(300).collect::<String>()
        ));
    }
    let value: Value =
        serde_json::from_str(&text).map_err(|e| format!("Invalid rollback response: {e}"))?;
    if let Some(error) = value.get("error") {
        return Err(format!("Rollback rejected: {error}"));
    }
    Ok(())
}

fn rollback_payload() -> String {
    r#"{"apiVersion":"1.0","context":"axis-device-manager-rollback","method":"rollback"}"#
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    #[test]
    fn parses_digest_challenge() {
        let h = digest_header(
            r#"Digest realm="AXIS", nonce="abc", qop="auth", opaque="xyz""#,
            "GET",
            "/test",
            "root",
            "pw",
        )
        .unwrap();
        assert!(h.contains("username=\"root\""));
        assert!(h.contains("qop=auth"));
    }

    #[test]
    fn safe_upgrade_payload_preserves_settings_and_enables_recovery() {
        let payload = upgrade_payload();
        assert!(payload.contains(r#""factoryDefaultMode":"none""#));
        assert!(payload.contains(r#""autoCommit":"default""#));
        assert!(payload.contains(r#""autoRollback":"default""#));
        assert!(!payload.contains("hard"));
        assert!(!payload.contains("soft"));
    }

    #[test]
    fn rollback_payload_only_requests_rollback() {
        let value: Value = serde_json::from_str(&rollback_payload()).unwrap();
        assert_eq!(
            value.get("method").and_then(Value::as_str),
            Some("rollback")
        );
        assert!(value.get("params").is_none());
        assert!(!rollback_payload().contains("factoryDefault"));
        assert!(!rollback_payload().contains("purge"));
        assert!(!rollback_payload().contains("commit"));
    }

    #[test]
    fn upload_reader_reports_all_transferred_bytes() {
        let reported = Arc::new(AtomicU64::new(0));
        let report_target = reported.clone();
        let mut reader = ProgressReader {
            bytes: Arc::new(vec![42; 32 * 1024]),
            position: 0,
            progress: Arc::new(move |sent, _| {
                report_target.store(sent, Ordering::Relaxed);
            }),
        };
        let mut received = Vec::new();
        reader.read_to_end(&mut received).unwrap();
        assert_eq!(received.len(), 32 * 1024);
        assert_eq!(reported.load(Ordering::Relaxed), 32 * 1024);
    }
}
