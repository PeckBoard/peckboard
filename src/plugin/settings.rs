//! Built-in plugin settings: schema definitions, storage, and validation.
//!
//! Plugins describe their settings through a small JSON Schema-inspired
//! vocabulary ([`SettingField`]) — a known set of typed field kinds the
//! Settings UI knows how to render directly. The shape is intentionally
//! narrower than full JSON Schema: it's a UI contract first, validation
//! second. The supported kinds are:
//!
//! * `string` — single-line text (optionally `secret = true` for
//!   password-style entry that the API never echoes back).
//! * `url` — URL, validated as `http(s)://…`.
//! * `integer` — numeric input with optional `min` / `max`.
//! * `boolean` — checkbox.
//! * `enum` — pick-one from a list of string options.
//! * `key_value_list` — variable-length list of `{key,value}` pairs,
//!   used for the Ollama plugin's "additional HTTP headers" setting.
//!   Values may be marked `secret_values = true`; the API masks them
//!   on read, mirroring how `secret` strings are handled.
//! * `string_list` — variable-length list of plain strings with no
//!   header-name restriction, used for the Ollama plugin's "additional
//!   models" setting (model ids may carry `:tag` / `path/` characters).
//!
//! Storage is the `plugin_settings` table (one row per
//! `(plugin_id, key)`). Values are JSON-encoded so a key-value list can
//! round-trip through the same column as a scalar string.
//!
//! [`PluginSettingsStore`] is the Arc-cloneable handle a provider holds
//! to fetch the current configured values at request time. The schema
//! lives on the plugin; settings rows live in the database.

use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::db::Db;

/// Maximum number of stored characters in a single string value or one
/// entry of a key-value list. Picked to comfortably fit any realistic
/// secret while rejecting pathological inputs (multi-MB blobs) that
/// would bloat the DB and the HTTP response.
const MAX_STRING_LEN: usize = 8 * 1024;

/// Maximum number of entries in a single key-value list. Realistic
/// header sets are well under 16; the cap stops a runaway form from
/// turning the settings row into a multi-MB JSON blob.
const MAX_KV_ENTRIES: usize = 32;

/// One configurable field exposed by a plugin's settings schema.
///
/// The shape is shared between the backend (validation, storage,
/// runtime lookups) and the Settings UI (which renders directly from
/// the wire JSON). Fields that are inappropriate for a given variant
/// (e.g. `min` on a boolean) are dropped by `serde(skip_serializing_if)`
/// so the wire payload stays minimal.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FieldKind {
    String {
        #[serde(default, skip_serializing_if = "is_false")]
        secret: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        placeholder: Option<String>,
    },
    /// URL field. Validated as `http://` or `https://` (no `file://`,
    /// no opaque schemes) to avoid the provider being pointed at
    /// surprising transports.
    Url {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        placeholder: Option<String>,
    },
    Integer {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default: Option<i64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        min: Option<i64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max: Option<i64>,
    },
    Boolean {
        #[serde(default, skip_serializing_if = "is_false")]
        default: bool,
    },
    Enum {
        options: Vec<EnumOption>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default: Option<String>,
    },
    /// Variable-length list of `{key,value}` string pairs. Values may
    /// be marked `secret_values` so the API masks them — the Ollama
    /// "additional headers" setting uses this for `Authorization`.
    KeyValueList {
        #[serde(default, skip_serializing_if = "is_false")]
        secret_values: bool,
        /// When set, every value is validated as an http(s) URL at save
        /// time — the Ollama "servers" setting uses this so a typo'd
        /// scheme is caught in the form instead of at request time.
        #[serde(default, skip_serializing_if = "is_false")]
        url_values: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        key_placeholder: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        value_placeholder: Option<String>,
    },
    /// Variable-length list of plain strings. Unlike `KeyValueList`,
    /// entries are not constrained to the HTTP header-name token set —
    /// the Ollama "additional models" setting uses this so model ids can
    /// carry tags (`llama3.1:8b`) and registry paths (`me/model`).
    StringList {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        item_placeholder: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnumOption {
    pub value: String,
    pub label: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SettingField {
    /// Storage key. Stable across renames in the UI; treat as snake_case.
    pub key: String,
    /// Human-readable label shown above the input.
    pub title: String,
    /// Help text rendered under the label.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Whether the value must be present and non-empty for the plugin
    /// to function. Surfaced as a validation requirement on PUT.
    #[serde(default, skip_serializing_if = "is_false")]
    pub required: bool,
    #[serde(flatten)]
    pub kind: FieldKind,
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// Per-plugin settings schema. Empty means the plugin has no
/// configurable surface.
#[derive(Debug, Clone, Default, Serialize)]
pub struct SettingsSchema {
    pub fields: Vec<SettingField>,
}

impl SettingsSchema {
    pub fn new(fields: Vec<SettingField>) -> Self {
        SettingsSchema { fields }
    }

    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }

    pub fn field(&self, key: &str) -> Option<&SettingField> {
        self.fields.iter().find(|f| f.key == key)
    }
}

/// Validate `value` against the supplied `field`, returning a normalized
/// JSON value (trimmed strings, parsed integers) or a structured error
/// the route handler turns into a 400.
pub fn validate_value(
    field: &SettingField,
    value: &serde_json::Value,
) -> Result<serde_json::Value, ValidationError> {
    match &field.kind {
        FieldKind::String { .. } => {
            let s = value
                .as_str()
                .ok_or_else(|| ValidationError::wrong_type(&field.key, "string"))?;
            if s.len() > MAX_STRING_LEN {
                return Err(ValidationError::too_long(&field.key, MAX_STRING_LEN));
            }
            // Empty string is treated as "no value" — caller decides
            // whether that violates `required` (handled in `apply_updates`).
            Ok(serde_json::Value::String(s.to_string()))
        }
        FieldKind::Url { .. } => {
            let s = value
                .as_str()
                .ok_or_else(|| ValidationError::wrong_type(&field.key, "string"))?
                .trim();
            if s.is_empty() {
                return Ok(serde_json::Value::String(String::new()));
            }
            if s.len() > MAX_STRING_LEN {
                return Err(ValidationError::too_long(&field.key, MAX_STRING_LEN));
            }
            validate_http_url(&field.key, s)?;
            Ok(serde_json::Value::String(s.to_string()))
        }
        FieldKind::Integer { min, max, .. } => {
            // Accept both JSON numbers and stringified numbers — the HTML
            // <input type="number"> sometimes sends one or the other
            // depending on the form library.
            let n = if let Some(n) = value.as_i64() {
                n
            } else if let Some(s) = value.as_str() {
                s.parse::<i64>()
                    .map_err(|_| ValidationError::wrong_type(&field.key, "integer"))?
            } else {
                return Err(ValidationError::wrong_type(&field.key, "integer"));
            };
            if let Some(min) = min
                && n < *min
            {
                return Err(ValidationError::out_of_range(
                    &field.key,
                    format!("must be ≥ {min}"),
                ));
            }
            if let Some(max) = max
                && n > *max
            {
                return Err(ValidationError::out_of_range(
                    &field.key,
                    format!("must be ≤ {max}"),
                ));
            }
            Ok(serde_json::Value::Number(n.into()))
        }
        FieldKind::Boolean { .. } => {
            let b = value
                .as_bool()
                .ok_or_else(|| ValidationError::wrong_type(&field.key, "boolean"))?;
            Ok(serde_json::Value::Bool(b))
        }
        FieldKind::Enum { options, .. } => {
            let s = value
                .as_str()
                .ok_or_else(|| ValidationError::wrong_type(&field.key, "string"))?;
            if !options.iter().any(|o| o.value == s) {
                return Err(ValidationError::out_of_range(
                    &field.key,
                    "not one of the allowed values".to_string(),
                ));
            }
            Ok(serde_json::Value::String(s.to_string()))
        }
        FieldKind::KeyValueList { url_values, .. } => {
            // Accept either a JSON object ({"K": "V"}) or an array of
            // {key,value} pairs (the order-preserving form the UI uses).
            // Normalize to the array form on storage so insertion order
            // survives a round-trip.
            let pairs = parse_kv_pairs(&field.key, value)?;
            if pairs.len() > MAX_KV_ENTRIES {
                return Err(ValidationError::out_of_range(
                    &field.key,
                    format!("at most {MAX_KV_ENTRIES} entries"),
                ));
            }
            for (k, v) in &pairs {
                validate_header_name(&field.key, k)?;
                if *url_values {
                    validate_http_url(&field.key, v)?;
                }
                if v.contains('\r') || v.contains('\n') {
                    // CRLF in a header value would let a hostile setting
                    // smuggle a second header. Disallow at validation time.
                    return Err(ValidationError::out_of_range(
                        &field.key,
                        format!("value for '{k}' must not contain newlines"),
                    ));
                }
                if v.len() > MAX_STRING_LEN {
                    return Err(ValidationError::too_long(&field.key, MAX_STRING_LEN));
                }
            }
            let array: Vec<serde_json::Value> = pairs
                .into_iter()
                .map(|(k, v)| serde_json::json!({ "key": k, "value": v }))
                .collect();
            Ok(serde_json::Value::Array(array))
        }
        FieldKind::StringList { .. } => {
            let arr = value
                .as_array()
                .ok_or_else(|| ValidationError::wrong_type(&field.key, "array of strings"))?;
            let mut out: Vec<serde_json::Value> = Vec::with_capacity(arr.len());
            for entry in arr {
                let s = entry
                    .as_str()
                    .ok_or_else(|| ValidationError::wrong_type(&field.key, "array of strings"))?
                    .trim();
                // Skip blanks so a stray empty row in the form doesn't
                // register an unusable model id.
                if s.is_empty() {
                    continue;
                }
                if s.len() > MAX_STRING_LEN {
                    return Err(ValidationError::too_long(&field.key, MAX_STRING_LEN));
                }
                // Reject control characters (CR/LF/tab) — an entry flows
                // into a `provider:model` id and the model picker; a
                // smuggled newline has no legitimate use here.
                if s.chars().any(|c| c.is_control()) {
                    return Err(ValidationError::out_of_range(
                        &field.key,
                        "entries must not contain control characters".into(),
                    ));
                }
                out.push(serde_json::Value::String(s.to_string()));
            }
            if out.len() > MAX_KV_ENTRIES {
                return Err(ValidationError::out_of_range(
                    &field.key,
                    format!("at most {MAX_KV_ENTRIES} entries"),
                ));
            }
            Ok(serde_json::Value::Array(out))
        }
    }
}

fn parse_kv_pairs(
    key: &str,
    value: &serde_json::Value,
) -> Result<Vec<(String, String)>, ValidationError> {
    if let Some(arr) = value.as_array() {
        let mut out = Vec::with_capacity(arr.len());
        for entry in arr {
            let k = entry
                .get("key")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ValidationError::wrong_type(key, "array of {key,value}"))?;
            let v = entry
                .get("value")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ValidationError::wrong_type(key, "array of {key,value}"))?;
            if k.trim().is_empty() {
                continue;
            }
            out.push((k.trim().to_string(), v.to_string()));
        }
        Ok(out)
    } else if let Some(obj) = value.as_object() {
        let mut out = Vec::with_capacity(obj.len());
        for (k, v) in obj {
            let s = v
                .as_str()
                .ok_or_else(|| ValidationError::wrong_type(key, "string values"))?;
            if k.trim().is_empty() {
                continue;
            }
            out.push((k.trim().to_string(), s.to_string()));
        }
        Ok(out)
    } else {
        Err(ValidationError::wrong_type(
            key,
            "object or array of {key,value}",
        ))
    }
}

/// Limit header names to the ASCII token set HTTP defines (RFC 7230 §3.2.6).
/// This blocks CRLF / colon / whitespace from sneaking into a header line.
fn validate_header_name(field: &str, name: &str) -> Result<(), ValidationError> {
    if name.is_empty() {
        return Err(ValidationError::out_of_range(
            field,
            "header name must not be empty".into(),
        ));
    }
    for b in name.bytes() {
        let ok = b.is_ascii_alphanumeric()
            || matches!(
                b,
                b'!' | b'#'
                    | b'$'
                    | b'%'
                    | b'&'
                    | b'\''
                    | b'*'
                    | b'+'
                    | b'-'
                    | b'.'
                    | b'^'
                    | b'_'
                    | b'`'
                    | b'|'
                    | b'~'
            );
        if !ok {
            return Err(ValidationError::out_of_range(
                field,
                format!("invalid character in header name '{name}'"),
            ));
        }
    }
    Ok(())
}

fn validate_http_url(field: &str, raw: &str) -> Result<(), ValidationError> {
    // We accept http and https only. A url crate would be heavier than
    // necessary for this single check; the parsing here is intentionally
    // light — providers re-parse via reqwest at request time, which is
    // the authoritative validator.
    let lower = raw.to_ascii_lowercase();
    if !(lower.starts_with("http://") || lower.starts_with("https://")) {
        return Err(ValidationError::out_of_range(
            field,
            "URL must start with http:// or https://".into(),
        ));
    }
    // Reject control / whitespace anywhere in the URL — defence against
    // smuggled header injection if the value is ever pasted into headers.
    if raw.chars().any(|c| c.is_control() || c == ' ') {
        return Err(ValidationError::out_of_range(
            field,
            "URL must not contain whitespace or control characters".into(),
        ));
    }
    Ok(())
}

/// Validation error returned by [`validate_value`] / `apply_updates`.
/// Carries the field key plus a human message so the route can surface
/// "base_url: URL must start with http://" without further translation.
#[derive(Debug, Clone, thiserror::Error)]
#[error("{field}: {message}")]
pub struct ValidationError {
    pub field: String,
    pub message: String,
}

impl ValidationError {
    fn wrong_type(field: &str, expected: &str) -> Self {
        ValidationError {
            field: field.to_string(),
            message: format!("expected {expected}"),
        }
    }
    fn out_of_range(field: &str, msg: String) -> Self {
        ValidationError {
            field: field.to_string(),
            message: msg,
        }
    }
    fn too_long(field: &str, max: usize) -> Self {
        ValidationError {
            field: field.to_string(),
            message: format!("value exceeds {max} characters"),
        }
    }
    fn required(field: &str) -> Self {
        ValidationError {
            field: field.to_string(),
            message: "required".into(),
        }
    }
}

/// Apply `updates` to the stored settings for the plugin described by
/// `schema`. Validates every value against its declared field, persists
/// non-empty results, and deletes rows whose value the caller cleared
/// (empty string / null) so the schema default takes over.
///
/// Returns the post-update map (raw values, not masked) so the caller
/// can re-mask and return the full settings shape from a single round-
/// trip.
pub async fn apply_updates(
    db: &Db,
    plugin_id: &str,
    schema: &SettingsSchema,
    updates: &serde_json::Map<String, serde_json::Value>,
) -> Result<HashMap<String, serde_json::Value>, ValidationError> {
    // Validate everything up front so a malformed value at row 3
    // doesn't get us into a half-applied state. The actual write is
    // one transactional batch — never N round-trips.
    let mut prepared: Vec<(String, serde_json::Value)> = Vec::with_capacity(updates.len());
    for (key, value) in updates {
        let Some(field) = schema.field(key) else {
            return Err(ValidationError {
                field: key.clone(),
                message: "unknown setting".into(),
            });
        };
        if is_blank(value) {
            if field.required {
                return Err(ValidationError::required(&field.key));
            }
            prepared.push((key.clone(), serde_json::Value::Null));
            continue;
        }
        prepared.push((key.clone(), validate_value(field, value)?));
    }
    db.set_plugin_settings_batch(plugin_id, prepared)
        .await
        .map_err(storage_err)?;
    db.list_plugin_settings(plugin_id)
        .await
        .map_err(storage_err)
}

/// True if the user submitted "empty" in any of the shapes the form
/// might use to indicate "clear this field": JSON null, a whitespace-
/// only string, an empty array, or an empty object.
fn is_blank(value: &serde_json::Value) -> bool {
    matches!(value, serde_json::Value::Null)
        || value.as_str().map(|s| s.trim().is_empty()).unwrap_or(false)
        || value.as_array().map(|a| a.is_empty()).unwrap_or(false)
        || value.as_object().map(|o| o.is_empty()).unwrap_or(false)
}

/// Wrap a storage error without echoing the inner error message back to
/// the client — it may include diesel internals (and, in pathological
/// cases, fragments of the value the user tried to store).
fn storage_err(e: anyhow::Error) -> ValidationError {
    tracing::error!(error = ?e, "plugin settings storage error");
    ValidationError {
        field: String::new(),
        message: "storage error".into(),
    }
}

/// Return the effective value for `field`, falling back to the schema
/// default when no row is stored. Used by providers at request time so
/// callers don't have to merge stored-vs-default themselves.
pub fn effective_value(
    field: &SettingField,
    stored: Option<&serde_json::Value>,
) -> serde_json::Value {
    if let Some(v) = stored
        && !v.is_null()
    {
        return v.clone();
    }
    match &field.kind {
        FieldKind::String { default, .. } => default
            .clone()
            .map(serde_json::Value::String)
            .unwrap_or(serde_json::Value::Null),
        FieldKind::Url { default, .. } => default
            .clone()
            .map(serde_json::Value::String)
            .unwrap_or(serde_json::Value::Null),
        FieldKind::Integer { default, .. } => default
            .map(|n| serde_json::Value::Number(n.into()))
            .unwrap_or(serde_json::Value::Null),
        FieldKind::Boolean { default } => serde_json::Value::Bool(*default),
        FieldKind::Enum { default, .. } => default
            .clone()
            .map(serde_json::Value::String)
            .unwrap_or(serde_json::Value::Null),
        FieldKind::KeyValueList { .. } => serde_json::Value::Array(Vec::new()),
        FieldKind::StringList { .. } => serde_json::Value::Array(Vec::new()),
    }
}

/// Convert stored settings into the wire shape the Settings UI consumes.
/// Secret string values are masked to `null` and secret-valued
/// key-value entries lose their values (the keys are still echoed so
/// the user can see what's set and re-enter or remove).
///
/// `has_value` tells the UI whether a secret slot is populated — useful
/// for rendering "•••••• (set)" vs an empty placeholder without
/// disclosing the value itself.
pub fn redact_for_wire(
    schema: &SettingsSchema,
    stored: &HashMap<String, serde_json::Value>,
) -> Vec<serde_json::Value> {
    schema
        .fields
        .iter()
        .map(|field| {
            let raw = stored.get(&field.key);
            let has_value = raw.map(|v| !v.is_null()).unwrap_or(false);
            let (value, masked) = match &field.kind {
                FieldKind::String { secret, .. } if *secret => (serde_json::Value::Null, true),
                FieldKind::KeyValueList { secret_values, .. } if *secret_values => {
                    let arr = raw
                        .and_then(|v| v.as_array())
                        .cloned()
                        .unwrap_or_default()
                        .into_iter()
                        .map(|entry| {
                            let key = entry
                                .get("key")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            // Drop the value, keep the key so the user can see
                            // what's stored without exposing the secret.
                            serde_json::json!({ "key": key, "value": null })
                        })
                        .collect::<Vec<_>>();
                    (serde_json::Value::Array(arr), true)
                }
                _ => (effective_value(field, raw), false),
            };
            serde_json::json!({
                "key": field.key,
                "value": value,
                "has_value": has_value,
                "masked": masked,
            })
        })
        .collect()
}

/// Arc-cloneable handle a provider holds to read its plugin's current
/// settings on the dispatch hot path. Wraps `Db` plus a snapshot of the
/// schema so callers don't need both.
#[derive(Clone)]
pub struct PluginSettingsStore {
    plugin_id: String,
    schema: Arc<SettingsSchema>,
    db: Db,
}

impl PluginSettingsStore {
    pub fn new(plugin_id: impl Into<String>, schema: SettingsSchema, db: Db) -> Self {
        PluginSettingsStore {
            plugin_id: plugin_id.into(),
            schema: Arc::new(schema),
            db,
        }
    }

    pub fn schema(&self) -> &SettingsSchema {
        &self.schema
    }

    /// Load the effective settings: stored value where set, otherwise
    /// the schema's declared default. The map only contains fields whose
    /// effective value is non-null, so a `get_str("base_url")` is the
    /// natural lookup.
    pub async fn load(&self) -> anyhow::Result<HashMap<String, serde_json::Value>> {
        let stored = self.db.list_plugin_settings(&self.plugin_id).await?;
        let mut out = HashMap::with_capacity(self.schema.fields.len());
        for field in &self.schema.fields {
            let value = effective_value(field, stored.get(&field.key));
            if !value.is_null() {
                out.insert(field.key.clone(), value);
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url_field() -> SettingField {
        SettingField {
            key: "base_url".into(),
            title: "Base URL".into(),
            description: None,
            required: true,
            kind: FieldKind::Url {
                default: Some("http://localhost:11434".into()),
                placeholder: None,
            },
        }
    }

    fn headers_field() -> SettingField {
        SettingField {
            key: "additional_headers".into(),
            title: "Additional Headers".into(),
            description: None,
            required: false,
            kind: FieldKind::KeyValueList {
                secret_values: true,
                url_values: false,
                key_placeholder: None,
                value_placeholder: None,
            },
        }
    }

    fn servers_field() -> SettingField {
        SettingField {
            key: "servers".into(),
            title: "Servers".into(),
            description: None,
            required: false,
            kind: FieldKind::KeyValueList {
                secret_values: false,
                url_values: true,
                key_placeholder: None,
                value_placeholder: None,
            },
        }
    }

    #[test]
    fn key_value_list_url_values_rejects_non_http_urls() {
        let field = servers_field();
        validate_value(
            &field,
            &serde_json::json!([{"key": "gpu-box", "value": "http://192.168.1.50:11434"}]),
        )
        .unwrap();
        assert!(
            validate_value(
                &field,
                &serde_json::json!([{"key": "gpu-box", "value": "file:///etc/passwd"}]),
            )
            .is_err()
        );
        assert!(
            validate_value(
                &field,
                &serde_json::json!([{"key": "gpu-box", "value": "localhost:11434"}]),
            )
            .is_err()
        );
    }

    #[test]
    fn url_validation_accepts_http_and_https_only() {
        let field = url_field();
        validate_value(&field, &serde_json::json!("http://localhost:11434")).unwrap();
        validate_value(&field, &serde_json::json!("https://api.example.com")).unwrap();
        // file:// is the canonical "this would be bad" — refusing it
        // means an attacker can't redirect the provider at a local
        // socket / arbitrary path.
        assert!(validate_value(&field, &serde_json::json!("file:///etc/passwd")).is_err());
        assert!(validate_value(&field, &serde_json::json!("javascript:alert(1)")).is_err());
        // Embedded whitespace / CR in the middle of the URL is rejected
        // (trailing whitespace is stripped, which is a UX nicety).
        assert!(validate_value(&field, &serde_json::json!("http://host\nname")).is_err());
        assert!(validate_value(&field, &serde_json::json!("http://host name")).is_err());
    }

    #[test]
    fn key_value_list_rejects_crlf_and_bad_header_names() {
        let field = headers_field();

        // Happy path: header survives normalization.
        let normalized = validate_value(
            &field,
            &serde_json::json!([{"key": "Authorization", "value": "Bearer tok"}]),
        )
        .unwrap();
        let arr = normalized.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["key"], "Authorization");

        // Reject CRLF in value: prevents response-splitting style attacks
        // if the value is ever emitted into an HTTP request line.
        assert!(
            validate_value(
                &field,
                &serde_json::json!([{"key": "X-Foo", "value": "bar\r\nInjected: yes"}])
            )
            .is_err()
        );

        // Reject control / colon / whitespace in name.
        assert!(
            validate_value(
                &field,
                &serde_json::json!([{"key": "Bad: Name", "value": "v"}])
            )
            .is_err()
        );
    }

    #[test]
    fn key_value_list_caps_entries() {
        let field = headers_field();
        let mut huge = Vec::new();
        for i in 0..(MAX_KV_ENTRIES + 1) {
            huge.push(serde_json::json!({"key": format!("X-Hdr-{i}"), "value": "v"}));
        }
        assert!(validate_value(&field, &serde_json::Value::Array(huge)).is_err());
    }

    #[test]
    fn redacted_for_wire_masks_secret_kv_values() {
        let schema = SettingsSchema::new(vec![headers_field()]);
        let mut stored = HashMap::new();
        stored.insert(
            "additional_headers".to_string(),
            serde_json::json!([{"key": "Authorization", "value": "Bearer SECRET"}]),
        );
        let wire = redact_for_wire(&schema, &stored);
        assert_eq!(wire.len(), 1);
        assert_eq!(wire[0]["key"], "additional_headers");
        assert_eq!(wire[0]["has_value"], true);
        assert_eq!(wire[0]["masked"], true);
        let entries = wire[0]["value"].as_array().unwrap();
        assert_eq!(entries[0]["key"], "Authorization");
        // The secret value is gone — only the key name survives so the
        // user can see what slot is set without disclosing the secret.
        assert!(entries[0]["value"].is_null());
    }
}
