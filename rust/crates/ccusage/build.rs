use std::{env, fs, path::PathBuf};

use serde_json::{Map, Value};

const FLAKE_LOCK_JSON: &str = "../../../flake.lock";
const PACKAGE_JSON: &str = "../../../package.json";
const LITELLM_PRICING_JSON: &str = "model_prices_and_context_window.json";
const OUT_PRICING_JSON: &str = "litellm-pricing.json";
const PRICING_JSON_PATH_ENV: &str = "CCUSAGE_PRICING_JSON_PATH";
const VERSION_ENV: &str = "CCUSAGE_VERSION";
const PRICING_FETCH_TIMEOUT_SECONDS: u64 = 10;

fn main() {
    println!("cargo:rerun-if-env-changed={PRICING_JSON_PATH_ENV}");
    println!("cargo:rerun-if-env-changed={VERSION_ENV}");
    println!("cargo:rerun-if-changed={PACKAGE_JSON}");
    let version = env::var(VERSION_ENV).unwrap_or_else(|_| {
        let package_json = fs::read_to_string(PACKAGE_JSON).expect("read root package.json");
        serde_json::from_str::<Value>(&package_json)
            .ok()
            .and_then(|package| package.get("version")?.as_str().map(str::to_owned))
            .expect("read version from root package.json")
    });
    println!("cargo:rustc-env={VERSION_ENV}={version}");

    let out_path = out_dir_path(OUT_PRICING_JSON);
    let pricing_json = if let Some(path) = env::var_os(PRICING_JSON_PATH_ENV) {
        let path = PathBuf::from(path);
        println!("cargo:rerun-if-changed={}", path.display());
        fs::read_to_string(path).expect("read pricing snapshot from CCUSAGE_PRICING_JSON_PATH")
    } else {
        println!("cargo:rerun-if-changed={FLAKE_LOCK_JSON}");
        fetch_pricing_json().expect("fetch LiteLLM pricing for embed")
    };
    let pricing_json = compact_litellm(&pricing_json, false).expect("compact LiteLLM pricing JSON");

    fs::write(out_path, pricing_json).expect("write build-time pricing snapshot");
}

fn out_dir_path(file_name: &str) -> PathBuf {
    PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR is set by cargo")).join(file_name)
}

fn fetch_pricing_json() -> std::io::Result<String> {
    let response = minreq::get(litellm_pricing_url()?)
        .with_timeout(PRICING_FETCH_TIMEOUT_SECONDS)
        .send()
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    if response.status_code != 200 {
        return Err(std::io::Error::other(format!(
            "HTTP {}",
            response.status_code
        )));
    }
    Ok(response
        .as_str()
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error.to_string()))?
        .to_string())
}

fn litellm_pricing_url() -> std::io::Result<String> {
    let flake_lock = fs::read_to_string(FLAKE_LOCK_JSON)?;
    let Value::Object(root) = serde_json::from_str::<Value>(&flake_lock)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error.to_string()))?
    else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "flake.lock must be a JSON object",
        ));
    };
    let locked = root
        .get("nodes")
        .and_then(|nodes| nodes.get("litellm"))
        .and_then(|litellm| litellm.get("locked"))
        .and_then(Value::as_object)
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "flake.lock is missing nodes.litellm.locked",
            )
        })?;
    let owner = required_flake_lock_string_field(locked, "owner")?;
    let repo = required_flake_lock_string_field(locked, "repo")?;
    let rev = required_flake_lock_string_field(locked, "rev")?;

    Ok(format!(
        "https://raw.githubusercontent.com/{owner}/{repo}/{rev}/{LITELLM_PRICING_JSON}"
    ))
}

fn required_flake_lock_string_field(
    object: &Map<String, Value>,
    field: &str,
) -> std::io::Result<String> {
    object
        .get(field)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("flake.lock nodes.litellm.locked.{field} must be a string"),
            )
        })
}

include!("src/pricing_compact.rs");
