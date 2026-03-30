use hbb_common::{
    anyhow::anyhow,
    bail,
    config::Config,
    get_time,
    password_security::decrypt_vec_or_original,
    sodiumoxide::{base64, crypto::secretbox},
    ResultType,
};
use serde_derive::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::convert::TryInto;
use std::sync::Mutex;
use totp_rs::{Algorithm, Secret, TOTP};

lazy_static::lazy_static! {
    static ref CURRENT_2FA: Mutex<Option<(TOTPInfo, TOTP)>> = Mutex::new(None);
}

const ISSUER: &str = "RustDesk";
const TAG_LOGIN: &str = "Connection";
const TOTP_VERSION: &[u8] = b"01";
const TOTP_KEY_SALT: &[u8] = b"rustdesk-totp-2fa-encryption";

/// Derive a TOTP-specific encryption key by hashing the machine UUID with a
/// domain-specific salt. This ensures that TOTP secrets are encrypted with a
/// key distinct from the generic "00" scheme used elsewhere.
fn totp_derive_key() -> secretbox::Key {
    let uuid = hbb_common::get_uuid();
    let mut hasher = Sha256::new();
    hasher.update(TOTP_KEY_SALT);
    hasher.update(&uuid);
    let hash = hasher.finalize();
    // secretbox::KEYBYTES is 32, same as SHA-256 output
    secretbox::Key(hash.as_slice().try_into().expect("SHA-256 output is 32 bytes"))
}

/// Encrypt bytes using the TOTP-specific key. Output is prefixed with "01".
fn totp_encrypt_vec(plaintext: &[u8]) -> Result<Vec<u8>, ()> {
    if plaintext.is_empty() {
        return Err(());
    }
    let key = totp_derive_key();
    let nonce = secretbox::Nonce([0; secretbox::NONCEBYTES]);
    let ciphertext = secretbox::seal(plaintext, &nonce, &key);
    let encoded = base64::encode(&ciphertext, base64::Variant::Original);
    let mut result = TOTP_VERSION.to_vec();
    result.extend_from_slice(encoded.as_bytes());
    Ok(result)
}

/// Decrypt bytes that were encrypted with the TOTP-specific key (version "01").
/// Returns (plaintext, success).
fn totp_decrypt_vec(data: &[u8]) -> (Vec<u8>, bool) {
    if data.len() > 2 && data.starts_with(TOTP_VERSION) {
        if let Ok(decoded) = base64::decode(&data[2..], base64::Variant::Original) {
            let key = totp_derive_key();
            let nonce = secretbox::Nonce([0; secretbox::NONCEBYTES]);
            if let Ok(plaintext) = secretbox::open(&decoded, &nonce, &key) {
                return (plaintext, true);
            }
        }
    }
    (data.to_vec(), false)
}

/// Decrypt TOTP/bot secret data with migration support.
/// Tries the new "01" key first, then falls back to the legacy "00" scheme.
fn decrypt_with_migration(data: &[u8]) -> (Vec<u8>, bool, bool) {
    // Try new TOTP-specific key first
    let (plaintext, success) = totp_decrypt_vec(data);
    if success {
        return (plaintext, true, false); // decrypted with new key, no migration needed
    }

    // Fall back to legacy "00" scheme
    let (plaintext, success, _) = decrypt_vec_or_original(data, "00");
    if success {
        return (plaintext, true, true); // decrypted with old key, migration needed
    }

    (data.to_vec(), false, false)
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TOTPInfo {
    pub name: String,
    pub secret: Vec<u8>,
    pub digits: usize,
    pub created_at: i64,
}

impl TOTPInfo {
    fn new_totp(&self) -> ResultType<TOTP> {
        let totp = TOTP::new(
            Algorithm::SHA1,
            self.digits,
            1,
            30,
            self.secret.clone(),
            Some(format!("{} {}", ISSUER, TAG_LOGIN)),
            self.name.clone(),
        )?;
        Ok(totp)
    }

    fn gen_totp_info(name: String, digits: usize) -> ResultType<TOTPInfo> {
        let secret = Secret::generate_secret();
        let totp = TOTPInfo {
            secret: secret.to_bytes()?,
            name,
            digits,
            created_at: get_time(),
            ..Default::default()
        };
        Ok(totp)
    }

    pub fn into_string(&self) -> ResultType<String> {
        let secret = totp_encrypt_vec(self.secret.as_slice())
            .unwrap_or_else(|_| self.secret.clone());
        let totp_info = TOTPInfo {
            secret,
            ..self.clone()
        };
        let s = serde_json::to_string(&totp_info)?;
        Ok(s)
    }

    pub fn from_str(data: &str) -> ResultType<TOTP> {
        let mut totp_info = serde_json::from_str::<TOTPInfo>(data)?;
        let (secret, success, needs_migration) =
            decrypt_with_migration(&totp_info.secret);
        if success {
            totp_info.secret = secret;
            if needs_migration {
                // Re-encrypt with the new TOTP-specific key and persist
                if let Ok(migrated) = totp_info.into_string() {
                    #[cfg(not(any(target_os = "android", target_os = "ios")))]
                    crate::ipc::set_option("2fa", &migrated);
                    #[cfg(any(target_os = "android", target_os = "ios"))]
                    Config::set_option("2fa".to_owned(), migrated);
                }
            }
            return Ok(totp_info.new_totp()?);
        } else {
            bail!("decrypt 2fa secret failed")
        }
    }
}

pub fn generate2fa() -> String {
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    let id = crate::ipc::get_id();
    #[cfg(any(target_os = "android", target_os = "ios"))]
    let id = Config::get_id();
    if let Ok(info) = TOTPInfo::gen_totp_info(id, 6) {
        if let Ok(totp) = info.new_totp() {
            let code = totp.get_url();
            *CURRENT_2FA.lock().unwrap() = Some((info, totp));
            return code;
        }
    }
    "".to_owned()
}

pub fn verify2fa(code: String) -> bool {
    if let Some((info, totp)) = CURRENT_2FA.lock().unwrap().as_ref() {
        if let Ok(res) = totp.check_current(&code) {
            if res {
                if let Ok(v) = info.into_string() {
                    #[cfg(not(any(target_os = "android", target_os = "ios")))]
                    crate::ipc::set_option("2fa", &v);
                    #[cfg(any(target_os = "android", target_os = "ios"))]
                    Config::set_option("2fa".to_owned(), v);
                    return res;
                }
            }
        }
    }
    false
}

pub fn get_2fa(raw: Option<String>) -> Option<TOTP> {
    TOTPInfo::from_str(&raw.unwrap_or(Config::get_option("2fa")))
        .map(|x| Some(x))
        .unwrap_or_default()
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TelegramBot {
    #[serde(skip)]
    pub token_str: String,
    pub token: Vec<u8>,
    pub chat_id: String,
}

impl TelegramBot {
    fn into_string(&self) -> ResultType<String> {
        let token = totp_encrypt_vec(self.token_str.as_bytes())
            .unwrap_or_else(|_| self.token_str.as_bytes().to_vec());
        let bot = TelegramBot {
            token,
            ..self.clone()
        };
        let s = serde_json::to_string(&bot)?;
        Ok(s)
    }

    fn save(&self) -> ResultType<()> {
        let s = self.into_string()?;
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        crate::ipc::set_option("bot", &s);
        #[cfg(any(target_os = "android", target_os = "ios"))]
        Config::set_option("bot".to_owned(), s);
        Ok(())
    }

    pub fn get() -> ResultType<Option<TelegramBot>> {
        let data = Config::get_option("bot");
        if data.is_empty() {
            return Ok(None);
        }
        let mut bot = serde_json::from_str::<TelegramBot>(&data)?;
        let (token, success, needs_migration) = decrypt_with_migration(&bot.token);
        if success {
            bot.token_str = String::from_utf8(token)?;
            if needs_migration {
                // Re-encrypt with the new TOTP-specific key and persist
                let _ = bot.save();
            }
            return Ok(Some(bot));
        }
        bail!("decrypt telegram bot token failed")
    }
}

// https://gist.github.com/dideler/85de4d64f66c1966788c1b2304b9caf1
pub async fn send_2fa_code_to_telegram(text: &str, bot: TelegramBot) -> ResultType<()> {
    let url = format!("https://api.telegram.org/bot{}/sendMessage", bot.token_str);
    let params = serde_json::json!({"chat_id": bot.chat_id, "text": text});
    crate::post_request(url, params.to_string(), "").await?;
    Ok(())
}

pub fn get_chatid_telegram(bot_token: &str) -> ResultType<Option<String>> {
    let url = format!("https://api.telegram.org/bot{}/getUpdates", bot_token);
    // because caller is in tokio runtime, so we must call post_request_sync in new thread.
    let handle = std::thread::spawn(move || crate::post_request_sync(url, "".to_owned(), ""));
    let resp = handle.join().map_err(|_| anyhow!("Thread panicked"))??;
    let value = serde_json::from_str::<serde_json::Value>(&resp).map_err(|e| anyhow!(e))?;

    // Check for an error_code in the response
    if let Some(error_code) = value.get("error_code").and_then(|code| code.as_i64()) {
        // If there's an error_code, try to use the description for the error message
        let description = value["description"]
            .as_str()
            .unwrap_or("Unknown error occurred");
        return Err(anyhow!(
            "Telegram API error: {} (error_code: {})",
            description,
            error_code
        ));
    }

    let chat_id = &value["result"][0]["message"]["chat"]["id"];
    let chat_id = if let Some(id) = chat_id.as_i64() {
        Some(id.to_string())
    } else if let Some(id) = chat_id.as_str() {
        Some(id.to_owned())
    } else {
        None
    };

    if let Some(chat_id) = chat_id.as_ref() {
        let bot = TelegramBot {
            token_str: bot_token.to_owned(),
            chat_id: chat_id.to_owned(),
            ..Default::default()
        };
        bot.save()?;
    }

    Ok(chat_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hbb_common::password_security::{decrypt_vec_or_original, encrypt_vec_or_original};
    use totp_rs::{Algorithm, TOTP};

    // --- TOTPInfo generation ---

    #[test]
    fn test_gen_totp_info_produces_valid_secret() {
        let info = TOTPInfo::gen_totp_info("test-user".to_string(), 6).unwrap();
        assert_eq!(info.name, "test-user");
        assert_eq!(info.digits, 6);
        assert!(!info.secret.is_empty());
        assert!(info.created_at > 0);
    }

    #[test]
    fn test_gen_totp_info_unique_secrets() {
        let info1 = TOTPInfo::gen_totp_info("user".to_string(), 6).unwrap();
        let info2 = TOTPInfo::gen_totp_info("user".to_string(), 6).unwrap();
        // Two generations should produce different secrets
        assert_ne!(info1.secret, info2.secret);
    }

    #[test]
    fn test_gen_totp_info_custom_digits() {
        let info = TOTPInfo::gen_totp_info("user".to_string(), 8).unwrap();
        assert_eq!(info.digits, 8);
    }

    // --- TOTP creation from TOTPInfo ---

    #[test]
    fn test_new_totp_creates_valid_totp() {
        let info = TOTPInfo::gen_totp_info("test-device".to_string(), 6).unwrap();
        let totp = info.new_totp().unwrap();
        // Should be able to generate a code
        let code = totp.generate_current().unwrap();
        assert_eq!(code.len(), 6);
        // Code should be all digits
        assert!(code.chars().all(|c| c.is_ascii_digit()));
    }

    #[test]
    fn test_new_totp_generates_url() {
        let info = TOTPInfo::gen_totp_info("my-device-id".to_string(), 6).unwrap();
        let totp = info.new_totp().unwrap();
        let url = totp.get_url();
        assert!(url.starts_with("otpauth://totp/"));
        assert!(url.contains("my-device-id"));
        assert!(url.contains("RustDesk"));
    }

    #[test]
    fn test_totp_code_verification() {
        let info = TOTPInfo::gen_totp_info("test".to_string(), 6).unwrap();
        let totp = info.new_totp().unwrap();
        // Generate the current code, then verify it
        let code = totp.generate_current().unwrap();
        let result = totp.check_current(&code).unwrap();
        assert!(result, "Current TOTP code should verify successfully");
    }

    #[test]
    fn test_totp_wrong_code_fails() {
        let info = TOTPInfo::gen_totp_info("test".to_string(), 6).unwrap();
        let totp = info.new_totp().unwrap();
        // "000000" is almost certainly not the current code (1 in 1M chance)
        // We check against two codes to make the probability of false positive negligible
        let result1 = totp.check_current("000000").unwrap();
        let result2 = totp.check_current("999999").unwrap();
        // At least one of these must fail (both being valid simultaneously is impossible)
        assert!(
            !result1 || !result2,
            "Both 000000 and 999999 cannot be valid TOTP codes simultaneously"
        );
    }

    #[test]
    fn test_totp_uses_sha1_and_30s_period() {
        // Verify the TOTP parameters match expected configuration
        let info = TOTPInfo::gen_totp_info("test".to_string(), 6).unwrap();
        let totp = info.new_totp().unwrap();
        // The TOTP is created with Algorithm::SHA1, step=30, digits from info
        // We verify by creating a second TOTP with same params and checking codes match
        let totp2 = TOTP::new(
            Algorithm::SHA1,
            6,
            1,
            30,
            info.secret.clone(),
            Some(format!("{} {}", ISSUER, TAG_LOGIN)),
            info.name.clone(),
        )
        .unwrap();
        let code1 = totp.generate_current().unwrap();
        let code2 = totp2.generate_current().unwrap();
        assert_eq!(code1, code2);
    }

    // --- TOTP-specific encryption (version "01") ---

    #[test]
    fn test_totp_key_is_not_hardcoded_00() {
        // The new encryption uses version "01", not the old hardcoded "00"
        let data = b"sensitive-totp-secret";
        let encrypted = totp_encrypt_vec(data).unwrap();
        assert!(encrypted.starts_with(b"01"), "New encryption must use version '01', not '00'");
        assert!(!encrypted.starts_with(b"00"));
    }

    #[test]
    fn test_totp_encrypt_decrypt_roundtrip() {
        let data = b"my-totp-secret-key-bytes";
        let encrypted = totp_encrypt_vec(data).unwrap();
        let (decrypted, success) = totp_decrypt_vec(&encrypted);
        assert!(success);
        assert_eq!(decrypted, data);
    }

    #[test]
    fn test_totp_encrypt_produces_different_output_than_00() {
        let data = b"test-secret-data";
        let encrypted_01 = totp_encrypt_vec(data).unwrap();
        let encrypted_00 = encrypt_vec_or_original(data, "00", 1024);

        // The two encryption schemes must produce different ciphertexts
        assert_ne!(encrypted_01, encrypted_00);
        // Version prefixes differ
        assert_eq!(&encrypted_01[..2], b"01");
        assert_eq!(&encrypted_00[..2], b"00");
    }

    #[test]
    fn test_totp_decrypt_rejects_wrong_version() {
        // Data encrypted with the old "00" scheme cannot be decrypted by totp_decrypt_vec
        let data = b"secret";
        let encrypted_00 = encrypt_vec_or_original(data, "00", 1024);
        let (_, success) = totp_decrypt_vec(&encrypted_00);
        assert!(!success, "totp_decrypt_vec should not decrypt '00'-versioned data");
    }

    #[test]
    fn test_totp_encrypt_empty_fails() {
        assert!(totp_encrypt_vec(b"").is_err());
    }

    #[test]
    fn test_totp_decrypt_empty_fails() {
        let (_, success) = totp_decrypt_vec(b"");
        assert!(!success);
    }

    #[test]
    fn test_totp_decrypt_garbage_fails() {
        let (_, success) = totp_decrypt_vec(b"01not-valid-base64!!!");
        assert!(!success);
    }

    // --- Migration from legacy "00" to new "01" ---

    #[test]
    fn test_migration_old_00_encrypted_data_can_be_read() {
        // Simulate data that was encrypted with the old "00" scheme
        let secret = b"legacy-totp-secret-bytes";
        let encrypted_00 = encrypt_vec_or_original(secret, "00", 1024);

        // decrypt_with_migration should successfully decrypt it
        let (decrypted, success, needs_migration) = decrypt_with_migration(&encrypted_00);
        assert!(success, "Should be able to decrypt legacy '00' data");
        assert_eq!(decrypted, secret);
        assert!(needs_migration, "Legacy data should be flagged for migration");
    }

    #[test]
    fn test_migration_new_01_encrypted_data_no_migration() {
        let secret = b"new-totp-secret-bytes";
        let encrypted_01 = totp_encrypt_vec(secret).unwrap();

        let (decrypted, success, needs_migration) = decrypt_with_migration(&encrypted_01);
        assert!(success, "Should decrypt new '01' data");
        assert_eq!(decrypted, secret);
        assert!(!needs_migration, "New data should not need migration");
    }

    #[test]
    fn test_migration_unencrypted_data_fails() {
        let raw = b"plain-text-not-encrypted";
        let (_, success, needs_migration) = decrypt_with_migration(raw);
        assert!(!success, "Unencrypted data should fail decryption");
        assert!(!needs_migration);
    }

    // --- Serialization / Encryption round-trip ---

    #[test]
    fn test_totp_info_into_string_produces_valid_json() {
        let info = TOTPInfo::gen_totp_info("device-123".to_string(), 6).unwrap();
        let serialized = info.into_string().unwrap();

        // Should be valid JSON
        let parsed: serde_json::Value = serde_json::from_str(&serialized).unwrap();
        assert_eq!(parsed["name"], "device-123");
        assert_eq!(parsed["digits"], 6);

        // The secret field should be encrypted with the new "01" version
        let encrypted_secret = parsed["secret"].as_array().unwrap();
        let first_two: Vec<u8> = encrypted_secret[0..2]
            .iter()
            .map(|v| v.as_u64().unwrap() as u8)
            .collect();
        assert_eq!(
            first_two, b"01",
            "Encrypted secret should start with version tag '01'"
        );
    }

    #[test]
    fn test_totp_info_roundtrip_into_string_from_str() {
        let info = TOTPInfo::gen_totp_info("roundtrip-test".to_string(), 6).unwrap();
        let original_secret = info.secret.clone();

        // Serialize (encrypts the secret with "01")
        let serialized = info.into_string().unwrap();

        // Deserialize (decrypts the secret and creates TOTP)
        let totp = TOTPInfo::from_str(&serialized).unwrap();

        // The reconstructed TOTP should generate the same codes
        let expected_totp = TOTP::new(
            Algorithm::SHA1,
            6,
            1,
            30,
            original_secret,
            Some(format!("{} {}", ISSUER, TAG_LOGIN)),
            "roundtrip-test".to_string(),
        )
        .unwrap();

        assert_eq!(
            totp.generate_current().unwrap(),
            expected_totp.generate_current().unwrap()
        );
    }

    #[test]
    fn test_totp_info_from_str_with_legacy_00_data() {
        // Simulate a TOTPInfo serialized with the old "00" encryption
        let info = TOTPInfo::gen_totp_info("legacy-device".to_string(), 6).unwrap();
        let original_secret = info.secret.clone();

        // Manually encrypt with the old "00" scheme
        let encrypted_secret = encrypt_vec_or_original(info.secret.as_slice(), "00", 1024);
        let legacy_info = TOTPInfo {
            secret: encrypted_secret,
            ..info.clone()
        };
        let legacy_json = serde_json::to_string(&legacy_info).unwrap();

        // from_str should still work via migration fallback
        let totp = TOTPInfo::from_str(&legacy_json).unwrap();

        // Verify the TOTP generates correct codes
        let expected_totp = TOTP::new(
            Algorithm::SHA1,
            6,
            1,
            30,
            original_secret,
            Some(format!("{} {}", ISSUER, TAG_LOGIN)),
            "legacy-device".to_string(),
        )
        .unwrap();

        assert_eq!(
            totp.generate_current().unwrap(),
            expected_totp.generate_current().unwrap()
        );
    }

    #[test]
    fn test_totp_info_from_str_invalid_json() {
        assert!(TOTPInfo::from_str("not json").is_err());
    }

    #[test]
    fn test_totp_info_from_str_unencrypted_secret_fails() {
        // If the secret is not encrypted (no version prefix), from_str should fail
        let info = TOTPInfo {
            name: "test".to_string(),
            secret: vec![1, 2, 3, 4, 5],
            digits: 6,
            created_at: 12345,
        };
        let json = serde_json::to_string(&info).unwrap();
        assert!(
            TOTPInfo::from_str(&json).is_err(),
            "from_str should fail when secret is not encrypted"
        );
    }

    // --- TelegramBot serialization ---

    #[test]
    fn test_telegram_bot_into_string_encrypts_token() {
        let bot = TelegramBot {
            token_str: "123456:ABC-DEF1234ghIkl-zyx57W2v1u123ew11".to_string(),
            token: vec![],
            chat_id: "987654321".to_string(),
        };
        let serialized = bot.into_string().unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&serialized).unwrap();

        // chat_id should be in plaintext
        assert_eq!(parsed["chat_id"], "987654321");

        // token should be encrypted with new "01" version prefix
        let token_arr = parsed["token"].as_array().unwrap();
        let first_two: Vec<u8> = token_arr[0..2]
            .iter()
            .map(|v| v.as_u64().unwrap() as u8)
            .collect();
        assert_eq!(first_two, b"01");

        // token_str is #[serde(skip)] so should not appear in JSON
        assert!(parsed.get("token_str").is_none());
    }

    // --- TOTPInfo defaults ---

    #[test]
    fn test_totp_info_default() {
        let info = TOTPInfo::default();
        assert_eq!(info.name, "");
        assert!(info.secret.is_empty());
        assert_eq!(info.digits, 0);
        assert_eq!(info.created_at, 0);
    }

    // --- Secret generation quality ---

    #[test]
    fn test_generated_secret_is_sufficient_length() {
        let info = TOTPInfo::gen_totp_info("test".to_string(), 6).unwrap();
        // totp-rs generate_secret() creates a 160-bit (20 byte) secret by default
        // This is the minimum recommended by RFC 4226
        assert!(
            info.secret.len() >= 20,
            "TOTP secret should be at least 20 bytes (160 bits), got {}",
            info.secret.len()
        );
    }

    // --- Key derivation ---

    #[test]
    fn test_totp_derived_key_is_deterministic() {
        let key1 = totp_derive_key();
        let key2 = totp_derive_key();
        assert_eq!(key1.0, key2.0, "Same machine should produce same key");
    }

    #[test]
    fn test_new_encryption_not_decryptable_with_generic_00() {
        // Verify that data encrypted with the new TOTP key cannot be decrypted
        // using the generic decrypt_vec_or_original with "00"
        let data = b"totp-secret-bytes";
        let encrypted = totp_encrypt_vec(data).unwrap();

        let (_, success, _) = decrypt_vec_or_original(&encrypted, "00");
        assert!(
            !success,
            "TOTP-encrypted data must not be decryptable with the generic '00' scheme"
        );
    }
}
