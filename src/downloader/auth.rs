//! Credential selection for the download pipeline.
//!
//! One download keeps a single [`AuthState`]: the site's own cookie, the
//! stored logins it may use and the login that is currently in effect.
//!
//! Three rules shape the module:
//!
//! - Every candidate starts from the site's own cookie, so one account's
//!   cookies never leak into another account's request.
//! - The stored logins are read from the store once per run, and only when
//!   a credential is selected or recovered. A successful section never
//!   touches the store.
//! - A browser-issued token may be renewed once per logical operation. The
//!   renewed login replaces the stored one with the same id, so the account
//!   keeps its identity while its cookie becomes usable again.

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
    /// Whether the browser-issued token was already renewed in the current
    /// logical operation.
    token_refreshed: bool,
}

impl AuthState {
    /// Start a run that sends the site's own cookie.
    pub fn new(configured_cookie: Option<String>) -> Self {
        Self {
            configured_cookie,
            groups: None,
            active: None,
            used: Vec::new(),
            token_refreshed: false,
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
        let Some(cookie) = self
            .groups
            .as_deref()
            .and_then(|groups| groups.get(index))
            .map(|group| group.merged_cookie())
        else {
            return false;
        };
        if cookie.is_empty() {
            // A login without cookies cannot authenticate anything.
            self.used.push(index);
            return false;
        }
        let Some(group) = self
            .groups
            .as_deref()
            .and_then(|groups| groups.get(index))
        else {
            return false;
        };
        let id = group.id.clone();
        let hosts: Vec<String> = group
            .cookies
            .iter()
            .map(|entry| entry.host.clone())
            .collect();
        // Every candidate starts from the site's own cookie, so one account's
        // cookies never reach another account's request.
        setting.cookie = self.configured_cookie.clone();
        super::apply_login_cookie(setting, &cookie, &group.cookies);
        // The stored login is not read again, so its borrow ends here.
        self.used.push(index);
        self.active = Some(ActiveLogin { id, cookie, hosts });
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
    /// the run keeps the credential it started with. The account is the one
    /// that is kept, not its cookie: a token renewed in the meantime is used
    /// instead of the expired one the snapshot holds.
    pub fn restore(&mut self, setting: &mut SiteSetting, login: Option<&ActiveLogin>) {
        let Some(login) = login else {
            self.apply_configured(setting);
            return;
        };
        let current = self
            .groups
            .as_deref()
            .and_then(|groups| {
                groups
                    .iter()
                    .find(|group| !login.id.is_empty() && group.id == login.id)
            })
            .map(|group| {
                (
                    group.merged_cookie(),
                    group
                        .cookies
                        .iter()
                        .map(|entry| entry.host.clone())
                        .collect::<Vec<_>>(),
                )
            })
            .filter(|(cookie, _)| !cookie.is_empty());
        let (cookie, hosts) = current.unwrap_or_else(|| (login.cookie.clone(), login.hosts.clone()));
        setting.cookie = crate::platform::merge_cookie_headers(
            self.configured_cookie.as_deref(),
            Some(&cookie),
        );
        setting.login_cookie_hosts = hosts.clone();
        self.active = Some(ActiveLogin {
            id: login.id.clone(),
            cookie,
            hosts,
        });
    }

    /// Claim the single browser-token renewal of the current operation.
    ///
    /// The claim is given up by [`Self::reset_token_refresh`], which the
    /// caller does when the next operation starts. A renewed token that is
    /// still rejected must not start another renewal.
    pub fn claim_token_refresh(&mut self) -> bool {
        if self.token_refreshed {
            return false;
        }
        self.token_refreshed = true;
        true
    }

    /// Start a new operation, which may renew the browser-issued token again.
    pub fn reset_token_refresh(&mut self) {
        self.token_refreshed = false;
    }

    /// Send the renewed login and remember it under its own id.
    ///
    /// The renewed login replaces the stored one with the same id, so the
    /// account keeps its identity. The logins already tried stay tried, and
    /// the renewed login's cookies replace the ones in effect — one account's
    /// cookies never mix into another account's request.
    pub fn apply_refreshed(&mut self, setting: &mut SiteSetting, group: LoginGroup) -> bool {
        let cookie = group.merged_cookie();
        if cookie.is_empty() {
            // A login without cookies cannot authenticate anything.
            return false;
        }
        let hosts: Vec<String> = group.cookies.iter().map(|entry| entry.host.clone()).collect();
        let id = group.id.clone();
        // The site's own cookie is the base: the renewed login only replaces
        // the cookies it carries.
        setting.cookie = self.configured_cookie.clone();
        super::apply_login_cookie(setting, &cookie, &group.cookies);
        if !id.is_empty()
            && let Some(groups) = self.groups.as_mut()
        {
            // The position is read before the group is written: the borrow of
            // the search must not outlive it.
            let found = groups.iter().position(|existing| existing.id == id);
            let index = match found {
                Some(index) => {
                    groups[index] = group;
                    index
                }
                None => {
                    groups.push(group);
                    groups.len() - 1
                }
            };
            // The renewed login was tried already; it is not a new candidate.
            if !self.used.contains(&index) {
                self.used.push(index);
            }
        }
        self.active = Some(ActiveLogin { id, cookie, hosts });
        true
    }

    /// Send the site's own cookie only and forget the login in effect.
    pub fn apply_configured(&mut self, setting: &mut SiteSetting) {
        setting.cookie = self.configured_cookie.clone();
        setting.login_cookie_hosts.clear();
        self.active = None;
    }
}
