//! Execution plans as a tree: PostgreSQL's `EXPLAIN (FORMAT JSON)`, MySQL's `EXPLAIN FORMAT=TREE`
//! / `EXPLAIN ANALYZE` and MariaDB's `EXPLAIN` / `ANALYZE FORMAT=JSON`. The results view shows a
//! plan found in a result set this way; other engines keep their text plans.

use super::{Driver, ResultSet};
use serde_json::Value;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct PlanNode {
    /// `Seq Scan`, `Hash Join`, `Table scan`, `ref`…
    pub op: String,
    /// Table or index it reads, when it reads one.
    pub object: String,
    /// Conditions and keys (`Index Cond`, `Filter`, `Sort Key`…).
    pub detail: String,
    pub est_rows: Option<f64>,
    /// Estimated cost of the node with its children.
    pub cost: Option<f64>,
    pub actual_rows: Option<f64>,
    /// Time of the node with its children, all loops (ms).
    pub time_ms: Option<f64>,
    pub loops: Option<f64>,
    pub children: Vec<PlanNode>,
}

impl PlanNode {
    /// Whether the plan has run (Explain Analyze).
    pub fn analyzed(&self) -> bool {
        self.time_ms.is_some() || self.children.iter().any(PlanNode::analyzed)
    }

    /// What the node itself costs, without its children: time when analyzed, else cost.
    pub fn own_weight(&self, analyzed: bool) -> f64 {
        let total = |n: &PlanNode| if analyzed { n.time_ms } else { n.cost }.unwrap_or(0.0);
        (total(self) - self.children.iter().map(total).sum::<f64>()).max(0.0)
    }

    /// Sum of every node's own weight (the whole plan's).
    pub fn total_weight(&self, analyzed: bool) -> f64 {
        self.own_weight(analyzed)
            + self
                .children
                .iter()
                .map(|c| c.total_weight(analyzed))
                .sum::<f64>()
    }
}

/// The plan in a result set of the explain statements below: one column, one row.
pub fn detect(rs: &ResultSet) -> Option<PlanNode> {
    let [column] = rs.columns.as_slice() else {
        return None;
    };
    let [row] = rs.rows.as_slice() else {
        return None;
    };
    if !matches!(
        column.to_ascii_uppercase().as_str(),
        "QUERY PLAN" | "EXPLAIN" | "ANALYZE"
    ) {
        return None;
    }
    let text = row.first()?.as_deref()?.trim();
    if text.starts_with('{') || text.starts_with('[') {
        let json: Value = serde_json::from_str(text).ok()?;
        postgres(&json).or_else(|| mariadb(&json))
    } else if text.starts_with("->") {
        mysql_tree(text)
    } else {
        None
    }
}

/// The statements of "Explain Plan" (`analyze`: "Explain Analyze") on the engines with a tree;
/// None for the others, which keep `sql::explain_statements`.
pub fn explain_statement(stmt: &str, driver: Driver, analyze: bool) -> Option<String> {
    let s = stmt.trim().trim_end_matches(';').trim_end();
    if s.is_empty() {
        return None;
    }
    Some(match (driver, analyze) {
        (Driver::Postgres, false) => format!("EXPLAIN (FORMAT JSON) {s}"),
        (Driver::Postgres, true) => format!("EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON) {s}"),
        (Driver::MySql, false) => format!("EXPLAIN FORMAT=TREE {s}"),
        (Driver::MySql, true) => format!("EXPLAIN ANALYZE {s}"),
        (Driver::MariaDb, false) => format!("EXPLAIN FORMAT=JSON {s}"),
        (Driver::MariaDb, true) => format!("ANALYZE FORMAT=JSON {s}"),
        _ => return None,
    })
}

fn num(v: &Value, key: &str) -> Option<f64> {
    match v.get(key)? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

fn text(v: &Value, key: &str) -> Option<String> {
    match v.get(key)? {
        Value::String(s) => Some(s.clone()),
        Value::Array(a) => Some(
            a.iter()
                .filter_map(|x| x.as_str())
                .collect::<Vec<_>>()
                .join(", "),
        ),
        _ => None,
    }
}

/// `[{"Plan": {...}, "Execution Time": …}]`.
fn postgres(json: &Value) -> Option<PlanNode> {
    let plan = json.as_array()?.first()?.get("Plan")?;
    fn node(p: &Value) -> PlanNode {
        let mut op = text(p, "Node Type").unwrap_or_default();
        if let Some(join) =
            text(p, "Join Type").filter(|_| op.contains("Join") || op == "Nested Loop")
        {
            op = format!("{op} ({join})");
        }
        let object = match (
            text(p, "Relation Name"),
            text(p, "Index Name"),
            text(p, "Alias"),
        ) {
            (Some(rel), Some(idx), _) => format!("{rel} · {idx}"),
            (Some(rel), None, Some(alias)) if alias != rel => format!("{rel} {alias}"),
            (Some(rel), _, _) => rel,
            (None, Some(idx), _) => idx,
            (None, None, _) => text(p, "CTE Name")
                .or_else(|| text(p, "Function Name"))
                .unwrap_or_default(),
        };
        let detail = [
            "Index Cond",
            "Recheck Cond",
            "Hash Cond",
            "Merge Cond",
            "Join Filter",
            "Filter",
            "Sort Key",
            "Group Key",
        ]
        .iter()
        .filter_map(|k| text(p, k).map(|v| format!("{k}: {v}")))
        .collect::<Vec<_>>()
        .join(" · ");
        let loops = num(p, "Actual Loops");
        PlanNode {
            op,
            object,
            detail,
            est_rows: num(p, "Plan Rows"),
            cost: num(p, "Total Cost"),
            // PostgreSQL gives rows and time per loop.
            actual_rows: num(p, "Actual Rows").map(|r| r * loops.unwrap_or(1.0)),
            time_ms: num(p, "Actual Total Time").map(|t| t * loops.unwrap_or(1.0)),
            loops,
            children: p
                .get("Plans")
                .and_then(Value::as_array)
                .map(|c| c.iter().map(node).collect())
                .unwrap_or_default(),
        }
    }
    Some(node(plan))
}

/// MySQL's tree: `-> Op  (cost=1.2 rows=3) (actual time=0.05..0.07 rows=3 loops=1)` lines,
/// indented four spaces per level.
fn mysql_tree(text: &str) -> Option<PlanNode> {
    let mut stack: Vec<(usize, PlanNode)> = Vec::new();
    let mut root: Option<PlanNode> = None;
    let attach = |stack: &mut Vec<(usize, PlanNode)>,
                  root: &mut Option<PlanNode>,
                  node: PlanNode| match stack.last_mut() {
        Some((_, parent)) => parent.children.push(node),
        None => *root = Some(node),
    };
    for line in text.lines() {
        let depth = line.len() - line.trim_start().len();
        let Some(body) = line.trim_start().strip_prefix("-> ") else {
            continue;
        };
        while stack.last().is_some_and(|(d, _)| *d >= depth) {
            let (_, done) = stack.pop()?;
            attach(&mut stack, &mut root, done);
        }
        stack.push((depth, mysql_node(body)));
    }
    while let Some((_, done)) = stack.pop() {
        attach(&mut stack, &mut root, done);
    }
    root
}

fn mysql_node(body: &str) -> PlanNode {
    let mut node = PlanNode::default();
    let mut op = body;
    for (open, _) in body.match_indices(" (") {
        let rest = &body[open + 2..];
        if rest.starts_with("cost=") || rest.starts_with("actual ") || rest.starts_with("rows=") {
            op = body[..open].trim_end();
            break;
        }
    }
    let params = |prefix: &str| -> Vec<(String, String)> {
        let Some(i) = body.find(&format!("({prefix}")) else {
            return Vec::new();
        };
        let inner = &body[i + 1..];
        let inner = &inner[..inner.find(')').unwrap_or(inner.len())];
        inner
            .split_whitespace()
            .filter_map(|kv| kv.split_once('='))
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    };
    for (k, v) in params("cost=") {
        match k.as_str() {
            "cost" => node.cost = v.parse().ok(),
            "rows" => node.est_rows = v.parse().ok(),
            _ => {}
        }
    }
    let actual = params("actual ");
    let loops: Option<f64> = actual
        .iter()
        .find(|(k, _)| k == "loops")
        .and_then(|(_, v)| v.parse().ok());
    for (k, v) in &actual {
        match k.as_str() {
            // `time=first..last` per loop.
            "time" => {
                node.time_ms = v
                    .split("..")
                    .last()
                    .and_then(|t| t.parse::<f64>().ok())
                    .map(|t| t * loops.unwrap_or(1.0))
            }
            "rows" => node.actual_rows = v.parse::<f64>().ok().map(|r| r * loops.unwrap_or(1.0)),
            _ => {}
        }
    }
    node.loops = loops;
    // `Table scan on orders`, `Index lookup on o using idx (customer=c.id)`.
    let (main, detail) = match op.find(": ") {
        Some(i) => (&op[..i], op[i + 2..].to_string()),
        None => (op, String::new()),
    };
    match main.split_once(" on ") {
        Some((what, on)) => {
            node.op = what.to_string();
            let (object, cond) = match on.find(" (") {
                Some(i) => (&on[..i], on[i + 1..].to_string()),
                None => (on, String::new()),
            };
            node.object = object.replace(" using ", " · ");
            node.detail = [cond, detail]
                .into_iter()
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join(" · ");
        }
        None => {
            node.op = main.to_string();
            node.detail = detail;
        }
    }
    node
}

/// MariaDB's `{"query_block": {...}}`: each `table` is a node; blocks and steps (`filesort`,
/// `temporary_table`, subqueries…) hold the tables under them.
fn mariadb(json: &Value) -> Option<PlanNode> {
    let block = json.get("query_block")?;
    fn table(t: &Value) -> PlanNode {
        let rows = num(t, "rows");
        let loops = num(t, "r_loops");
        PlanNode {
            op: text(t, "access_type").unwrap_or_default(),
            object: match (text(t, "table_name"), text(t, "key")) {
                (Some(name), Some(key)) => format!("{name} · {key}"),
                (Some(name), None) => name,
                (None, _) => String::new(),
            },
            detail: ["attached_condition", "ref", "using_index"]
                .iter()
                .filter_map(|k| text(t, k).map(|v| format!("{k}: {v}")))
                .collect::<Vec<_>>()
                .join(" · "),
            est_rows: rows,
            cost: num(t, "cost"),
            actual_rows: num(t, "r_rows").map(|r| r * loops.unwrap_or(1.0)),
            time_ms: num(t, "r_total_time_ms"),
            loops,
            children: steps(t),
        }
    }
    /// The nodes under `v`: its tables, and its other steps as nodes of their own.
    fn steps(v: &Value) -> Vec<PlanNode> {
        let mut out = Vec::new();
        let Some(obj) = v.as_object() else { return out };
        for (k, child) in obj {
            let items: Vec<&Value> = match child {
                Value::Array(a) => a.iter().collect(),
                Value::Object(_) => vec![child],
                _ => continue,
            };
            for item in items {
                if k == "table" {
                    out.push(table(item));
                } else if item.get("table").is_some()
                    && item.as_object().is_some_and(|o| o.len() == 1)
                {
                    // `nested_loop: [{table: …}, …]`: just the table.
                    out.extend(steps(item));
                } else if item.is_object() {
                    out.push(PlanNode {
                        op: k.replace('_', " "),
                        cost: num(item, "cost").or_else(|| num(item, "query_cost")),
                        time_ms: num(item, "r_total_time_ms"),
                        children: steps(item),
                        ..Default::default()
                    });
                }
            }
        }
        out
    }
    let id = num(block, "select_id").unwrap_or(1.0);
    Some(PlanNode {
        op: format!("query block #{id}"),
        cost: num(block, "cost").or_else(|| num(block, "query_cost")),
        time_ms: num(block, "r_total_time_ms"),
        children: steps(block),
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rs(column: &str, value: &str) -> ResultSet {
        ResultSet {
            columns: vec![column.into()],
            rows: vec![vec![Some(value.into())]],
            ..Default::default()
        }
    }

    #[test]
    fn reads_postgres_json() {
        let json = r#"[{"Plan": {"Node Type": "Hash Join", "Join Type": "Inner", "Total Cost": 120.5, "Plan Rows": 40, "Actual Rows": 38, "Actual Total Time": 3.2, "Actual Loops": 1, "Hash Cond": "(o.customer_id = c.id)",
            "Plans": [
              {"Node Type": "Seq Scan", "Relation Name": "orders", "Alias": "o", "Total Cost": 80.0, "Plan Rows": 1000, "Actual Rows": 1000, "Actual Total Time": 2.0, "Actual Loops": 1, "Filter": "(total > 10)"},
              {"Node Type": "Hash", "Total Cost": 30.0, "Plan Rows": 10, "Actual Rows": 10, "Actual Total Time": 0.4, "Actual Loops": 1,
               "Plans": [{"Node Type": "Index Scan", "Relation Name": "customers", "Index Name": "customers_pkey", "Alias": "customers", "Total Cost": 28.0, "Plan Rows": 10, "Actual Rows": 5, "Actual Total Time": 0.15, "Actual Loops": 2}]}
            ]}, "Execution Time": 3.4}]"#;
        let plan = detect(&rs("QUERY PLAN", json)).unwrap();
        assert_eq!(
            (
                plan.op.as_str(),
                plan.detail.as_str(),
                plan.cost,
                plan.est_rows
            ),
            (
                "Hash Join (Inner)",
                "Hash Cond: (o.customer_id = c.id)",
                Some(120.5),
                Some(40.0)
            )
        );
        assert_eq!(
            (
                plan.children[0].object.as_str(),
                plan.children[0].detail.as_str()
            ),
            ("orders o", "Filter: (total > 10)")
        );
        let index = &plan.children[1].children[0];
        assert_eq!(
            (index.object.as_str(), index.actual_rows, index.time_ms),
            ("customers · customers_pkey", Some(10.0), Some(0.3))
        );
        assert!(plan.analyzed());
        // The scan of orders is where the time goes.
        assert!((plan.children[0].own_weight(true) - 2.0).abs() < 1e-9);
        assert!((plan.own_weight(true) - 0.8).abs() < 1e-9);
        // Estimated only: weights are costs.
        let estimate = detect(&rs("QUERY PLAN", r#"[{"Plan": {"Node Type": "Seq Scan", "Relation Name": "t", "Total Cost": 5.0, "Plan Rows": 3}}]"#)).unwrap();
        assert!(!estimate.analyzed());
        assert_eq!(estimate.own_weight(false), 5.0);
    }

    #[test]
    fn reads_mysql_tree() {
        let tree = "-> Nested loop inner join  (cost=4.5 rows=6) (actual time=0.10..0.30 rows=6 loops=1)\n    -> Table scan on c  (cost=1.2 rows=3) (actual time=0.05..0.07 rows=3 loops=1)\n    -> Index lookup on o using idx_customer (customer_id=c.id)  (cost=0.9 rows=2) (actual time=0.02..0.04 rows=2 loops=3)\n        -> Filter: (o.total > 10)  (cost=0.5 rows=1)\n";
        let plan = detect(&rs("EXPLAIN", tree)).unwrap();
        assert_eq!(
            (plan.op.as_str(), plan.cost, plan.est_rows, plan.time_ms),
            ("Nested loop inner join", Some(4.5), Some(6.0), Some(0.3))
        );
        assert_eq!(plan.children.len(), 2);
        assert_eq!(
            (
                plan.children[0].op.as_str(),
                plan.children[0].object.as_str()
            ),
            ("Table scan", "c")
        );
        let lookup = &plan.children[1];
        assert_eq!(
            (
                lookup.op.as_str(),
                lookup.object.as_str(),
                lookup.detail.as_str()
            ),
            ("Index lookup", "o · idx_customer", "(customer_id=c.id)")
        );
        assert_eq!((lookup.actual_rows, lookup.loops), (Some(6.0), Some(3.0)));
        assert!((lookup.time_ms.unwrap() - 0.12).abs() < 1e-9);
        assert_eq!(
            (
                lookup.children[0].op.as_str(),
                lookup.children[0].detail.as_str()
            ),
            ("Filter", "(o.total > 10)")
        );
    }

    #[test]
    fn reads_mariadb_json() {
        let json = r#"{"query_block": {"select_id": 1, "r_total_time_ms": 1.5, "filesort": {"sort_key": "o.total", "r_total_time_ms": 0.4,
            "temporary_table": {"nested_loop": [
              {"table": {"table_name": "c", "access_type": "ALL", "rows": 3, "r_rows": 3, "r_loops": 1, "r_total_time_ms": 0.1}},
              {"table": {"table_name": "o", "access_type": "ref", "key": "idx_customer", "ref": ["shop.c.id"], "rows": 2, "r_rows": 2, "r_loops": 3, "r_total_time_ms": 0.5, "attached_condition": "o.total > 10"}}
            ]}}}}"#;
        let plan = detect(&rs("ANALYZE", json)).unwrap();
        assert_eq!(plan.op, "query block #1");
        let sort = &plan.children[0];
        assert_eq!(sort.op, "filesort");
        let tables = &sort.children[0].children;
        assert_eq!(tables.len(), 2);
        assert_eq!(
            (
                tables[1].op.as_str(),
                tables[1].object.as_str(),
                tables[1].actual_rows
            ),
            ("ref", "o · idx_customer", Some(6.0))
        );
        assert!(
            tables[1]
                .detail
                .contains("attached_condition: o.total > 10")
        );
    }

    #[test]
    fn leaves_other_results_alone() {
        assert!(
            detect(&rs(
                "QUERY PLAN",
                "Seq Scan on t  (cost=0.00..1.01 rows=1 width=4)"
            ))
            .is_none()
        );
        assert!(detect(&rs("name", "[{\"Plan\": {}}]")).is_none());
        assert_eq!(
            explain_statement("select 1;", Driver::Postgres, false).as_deref(),
            Some("EXPLAIN (FORMAT JSON) select 1")
        );
        assert_eq!(
            explain_statement("select 1", Driver::MariaDb, true).as_deref(),
            Some("ANALYZE FORMAT=JSON select 1")
        );
        assert!(explain_statement("select 1", Driver::Oracle, false).is_none());
    }
}
