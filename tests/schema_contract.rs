use herdex::store::Store;
use std::path::PathBuf;
struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("herdex-schema-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
#[test]
fn initializes_and_reopens_the_current_schema_without_data_conversion() {
    let dir = Directory::new();
    let store = Store::open(dir.0.to_str().unwrap()).unwrap();
    store.add_api_key("fixture-key", "persisted").unwrap();
    drop(store);
    let reopened = Store::open(dir.0.to_str().unwrap()).unwrap();
    assert_eq!(reopened.list_api_keys().unwrap()[0].comment, "persisted");
}
#[test]
fn rejects_another_schema_without_migrating_or_resetting_it() {
    let dir = Directory::new();
    let conn = rusqlite::Connection::open(dir.0.join("herdex.db")).unwrap();
    conn.execute_batch("CREATE TABLE marker(value INTEGER);INSERT INTO marker VALUES(42);")
        .unwrap();
    drop(conn);
    assert!(Store::open(dir.0.to_str().unwrap())
        .err()
        .unwrap()
        .contains("no automatic migration or reset"));
    let conn = rusqlite::Connection::open(dir.0.join("herdex.db")).unwrap();
    assert_eq!(
        conn.query_row("SELECT value FROM marker", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        42
    );
    assert_eq!(
        conn.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
            .unwrap(),
        0
    );
}

#[test]
fn stable_user_identity_uses_claims_or_namespaced_oidc_subject_never_email() {
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
    for (claims, expected) in [
        (
            serde_json::json!({"chatgpt_user_id":"fixture-user"}),
            Some("chatgpt:fixture-user"),
        ),
        (
            serde_json::json!({"https://api.openai.com/auth":{"user_id":"fixture-user"}}),
            Some("chatgpt:fixture-user"),
        ),
        (
            serde_json::json!({"sub":"fixture-subject"}),
            Some("oidc:fixture-subject"),
        ),
        (serde_json::json!({"email":"a@example.test"}), None),
    ] {
        let token = format!(
            "fixture.{}.fixture",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap())
        );
        assert_eq!(
            herdex::oauth::parse_id_token(&token)
                .unwrap()
                .user_id()
                .as_deref(),
            expected
        );
    }
}
