use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::PathBuf,
    sync::{Mutex, OnceLock},
    time::{SystemTime, UNIX_EPOCH},
};

static LOG_FILE: OnceLock<Mutex<Option<std::fs::File>>> = OnceLock::new();
static LOG_PATH: OnceLock<PathBuf> = OnceLock::new();
static CAMERA_PASSWORD: OnceLock<Mutex<String>> = OnceLock::new();

pub fn set_camera_password(password: &str) {
    if let Ok(mut current) = CAMERA_PASSWORD
        .get_or_init(|| Mutex::new(String::new()))
        .lock()
    {
        *current = password.to_owned();
    }
}

fn sanitize(message: &str) -> String {
    let mut clean = message.replace(['\r', '\n'], " ");
    if let Ok(password) = CAMERA_PASSWORD
        .get_or_init(|| Mutex::new(String::new()))
        .lock()
    {
        if !password.is_empty() {
            clean = clean.replace(password.as_str(), "[REDACTED]");
        }
    }
    // Remove URL user-info (including percent-encoded passwords) before a URL
    // reaches disk, even if a user pasted credentials into the address field.
    if let Ok(pattern) = regex::Regex::new(r"(?i)(https?://)[^/@\s]+@") {
        clean = pattern.replace_all(&clean, "$1[REDACTED]@").into_owned();
    }
    // Defense in depth for common structured credential/header spellings.
    if let Ok(pattern) = regex::Regex::new(
        r#"(?i)(password|passwd|pwd|authorization|proxy-authorization|token)(\s*[:=]\s*)(\"[^\"]*\"|[^,;\s}]+)"#,
    ) {
        clean = pattern.replace_all(&clean, "$1$2[REDACTED]").into_owned();
    }
    clean
}

fn appimage_directory() -> Option<PathBuf> {
    let location = std::env::var_os("APPIMAGE")
        .map(PathBuf::from)
        .or_else(|| {
            let launched_as = PathBuf::from(std::env::args_os().next()?);
            let is_appimage = launched_as
                .file_name()
                .and_then(|name| name.to_str())
                .map(|name| name.to_ascii_lowercase().ends_with(".appimage"))
                .unwrap_or(false);
            is_appimage.then_some(launched_as)
        })?;
    let absolute = if location.is_absolute() {
        location
    } else {
        std::env::current_dir().ok()?.join(location)
    };
    absolute.parent().map(PathBuf::from)
}

pub fn path() -> PathBuf {
    if let Some(path) = LOG_PATH.get() {
        return path.clone();
    }
    if let Some(parent) = appimage_directory() {
        return parent.join("axis-device-manager-for-linux.log");
    }
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".local/state")))
        .unwrap_or_else(std::env::temp_dir);
    base.join("axis-device-manager-for-linux/axis-device-manager-for-linux.log")
}

pub fn init() {
    let preferred = path();
    let fallback = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".local/state")))
        .unwrap_or_else(std::env::temp_dir)
        .join("axis-device-manager-for-linux/axis-device-manager-for-linux.log");
    let mut opened = None;
    let mut chosen = preferred.clone();
    for candidate in [preferred, fallback] {
        if let Some(parent) = candidate.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Ok(file) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&candidate)
        {
            opened = Some(file);
            chosen = candidate;
            break;
        }
    }
    let _ = LOG_PATH.set(chosen);
    let _ = LOG_FILE.set(Mutex::new(opened));
    log(
        "INFO",
        &format!(
            "Axis Device Manager for Linux {} started",
            env!("CARGO_PKG_VERSION")
        ),
    );
}

pub fn available() -> bool {
    LOG_FILE
        .get()
        .and_then(|lock| lock.lock().ok())
        .map(|guard| guard.is_some())
        .unwrap_or(false)
}

pub fn log(level: &str, message: &str) {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let clean = sanitize(message);
    if let Some(lock) = LOG_FILE.get() {
        if let Ok(mut guard) = lock.lock() {
            if let Some(file) = guard.as_mut() {
                let _ = writeln!(file, "{seconds} {level} {clean}");
                let _ = file.flush();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_camera_credentials() {
        set_camera_password("super-secret");
        let clean = sanitize(
            r#"POST http://root:other-secret@10.0.0.2 password=super-secret Authorization: DigestStuff"#,
        );
        assert!(!clean.contains("super-secret"));
        assert!(!clean.contains("other-secret"));
        assert!(!clean.contains("DigestStuff"));
        assert!(clean.contains("[REDACTED]"));
    }
}
