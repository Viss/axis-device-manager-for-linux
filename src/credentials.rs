use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
struct SavedCredentials {
    username: String,
    password: String,
}

const SERVICE: &str = "org.axis-device-manager-for-linux.credentials";

fn entry(service: &str) -> Result<keyring::Entry, String> {
    keyring::Entry::new(service, "default").map_err(|e| e.to_string())
}

fn decode(value: &str) -> Result<(String, String), String> {
    let saved: SavedCredentials = serde_json::from_str(value).map_err(|e| e.to_string())?;
    Ok((saved.username, saved.password))
}

pub fn load() -> Result<Option<(String, String)>, String> {
    match entry(SERVICE)?.get_password() {
        Ok(value) => decode(&value).map(Some),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(error) => Err(error.to_string()),
    }
}

pub fn save(username: &str, password: &str) -> Result<(), String> {
    let value = serde_json::to_string(&SavedCredentials {
        username: username.to_owned(),
        password: password.to_owned(),
    })
    .map_err(|e| e.to_string())?;
    entry(SERVICE)?
        .set_password(&value)
        .map_err(|e| e.to_string())
}

pub fn clear() -> Result<(), String> {
    match entry(SERVICE)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(error) => Err(error.to_string()),
    }
}
