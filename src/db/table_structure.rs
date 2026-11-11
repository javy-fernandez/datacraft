//! Keys, indexes and foreign keys beneath a table in the Database Explorer.
use super::{Driver, ResultSet, Session, indexes};
use crate::i18n::tr;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Group {
    Keys,
    Indexes,
    ForeignKeys,
}

impl Group {
    pub const ALL: [Self; 3] = [Self::Keys, Self::Indexes, Self::ForeignKeys];

    pub fn label(self) -> &'static str {
        match self {
            Self::Keys => tr("Keys"),
            Self::Indexes => tr("Indexes"),
            Self::ForeignKeys => tr("Foreign Keys"),
        }
    }

    pub fn empty_label(self) -> &'static str {
        match self {
            Self::Keys => tr("No keys"),
            Self::Indexes => tr("No indexes"),
            Self::ForeignKeys => tr("No foreign keys"),
        }
    }

    pub fn prefix(self) -> &'static str {
        match self {
            Self::Keys => "k",
            Self::Indexes => "i",
            Self::ForeignKeys => "fk",
        }
    }

    pub fn supported(self, driver: Driver) -> bool {
        match self {
            Self::Indexes => indexes::supported(driver),
            Self::Keys => indexes::supported(driver) || driver == Driver::Snowflake,
            Self::ForeignKeys => matches!(
                driver,
                Driver::Postgres
                    | Driver::MySql
                    | Driver::MariaDb
                    | Driver::MsSql
                    | Driver::Oracle
                    | Driver::Sqlite
                    | Driver::Snowflake
            ),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Reference {
    pub database: Option<String>,
    pub schema: String,
    pub table: String,
    pub columns: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct TableObject {
    pub name: String,
    pub columns: Vec<String>,
    pub primary_key: bool,
    pub column_names: Vec<Option<String>>,
    pub unique: bool,
    pub kind: String,
    pub definition: String,
    pub reference: Option<Reference>,
}

impl From<indexes::IndexInfo> for TableObject {
    fn from(i: indexes::IndexInfo) -> Self {
        Self {
            name: i.name,
            columns: i.columns,
            column_names: i.column_names,
            primary_key: i.primary_key,
            unique: i.unique,
            kind: i.kind,
            definition: i.definition,
            reference: None,
        }
    }
}

fn query_sql(driver: Driver, schema: &str, table: &str, group: Group) -> Vec<String> {
    let lit = |s: &str| super::table_definition::lit(driver, s);
    let (sch, tab) = (lit(schema), lit(table));
    let foreign = group == Group::ForeignKeys;
    let sql = match driver {
        Driver::Postgres => {
            let types = if foreign { "'f'" } else { "'p','u'" };
            format!(
                "SELECT con.conname AS name, a.attname AS column_name, k.ord AS position, con.contype = 'p' AS is_primary, \
                pg_get_constraintdef(con.oid, false) AS definition, rn.nspname AS ref_schema, rt.relname AS ref_table, ra.attname AS ref_column \
                FROM pg_catalog.pg_constraint con JOIN pg_catalog.pg_class t ON t.oid = con.conrelid \
                JOIN pg_catalog.pg_namespace n ON n.oid = t.relnamespace \
                CROSS JOIN LATERAL unnest(con.conkey) WITH ORDINALITY k(attnum, ord) \
                JOIN pg_catalog.pg_attribute a ON a.attrelid = t.oid AND a.attnum = k.attnum \
                LEFT JOIN pg_catalog.pg_class rt ON rt.oid = con.confrelid LEFT JOIN pg_catalog.pg_namespace rn ON rn.oid = rt.relnamespace \
                LEFT JOIN pg_catalog.pg_attribute ra ON ra.attrelid = rt.oid AND ra.attnum = con.confkey[k.ord::int] \
                WHERE n.nspname = {sch} AND t.relname = {tab} AND con.contype IN ({types}) ORDER BY con.conname, k.ord"
            )
        }
        Driver::MySql | Driver::MariaDb => {
            let types = if foreign {
                "'FOREIGN KEY'"
            } else {
                "'PRIMARY KEY','UNIQUE'"
            };
            format!(
                "SELECT k.CONSTRAINT_NAME AS name, k.COLUMN_NAME AS column_name, k.ORDINAL_POSITION AS position, \
                tc.CONSTRAINT_TYPE = 'PRIMARY KEY' AS is_primary, k.REFERENCED_TABLE_SCHEMA AS ref_schema, \
                k.REFERENCED_TABLE_NAME AS ref_table, k.REFERENCED_COLUMN_NAME AS ref_column \
                FROM information_schema.KEY_COLUMN_USAGE k JOIN information_schema.TABLE_CONSTRAINTS tc \
                ON tc.CONSTRAINT_SCHEMA = k.CONSTRAINT_SCHEMA AND tc.TABLE_NAME = k.TABLE_NAME AND tc.CONSTRAINT_NAME = k.CONSTRAINT_NAME \
                WHERE k.TABLE_SCHEMA = {sch} AND k.TABLE_NAME = {tab} AND tc.CONSTRAINT_TYPE IN ({types}) ORDER BY k.CONSTRAINT_NAME, k.ORDINAL_POSITION"
            )
        }
        Driver::MsSql if foreign => format!(
            "SELECT fk.name, c.name AS column_name, fkc.constraint_column_id AS position, \
            SCHEMA_NAME(rt.schema_id) AS ref_schema, rt.name AS ref_table, rc.name AS ref_column \
            FROM sys.foreign_keys fk JOIN sys.foreign_key_columns fkc ON fkc.constraint_object_id = fk.object_id \
            JOIN sys.tables t ON t.object_id = fk.parent_object_id JOIN sys.columns c ON c.object_id = t.object_id AND c.column_id = fkc.parent_column_id \
            JOIN sys.tables rt ON rt.object_id = fk.referenced_object_id JOIN sys.columns rc ON rc.object_id = rt.object_id AND rc.column_id = fkc.referenced_column_id \
            WHERE SCHEMA_NAME(t.schema_id) = {sch} AND t.name = {tab} ORDER BY fk.name, fkc.constraint_column_id"
        ),
        Driver::MsSql => format!(
            "SELECT kc.name, c.name AS column_name, ic.key_ordinal AS position, CASE WHEN kc.type = 'PK' THEN 1 ELSE 0 END AS is_primary \
            FROM sys.key_constraints kc JOIN sys.tables t ON t.object_id = kc.parent_object_id \
            JOIN sys.index_columns ic ON ic.object_id = t.object_id AND ic.index_id = kc.unique_index_id \
            JOIN sys.columns c ON c.object_id = t.object_id AND c.column_id = ic.column_id \
            WHERE SCHEMA_NAME(t.schema_id) = {sch} AND t.name = {tab} AND ic.key_ordinal > 0 ORDER BY kc.name, ic.key_ordinal"
        ),
        Driver::Oracle => {
            let types = if foreign { "'R'" } else { "'P','U'" };
            format!(
                "SELECT c.constraint_name AS name, cc.column_name, cc.position, CASE WHEN c.constraint_type = 'P' THEN 1 ELSE 0 END AS is_primary, \
                r.owner AS ref_schema, r.table_name AS ref_table, rc.column_name AS ref_column \
                FROM all_constraints c JOIN all_cons_columns cc ON cc.owner = c.owner AND cc.constraint_name = c.constraint_name \
                LEFT JOIN all_constraints r ON r.owner = c.r_owner AND r.constraint_name = c.r_constraint_name \
                LEFT JOIN all_cons_columns rc ON rc.owner = r.owner AND rc.constraint_name = r.constraint_name AND rc.position = cc.position \
                WHERE c.owner = {sch} AND c.table_name = {tab} AND c.constraint_type IN ({types}) ORDER BY c.constraint_name, cc.position"
            )
        }
        Driver::Sqlite if foreign => format!(
            "SELECT 'foreign_key_' || p.id AS name, p.\"from\" AS column_name, p.seq AS position, \
            {sch} AS ref_schema, p.\"table\" AS ref_table, \
            COALESCE(NULLIF(p.\"to\", ''), (SELECT name FROM pragma_table_xinfo(p.\"table\", {sch}) WHERE pk = p.seq + 1)) AS ref_column, \
            'ON DELETE ' || p.on_delete || ' ON UPDATE ' || p.on_update AS definition \
            FROM pragma_foreign_key_list({tab}, {sch}) p ORDER BY p.id, p.seq"
        ),
        Driver::Sqlite => format!(
            "SELECT 'PRIMARY' AS name, name AS column_name, pk AS position, 1 AS is_primary \
            FROM pragma_table_xinfo({tab}, {sch}) WHERE pk > 0 UNION ALL \
            SELECT il.name, ii.name, ii.seqno, 0 FROM pragma_index_list({tab}, {sch}) il \
            JOIN pragma_index_xinfo(il.name, {sch}) ii ON ii.key = 1 WHERE il.origin = 'u' ORDER BY name, position"
        ),
        Driver::ClickHouse => format!(
            "SELECT 'PRIMARY' AS name, primary_key AS column_name, 1 AS is_primary FROM system.tables \
            WHERE database = {sch} AND name = {tab} AND primary_key != ''"
        ),
        Driver::Snowflake => {
            let target = format!(
                "{}.{}",
                driver.quote_ident(schema),
                driver.quote_ident(table)
            );
            return if foreign {
                vec![format!("SHOW IMPORTED KEYS IN TABLE {target}")]
            } else {
                vec![
                    format!("SHOW PRIMARY KEYS IN TABLE {target}"),
                    format!("SHOW UNIQUE KEYS IN TABLE {target}"),
                ]
            };
        }
        _ => String::new(),
    };
    vec![sql]
}

fn decode(rs: &ResultSet, driver: Driver, group: Group, primary: bool) -> Vec<TableObject> {
    let get = |row: &[super::Cell], name: &str| {
        rs.columns
            .iter()
            .position(|c| c.eq_ignore_ascii_case(name))
            .and_then(|i| row.get(i))
            .and_then(Option::as_deref)
            .unwrap_or("")
            .to_string()
    };
    let foreign = group == Group::ForeignKeys;
    let snow = driver == Driver::Snowflake;
    let mut rows: Vec<_> = rs.rows.iter().collect();
    let name = if snow {
        if foreign {
            "fk_name"
        } else {
            "constraint_name"
        }
    } else {
        "name"
    };
    let column = if snow && foreign {
        "fk_column_name"
    } else {
        "column_name"
    };
    let position = if snow { "key_sequence" } else { "position" };
    rows.sort_by_key(|row| {
        (
            get(row, name),
            get(row, position).parse::<u32>().unwrap_or(0),
        )
    });
    let mut objects: Vec<TableObject> = Vec::new();
    for row in rows {
        let name = get(row, name);
        if objects.last().is_none_or(|o| o.name != name) {
            objects.push(TableObject {
                name,
                primary_key: primary
                    || matches!(get(row, "is_primary").as_str(), "1" | "t" | "true" | "TRUE"),
                unique: !foreign && driver != Driver::ClickHouse,
                definition: get(row, "definition"),
                reference: foreign.then(|| Reference {
                    database: snow
                        .then(|| get(row, "pk_database_name"))
                        .filter(|s| !s.is_empty()),
                    schema: get(row, if snow { "pk_schema_name" } else { "ref_schema" }),
                    table: get(row, if snow { "pk_table_name" } else { "ref_table" }),
                    columns: Vec::new(),
                }),
                ..Default::default()
            });
        }
        let object = objects.last_mut().expect("inserted key");
        let column = get(row, column);
        object
            .column_names
            .push((!column.is_empty()).then_some(column.clone()));
        object.columns.push(column);
        if let Some(reference) = &mut object.reference {
            reference
                .columns
                .push(get(row, if snow { "pk_column_name" } else { "ref_column" }));
        }
    }
    objects
}

pub async fn load(
    session: &mut Session,
    driver: Driver,
    schema: &str,
    table: &str,
    group: Group,
) -> anyhow::Result<Vec<TableObject>> {
    anyhow::ensure!(group.supported(driver), tr("Not available for this engine"));
    if group == Group::Indexes {
        return indexes::load(session, driver, schema, table)
            .await
            .map(|items| items.into_iter().map(Into::into).collect());
    }
    let mut objects = Vec::new();
    for (i, sql) in query_sql(driver, schema, table, group)
        .into_iter()
        .enumerate()
    {
        for rs in session.execute(&sql, 100_000).await? {
            anyhow::ensure!(
                !rs.truncated,
                tr("The table metadata is too large to display")
            );
            objects.extend(decode(
                &rs,
                driver,
                group,
                driver == Driver::Snowflake && group == Group::Keys && i == 0,
            ));
        }
    }
    Ok(objects)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn sqlite_keys_and_foreign_keys_keep_order_and_do_not_turn_indexes_into_constraints() {
        let path =
            std::env::temp_dir().join(format!("datacraft-structure-{}.db", uuid::Uuid::new_v4()));
        let db = rusqlite::Connection::open(&path).unwrap();
        db.execute_batch("CREATE TABLE parent(a TEXT, b INTEGER, PRIMARY KEY(b,a));
            CREATE TABLE \"orders'quoted\"(id INTEGER PRIMARY KEY, x INTEGER, y TEXT, email TEXT UNIQUE,
                FOREIGN KEY(x,y) REFERENCES parent ON DELETE CASCADE);
            CREATE UNIQUE INDEX email_expression ON \"orders'quoted\"(lower(email));
            CREATE INDEX orders_x ON \"orders'quoted\"(x);
            CREATE TABLE without_keys(value TEXT);").unwrap();
        drop(db);
        let mut cfg = super::super::ConnectionConfig::new(1, Driver::Sqlite);
        cfg.file_path = path.to_string_lossy().into_owned();
        cfg.read_only = true;
        let mut session = Session::connect(&cfg).await.unwrap();
        let keys = load(
            &mut session,
            Driver::Sqlite,
            "main",
            "orders'quoted",
            Group::Keys,
        )
        .await
        .unwrap();
        assert_eq!(
            keys.len(),
            2,
            "unique expression indexes are not UNIQUE constraints"
        );
        assert_eq!(keys.iter().find(|k| k.primary_key).unwrap().columns, ["id"]);
        assert!(
            keys.iter()
                .any(|k| !k.primary_key && k.columns == ["email"] && k.unique)
        );
        let parent_keys = load(&mut session, Driver::Sqlite, "main", "parent", Group::Keys)
            .await
            .unwrap();
        assert_eq!(parent_keys[0].columns, ["b", "a"]);
        let foreign = load(
            &mut session,
            Driver::Sqlite,
            "main",
            "orders'quoted",
            Group::ForeignKeys,
        )
        .await
        .unwrap();
        assert_eq!(foreign.len(), 1);
        assert_eq!(foreign[0].columns, ["x", "y"]);
        let reference = foreign[0].reference.as_ref().unwrap();
        assert_eq!(
            (reference.schema.as_str(), reference.table.as_str()),
            ("main", "parent")
        );
        assert_eq!(
            reference.columns,
            ["b", "a"],
            "omitted columns resolve to the declared primary key order"
        );
        assert!(foreign[0].definition.contains("ON DELETE CASCADE"));
        let indexes = load(
            &mut session,
            Driver::Sqlite,
            "main",
            "orders'quoted",
            Group::Indexes,
        )
        .await
        .unwrap();
        assert!(indexes.iter().any(|i| i.name == "email_expression"));
        assert!(indexes.iter().any(|i| i.name == "orders_x"));
        for group in Group::ALL {
            assert!(
                load(&mut session, Driver::Sqlite, "main", "without_keys", group)
                    .await
                    .unwrap()
                    .is_empty()
            );
        }
        drop(session);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn foreign_key_rows_keep_constraint_identity_and_column_pairs() {
        let rs = ResultSet {
            columns: [
                "name",
                "column_name",
                "position",
                "ref_schema",
                "ref_table",
                "ref_column",
            ]
            .map(String::from)
            .to_vec(),
            rows: vec![
                vec!["fk_second", "owner", "1", "auth", "users", "id"],
                vec!["fk_parent", "b", "2", "public", "parent", "y"],
                vec!["fk_parent", "a", "1", "public", "parent", "x"],
            ]
            .into_iter()
            .map(|r| r.into_iter().map(|v| Some(v.into())).collect())
            .collect(),
            ..Default::default()
        };
        let objects = decode(&rs, Driver::Postgres, Group::ForeignKeys, false);
        assert_eq!(objects.len(), 2);
        assert_eq!(objects[0].name, "fk_parent");
        assert_eq!(objects[0].columns, ["a", "b"]);
        assert_eq!(objects[0].reference.as_ref().unwrap().columns, ["x", "y"]);
        assert!(!objects[0].unique);
    }
}
