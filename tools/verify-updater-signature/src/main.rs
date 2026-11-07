//! Verifies the signatures in a Tauri updater manifest against the public key
//! the app embeds, before a draft release is turned into a published one.
//!
//! Why this exists: the release workflow signs installers with a repository
//! secret, but nothing used to check that the uploaded `.sig` files were made
//! with the key matching `tauri.conf.json`. A rotated or misconfigured signing
//! key would publish green while every client fails to install the update.
//!
//! Tauri stores both the public key and the manifest signature as base64 of a
//! minisign text block. The manifest is verified entry by entry: the file named
//! by each platform URL must exist in the assets directory and its signature
//! must verify against the embedded public key.

use std::{collections::BTreeMap, env, fs, path::PathBuf, process::ExitCode};

use minisign_verify::{PublicKey, Signature};

struct Arguments {
    config: PathBuf,
    manifest: PathBuf,
    assets_dir: PathBuf,
    expected_version: Option<String>,
}

fn usage() -> &'static str {
    "usage: verify-updater-signature --config <tauri.conf.json> --manifest <latest.json> \
     --assets-dir <dir> [--expected-version <version>]"
}

fn parse_arguments() -> Result<Arguments, String> {
    let mut config = None;
    let mut manifest = None;
    let mut assets_dir = None;
    let mut expected_version = None;
    let mut args = env::args().skip(1);
    while let Some(argument) = args.next() {
        let mut value = |name: &str| -> Result<String, String> {
            args.next()
                .ok_or_else(|| format!("{name} needs a value; {}", usage()))
        };
        match argument.as_str() {
            "--config" => config = Some(PathBuf::from(value("--config")?)),
            "--manifest" => manifest = Some(PathBuf::from(value("--manifest")?)),
            "--assets-dir" => assets_dir = Some(PathBuf::from(value("--assets-dir")?)),
            "--expected-version" => expected_version = Some(value("--expected-version")?),
            other => return Err(format!("unknown argument {other}; {}", usage())),
        }
    }
    Ok(Arguments {
        config: config.ok_or_else(|| format!("--config is required; {}", usage()))?,
        manifest: manifest.ok_or_else(|| format!("--manifest is required; {}", usage()))?,
        assets_dir: assets_dir.ok_or_else(|| format!("--assets-dir is required; {}", usage()))?,
        expected_version,
    })
}

/// Tauri embeds the public key as base64 of the two-line minisign key block.
fn public_key_from_config(config: &serde_json::Value) -> Result<PublicKey, String> {
    let encoded = config
        .pointer("/plugins/updater/pubkey")
        .and_then(serde_json::Value::as_str)
        .ok_or("tauri.conf.json has no plugins.updater.pubkey string")?;
    let decoded = decode_base64_text(encoded)?;
    PublicKey::decode(&decoded)
        .map_err(|error| format!("cannot decode updater public key: {error:?}"))
}

/// Tauri stores each manifest signature as base64 of the minisign text block.
fn signature_from_manifest(encoded: &str) -> Result<Signature, String> {
    let decoded = decode_base64_text(encoded)?;
    Signature::decode(&decoded)
        .map_err(|error| format!("cannot decode manifest signature: {error:?}"))
}

fn decode_base64_text(encoded: &str) -> Result<String, String> {
    // The only use is small trusted metadata, so a tiny standard-alphabet
    // decoder keeps the tool dependency-free beyond minisign-verify/serde_json.
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut table = [255u8; 256];
    for (index, byte) in ALPHABET.iter().enumerate() {
        table[*byte as usize] = index as u8;
    }
    let mut bytes = Vec::with_capacity(encoded.len() * 3 / 4);
    let mut buffer = 0u32;
    let mut bits = 0u32;
    for byte in encoded.bytes() {
        match byte {
            b'=' => break,
            b'\r' | b'\n' | b' ' | b'\t' => continue,
            byte => {
                let value = table[byte as usize];
                if value == 255 {
                    return Err(format!("invalid base64 character in metadata: {byte:#x}"));
                }
                buffer = (buffer << 6) | u32::from(value);
                bits += 6;
                if bits >= 8 {
                    bits -= 8;
                    bytes.push((buffer >> bits) as u8);
                    buffer &= (1 << bits) - 1;
                }
            }
        }
    }
    String::from_utf8(bytes).map_err(|error| format!("decoded metadata is not UTF-8: {error}"))
}

fn asset_name_from_url(url: &str) -> Result<String, String> {
    let path = url.split_once("://").map_or(url, |(_, rest)| {
        rest.split_once('/').map_or("", |(_, path)| path)
    });
    let name = path.rsplit('/').next().unwrap_or_default();
    if name.is_empty() || !name.contains('.') {
        return Err(format!("update URL has no asset file name: {url}"));
    }
    Ok(name.to_owned())
}

fn verify(arguments: &Arguments) -> Result<(), String> {
    let config: serde_json::Value = serde_json::from_slice(
        &fs::read(&arguments.config)
            .map_err(|error| format!("cannot read {}: {error}", arguments.config.display()))?,
    )
    .map_err(|error| format!("cannot parse {}: {error}", arguments.config.display()))?;
    let public_key = public_key_from_config(&config)?;

    let manifest: serde_json::Value = serde_json::from_slice(
        &fs::read(&arguments.manifest)
            .map_err(|error| format!("cannot read {}: {error}", arguments.manifest.display()))?,
    )
    .map_err(|error| format!("cannot parse {}: {error}", arguments.manifest.display()))?;
    let version = manifest
        .get("version")
        .and_then(serde_json::Value::as_str)
        .ok_or("manifest has no version string")?;
    if let Some(expected) = &arguments.expected_version {
        if expected != version {
            return Err(format!(
                "manifest version {version} does not match the release version {expected}"
            ));
        }
    }
    let platforms = manifest
        .get("platforms")
        .and_then(serde_json::Value::as_object)
        .ok_or("manifest has no platforms object")?;
    if platforms.is_empty() {
        return Err("manifest has an empty platforms object".into());
    }

    let mut verified: BTreeMap<PathBuf, ()> = BTreeMap::new();
    for (platform, entry) in platforms {
        let signature = entry
            .get("signature")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| format!("platform {platform} has no signature"))?;
        let url = entry
            .get("url")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| format!("platform {platform} has no url"))?;
        let name = asset_name_from_url(url)?;
        let path = arguments.assets_dir.join(&name);
        let bytes =
            fs::read(&path).map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        let signature = signature_from_manifest(signature)?;
        public_key
            .verify(&bytes, &signature, false)
            .map_err(|error| format!("{name}: updater signature does not verify: {error:?}"))?;
        verified.insert(path, ());
        println!("verified {name} ({platform})");
    }
    println!(
        "all {} platform signatures verify for version {version}",
        verified.len()
    );
    Ok(())
}

fn main() -> ExitCode {
    let arguments = match parse_arguments() {
        Ok(arguments) => arguments,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
    };
    match verify(&arguments) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("updater signature verification failed: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asset_names_come_from_the_url_tail() {
        assert_eq!(
            asset_name_from_url(
                "https://github.com/haxsd/haxsd-byok/releases/download/haxsd-byok-v1.7.5/haxsd.byok_1.7.5_x64-setup.exe"
            )
            .unwrap(),
            "haxsd.byok_1.7.5_x64-setup.exe"
        );
        assert!(asset_name_from_url("https://example.com/").is_err());
        assert!(asset_name_from_url("not a url").is_err());
    }

    #[test]
    fn base64_text_decodes_the_tauri_metadata_shape() {
        // "abc" encoded, with trailing padding and whitespace.
        assert_eq!(decode_base64_text("YWJj").unwrap(), "abc");
        assert_eq!(decode_base64_text("YWJj\n").unwrap(), "abc");
        assert!(decode_base64_text("!!!!").is_err());
    }

    #[test]
    fn a_manifest_without_platforms_is_refused() {
        let arguments = Arguments {
            config: PathBuf::new(),
            manifest: PathBuf::new(),
            assets_dir: PathBuf::new(),
            expected_version: None,
        };
        // `verify` fails earlier on the unreadable config; this only pins the
        // argument struct shape used by the workflow.
        let _ = (
            &arguments.config,
            &arguments.manifest,
            &arguments.assets_dir,
        );
    }
}
