use hbb_common::{
    bail,
    base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _},
    sodiumoxide::crypto::sign,
    ResultType,
};
use serde_derive::{Deserialize, Serialize};

#[derive(Debug, PartialEq, Default, Serialize, Deserialize, Clone)]
pub struct CustomServer {
    #[serde(default)]
    pub key: String,
    #[serde(default)]
    pub host: String,
    #[serde(default)]
    pub api: String,
    #[serde(default)]
    pub relay: String,
}

fn get_custom_server_from_config_string(s: &str) -> ResultType<CustomServer> {
    let tmp: String = s.chars().rev().collect();
    const PK: &[u8; 32] = &[
        88, 168, 68, 104, 60, 5, 163, 198, 165, 38, 12, 85, 114, 203, 96, 163, 70, 48, 0, 131, 57,
        12, 46, 129, 83, 17, 84, 193, 119, 197, 130, 103,
    ];
    let pk = sign::PublicKey(*PK);
    let data = URL_SAFE_NO_PAD.decode(tmp)?;
    if let Ok(lic) = serde_json::from_slice::<CustomServer>(&data) {
        return Ok(lic);
    }
    if let Ok(data) = sign::verify(&data, &pk) {
        Ok(serde_json::from_slice::<CustomServer>(&data)?)
    } else {
        bail!("sign:verify failed");
    }
}

pub fn get_custom_server_from_string(s: &str) -> ResultType<CustomServer> {
    let s = if s.to_lowercase().ends_with(".exe.exe") {
        &s[0..s.len() - 8]
    } else if s.to_lowercase().ends_with(".exe") {
        &s[0..s.len() - 4]
    } else {
        s
    };
    /*
     * The following code tokenizes the file name based on commas and
     * extracts relevant parts sequentially.
     *
     * host= is expected to be the first part.
     *
     * Since Windows renames files adding (1), (2) etc. before the .exe
     * in case of duplicates, which causes the host or key values to be
     * garbled.
     *
     * This allows using a ',' (comma) symbol as a final delimiter.
     */
    if s.to_lowercase().contains("host=") {
        let stripped = &s[s.to_lowercase().find("host=").unwrap_or(0)..s.len()];
        let strs: Vec<&str> = stripped.split(",").collect();
        let mut host = String::default();
        let mut key = String::default();
        let mut api = String::default();
        let mut relay = String::default();
        let strs_iter = strs.iter();
        for el in strs_iter {
            let el_lower = el.to_lowercase();
            if el_lower.starts_with("host=") {
                host = el.chars().skip(5).collect();
            }
            if el_lower.starts_with("key=") {
                key = el.chars().skip(4).collect();
            }
            if el_lower.starts_with("api=") {
                api = el.chars().skip(4).collect();
            }
            if el_lower.starts_with("relay=") {
                relay = el.chars().skip(6).collect();
            }
        }
        return Ok(CustomServer {
            host,
            key,
            api,
            relay,
        });
    } else {
        let s = s
            .replace("-licensed---", "--")
            .replace("-licensed--", "--")
            .replace("-licensed-", "--");
        let strs = s.split("--");
        for s in strs {
            if let Ok(lic) = get_custom_server_from_config_string(s.trim()) {
                return Ok(lic);
            } else if s.contains("(") {
                // https://github.com/rustdesk/rustdesk/issues/4162
                for s in s.split("(") {
                    if let Ok(lic) = get_custom_server_from_config_string(s.trim()) {
                        return Ok(lic);
                    }
                }
            }
        }
    }
    bail!("Failed to parse");
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_filename_license_string() {
        assert!(get_custom_server_from_string("rustdesk.exe").is_err());
        assert!(get_custom_server_from_string("rustdesk").is_err());
        assert_eq!(
            get_custom_server_from_string("rustdesk-host=server.example.net.exe").unwrap(),
            CustomServer {
                host: "server.example.net".to_owned(),
                key: "".to_owned(),
                api: "".to_owned(),
                relay: "".to_owned(),
            }
        );
        assert_eq!(
            get_custom_server_from_string("rustdesk-host=server.example.net,.exe").unwrap(),
            CustomServer {
                host: "server.example.net".to_owned(),
                key: "".to_owned(),
                api: "".to_owned(),
                relay: "".to_owned(),
            }
        );
        // key in these tests is "foobar.,2" base64 encoded
        assert_eq!(
            get_custom_server_from_string(
                "rustdesk-host=server.example.net,api=abc,key=Zm9vYmFyLiwyCg==.exe"
            )
            .unwrap(),
            CustomServer {
                host: "server.example.net".to_owned(),
                key: "Zm9vYmFyLiwyCg==".to_owned(),
                api: "abc".to_owned(),
                relay: "".to_owned(),
            }
        );
        assert_eq!(
            get_custom_server_from_string(
                "rustdesk-host=server.example.net,key=Zm9vYmFyLiwyCg==,.exe"
            )
            .unwrap(),
            CustomServer {
                host: "server.example.net".to_owned(),
                key: "Zm9vYmFyLiwyCg==".to_owned(),
                api: "".to_owned(),
                relay: "".to_owned(),
            }
        );
        assert_eq!(
            get_custom_server_from_string(
                "rustdesk-host=server.example.net,key=Zm9vYmFyLiwyCg==,relay=server.example.net.exe"
            )
            .unwrap(),
            CustomServer {
                host: "server.example.net".to_owned(),
                key: "Zm9vYmFyLiwyCg==".to_owned(),
                api: "".to_owned(),
                relay: "server.example.net".to_owned(),
            }
        );
        assert_eq!(
            get_custom_server_from_string(
                "rustdesk-Host=server.example.net,Key=Zm9vYmFyLiwyCg==,RELAY=server.example.net.exe"
            )
            .unwrap(),
            CustomServer {
                host: "server.example.net".to_owned(),
                key: "Zm9vYmFyLiwyCg==".to_owned(),
                api: "".to_owned(),
                relay: "server.example.net".to_owned(),
            }
        );
        let lic = CustomServer {
            host: "1.1.1.1".to_owned(),
            key: "5Qbwsde3unUcJBtrx9ZkvUmwFNoExHzpryHuPUdqlWM=".to_owned(),
            api: "".to_owned(),
            relay: "".to_owned(),
        };
        assert_eq!(
            get_custom_server_from_string("rustdesk-licensed-0nI900VsFHZVBVdIlncwpHS4V0bOZ0dtVldrpVO4JHdCp0YV5WdzUGZzdnYRVjI6ISeltmIsISMuEjLx4SMiojI0N3boJye.exe")
                .unwrap(), lic);
        assert_eq!(
            get_custom_server_from_string("rustdesk-licensed-0nI900VsFHZVBVdIlncwpHS4V0bOZ0dtVldrpVO4JHdCp0YV5WdzUGZzdnYRVjI6ISeltmIsISMuEjLx4SMiojI0N3boJye(1).exe")
                .unwrap(), lic);
        assert_eq!(
            get_custom_server_from_string("rustdesk--0nI900VsFHZVBVdIlncwpHS4V0bOZ0dtVldrpVO4JHdCp0YV5WdzUGZzdnYRVjI6ISeltmIsISMuEjLx4SMiojI0N3boJye(1).exe")
                .unwrap(), lic);
        assert_eq!(
            get_custom_server_from_string("rustdesk-licensed-0nI900VsFHZVBVdIlncwpHS4V0bOZ0dtVldrpVO4JHdCp0YV5WdzUGZzdnYRVjI6ISeltmIsISMuEjLx4SMiojI0N3boJye (1).exe")
                .unwrap(), lic);
        assert_eq!(
            get_custom_server_from_string("rustdesk-licensed-0nI900VsFHZVBVdIlncwpHS4V0bOZ0dtVldrpVO4JHdCp0YV5WdzUGZzdnYRVjI6ISeltmIsISMuEjLx4SMiojI0N3boJye (1) (2).exe")
                .unwrap(), lic);
        assert_eq!(
            get_custom_server_from_string("rustdesk-licensed-0nI900VsFHZVBVdIlncwpHS4V0bOZ0dtVldrpVO4JHdCp0YV5WdzUGZzdnYRVjI6ISeltmIsISMuEjLx4SMiojI0N3boJye--abc.exe")
                .unwrap(), lic);
        assert_eq!(
            get_custom_server_from_string("rustdesk-licensed--0nI900VsFHZVBVdIlncwpHS4V0bOZ0dtVldrpVO4JHdCp0YV5WdzUGZzdnYRVjI6ISeltmIsISMuEjLx4SMiojI0N3boJye--.exe")
                .unwrap(), lic);
        assert_eq!(
            get_custom_server_from_string("rustdesk-licensed---0nI900VsFHZVBVdIlncwpHS4V0bOZ0dtVldrpVO4JHdCp0YV5WdzUGZzdnYRVjI6ISeltmIsISMuEjLx4SMiojI0N3boJye--.exe")
                .unwrap(), lic);
        assert_eq!(
            get_custom_server_from_string("rustdesk-licensed--0nI900VsFHZVBVdIlncwpHS4V0bOZ0dtVldrpVO4JHdCp0YV5WdzUGZzdnYRVjI6ISeltmIsISMuEjLx4SMiojI0N3boJye--.exe")
                .unwrap(), lic);
    }

    // --- Edge cases for host= parsing ---

    #[test]
    fn test_empty_string() {
        assert!(get_custom_server_from_string("").is_err());
    }

    #[test]
    fn test_just_exe_extension() {
        assert!(get_custom_server_from_string(".exe").is_err());
    }

    #[test]
    fn test_double_exe_extension_stripped() {
        // .exe.exe should be stripped, then parsed
        assert_eq!(
            get_custom_server_from_string("rustdesk-host=myhost.exe.exe").unwrap(),
            CustomServer {
                host: "myhost".to_owned(),
                key: "".to_owned(),
                api: "".to_owned(),
                relay: "".to_owned(),
            }
        );
    }

    #[test]
    fn test_host_only_no_exe() {
        // No .exe extension
        assert_eq!(
            get_custom_server_from_string("rustdesk-host=10.0.0.1").unwrap(),
            CustomServer {
                host: "10.0.0.1".to_owned(),
                key: "".to_owned(),
                api: "".to_owned(),
                relay: "".to_owned(),
            }
        );
    }

    #[test]
    fn test_all_fields() {
        assert_eq!(
            get_custom_server_from_string(
                "rustdesk-host=myhost.com,key=MYKEY,api=https://api.myhost.com,relay=relay.myhost.com.exe"
            )
            .unwrap(),
            CustomServer {
                host: "myhost.com".to_owned(),
                key: "MYKEY".to_owned(),
                api: "https://api.myhost.com".to_owned(),
                relay: "relay.myhost.com".to_owned(),
            }
        );
    }

    #[test]
    fn test_fields_in_different_order() {
        // Parsing starts from "host=" and only sees fields AFTER it.
        // Fields before "host=" are ignored (by design — see line 60).
        // So host must come first for other fields to be parsed.
        assert_eq!(
            get_custom_server_from_string("rustdesk-host=h.com,relay=r.com,key=K,api=a.com.exe")
                .unwrap(),
            CustomServer {
                host: "h.com".to_owned(),
                key: "K".to_owned(),
                api: "a.com".to_owned(),
                relay: "r.com".to_owned(),
            }
        );
    }

    #[test]
    fn test_duplicate_fields_last_wins() {
        // When a field appears multiple times, each overwrites the previous
        let result =
            get_custom_server_from_string("rustdesk-host=first.com,host=second.com.exe").unwrap();
        assert_eq!(result.host, "second.com");
    }

    #[test]
    fn test_case_insensitive_keys() {
        // Keys are case-insensitive
        assert_eq!(
            get_custom_server_from_string("rustdesk-HOST=h.com,KEY=k,API=a.com,RELAY=r.com.exe")
                .unwrap(),
            CustomServer {
                host: "h.com".to_owned(),
                key: "k".to_owned(),
                api: "a.com".to_owned(),
                relay: "r.com".to_owned(),
            }
        );
    }

    #[test]
    fn test_host_with_port() {
        assert_eq!(
            get_custom_server_from_string("rustdesk-host=myserver.com:21116.exe").unwrap(),
            CustomServer {
                host: "myserver.com:21116".to_owned(),
                key: "".to_owned(),
                api: "".to_owned(),
                relay: "".to_owned(),
            }
        );
    }

    #[test]
    fn test_host_ipv4() {
        assert_eq!(
            get_custom_server_from_string("rustdesk-host=192.168.1.100.exe").unwrap(),
            CustomServer {
                host: "192.168.1.100".to_owned(),
                key: "".to_owned(),
                api: "".to_owned(),
                relay: "".to_owned(),
            }
        );
    }

    #[test]
    fn test_prefix_before_host_is_ignored() {
        // Anything before "host=" is stripped
        assert_eq!(
            get_custom_server_from_string("some-prefix-garbage-host=myhost.com.exe").unwrap(),
            CustomServer {
                host: "myhost.com".to_owned(),
                key: "".to_owned(),
                api: "".to_owned(),
                relay: "".to_owned(),
            }
        );
    }

    #[test]
    fn test_trailing_comma_produces_empty_token() {
        // Trailing comma: the last token is empty, doesn't match any key
        assert_eq!(
            get_custom_server_from_string("rustdesk-host=myhost.com,.exe").unwrap(),
            CustomServer {
                host: "myhost.com".to_owned(),
                key: "".to_owned(),
                api: "".to_owned(),
                relay: "".to_owned(),
            }
        );
    }

    #[test]
    fn test_empty_host_value() {
        // host= with no value
        assert_eq!(
            get_custom_server_from_string("rustdesk-host=.exe").unwrap(),
            CustomServer {
                host: "".to_owned(),
                key: "".to_owned(),
                api: "".to_owned(),
                relay: "".to_owned(),
            }
        );
    }

    #[test]
    fn test_no_host_no_license_fails() {
        // No "host=" and no valid license blob
        assert!(get_custom_server_from_string("rustdesk-something-else.exe").is_err());
    }

    #[test]
    fn test_garbage_license_blob_fails() {
        // Double-dash delimited but invalid base64/signature
        assert!(get_custom_server_from_string("rustdesk--notavalidblob--.exe").is_err());
    }

    #[test]
    fn test_custom_server_default() {
        let cs = CustomServer::default();
        assert_eq!(cs.host, "");
        assert_eq!(cs.key, "");
        assert_eq!(cs.api, "");
        assert_eq!(cs.relay, "");
    }

    #[test]
    fn test_custom_server_serialization_roundtrip() {
        let cs = CustomServer {
            host: "h.com".to_owned(),
            key: "k".to_owned(),
            api: "a.com".to_owned(),
            relay: "r.com".to_owned(),
        };
        let json = serde_json::to_string(&cs).unwrap();
        let deserialized: CustomServer = serde_json::from_str(&json).unwrap();
        assert_eq!(cs, deserialized);
    }

    #[test]
    fn test_custom_server_deserialize_missing_fields() {
        // All fields have #[serde(default)], so missing fields should be empty strings
        let json = r#"{"host":"h.com"}"#;
        let cs: CustomServer = serde_json::from_str(json).unwrap();
        assert_eq!(cs.host, "h.com");
        assert_eq!(cs.key, "");
        assert_eq!(cs.api, "");
        assert_eq!(cs.relay, "");
    }

    #[test]
    fn test_custom_server_deserialize_empty_json() {
        let json = "{}";
        let cs: CustomServer = serde_json::from_str(json).unwrap();
        assert_eq!(cs, CustomServer::default());
    }
}
