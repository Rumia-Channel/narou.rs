//! Credential selection for the download pipeline.
//!
//! One download keeps a single [`AuthState`]: the site's own cookie, the
//! stored logins it may use and the login that is currently in effect.
//!
//! Two rules shape the module:
//!
//! - Every candidate starts from the site's own cookie, so one account's
//!   cookies never leak into another account's request.
//! - The stored logins are read from the store once per run, and only when
//!   a credential is selected or recovered. A successful section never
//!   touches the store.

use crate::platform::LoginGroup;

use super::site_setting::SiteSetting;

/// The login a run sends.
#[derive(Clone, Debug)]
pub(crate) struct ActiveLogin {
    /// Identifier the login is remembered by.
    pub id: String,
    /// The cookies of the login, merged over the site's own hosts.
    pub cookie: String,
    /// Hosts the login's cookies may be sent to.
    pub hosts: Vec<String>,
}

/// The credentials one download may use.
pub(crate) struct AuthState {
    /// The site's own cookie, without any stored login.
    configured_cookie: Option<String>,
    /// The stored logins of the site; read once, when they are needed.
    groups: Option<Vec<LoginGroup>>,
    /// The login in effect, if any.
    active: Option<ActiveLogin>,
    /// Indexes of the logins already used in this run.
    used: Vec<usize>,
}

impl AuthState {
    /// Start a run that sends the site's own cookie.
    pub fn new(configured_cookie: Option<String>) -> Self {
        Self {
            configured_cookie,
            groups: None,
            active: None,
            used: Vec::new(),
        }
    }

    /// The stored logins of the site, once they have been read.
    pub fn groups(&self) -> Option<&[LoginGroup]> {
        self.groups.as_deref()
    }

    /// Remember the stored logins of the site.
    pub fn set_groups(&mut self, groups: Vec<LoginGroup>) {
        self.groups = Some(groups);
    }

    /// The login in effect.
    pub fn active(&self) -> Option<&ActiveLogin> {
        self.active.as_ref()
    }

    /// Whether the run sends a stored login.
    pub fn requires_login(&self) -> bool {
        self.active.is_some()
    }

    /// The index of the login the record remembers.
    pub fn index_of(&self, id: &str) -> Option<usize> {
        self.groups
            .as_deref()?
            .iter()
            .position(|group| group.id == id)
    }

    /// The index of the first login that has not been used yet.
    pub fn next_unused(&self) -> Option<usize> {
        let groups = self.groups.as_deref()?;
        groups
            .iter()
            .enumerate()
            .find(|(index, _)| !self.used.contains(index))
            .map(|(index, _)| index)
    }

    /// Send a stored login, starting from the site's own cookie.
    ///
    /// The login's cookies replace the site's own cookie of the same name.
    /// Cookies of another login never survive, because every candidate
    /// starts from the pristine configured cookie.
    pub fn apply(&mut self, setting: &mut SiteSetting, index: usize) -> bool {
        let details = self
            .groups
            .as_deref()
            .and_then(|groups| groups.get(index))
            .map(|group| {
                (
                    group.merged_cookie(),
                    group
                        .cookies
                        .iter()
                        .map(|entry| entry.host.clone())
                        .collect::<Vec<_>>(),
                    group.id.clone(),
                )
            });
        let Some((cookie, hosts, id)) = details else {
            return false;
        };
        if cookie.is_empty() {
            // A login without cookies cannot authenticate anything.
            self.used.push(index);
            return false;
        }
        self.used.push(index);
        self.active = Some(ActiveLogin {
            id,
            cookie: cookie.clone(),
            hosts: hosts.clone(),
        });
        setting.cookie =
            crate::platform::merge_cookie_headers(self.configured_cookie.as_deref(), Some(&cookie));
        setting.login_cookie_hosts = hosts;
        true
    }

    /// Send the first login that has not been used yet.
    pub fn apply_next(&mut self, setting: &mut SiteSetting) -> bool {
        while let Some(index) = self.next_unused() {
            if self.apply(setting, index) {
                return true;
            }
        }
        false
    }

    /// Send `login` again, or the site's own cookie when there is none.
    ///
    /// A candidate that did not improve the listing is undone this way, so
    /// the run keeps the credential it started with.
    pub fn restore(&mut self, setting: &mut SiteSetting, login: Option<&ActiveLogin>) {
        let Some(login) = login else {
            self.apply_configured(setting);
            return;
        };
        setting.cookie = crate::platform::merge_cookie_headers(
            self.configured_cookie.as_deref(),
            Some(&login.cookie),
        );
        setting.login_cookie_hosts = login.hosts.clone();
        self.active = Some(login.clone());
    }

    /// Send the site's own cookie only and forget the login in effect.
    pub fn apply_configured(&mut self, setting: &mut SiteSetting) {
        setting.cookie = self.configured_cookie.clone();
        setting.login_cookie_hosts.clear();
        self.active = None;
    }
}
