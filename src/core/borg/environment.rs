use super::{Error, PrivateState, Result};
use std::{collections::BTreeMap, ffi::OsString};

/// Closed environment. Values (including trusted operator helper configuration)
/// deliberately have no Debug/Display implementation. The map is read-only.
///
/// ```compile_fail
/// use backupsage_core::borg::BorgEnvironment;
/// fn inject(e: &mut BorgEnvironment<'_>) { e.env("BORG_REPO", "other"); }
/// ```
/// ```compile_fail
/// use backupsage_core::borg::BorgEnvironment;
/// fn inject(e: &mut BorgEnvironment<'_>) { e.as_map().insert("X".into(), "Y".into()); }
/// ```
pub struct BorgEnvironment<'a> {
    pub(super) state: &'a PrivateState,
    values: BTreeMap<OsString, OsString>,
}
impl<'a> BorgEnvironment<'a> {
    /// Inherit only trusted BORG_PASSCOMMAND; no flag/setter accepts a helper.
    /// BackupSage neither parses nor executes it. Borg performs shlex splitting.
    pub fn inherit(state: &'a PrivateState) -> Result<Self> {
        Self::from_inherited(state, std::env::vars_os())
    }
    pub(super) fn from_inherited(
        state: &'a PrivateState,
        inherited: impl IntoIterator<Item = (OsString, OsString)>,
    ) -> Result<Self> {
        state.validate()?;
        let mut values = BTreeMap::from([
            ("PATH".into(), "/usr/bin:/bin".into()),
            ("LC_ALL".into(), "C.UTF-8".into()),
            ("TZ".into(), "UTC".into()),
            ("BORG_BASE_DIR".into(), state.base.as_os_str().to_owned()),
            ("BORG_CACHE_DIR".into(), state.cache.as_os_str().to_owned()),
            (
                "BORG_SECURITY_DIR".into(),
                state.security.as_os_str().to_owned(),
            ),
            ("BORG_KEYS_DIR".into(), state.keys.as_os_str().to_owned()),
        ]);
        for (key, value) in inherited {
            if [
                "BORG_PASSPHRASE",
                "BORG_NEW_PASSPHRASE",
                "BORG_PASSPHRASE_FD",
            ]
            .iter()
            .any(|s| key == *s)
            {
                return Err(Error::ConflictingSecretEnvironment);
            }
            if key == "BORG_PASSCOMMAND" {
                values.insert(key, value);
            }
        }
        Ok(Self { state, values })
    }
    pub fn as_map(&self) -> &BTreeMap<OsString, OsString> {
        &self.values
    }
}
