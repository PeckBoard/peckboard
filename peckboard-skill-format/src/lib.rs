//! `SKILL.md` format + proprietary-content guard, shared by PeckBoard and
//! the public skills hub (`peckboard-skills-hub`), so a skill is parsed,
//! validated and screened identically on both ends.
//!
//! - [`format`] — frontmatter parser/serializer, name/semver validation.
//! - [`guard`] — refuses text that would leak a credential or an internal
//!   detail. Instance secret values are a parameter ([`guard::SecretValues`]);
//!   the hub passes [`guard::NoSecrets`].

pub mod format;
pub mod guard;
mod patterns;
