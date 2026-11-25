//! The whole life of a procedure and a function on real servers: create, list, read back,
//! edit, rename, call and drop. Run with `cargo test db::routines -- --ignored` and the
//! variables of each driver's live tests (DATACRAFT_PG_HOST, DATACRAFT_MY_HOST,
//! DATACRAFT_MSSQL_HOST, DATACRAFT_ORA_HOST…); engines without them are skipped.

use super::*;
use crate::db::{ConnectionConfig, ResultSet};

fn env(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_string())
}

/// Connection settings from the environment, or `None` to skip the engine.
fn config(driver: Driver) -> Option<ConnectionConfig> {
    let (prefix, port, user, db) = match driver {
        Driver::Postgres => ("PG", "5432", "postgres", "postgres"),
        Driver::MySql => ("MY", "3306", "root", ""),
        Driver::MariaDb => ("MARIA", "3306", "root", ""),
        Driver::MsSql => ("MSSQL", "1433", "sa", "master"),
        Driver::Oracle => ("ORA", "1521", "system", "FREEPDB1"),
        _ => return None,
    };
    let Ok(host) = std::env::var(format!("DATACRAFT_{prefix}_HOST")) else {
        eprintln!("DATACRAFT_{prefix}_HOST not set: skipping the {driver:?} routine test");
        return None;
    };
    let mut cfg = ConnectionConfig::new(1, driver);
    cfg.host = host;
    cfg.port = env(&format!("DATACRAFT_{prefix}_PORT"), port)
        .parse()
        .expect("port");
    cfg.user = env(&format!("DATACRAFT_{prefix}_USER"), user);
    cfg.password = env(&format!("DATACRAFT_{prefix}_PASSWORD"), "");
    cfg.database = env(
        &format!(
            "DATACRAFT_{prefix}_{}",
            if driver == Driver::Oracle {
                "SERVICE"
            } else {
                "DB"
            }
        ),
        db,
    );
    Some(cfg)
}

async fn run(s: &mut Session, st: &RoutineStatements) -> anyhow::Result<()> {
    if st.transaction {
        s.run_transaction(&st.statements).await?;
    } else {
        for stmt in &st.statements {
            s.execute(stmt, 0).await?;
        }
    }
    Ok(())
}

/// Runs a call script as the console would and returns its last result set with rows.
async fn call(s: &mut Session, driver: Driver, script: &str) -> ResultSet {
    let mut last = None;
    for stmt in crate::sql::split_batches(script, driver) {
        let results = s
            .execute(&stmt.text, 100)
            .await
            .unwrap_or_else(|e| panic!("{script}\n→ {e:#}"));
        if let Some(rs) = results.into_iter().rfind(|r| !r.columns.is_empty()) {
            last = Some(rs);
        }
    }
    last.unwrap_or_else(|| panic!("no rows from {script}"))
}

fn p(name: &str, mode: ParamMode, ty: &str, default: &str) -> Param {
    Param {
        name: name.into(),
        mode,
        data_type: ty.into(),
        default: default.into(),
    }
}

/// `dc_total(a, b = 2) = a * b` and `dc_split(total, OUT half)`.
fn specs(driver: Driver, schema: &str) -> (RoutineSpec, RoutineSpec) {
    let mut f = RoutineSpec::new(driver, RoutineKind::Function, schema.into());
    let mut pr = RoutineSpec::new(driver, RoutineKind::Procedure, schema.into());
    f.name = "dc_total".into();
    pr.name = "dc_split".into();
    let default = if has_defaults(driver) { "2" } else { "" };
    match driver {
        Driver::Postgres => {
            f.params = vec![
                p("a", ParamMode::In, "integer", ""),
                p("b", ParamMode::In, "integer", default),
            ];
            f.options.volatility = "IMMUTABLE".into();
            f.body = "begin\n    return a * b;\nend;".into();
            pr.params = vec![
                p("total", ParamMode::In, "integer", ""),
                p("half", ParamMode::Out, "integer", ""),
            ];
            pr.body = "begin\n    half := total / 2;\nend;".into();
        }
        Driver::MySql | Driver::MariaDb => {
            f.params = vec![
                p("a", ParamMode::In, "int", ""),
                p("b", ParamMode::In, "int", ""),
            ];
            f.options.deterministic = true;
            f.options.data_access = "NO SQL".into();
            f.body = "return a * b".into();
            pr.params = vec![
                p("total", ParamMode::In, "int", ""),
                p("half", ParamMode::Out, "int", ""),
            ];
            pr.body = "begin\n    set half = total div 2;\nend".into();
        }
        Driver::MsSql => {
            f.params = vec![
                p("@a", ParamMode::In, "int", ""),
                p("@b", ParamMode::In, "int", default),
            ];
            f.body = "begin\n    return @a * @b;\nend".into();
            pr.params = vec![
                p("@total", ParamMode::In, "int", ""),
                p("@half", ParamMode::Out, "int", ""),
            ];
            pr.body = "begin\n    set @half = @total / 2;\nend".into();
        }
        Driver::Oracle => {
            f.params = vec![
                p("a", ParamMode::In, "number", ""),
                p("b", ParamMode::In, "number", default),
            ];
            f.options.deterministic = true;
            f.body = "begin\n    return a * b;\nend;".into();
            pr.params = vec![
                p("total", ParamMode::In, "number", ""),
                p("half", ParamMode::Out, "number", ""),
            ];
            pr.body = "begin\n    half := total / 2;\nend;".into();
        }
        _ => {}
    }
    (f, pr)
}

async fn find(s: &mut Session, driver: Driver, db: &str, name: &str) -> Option<RoutineInfo> {
    let list = s.list_routines(driver, db).await.unwrap();
    list.into_iter().find(|r| r.name.eq_ignore_ascii_case(name))
}

async fn lifecycle(driver: Driver) {
    let Some(cfg) = config(driver) else { return };
    let mut s = Session::connect(&cfg).await.expect("connection");
    let db = s.current_database().await.unwrap();
    let schema = match driver {
        Driver::Postgres => "public".to_string(),
        Driver::MsSql => "dbo".to_string(),
        Driver::Oracle => cfg.user.to_uppercase(),
        _ => db.clone(),
    };
    let names = ["dc_total", "dc_total2", "dc_split", "dc_broken"];
    for name in names {
        for kind in [RoutineKind::Function, RoutineKind::Procedure] {
            if let Some(r) = find(&mut s, driver, &db, name)
                .await
                .filter(|r| r.kind == kind)
            {
                let _ = s
                    .execute(&drop_sql(driver, kind, &r.schema, &r.name, &r.signature), 0)
                    .await;
            }
        }
    }
    let (f, pr) = specs(driver, &schema);
    run(&mut s, &routine_statements(driver, &f))
        .await
        .unwrap_or_else(|e| panic!("{driver:?} create function: {e:#}"));
    run(&mut s, &routine_statements(driver, &pr))
        .await
        .unwrap_or_else(|e| panic!("{driver:?} create procedure: {e:#}"));

    // Listed, read back and understood.
    let info = find(&mut s, driver, &db, "dc_total")
        .await
        .unwrap_or_else(|| panic!("{driver:?}: dc_total not listed"));
    assert_eq!(info.kind, RoutineKind::Function);
    assert!(info.valid);
    let r = RoutineRef::new(1, &db, &info);
    let def = s
        .routine_definition(driver, &r)
        .await
        .unwrap_or_else(|e| panic!("{driver:?} definition: {e:#}"));
    let loaded = def
        .spec
        .clone()
        .unwrap_or_else(|| panic!("{driver:?} definition not understood:\n{}", def.ddl));
    let types = |ps: &[Param]| {
        ps.iter()
            .map(|p| {
                (
                    p.name.to_lowercase(),
                    p.mode,
                    p.data_type.to_lowercase(),
                    p.default.clone(),
                )
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(
        types(&loaded.params),
        types(&f.params),
        "{driver:?}:\n{}",
        def.ddl
    );
    assert_eq!(loaded.body.trim(), f.body.trim(), "{driver:?}");
    assert!(!def.ddl.trim().is_empty());

    // Called with the default, then with both arguments.
    let mut args = vec![
        ArgValue {
            value: "3".into(),
            ..Default::default()
        },
        ArgValue {
            value: "5".into(),
            ..Default::default()
        },
    ];
    let rs = call(&mut s, driver, &call_sql(driver, &loaded, &args)).await;
    assert_eq!(
        rs.rows[0][0].as_deref().map(|v| v.trim_end_matches(".0")),
        Some("15"),
        "{driver:?}"
    );
    if has_defaults(driver) {
        args[1].use_default = true;
        let rs = call(&mut s, driver, &call_sql(driver, &loaded, &args)).await;
        assert_eq!(rs.rows[0][0].as_deref(), Some("6"), "{driver:?}");
    }

    // A broken replacement must leave the working function callable.
    if matches!(driver, Driver::Oracle | Driver::MySql) {
        let mut broken = loaded.clone();
        broken.body = if driver == Driver::Oracle {
            "begin return missing_variable; end;"
        } else {
            "begin this is not valid sql; end"
        }
        .into();
        assert!(
            run(&mut s, &routine_statements(driver, &broken))
                .await
                .is_err()
        );
        args[1].use_default = false;
        let rs = call(&mut s, driver, &call_sql(driver, &loaded, &args)).await;
        assert_eq!(
            rs.rows[0][0].as_deref().map(|v| v.trim_end_matches(".0")),
            Some("15")
        );
        broken.original = None;
        broken.or_replace = true;
        assert!(
            run(&mut s, &routine_statements(driver, &broken))
                .await
                .is_err()
        );
        let rs = call(&mut s, driver, &call_sql(driver, &loaded, &args)).await;
        assert_eq!(
            rs.rows[0][0].as_deref().map(|v| v.trim_end_matches(".0")),
            Some("15")
        );
    }

    // Edited in place, then renamed.
    let mut edit = loaded.clone();
    edit.body = edit.body.replace("return ", "return 1 + ");
    run(&mut s, &routine_statements(driver, &edit))
        .await
        .unwrap_or_else(|e| panic!("{driver:?} edit: {e:#}"));
    args[1].use_default = false;
    let rs = call(&mut s, driver, &call_sql(driver, &edit, &args)).await;
    assert_eq!(rs.rows[0][0].as_deref(), Some("16"), "{driver:?}");
    let mut renamed = edit.clone();
    renamed.name = "dc_total2".into();
    run(&mut s, &routine_statements(driver, &renamed))
        .await
        .unwrap_or_else(|e| panic!("{driver:?} rename: {e:#}"));
    assert!(
        find(&mut s, driver, &db, "dc_total").await.is_none(),
        "{driver:?}: old name still there"
    );
    let info = find(&mut s, driver, &db, "dc_total2")
        .await
        .unwrap_or_else(|| panic!("{driver:?}: renamed function not listed"));

    // A changed parameter type (PostgreSQL drops and recreates it).
    let r2 = RoutineRef::new(1, &db, &info);
    let mut retyped = s
        .routine_definition(driver, &r2)
        .await
        .unwrap()
        .spec
        .unwrap();
    retyped.params[1].data_type = if driver == Driver::Oracle {
        "integer".into()
    } else {
        "bigint".into()
    };
    run(&mut s, &routine_statements(driver, &retyped))
        .await
        .unwrap_or_else(|e| panic!("{driver:?} retype: {e:#}"));

    // The procedure's OUT parameter comes back.
    let info = find(&mut s, driver, &db, "dc_split")
        .await
        .unwrap_or_else(|| panic!("{driver:?}: dc_split not listed"));
    let def = s
        .routine_definition(driver, &RoutineRef::new(1, &db, &info))
        .await
        .unwrap();
    let proc_spec = def
        .spec
        .unwrap_or_else(|| panic!("{driver:?} procedure not understood:\n{}", def.ddl));
    let rs = call(
        &mut s,
        driver,
        &call_sql(
            driver,
            &proc_spec,
            &[ArgValue {
                value: "10".into(),
                ..Default::default()
            }],
        ),
    )
    .await;
    let shown = rs.rows[0][0].clone().unwrap_or_default();
    assert!(shown == "5" || shown == "half = 5", "{driver:?}: {rs:?}");

    // Oracle reports compilation errors.
    if driver == Driver::Oracle {
        let mut broken = RoutineSpec::new(driver, RoutineKind::Function, schema.clone());
        broken.name = "dc_broken".into();
        broken.body = "begin\n    return missing_variable;\nend;".into();
        let err = run(&mut s, &routine_statements(driver, &broken))
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("PLS-00201"), "{err:#}");
        let listed = find(&mut s, driver, &db, "dc_broken").await.unwrap();
        assert!(!listed.valid);
        let _ = s
            .execute(
                &drop_sql(driver, RoutineKind::Function, &schema, "DC_BROKEN", ""),
                0,
            )
            .await;
    }

    for name in ["dc_total2", "dc_split"] {
        let r = find(&mut s, driver, &db, name).await.unwrap();
        s.execute(
            &drop_sql(driver, r.kind, &r.schema, &r.name, &r.signature),
            0,
        )
        .await
        .unwrap();
        assert!(find(&mut s, driver, &db, name).await.is_none());
    }
}

#[tokio::test]
#[ignore]
async fn postgres_routines() {
    lifecycle(Driver::Postgres).await;
}

#[tokio::test]
#[ignore]
async fn mysql_routines() {
    lifecycle(Driver::MySql).await;
}

#[tokio::test]
#[ignore]
async fn mariadb_routines() {
    lifecycle(Driver::MariaDb).await;
}

#[tokio::test]
#[ignore]
async fn sql_server_routines() {
    lifecycle(Driver::MsSql).await;
}

#[tokio::test]
#[ignore]
async fn oracle_routines() {
    lifecycle(Driver::Oracle).await;
}
