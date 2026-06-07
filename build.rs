// Tell cargo to re-run the proc-macro pipeline (and thus
// `embed_migrations!()`) whenever a migration file changes. Without
// this, adding or editing a SQL file silently produces a binary with
// the old migration set, which manifests at runtime as
// "no such table" errors.
fn main() {
    println!("cargo:rerun-if-changed=migrations");
}
