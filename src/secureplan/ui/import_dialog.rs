//! The import dialogs (PUB-01) are the shared SecurePlan dialogs: while the
//! drawing is read, a progress dialog with Cancel ([`super::Dialog::progress`]);
//! then the report of format, version, units, extents and warnings, or why
//! nothing changed ([`super::Dialog::notice`]). The import itself is in
//! [`crate::app::secureplan::import`].
