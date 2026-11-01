use diesel::prelude::*;

use crate::db::Db;
use crate::db::models::*;
use crate::db::schema::*;

impl Db {
    /// Every stored DNS credential, oldest first.
    pub async fn list_dns_credentials(&self) -> anyhow::Result<Vec<DnsCredentialRow>> {
        self.with_conn(move |conn| {
            dns_credentials::table
                .select(DnsCredentialRow::as_select())
                .order(dns_credentials::created_at.asc())
                .load(conn)
                .map_err(Into::into)
        })
        .await
    }

    pub async fn get_dns_credential(&self, id: &str) -> anyhow::Result<Option<DnsCredentialRow>> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            dns_credentials::table
                .find(&id)
                .select(DnsCredentialRow::as_select())
                .first(conn)
                .optional()
                .map_err(Into::into)
        })
        .await
    }

    pub async fn insert_dns_credential(&self, row: DnsCredentialRow) -> anyhow::Result<()> {
        self.with_conn(move |conn| {
            diesel::insert_into(dns_credentials::table)
                .values(&row)
                .execute(conn)?;
            Ok(())
        })
        .await
    }

    /// Delete a credential unless a certificate still references it.
    /// `Ok(None)` = not found, `Ok(Some(false))` = in use, `Ok(Some(true))`
    /// = deleted. The check and delete run under one connection lock so a
    /// concurrent certificate insert can't slip between them.
    pub async fn delete_dns_credential(&self, id: &str) -> anyhow::Result<Option<bool>> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            let in_use: i64 = tls_certificates::table
                .filter(tls_certificates::dns_credential_id.eq(&id))
                .count()
                .get_result(conn)?;
            if in_use > 0 {
                let exists: i64 = dns_credentials::table.find(&id).count().get_result(conn)?;
                return Ok(if exists > 0 { Some(false) } else { None });
            }
            let n = diesel::delete(dns_credentials::table.find(&id)).execute(conn)?;
            Ok(if n > 0 { Some(true) } else { None })
        })
        .await
    }

    /// Every certificate row, oldest first.
    pub async fn list_tls_certificates(&self) -> anyhow::Result<Vec<TlsCertificateRow>> {
        self.with_conn(move |conn| {
            tls_certificates::table
                .select(TlsCertificateRow::as_select())
                .order(tls_certificates::created_at.asc())
                .load(conn)
                .map_err(Into::into)
        })
        .await
    }

    pub async fn get_tls_certificate(&self, id: &str) -> anyhow::Result<Option<TlsCertificateRow>> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            tls_certificates::table
                .find(&id)
                .select(TlsCertificateRow::as_select())
                .first(conn)
                .optional()
                .map_err(Into::into)
        })
        .await
    }

    pub async fn insert_tls_certificate(&self, row: TlsCertificateRow) -> anyhow::Result<()> {
        self.with_conn(move |conn| {
            diesel::insert_into(tls_certificates::table)
                .values(&row)
                .execute(conn)?;
            Ok(())
        })
        .await
    }

    /// Record an issuance outcome. `not_after` is left unchanged when
    /// `None` (a failed renewal keeps the still-valid cert's expiry).
    pub async fn update_tls_certificate_status(
        &self,
        id: &str,
        status: &str,
        last_error: Option<String>,
        not_after: Option<String>,
    ) -> anyhow::Result<bool> {
        let id = id.to_string();
        let status = status.to_string();
        let now = chrono::Utc::now().to_rfc3339();
        self.with_conn(move |conn| {
            let target = tls_certificates::table.find(&id);
            let n = match not_after {
                Some(na) => diesel::update(target)
                    .set((
                        tls_certificates::status.eq(&status),
                        tls_certificates::last_error.eq(&last_error),
                        tls_certificates::not_after.eq(Some(na)),
                        tls_certificates::updated_at.eq(&now),
                    ))
                    .execute(conn)?,
                None => diesel::update(target)
                    .set((
                        tls_certificates::status.eq(&status),
                        tls_certificates::last_error.eq(&last_error),
                        tls_certificates::updated_at.eq(&now),
                    ))
                    .execute(conn)?,
            };
            Ok(n > 0)
        })
        .await
    }

    pub async fn delete_tls_certificate(&self, id: &str) -> anyhow::Result<bool> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            let n = diesel::delete(tls_certificates::table.find(&id)).execute(conn)?;
            Ok(n > 0)
        })
        .await
    }
}
