use coding_agent_search::encryption::{
    Argon2Params, aes_gcm_decrypt, aes_gcm_encrypt, argon2id_hash, hkdf_extract_expand,
};
use serde::{Deserialize, de::DeserializeOwned};
use std::io::{Cursor, Read};

fn parse_yaml<T: DeserializeOwned>(mut reader: impl Read) -> Result<T, serde_saphyr::Error> {
    let mut input = Vec::new();
    reader
        .read_to_end(&mut input)
        .map_err(|cause| serde_saphyr::Error::IOError { cause })?;
    // serde-saphyr treats an empty stream as an empty sequence. A vector
    // fixture without a document is incomplete input, not an empty vector.
    if input.iter().all(u8::is_ascii_whitespace) {
        return Err(serde_saphyr::Error::IOError {
            cause: std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "empty YAML document"),
        });
    }
    serde_saphyr::from_reader_with_options(
        Cursor::new(input),
        serde_saphyr::options! {
            strict_booleans: true,
            with_snippet: false,
            budget: serde_saphyr::budget! {
                max_reader_input_bytes: Some(1024 * 1024),
                max_total_scalar_bytes: 1024 * 1024,
                max_recorded_anchor_bytes: 1024 * 1024,
                max_recorded_anchor_events: 8192,
                max_events: 16384,
                max_nodes: 8192,
                max_depth: 32,
                max_documents: 1,
                max_anchors: 64,
                max_aliases: 64,
            },
            alias_limits: serde_saphyr::alias_limits! {
                max_total_replayed_events: 8192,
                max_replay_stack_depth: 16,
                max_alias_expansions_per_anchor: 64,
            },
        },
    )
}

#[derive(Deserialize)]
struct AesGcmVector {
    name: String,
    key: String,
    nonce: String,
    plaintext: String,
    aad: String,
    ciphertext: String,
    tag: String,
}

#[derive(Deserialize)]
#[allow(dead_code)]
struct Argon2Vector {
    name: String,
    password: String,
    salt: String,
    memory_kb: u32,
    iterations: u32,
    parallelism: u32,
    output_len: u32,
    expected_hash_hex: String,
}

#[derive(Deserialize)]
struct HkdfVector {
    name: String,
    ikm: String,
    salt: String,
    info: String,
    output_len: usize,
    expected_okm: String,
}

fn load_test_vectors<T: for<'de> Deserialize<'de>>(filename: &str) -> Vec<T> {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/vectors")
        .join(filename);
    let f = std::fs::File::open(path).expect("failed to open vector file");
    parse_yaml(f).expect("failed to parse yaml")
}

#[test]
fn yaml_preserves_workflow_string_keys_and_sequences() {
    let value: serde_json::Value =
        parse_yaml(b"on:\n  push:\n    branches: [main]\nlabel: off\n".as_slice()).unwrap();
    assert_eq!(value["on"]["push"]["branches"], serde_json::json!(["main"]));
    assert_eq!(value["label"], "off");
    assert!(value.get("true").is_none());
}

#[test]
fn yaml_rejects_malformed_duplicate_and_empty_vectors() {
    for input in [
        "[unterminated",
        "name: first\nname: second\n",
        "---\n[]\n---\n[]\n",
    ] {
        assert!(parse_yaml::<serde_json::Value>(input.as_bytes()).is_err());
    }
    assert!(parse_yaml::<Vec<AesGcmVector>>(b"".as_slice()).is_err());
    assert!(parse_yaml::<Vec<AesGcmVector>>(b"- name: incomplete\n".as_slice()).is_err());
}

#[test]
fn yaml_bounds_input_depth_and_alias_expansion() {
    let at_limit = format!("#{}\n[]\n", "x".repeat(1024 * 1024 - 5));
    assert_eq!(at_limit.len(), 1024 * 1024);
    let parsed: Vec<serde_json::Value> = parse_yaml(at_limit.as_bytes()).unwrap();
    assert!(parsed.is_empty());

    let oversized = format!("#{}\n[]\n", "x".repeat(1024 * 1024));
    let error = parse_yaml::<Vec<serde_json::Value>>(oversized.as_bytes()).unwrap_err();
    assert!(matches!(
        error.without_snippet(),
        serde_saphyr::Error::IOError { cause }
            if cause.kind() == std::io::ErrorKind::FileTooLarge
    ));

    let deep = format!("{}0{}", "[".repeat(33), "]".repeat(33));
    let error = parse_yaml::<serde_json::Value>(deep.as_bytes()).unwrap_err();
    assert!(matches!(
        error.without_snippet(),
        serde_saphyr::Error::Budget { .. }
    ));

    let aliases = std::iter::repeat_n("*base", 65)
        .collect::<Vec<_>>()
        .join(", ");
    let input = format!("base: &base [1, 2]\nreferences: [{aliases}]\n");
    let error = parse_yaml::<serde_json::Value>(input.as_bytes()).unwrap_err();
    assert!(matches!(
        error.without_snippet(),
        serde_saphyr::Error::Budget { .. }
    ));
    assert!(error.to_string().len() < 1024, "bounded diagnostic");
    assert!(
        !error.to_string().contains("references:"),
        "no input snippet"
    );
}

#[test]
fn test_aes_gcm_vectors() {
    let vectors: Vec<AesGcmVector> = load_test_vectors("aes_gcm.yaml");
    assert!(!vectors.is_empty(), "AES-GCM known-answer vectors must run");
    for v in vectors {
        let key = hex::decode(&v.key).unwrap();
        let nonce = hex::decode(&v.nonce).unwrap();
        let plaintext = hex::decode(&v.plaintext).unwrap();
        let aad = hex::decode(&v.aad).unwrap();
        let expected_ciphertext = hex::decode(&v.ciphertext).unwrap();
        let expected_tag = hex::decode(&v.tag).unwrap();

        let (ciphertext, tag) = aes_gcm_encrypt(&key, &nonce, &plaintext, &aad).unwrap();

        assert_eq!(
            ciphertext, expected_ciphertext,
            "Ciphertext mismatch for {}",
            v.name
        );
        assert_eq!(tag, expected_tag, "Tag mismatch for {}", v.name);

        let decrypted =
            aes_gcm_decrypt(&key, &nonce, &ciphertext, &aad, &tag).expect("decryption failed");
        assert_eq!(decrypted, plaintext, "Decryption mismatch for {}", v.name);
    }
}

#[test]
fn test_argon2_vectors() {
    let vectors: Vec<Argon2Vector> = load_test_vectors("argon2.yaml");
    assert!(!vectors.is_empty(), "Argon2 known-answer vectors must run");
    for v in vectors {
        let password = v.password.as_bytes();
        let salt = v.salt.as_bytes();
        let params = Argon2Params::new(
            v.memory_kb,
            v.iterations,
            v.parallelism,
            Some(v.output_len as usize),
        )
        .unwrap();

        let expected = hex::decode(&v.expected_hash_hex).unwrap();
        let result = argon2id_hash(password, salt, &params).unwrap();

        assert_eq!(result, expected, "Argon2 mismatch for {}", v.name);
    }
}

#[test]
fn test_hkdf_vectors() {
    let vectors: Vec<HkdfVector> = load_test_vectors("hkdf.yaml");
    assert!(!vectors.is_empty(), "HKDF known-answer vectors must run");
    for v in vectors {
        let ikm = hex::decode(v.ikm).unwrap();
        let salt = hex::decode(v.salt).unwrap();
        let info = hex::decode(v.info).unwrap();
        let expected_okm = hex::decode(&v.expected_okm).unwrap();

        // This function performs both extract and expand
        let result = hkdf_extract_expand(&ikm, &salt, &info, v.output_len).unwrap();

        assert_eq!(result, expected_okm, "HKDF vector failed: {}", v.name);
    }
}
