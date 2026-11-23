//! Proprietary-content guard. The patterns live in the shared
//! `peckboard-skill-format` crate (the public skills hub runs the same
//! check); this adapter feeds it this instance's configured secret values
//! (`service::secret_mask` — Settings → Environment Variables, sensitive
//! host env, the server's own keys).

use peckboard_skill_format::guard as shared;

use crate::service::secret_mask::{self, SecretMasker};

pub use shared::Finding;

impl shared::SecretValues for SecretMasker {
    fn is_empty(&self) -> bool {
        SecretMasker::is_empty(self)
    }

    fn mask_line(&self, line: &str) -> Option<(String, usize)> {
        match self.mask(line) {
            std::borrow::Cow::Owned(masked) => {
                let focus = masked.find(secret_mask::MASK).unwrap_or(0);
                Some((masked, focus))
            }
            std::borrow::Cow::Borrowed(_) => None,
        }
    }
}

/// Scan `text`. `secrets` holds the configured secret values (see
/// [`check`]); pass an empty masker to skip that pass.
pub fn scan(text: &str, secrets: &SecretMasker) -> Vec<Finding> {
    shared::scan(text, secrets)
}

/// [`scan`] with the configured secrets of this instance (every env-var
/// value from every scope, sensitive host env, the server's own keys).
pub async fn check(db: &crate::db::Db, text: &str) -> Vec<Finding> {
    scan(text, &masker(db).await)
}

async fn masker(db: &crate::db::Db) -> SecretMasker {
    let db = db.clone();
    tokio::task::spawn_blocking(move || secret_mask::masker_blocking(&db))
        .await
        .unwrap_or_else(|_| SecretMasker::new(Vec::new()))
}

/// [`check`] for a document whose `body` the user wrote and whose metadata
/// (name, description, tags, title — `meta`) rides in frontmatter. Body
/// findings keep the body's own 1-based line numbers (what the user sees in
/// the editor); metadata findings are reported as line 0.
pub async fn check_document(db: &crate::db::Db, meta: &str, body: &str) -> Vec<Finding> {
    shared::scan_document(meta, body, &masker(db).await)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configured_secret_values_are_flagged_without_echoing_them() {
        let masker = SecretMasker::new(vec!["hunter2-very-secret".to_string()]);
        let f = scan("password is hunter2-very-secret ok", &masker);
        let hit = f.iter().find(|f| f.kind == "configured_secret").unwrap();
        assert!(!hit.excerpt.contains("hunter2"));
    }

    /// Findings point at the body line the user wrote, not at a line of the
    /// serialized file (whose frontmatter would shift them).
    #[tokio::test]
    async fn document_findings_use_body_line_numbers_and_meta_is_line_zero() {
        let db = crate::db::Db::in_memory().unwrap();
        let body = "How we deploy:\nssh into pg01.prod.internal first\nthen export AWS_KEY=AKIAABCDEFGHIJKLMNOP\n";
        let f = check_document(&db, "notes\nmail ops@acme-corp.com", body).await;
        let lines: Vec<(usize, &str)> = f.iter().map(|x| (x.line, x.kind.as_str())).collect();
        assert!(lines.contains(&(0, "email")), "{lines:?}");
        assert!(lines.contains(&(2, "internal_host")), "{lines:?}");
        assert!(lines.contains(&(3, "aws_access_key")), "{lines:?}");
    }
}
