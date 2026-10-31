//! Native routine comments, kept separate from the editable definition.

use super::{Driver, RoutineRef};
use crate::edit::literal;
use crate::i18n::tr;

pub fn supported(driver: Driver) -> bool {
    matches!(
        driver,
        Driver::MsSql | Driver::Postgres | Driver::MySql | Driver::MariaDb | Driver::Snowflake
    )
}

pub(super) fn query(driver: Driver, routine: &RoutineRef) -> Option<String> {
    let lit = |s: &str| literal(driver, &Some(s.to_string()));
    match driver {
        Driver::MySql | Driver::MariaDb => Some(format!(
            "SELECT '', ROUTINE_COMMENT FROM information_schema.ROUTINES WHERE ROUTINE_SCHEMA = {} AND ROUTINE_NAME = {} AND ROUTINE_TYPE = {}",
            lit(&routine.db),
            lit(&routine.name),
            lit(&routine.kind.keyword().to_uppercase())
        )),
        Driver::Snowflake => {
            let prefix = routine.kind.keyword().to_uppercase();
            Some(format!(
                "SELECT ARGUMENT_SIGNATURE, COMMENT FROM {}.INFORMATION_SCHEMA.{prefix}S WHERE {prefix}_SCHEMA = {} AND {prefix}_NAME = {}",
                driver.quote_ident(&routine.db),
                lit(&routine.schema),
                lit(&routine.name)
            ))
        }
        _ => None,
    }
}

pub fn statement(driver: Driver, routine: &RoutineRef, comment: &str) -> Result<String, String> {
    let value = literal(driver, &Some(comment.to_string()));
    let name = crate::db::Session::quote_table(driver, &routine.schema, &routine.name);
    let kind = routine.kind.keyword();
    Ok(match driver {
        Driver::MsSql => {
            if comment.encode_utf16().count() > 3750 {
                return Err(tr("SQL Server comments cannot exceed 7,500 bytes.").into());
            }
            let schema = literal(driver, &Some(routine.schema.clone()));
            let object = literal(driver, &Some(routine.name.clone()));
            let object_id = literal(driver, &Some(name));
            let exists = format!(
                "EXISTS (SELECT 1 FROM sys.extended_properties WHERE class = 1 AND major_id = OBJECT_ID({object_id}) AND minor_id = 0 AND name = N'MS_Description')"
            );
            let levels = format!(
                "@name=N'MS_Description', @level0type=N'SCHEMA', @level0name={schema}, @level1type=N'{}', @level1name={object}",
                kind.to_uppercase()
            );
            if comment.is_empty() {
                format!("IF {exists}\n    EXEC sys.sp_dropextendedproperty {levels};")
            } else {
                format!(
                    "IF {exists}\n    EXEC sys.sp_updateextendedproperty {levels}, @value={value};\nELSE\n    EXEC sys.sp_addextendedproperty {levels}, @value={value};"
                )
            }
        }
        Driver::Postgres => {
            let value = if comment.is_empty() { "NULL" } else { &value };
            format!("COMMENT ON {kind} {name}({}) IS {value}", routine.signature)
        }
        Driver::MySql | Driver::MariaDb => format!("ALTER {kind} {name} COMMENT {value}"),
        Driver::Snowflake => {
            let name = format!(
                "{}.{name}({})",
                driver.quote_ident(&routine.db),
                routine.signature
            );
            if comment.is_empty() {
                format!("ALTER {kind} {name} UNSET COMMENT")
            } else {
                format!("ALTER {kind} {name} SET COMMENT = {value}")
            }
        }
        _ => return Err(tr("Routine comments are not supported by this engine.").into()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::routines::RoutineKind;

    fn routine() -> RoutineRef {
        RoutineRef {
            conn: 1,
            db: "db".into(),
            schema: "own'er]".into(),
            name: "proc'name]".into(),
            kind: RoutineKind::Procedure,
            signature: "integer".into(),
        }
    }

    #[test]
    fn sql_server_comment_handles_add_update_delete_unicode_and_quoting() {
        let sql = statement(Driver::MsSql, &routine(), "Zażółć 'quoted' 😀").unwrap();
        assert!(sql.contains("@value=N'Zażółć ''quoted'' 😀'"));
        assert!(sql.contains("@level0name=N'own''er]'"));
        assert!(sql.contains("@level1name=N'proc''name]'"));
        assert!(
            sql.contains("sp_addextendedproperty") && sql.contains("sp_updateextendedproperty")
        );
        assert!(sql.contains("minor_id = 0") && sql.contains("class = 1"));
        let sql = statement(Driver::MsSql, &routine(), "").unwrap();
        assert!(sql.contains("sp_dropextendedproperty"));
        assert!(!sql.contains("sp_addextendedproperty"));
        assert!(statement(Driver::MsSql, &routine(), &"😀".repeat(1876)).is_err());
        assert!(statement(Driver::MsSql, &routine(), &"a".repeat(3750)).is_ok());
    }

    #[test]
    fn native_comments_preserve_overloads_and_support_removal() {
        assert_eq!(
            statement(Driver::Postgres, &routine(), "").unwrap(),
            "COMMENT ON procedure \"own'er]\".\"proc'name]\"(integer) IS NULL"
        );
        assert!(
            statement(Driver::MySql, &routine(), "it's ok")
                .unwrap()
                .ends_with("COMMENT 'it''s ok'")
        );
        assert!(
            statement(Driver::Snowflake, &routine(), "")
                .unwrap()
                .ends_with("(integer) UNSET COMMENT")
        );
        assert!(statement(Driver::Oracle, &routine(), "x").is_err());
    }
}
