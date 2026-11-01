-- Multi-certificate HTTPS: DNS provider credentials for Let's Encrypt
-- DNS-01 issuance, and the certificates the HTTPS listener serves by SNI
-- (`service::acme`). Key material lives on disk under
-- `<data>/certs/managed/<id>/`, never in the DB. Brand-new tables only —
-- no existing table is touched.
CREATE TABLE IF NOT EXISTS dns_credentials (
    id          TEXT PRIMARY KEY NOT NULL,
    provider    TEXT NOT NULL,
    label       TEXT NOT NULL,
    credentials TEXT NOT NULL,
    created_at  TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS tls_certificates (
    id                TEXT PRIMARY KEY NOT NULL,
    source            TEXT NOT NULL CHECK (source IN ('acme', 'uploaded')),
    domains           TEXT NOT NULL,
    dns_credential_id TEXT REFERENCES dns_credentials(id),
    acme_email        TEXT,
    staging           INTEGER NOT NULL DEFAULT 0,
    status            TEXT NOT NULL CHECK (status IN ('pending', 'issued', 'error')),
    last_error        TEXT,
    not_after         TEXT,
    created_at        TEXT NOT NULL,
    updated_at        TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_tls_certificates_dns_credential
    ON tls_certificates (dns_credential_id);
