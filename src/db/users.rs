//! Users and privileges ("Users" / "Roles" in the Database Explorer): reading accounts and their
//! privileges, and turning an edit into the statements that apply it (a diff between the loaded
//! state and the edited one: CREATE / ALTER / RENAME / GRANT / REVOKE / DROP).
//!
//! MySQL and MariaDB manage accounts (`'user'@'host'`) with global and per-database privileges;
//! PostgreSQL manages roles with attributes, memberships and privileges on databases, schemas and
//! tables. The other engines have no user editor yet: `UserModel::of` returns `None` for them, and
//! that is the only place to extend.
//!
//! Inspired by the "Users" editor of DBCraft (https://github.com/DarkSoe/DBCraft, MIT OR
//! Apache-2.0).
//!
//! Passwords: the statements are built twice, once for the preview with the password replaced by
//! `MASK` and once with the real one only to send to the worker. They are never logged or saved.

use super::{Driver, ResultSet, Session};
use crate::i18n::tr;
use crate::ui::quote_ident_if_needed;
use anyhow::Context;
use std::collections::{BTreeMap, BTreeSet};

/// What the preview shows instead of a password.
pub const MASK: &str = "********";

/// How an engine models users. `None` for engines without a user editor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UserModel {
    /// MySQL / MariaDB accounts.
    MySql,
    /// PostgreSQL roles.
    Postgres,
}

impl UserModel {
    pub fn of(driver: Driver) -> Option<UserModel> {
        match driver {
            Driver::MySql | Driver::MariaDb => Some(UserModel::MySql),
            Driver::Postgres => Some(UserModel::Postgres),
            // Extension points: SQL Server logins, Oracle users, Snowflake users and roles…
            Driver::Sqlite | Driver::MsSql | Driver::Oracle | Driver::Snowflake | Driver::ClickHouse | Driver::Redis | Driver::MongoDb => None,
        }
    }
}

/// A MySQL account (`name`@`host`) or a PostgreSQL role (`host` empty).
#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Account {
    pub name: String,
    pub host: String,
}

impl Account {
    pub fn new(name: &str, host: &str) -> Self {
        Self { name: name.into(), host: host.into() }
    }

    /// The account as written in statements: `'bob'@'%'` or a quoted role name.
    pub fn sql(&self, driver: Driver) -> String {
        match UserModel::of(driver) {
            Some(UserModel::MySql) => format!("{}@{}", my_lit(&self.name), my_lit(&self.host)),
            _ => pg_ident(&self.name),
        }
    }

    /// The account as shown in the explorer: `bob@%` or the role name.
    pub fn label(&self, driver: Driver) -> String {
        match UserModel::of(driver) {
            Some(UserModel::MySql) => format!("{}@{}", self.name, self.host),
            _ => self.name.clone(),
        }
    }
}

/// Object privileges are granted on.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PrivTarget {
    /// MySQL: `db`.*; PostgreSQL: DATABASE db.
    Database(String),
    /// PostgreSQL: SCHEMA s (of the connection's database).
    Schema(String),
    /// PostgreSQL: TABLE s.t (of the connection's database).
    Table(String, String),
}

impl PrivTarget {
    pub fn label(&self) -> String {
        match self {
            PrivTarget::Database(d) | PrivTarget::Schema(d) => d.clone(),
            PrivTarget::Table(s, t) => format!("{s}.{t}"),
        }
    }

    /// The object in GRANT / REVOKE.
    fn sql(&self, driver: Driver) -> String {
        match (UserModel::of(driver), self) {
            (Some(UserModel::MySql), PrivTarget::Database(d)) => format!("{}.*", driver.quote_ident(d)),
            (_, PrivTarget::Database(d)) => format!("database {}", pg_ident(d)),
            (_, PrivTarget::Schema(s)) => format!("schema {}", pg_ident(s)),
            (_, PrivTarget::Table(s, t)) => format!("table {}.{}", pg_ident(s), pg_ident(t)),
        }
    }

    /// PostgreSQL privileges that apply to this kind of object.
    pub fn pg_privileges(&self) -> &'static [&'static str] {
        match self {
            PrivTarget::Database(_) => &["CONNECT", "CREATE", "TEMPORARY"],
            PrivTarget::Schema(_) => &["USAGE", "CREATE"],
            PrivTarget::Table(..) => &["SELECT", "INSERT", "UPDATE", "DELETE", "TRUNCATE", "REFERENCES", "TRIGGER"],
        }
    }
}

/// Privileges on one level (global or an object) and whether they can be granted to others.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PrivSet {
    /// Upper-case names as in GRANT (`SELECT`, `CREATE TEMPORARY TABLES`…).
    pub privs: BTreeSet<String>,
    pub grant_option: bool,
}

impl PrivSet {
    pub fn is_empty(&self) -> bool {
        self.privs.is_empty() && !self.grant_option
    }
}

/// PostgreSQL role attributes, in the order of `UserState::attrs`.
pub const ROLE_ATTRS: [&str; 7] = ["LOGIN", "SUPERUSER", "CREATEDB", "CREATEROLE", "INHERIT", "REPLICATION", "BYPASSRLS"];
/// `pg_roles` columns of `ROLE_ATTRS`.
const ROLE_ATTR_COLUMNS: [&str; 7] = ["rolcanlogin", "rolsuper", "rolcreatedb", "rolcreaterole", "rolinherit", "rolreplication", "rolbypassrls"];
const INHERIT: usize = 4;

/// MySQL resource limits (0 = unlimited), in the order of `UserState::limits`.
pub const LIMIT_CLAUSES: [&str; 4] = ["MAX_QUERIES_PER_HOUR", "MAX_UPDATES_PER_HOUR", "MAX_CONNECTIONS_PER_HOUR", "MAX_USER_CONNECTIONS"];
const LIMIT_COLUMNS: [&str; 4] = ["max_questions", "max_updates", "max_connections", "max_user_connections"];

/// `mysql.user` / `mysql.db` privilege columns and their GRANT names. Only the columns the server
/// has are offered (they vary between MySQL and MariaDB versions); `Grant_priv` is the grant option.
const MYSQL_PRIV_COLUMNS: [(&str, &str); 32] = [
    ("Select_priv", "SELECT"),
    ("Insert_priv", "INSERT"),
    ("Update_priv", "UPDATE"),
    ("Delete_priv", "DELETE"),
    ("Create_priv", "CREATE"),
    ("Drop_priv", "DROP"),
    ("Reload_priv", "RELOAD"),
    ("Shutdown_priv", "SHUTDOWN"),
    ("Process_priv", "PROCESS"),
    ("File_priv", "FILE"),
    ("References_priv", "REFERENCES"),
    ("Index_priv", "INDEX"),
    ("Alter_priv", "ALTER"),
    ("Show_db_priv", "SHOW DATABASES"),
    ("Super_priv", "SUPER"),
    ("Create_tmp_table_priv", "CREATE TEMPORARY TABLES"),
    ("Lock_tables_priv", "LOCK TABLES"),
    ("Execute_priv", "EXECUTE"),
    ("Repl_slave_priv", "REPLICATION SLAVE"),
    ("Repl_client_priv", "REPLICATION CLIENT"),
    ("Create_view_priv", "CREATE VIEW"),
    ("Show_view_priv", "SHOW VIEW"),
    ("Create_routine_priv", "CREATE ROUTINE"),
    ("Alter_routine_priv", "ALTER ROUTINE"),
    ("Create_user_priv", "CREATE USER"),
    ("Event_priv", "EVENT"),
    ("Trigger_priv", "TRIGGER"),
    ("Create_tablespace_priv", "CREATE TABLESPACE"),
    ("Create_role_priv", "CREATE ROLE"),
    ("Drop_role_priv", "DROP ROLE"),
    ("Delete_history_priv", "DELETE HISTORY"),
    ("Show_create_routine_priv", "SHOW CREATE ROUTINE"),
];

/// Authentication plugins offered for new MySQL / MariaDB users (empty = the server default).
pub fn auth_plugins(driver: Driver) -> &'static [&'static str] {
    match driver {
        Driver::MariaDb => &["", "mysql_native_password", "ed25519", "unix_socket"],
        Driver::MySql => &["", "caching_sha2_password", "mysql_native_password", "sha256_password", "auth_socket"],
        _ => &[],
    }
}

/// Plugins that authenticate through the OS socket: they take no password.
pub fn is_socket_plugin(plugin: &str) -> bool {
    matches!(plugin.trim(), "unix_socket" | "auth_socket")
}

/// A user's settings and privileges, as loaded or as edited. The password is not part of it.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct UserState {
    pub account: Account,
    /// MySQL: authentication plugin (empty = the server default).
    pub plugin: String,
    /// MySQL: ACCOUNT LOCK.
    pub locked: bool,
    /// MySQL: `LIMIT_CLAUSES`.
    pub limits: [u64; 4],
    /// MySQL: privileges ON *.*.
    pub global: PrivSet,
    /// MySQL: per database; PostgreSQL: databases, schemas and tables.
    pub grants: BTreeMap<PrivTarget, PrivSet>,
    /// PostgreSQL: `ROLE_ATTRS`.
    pub attrs: [bool; 7],
    /// PostgreSQL: CONNECTION LIMIT (-1 = unlimited).
    pub conn_limit: i64,
    /// PostgreSQL: VALID UNTIL (empty = forever).
    pub valid_until: String,
    /// PostgreSQL: roles this role is a member of.
    pub member_of: BTreeSet<String>,
}

impl UserState {
    /// A user that does not exist yet: MySQL host `%`, PostgreSQL role that can log in and inherits.
    pub fn new_user(driver: Driver) -> Self {
        let mut attrs = [false; 7];
        attrs[0] = true;
        attrs[INHERIT] = true;
        let host = if UserModel::of(driver) == Some(UserModel::MySql) { "%" } else { "" };
        Self { account: Account::new("", host), attrs, conn_limit: -1, ..Default::default() }
    }
}

/// An account in the explorer list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccountRow {
    pub account: Account,
    /// PostgreSQL: LOGIN (users) or not (group roles). Always true on MySQL.
    pub can_login: bool,
}

/// The "Users" / "Roles" folder of a connection.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UserList {
    pub accounts: Vec<AccountRow>,
    /// MySQL: the global privileges this server has (GRANT names, in `MYSQL_PRIV_COLUMNS` order).
    pub global_privileges: Vec<String>,
    /// MySQL: the database privileges this server has.
    pub db_privileges: Vec<String>,
}

pub enum UsersRequest {
    List,
    Detail(Account),
}

pub enum UsersReply {
    List(UserList),
    Detail(Account, Box<UserState>),
}

/// Reads the list or one user's detail on `session`.
pub async fn load(session: &mut Session, driver: Driver, request: &UsersRequest) -> anyhow::Result<UsersReply> {
    match (UserModel::of(driver), request) {
        (Some(UserModel::MySql), UsersRequest::List) => my_list(session).await.map(UsersReply::List),
        (Some(UserModel::MySql), UsersRequest::Detail(a)) => my_detail(session, a).await.map(|u| UsersReply::Detail(a.clone(), Box::new(u))),
        (Some(UserModel::Postgres), UsersRequest::List) => pg_list(session).await.map(UsersReply::List),
        (Some(UserModel::Postgres), UsersRequest::Detail(a)) => pg_detail(session, a).await.map(|u| UsersReply::Detail(a.clone(), Box::new(u))),
        (None, _) => anyhow::bail!(tr("Not available for this engine")),
    }
}

/// First result set of `sql` (empty if it returned none).
async fn query(session: &mut Session, sql: &str) -> anyhow::Result<ResultSet> {
    Ok(session.execute(sql, 100_000).await?.into_iter().next().unwrap_or_default())
}

fn column(rs: &ResultSet, name: &str) -> Option<usize> {
    rs.columns.iter().position(|c| c.eq_ignore_ascii_case(name))
}

fn cell<'a>(rs: &ResultSet, row: &'a [Option<String>], name: &str) -> Option<&'a str> {
    row.get(column(rs, name)?)?.as_deref()
}

fn truthy(v: Option<&str>) -> bool {
    v.is_some_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "y" | "t" | "true" | "yes" | "1"))
}

/// Privileges of a `mysql.user` / `mysql.db` row.
fn my_privs(rs: &ResultSet, row: &[Option<String>]) -> PrivSet {
    let privs = MYSQL_PRIV_COLUMNS.iter().filter(|(col, _)| truthy(cell(rs, row, col))).map(|(_, p)| p.to_string()).collect();
    PrivSet { privs, grant_option: truthy(cell(rs, row, "Grant_priv")) }
}

fn my_available(rs: &ResultSet) -> Vec<String> {
    MYSQL_PRIV_COLUMNS.iter().filter(|(col, _)| column(rs, col).is_some()).map(|(_, p)| p.to_string()).collect()
}

async fn my_list(session: &mut Session) -> anyhow::Result<UserList> {
    let context = || tr("Could not read the users (SELECT on mysql.user is needed)");
    let user_cols = query(session, "select * from mysql.user limit 0").await.with_context(context)?;
    let db_cols = query(session, "select * from mysql.db limit 0").await.with_context(context)?;
    // MariaDB keeps its roles in mysql.user too.
    let filter = if column(&user_cols, "is_role").is_some() { " where ifnull(is_role, 'N') <> 'Y'" } else { "" };
    let sql = format!("select convert(User using utf8mb4), convert(Host using utf8mb4) from mysql.user{filter} order by 1, 2");
    let rs = query(session, &sql).await.with_context(context)?;
    let accounts = rs
        .rows
        .iter()
        .map(|r| {
            let text = |i: usize| r.get(i).cloned().flatten().unwrap_or_default();
            AccountRow { account: Account { name: text(0), host: text(1) }, can_login: true }
        })
        .collect();
    Ok(UserList { accounts, global_privileges: my_available(&user_cols), db_privileges: my_available(&db_cols) })
}

async fn my_detail(session: &mut Session, account: &Account) -> anyhow::Result<UserState> {
    let filter = format!("where User = {} and Host = {}", my_lit(&account.name), my_lit(&account.host));
    let rs = query(session, &format!("select * from mysql.user {filter}")).await?;
    let Some(row) = rs.rows.first() else { anyhow::bail!(tr("The user no longer exists")) };
    let mut user = UserState {
        account: account.clone(),
        plugin: cell(&rs, row, "plugin").unwrap_or_default().to_string(),
        global: my_privs(&rs, row),
        ..Default::default()
    };
    for (limit, col) in user.limits.iter_mut().zip(LIMIT_COLUMNS) {
        *limit = cell(&rs, row, col).and_then(|v| v.trim().parse().ok()).unwrap_or(0);
    }
    // MySQL has an `account_locked` column; MariaDB only says it in SHOW CREATE USER.
    user.locked = match cell(&rs, row, "account_locked") {
        Some(v) => truthy(Some(v)),
        None => match query(session, &format!("show create user {}", account.sql(Driver::MySql))).await {
            Ok(r) => r.rows.first().and_then(|r| r.first().cloned().flatten()).is_some_and(|s| s.to_ascii_uppercase().contains(" ACCOUNT LOCK")),
            Err(_) => false,
        },
    };
    drop(rs);
    let dbs = query(session, &format!("select * from mysql.db {filter}")).await?;
    for row in &dbs.rows {
        let Some(db) = cell(&dbs, row, "Db") else { continue };
        let privs = my_privs(&dbs, row);
        if !privs.is_empty() {
            user.grants.insert(PrivTarget::Database(db.to_string()), privs);
        }
    }
    Ok(user)
}

async fn pg_list(session: &mut Session) -> anyhow::Result<UserList> {
    let rs = query(session, "select rolname, rolcanlogin from pg_roles where rolname !~ '^pg_' order by 1").await?;
    let accounts = rs
        .rows
        .iter()
        .map(|r| AccountRow {
            account: Account::new(r.first().cloned().flatten().unwrap_or_default().as_str(), ""),
            can_login: truthy(r.get(1).and_then(|v| v.as_deref())),
        })
        .collect();
    Ok(UserList { accounts, ..Default::default() })
}

async fn pg_detail(session: &mut Session, account: &Account) -> anyhow::Result<UserState> {
    let name = pg_lit(&account.name);
    let rs = query(session, &format!("select * from pg_roles where rolname = {name}")).await?;
    let Some(row) = rs.rows.first() else { anyhow::bail!(tr("The role no longer exists")) };
    let mut user = UserState { account: account.clone(), ..Default::default() };
    for (attr, col) in user.attrs.iter_mut().zip(ROLE_ATTR_COLUMNS) {
        *attr = truthy(cell(&rs, row, col));
    }
    user.conn_limit = cell(&rs, row, "rolconnlimit").and_then(|v| v.trim().parse().ok()).unwrap_or(-1);
    user.valid_until = cell(&rs, row, "rolvaliduntil").unwrap_or_default().to_string();
    let oid = format!("(select oid from pg_roles where rolname = {name})");
    let members = query(
        session,
        &format!("select r.rolname from pg_auth_members m join pg_roles r on r.oid = m.roleid where m.member = {oid} order by 1"),
    )
    .await?;
    user.member_of = members.rows.iter().filter_map(|r| r.first().cloned().flatten()).collect();
    // Explicit grants only (ACL entries for this role), not those through PUBLIC or ownership.
    let sources = [
        format!("select d.datname, null, a.privilege_type, a.is_grantable from pg_database d, aclexplode(d.datacl) a where a.grantee = {oid}"),
        format!(
            "select n.nspname, null, a.privilege_type, a.is_grantable from pg_namespace n, aclexplode(n.nspacl) a \
             where a.grantee = {oid} and n.nspname !~ '^pg_' and n.nspname <> 'information_schema'"
        ),
        format!(
            "select n.nspname, c.relname, a.privilege_type, a.is_grantable from pg_class c join pg_namespace n on n.oid = c.relnamespace, \
             aclexplode(c.relacl) a where a.grantee = {oid} and c.relkind in ('r', 'v', 'm', 'f', 'p')"
        ),
    ];
    for (kind, sql) in sources.iter().enumerate() {
        let rs = query(session, sql).await?;
        for r in &rs.rows {
            let text = |i: usize| r.get(i).cloned().flatten().unwrap_or_default();
            let target = match kind {
                0 => PrivTarget::Database(text(0)),
                1 => PrivTarget::Schema(text(0)),
                _ => PrivTarget::Table(text(0), text(1)),
            };
            let set = user.grants.entry(target).or_default();
            set.privs.insert(text(2).to_ascii_uppercase());
            set.grant_option |= truthy(r.get(3).and_then(|v| v.as_deref()));
        }
    }
    Ok(user)
}

/// MySQL string literal.
pub fn my_lit(s: &str) -> String {
    format!("'{}'", s.replace('\\', "\\\\").replace('\'', "''"))
}

/// PostgreSQL string literal (standard_conforming_strings, the default since 9.1).
pub fn pg_lit(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

fn pg_ident(s: &str) -> String {
    quote_ident_if_needed(Driver::Postgres, s)
}

fn lit(driver: Driver, s: &str) -> String {
    match UserModel::of(driver) {
        Some(UserModel::MySql) => my_lit(s),
        _ => pg_lit(s),
    }
}

fn priv_list(privs: &BTreeSet<String>) -> String {
    privs.iter().map(|p| p.to_lowercase()).collect::<Vec<_>>().join(", ")
}

/// MySQL: the authentication clause (`identified …`), or None when nothing changes.
fn my_auth(driver: Driver, plugin: &str, password: Option<&str>, plugin_changed: bool) -> Option<String> {
    let plugin = plugin.trim();
    let mariadb = driver == Driver::MariaDb;
    if is_socket_plugin(plugin) {
        return plugin_changed.then(|| if mariadb { format!("identified via {plugin}") } else { format!("identified with {plugin}") });
    }
    let pw = password.map(my_lit);
    match (pw, plugin.is_empty()) {
        (Some(pw), true) => Some(format!("identified by {pw}")),
        (Some(pw), false) if mariadb && plugin == "mysql_native_password" => Some(format!("identified by {pw}")),
        (Some(pw), false) if mariadb => Some(format!("identified via {plugin} using password({pw})")),
        (Some(pw), false) => Some(format!("identified with {plugin} by {pw}")),
        // A password plugin without a password: the dialog asks for one first.
        (None, _) => None,
    }
}

/// GRANT / REVOKE that turn `old` into `new` on one level (`on`: `*.*`, `` `db`.* ``, `table s.t`…).
fn priv_statements(driver: Driver, on: &str, to: &str, old: &PrivSet, new: &PrivSet, out: &mut Vec<String>) {
    let mysql = UserModel::of(driver) == Some(UserModel::MySql);
    let added: BTreeSet<String> = new.privs.difference(&old.privs).cloned().collect();
    let removed: BTreeSet<String> = old.privs.difference(&new.privs).cloned().collect();
    if !removed.is_empty() {
        out.push(format!("revoke {} on {on} from {to}", priv_list(&removed)));
    }
    if old.grant_option && !new.grant_option {
        if mysql {
            out.push(format!("revoke grant option on {on} from {to}"));
        } else {
            let kept: BTreeSet<String> = old.privs.intersection(&new.privs).cloned().collect();
            if !kept.is_empty() {
                out.push(format!("revoke grant option for {} on {on} from {to}", priv_list(&kept)));
            }
        }
    }
    if new.grant_option && !old.grant_option {
        // Every privilege kept or added becomes grantable (MySQL: the grant option is a privilege of the level).
        let privs = if mysql { added } else { new.privs.clone() };
        if mysql || !privs.is_empty() {
            let list = if privs.is_empty() { "usage".to_string() } else { priv_list(&privs) };
            out.push(format!("grant {list} on {on} to {to} with grant option"));
        }
    } else if !added.is_empty() {
        let with = if new.grant_option { " with grant option" } else { "" };
        out.push(format!("grant {} on {on} to {to}{with}", priv_list(&added)));
    }
}

/// Statements that turn `old` (None = a new user) into `new`. `password`: a new password (None =
/// keep it). `reveal`: false for the preview, which shows `MASK` instead of the password.
pub fn statements(driver: Driver, old: Option<&UserState>, new: &UserState, password: Option<&str>, reveal: bool) -> Vec<String> {
    let password = password.filter(|p| !p.is_empty()).map(|p| if reveal { p } else { MASK });
    let mut out = Vec::new();
    let to = new.account.sql(driver);
    match UserModel::of(driver) {
        Some(UserModel::MySql) => {
            let limits = |from: Option<&[u64; 4]>| {
                let changed: Vec<String> = LIMIT_CLAUSES
                    .iter()
                    .zip(new.limits)
                    .enumerate()
                    .filter(|(i, (_, v))| from.map_or(*v != 0, |f| f[*i] != *v))
                    .map(|(_, (c, v))| format!("{} {v}", c.to_lowercase()))
                    .collect();
                (!changed.is_empty()).then(|| format!("with {}", changed.join(" ")))
            };
            match old {
                None => {
                    let mut parts = vec![format!("create user {to}")];
                    parts.extend(my_auth(driver, &new.plugin, password, true));
                    parts.extend(limits(None));
                    if new.locked {
                        parts.push("account lock".into());
                    }
                    out.push(parts.join(" "));
                }
                Some(old) => {
                    if old.account != new.account {
                        out.push(format!("rename user {} to {to}", old.account.sql(driver)));
                    }
                    let plugin_changed = old.plugin.trim() != new.plugin.trim();
                    let mut parts = Vec::new();
                    parts.extend(my_auth(driver, &new.plugin, password, plugin_changed));
                    parts.extend(limits(Some(&old.limits)));
                    if old.locked != new.locked {
                        parts.push(if new.locked { "account lock" } else { "account unlock" }.into());
                    }
                    if !parts.is_empty() {
                        out.push(format!("alter user {to} {}", parts.join(" ")));
                    }
                }
            }
            let empty = PrivSet::default();
            priv_statements(driver, "*.*", &to, old.map_or(&empty, |o| &o.global), &new.global, &mut out);
        }
        Some(UserModel::Postgres) => {
            let attrs = |from: Option<&[bool; 7]>| -> Vec<String> {
                ROLE_ATTRS
                    .iter()
                    .zip(new.attrs)
                    .enumerate()
                    .filter(|(i, (_, on))| match from {
                        Some(f) => f[*i] != *on,
                        // CREATE ROLE: only what differs from the defaults (NOLOGIN, INHERIT…).
                        None => *on != (*i == INHERIT),
                    })
                    .map(|(_, (a, on))| if on { a.to_lowercase() } else { format!("no{}", a.to_lowercase()) })
                    .collect()
            };
            let mut parts = attrs(old.map(|o| &o.attrs));
            if old.map_or(new.conn_limit != -1, |o| o.conn_limit != new.conn_limit) {
                parts.push(format!("connection limit {}", new.conn_limit));
            }
            let valid = new.valid_until.trim();
            if old.map_or(!valid.is_empty(), |o| o.valid_until.trim() != valid) {
                parts.push(format!("valid until {}", pg_lit(if valid.is_empty() { "infinity" } else { valid })));
            }
            if let Some(pw) = password {
                parts.push(format!("password {}", pg_lit(pw)));
            }
            match old {
                None => out.push(format!("create role {to}{}", if parts.is_empty() { String::new() } else { format!(" with {}", parts.join(" ")) })),
                Some(old) => {
                    if old.account.name != new.account.name {
                        out.push(format!("alter role {} rename to {to}", old.account.sql(driver)));
                    }
                    if !parts.is_empty() {
                        out.push(format!("alter role {to} with {}", parts.join(" ")));
                    }
                }
            }
            let no_roles = BTreeSet::new();
            let old_roles = old.map_or(&no_roles, |o| &o.member_of);
            for r in new.member_of.difference(old_roles) {
                out.push(format!("grant {} to {to}", pg_ident(r)));
            }
            for r in old_roles.difference(&new.member_of) {
                out.push(format!("revoke {} from {to}", pg_ident(r)));
            }
        }
        None => return out,
    }
    let no_grants = BTreeMap::new();
    let old_grants = old.map_or(&no_grants, |o| &o.grants);
    let targets: BTreeSet<&PrivTarget> = old_grants.keys().chain(new.grants.keys()).collect();
    let empty = PrivSet::default();
    for t in targets {
        let (from, into) = (old_grants.get(t).unwrap_or(&empty), new.grants.get(t).unwrap_or(&empty));
        priv_statements(driver, &t.sql(driver), &to, from, into, &mut out);
    }
    out
}

/// DROP USER / DROP ROLE.
pub fn drop_statement(driver: Driver, account: &Account) -> String {
    match UserModel::of(driver) {
        Some(UserModel::MySql) => format!("drop user {}", account.sql(driver)),
        _ => format!("drop role {}", account.sql(driver)),
    }
}

/// `text` with every form of `password` the statements could contain replaced by `MASK` (server
/// errors may quote the statement).
pub fn mask_password(driver: Driver, text: &str, password: &str) -> String {
    if password.is_empty() {
        return text.to_string();
    }
    let quoted = lit(driver, password);
    let inner = &quoted[1..quoted.len() - 1];
    text.replace(inner, MASK).replace(password, MASK)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(privs: &[&str], grant_option: bool) -> PrivSet {
        PrivSet { privs: privs.iter().map(|p| p.to_string()).collect(), grant_option }
    }

    #[test]
    fn mysql_new_user_with_privileges_and_masked_password() {
        let mut u = UserState::new_user(Driver::MySql);
        u.account = Account::new("bob", "%");
        u.limits[3] = 5;
        u.locked = true;
        u.global = set(&["PROCESS"], false);
        u.grants.insert(PrivTarget::Database("shop".into()), set(&["SELECT", "INSERT"], true));
        let preview = statements(Driver::MySql, None, &u, Some("s3cr'et"), false);
        assert_eq!(
            preview,
            [
                "create user 'bob'@'%' identified by '********' with max_user_connections 5 account lock",
                "grant process on *.* to 'bob'@'%'",
                "grant insert, select on `shop`.* to 'bob'@'%' with grant option",
            ]
        );
        assert!(!preview.join("\n").contains("s3cr"));
        let real = statements(Driver::MySql, None, &u, Some("s3cr'et"), true);
        assert_eq!(real[0], "create user 'bob'@'%' identified by 's3cr''et' with max_user_connections 5 account lock");
    }

    #[test]
    fn mysql_plugins_per_dialect() {
        let mut u = UserState::new_user(Driver::MariaDb);
        u.account = Account::new("a", "localhost");
        u.plugin = "ed25519".into();
        assert_eq!(statements(Driver::MariaDb, None, &u, Some("pw"), true), ["create user 'a'@'localhost' identified via ed25519 using password('pw')"]);
        u.plugin = "unix_socket".into();
        assert_eq!(statements(Driver::MariaDb, None, &u, Some("pw"), true), ["create user 'a'@'localhost' identified via unix_socket"]);
        u.plugin = "caching_sha2_password".into();
        assert_eq!(statements(Driver::MySql, None, &u, Some("pw"), false), ["create user 'a'@'localhost' identified with caching_sha2_password by '********'"]);
        u.plugin.clear();
        assert_eq!(statements(Driver::MySql, None, &u, None, true), ["create user 'a'@'localhost'"]);
    }

    #[test]
    fn mysql_diff_renames_alters_grants_and_revokes() {
        let mut old = UserState::new_user(Driver::MariaDb);
        old.account = Account::new("bob", "%");
        old.plugin = "mysql_native_password".into();
        old.limits = [10, 0, 0, 0];
        old.global = set(&["SELECT", "RELOAD"], true);
        old.grants.insert(PrivTarget::Database("shop".into()), set(&["SELECT", "INSERT"], false));
        old.grants.insert(PrivTarget::Database("old`db".into()), set(&["SELECT"], false));
        assert!(statements(Driver::MariaDb, Some(&old), &old, None, true).is_empty(), "no change, no statements");

        let mut new = old.clone();
        new.account = Account::new("robert", "10.0.%");
        new.limits = [0, 0, 3, 0];
        new.locked = true;
        new.global = set(&["SELECT", "PROCESS"], false);
        new.grants.insert(PrivTarget::Database("shop".into()), set(&["SELECT", "UPDATE"], true));
        new.grants.remove(&PrivTarget::Database("old`db".into()));
        new.grants.insert(PrivTarget::Database("new_db".into()), set(&[], true));
        assert_eq!(
            statements(Driver::MariaDb, Some(&old), &new, Some("pw"), false),
            [
                "rename user 'bob'@'%' to 'robert'@'10.0.%'",
                "alter user 'robert'@'10.0.%' identified by '********' with max_queries_per_hour 0 max_connections_per_hour 3 account lock",
                "revoke reload on *.* from 'robert'@'10.0.%'",
                "revoke grant option on *.* from 'robert'@'10.0.%'",
                "grant process on *.* to 'robert'@'10.0.%'",
                "grant usage on `new_db`.* to 'robert'@'10.0.%' with grant option",
                "revoke select on `old``db`.* from 'robert'@'10.0.%'",
                "revoke insert on `shop`.* from 'robert'@'10.0.%'",
                "grant update on `shop`.* to 'robert'@'10.0.%' with grant option",
            ]
        );
    }

    #[test]
    fn mysql_quoting_and_drop() {
        let a = Account::new("o'neil\\x", "%.example.com");
        assert_eq!(a.sql(Driver::MySql), "'o''neil\\\\x'@'%.example.com'");
        assert_eq!(drop_statement(Driver::MariaDb, &a), "drop user 'o''neil\\\\x'@'%.example.com'");
        assert_eq!(a.label(Driver::MySql), "o'neil\\x@%.example.com");
    }

    #[test]
    fn postgres_new_role() {
        let mut u = UserState::new_user(Driver::Postgres);
        u.account = Account::new("App User", "");
        u.attrs[2] = true; // CREATEDB
        u.attrs[INHERIT] = false;
        u.conn_limit = 4;
        u.valid_until = "2027-01-01".into();
        u.member_of.insert("readers".into());
        u.grants.insert(PrivTarget::Database("shop".into()), set(&["CONNECT"], false));
        u.grants.insert(PrivTarget::Schema("public".into()), set(&["USAGE"], false));
        u.grants.insert(PrivTarget::Table("public".into(), "Orders".into()), set(&["SELECT", "UPDATE"], true));
        let preview = statements(Driver::Postgres, None, &u, Some("it's"), false);
        assert_eq!(
            preview,
            [
                "create role \"App User\" with login createdb noinherit connection limit 4 valid until '2027-01-01' password '********'",
                "grant readers to \"App User\"",
                "grant connect on database shop to \"App User\"",
                "grant usage on schema public to \"App User\"",
                "grant select, update on table public.\"Orders\" to \"App User\" with grant option",
            ]
        );
        assert!(statements(Driver::Postgres, None, &u, Some("it's"), true)[0].ends_with("password 'it''s'"));
    }

    #[test]
    fn postgres_diff() {
        let mut old = UserState::new_user(Driver::Postgres);
        old.account = Account::new("app", "");
        old.valid_until = "2026-01-01 00:00:00+00".into();
        old.member_of = ["readers".to_string(), "writers".to_string()].into();
        old.grants.insert(PrivTarget::Table("public".into(), "t".into()), set(&["SELECT", "INSERT"], true));
        assert!(statements(Driver::Postgres, Some(&old), &old, None, true).is_empty());

        let mut new = old.clone();
        new.account.name = "service".into();
        new.attrs[0] = false; // NOLOGIN
        new.attrs[1] = true; // SUPERUSER
        new.conn_limit = 10;
        new.valid_until.clear();
        new.member_of = ["admins".to_string(), "readers".to_string()].into();
        new.grants.insert(PrivTarget::Table("public".into(), "t".into()), set(&["SELECT", "DELETE"], false));
        new.grants.insert(PrivTarget::Database("shop".into()), set(&["TEMPORARY"], false));
        assert_eq!(
            statements(Driver::Postgres, Some(&old), &new, None, true),
            [
                "alter role app rename to service",
                "alter role service with nologin superuser connection limit 10 valid until 'infinity'",
                "grant admins to service",
                "revoke writers from service",
                "grant temporary on database shop to service",
                "revoke insert on table public.t from service",
                "revoke grant option for select on table public.t from service",
                "grant delete on table public.t to service",
            ]
        );
        assert_eq!(drop_statement(Driver::Postgres, &new.account), "drop role service");
    }

    #[test]
    fn grant_option_turned_on_regrants_everything_on_postgres() {
        let old = set(&["USAGE"], false);
        let new = set(&["USAGE", "CREATE"], true);
        let mut out = Vec::new();
        priv_statements(Driver::Postgres, "schema s", "r", &old, &new, &mut out);
        assert_eq!(out, ["grant create, usage on schema s to r with grant option"]);
    }

    #[test]
    fn errors_are_masked() {
        let err = "You have an error near 'identified by 'p''w' at line 1";
        assert_eq!(mask_password(Driver::MySql, err, "p'w"), "You have an error near 'identified by '********' at line 1");
        assert_eq!(mask_password(Driver::Postgres, "bad pw secret", "secret"), "bad pw ********");
        assert_eq!(mask_password(Driver::Postgres, "x", ""), "x");
    }

    #[test]
    fn engines_without_users() {
        for d in [Driver::Sqlite, Driver::MsSql, Driver::Oracle, Driver::Snowflake, Driver::Redis, Driver::MongoDb] {
            assert_eq!(UserModel::of(d), None);
            assert!(statements(d, None, &UserState::default(), Some("x"), true).is_empty());
        }
    }
}
