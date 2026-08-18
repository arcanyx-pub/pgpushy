use pg_query::NodeEnum;
use pg_query::protobuf::{FuncCall, Node, TypeCast, a_const};
use serde_json::Value;

/// A name Postgres resolves from inside a string literal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NameLiteral {
    /// The literal exactly as the author wrote it, for diagnostics.
    pub raw: String,
    /// What it names, for the message: `sequence`, `type`, and so on.
    pub what: &'static str,
}

/// Types whose input is a **schema-qualified** object name.
///
/// `regrole` and `regnamespace` are deliberately absent: a role and a schema
/// are not themselves schema-scoped, so demanding a qualifier for them would
/// reject something correct.
const QUALIFIED_REG_TYPES: &[(&str, &str)] = &[
    ("regclass", "table, index or sequence"),
    ("regcollation", "collation"),
    ("regconfig", "text search configuration"),
    ("regdictionary", "text search dictionary"),
    ("regoper", "operator"),
    ("regoperator", "operator"),
    ("regproc", "function"),
    ("regprocedure", "function"),
    ("regtype", "type or domain"),
];

/// Functions whose first argument is coerced to `regclass`.
const SEQUENCE_FUNCTIONS: &[&str] = &["nextval", "currval", "setval"];

/// What a literal reached through one of those functions names.
const SEQUENCE_CALL: &str = "sequence";

/// Every name literal reachable from a statement.
///
/// The search runs over the AST serialized to JSON rather than over the typed
/// tree. `pg_query`'s own node walk is hand-written and does not descend into
/// every field — it stops before a `CREATE TABLE`'s column list, which is
/// exactly where a column default lives — so walking it would miss the case
/// this module exists for. Serialization covers every field by construction.
pub fn find(node: &NodeEnum) -> Vec<NameLiteral> {
    let Ok(value) = serde_json::to_value(node) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    walk(&value, &mut found);
    // `nextval('s'::regclass)` is matched twice, once as the call and once as
    // the cast inside it. They name the same literal, and one diagnostic per
    // literal is what a reader wants.
    found.dedup_by(|a, b| a.raw == b.raw);
    found
}

fn walk(value: &Value, out: &mut Vec<NameLiteral>) {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                match key.as_str() {
                    "FuncCall" => out.extend(from_call(child)),
                    "TypeCast" => out.extend(from_cast(child)),
                    _ => {}
                }
                walk(child, out);
            }
        }
        Value::Array(items) => {
            for item in items {
                walk(item, out);
            }
        }
        _ => {}
    }
}

fn from_cast(cast: &Value) -> Option<NameLiteral> {
    let last = last_string(cast.get("type_name")?.get("names")?)?;
    let what = QUALIFIED_REG_TYPES
        .iter()
        .find(|(name, _)| *name == last)
        .map(|(_, what)| *what)?;
    Some(NameLiteral {
        raw: const_string(cast.get("arg")?)?,
        what,
    })
}

fn from_call(call: &Value) -> Option<NameLiteral> {
    let name = last_string(call.get("funcname")?)?;
    if !SEQUENCE_FUNCTIONS.contains(&name.as_str()) {
        return None;
    }
    Some(NameLiteral {
        raw: const_string(call.get("args")?.get(0)?)?,
        what: SEQUENCE_CALL,
    })
}

/// The string value of a node, seeing through casts.
///
/// `'s'::text::regclass` nests, and Postgres still resolves the innermost
/// literal as the name.
fn const_string(node: &Value) -> Option<String> {
    let inner = node.get("node")?;
    if let Some(constant) = inner.get("AConst") {
        return Some(
            constant
                .get("val")?
                .get("Sval")?
                .get("sval")?
                .as_str()?
                .to_owned(),
        );
    }
    const_string(inner.get("TypeCast")?.get("arg")?)
}

/// The last element of a dotted name list, which is the name itself.
fn last_string(names: &Value) -> Option<String> {
    let last = names.as_array()?.last()?;
    Some(
        last.get("node")?
            .get("String")?
            .get("sval")?
            .as_str()?
            .to_owned(),
    )
}

/// Whether this literal came from a call to `nextval` and friends, rather than
/// from a cast.
///
/// The distinction matters because pgschema can manage a sequence but not a
/// default that draws from one (spec §4.3).
pub fn names_a_sequence_call(literal: &NameLiteral) -> bool {
    literal.what == SEQUENCE_CALL
}

/// Split a qualified-name literal the way Postgres does.
///
/// Postgres reads these with `SplitIdentifierString`: parts are separated by
/// dots, a double-quoted part is taken verbatim with `""` meaning one quote,
/// and an unquoted part is down-cased. `"my.table"` is therefore *one* part,
/// which is why this cannot be a `split('.')`.
///
/// Returns `None` for anything malformed — an unterminated quote, an empty
/// part — since such a literal is not a name pgpushy can reason about.
pub fn name_parts(raw: &str) -> Option<Vec<String>> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut chars = raw.chars().peekable();
    let mut quoted_part = false;

    loop {
        match chars.next() {
            Some('"') => {
                quoted_part = true;
                loop {
                    match chars.next() {
                        Some('"') if chars.peek() == Some(&'"') => {
                            chars.next();
                            current.push('"');
                        }
                        Some('"') => break,
                        Some(c) => current.push(c),
                        None => return None,
                    }
                }
            }
            Some('.') => {
                if current.is_empty() && !quoted_part {
                    return None;
                }
                parts.push(std::mem::take(&mut current));
                quoted_part = false;
            }
            Some(c) if c.is_whitespace() && current.is_empty() && !quoted_part => {}
            Some(c) => current.push(c.to_ascii_lowercase()),
            None => break,
        }
    }

    if current.is_empty() && !quoted_part {
        return None;
    }
    parts.push(current);
    Some(parts)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(sql: &str) -> NodeEnum {
        pg_query::parse(sql).unwrap().protobuf.stmts[0]
            .stmt
            .as_ref()
            .unwrap()
            .node
            .clone()
            .unwrap()
    }

    #[test]
    fn finds_nextval_in_a_column_default() {
        let found = find(&parse(
            "CREATE TABLE t (id int DEFAULT nextval('billing.s'));",
        ));
        assert_eq!(
            found,
            vec![NameLiteral {
                raw: "billing.s".to_owned(),
                what: SEQUENCE_CALL,
            }]
        );
    }

    #[test]
    fn finds_the_pg_dump_regclass_form() {
        let found = find(&parse(
            "CREATE TABLE t (id int DEFAULT nextval('billing.s'::regclass));",
        ));
        // The cast and the call both name it; one diagnostic per site is what
        // matters, and both point at the same literal.
        assert!(found.iter().all(|l| l.raw == "billing.s"));
        assert!(!found.is_empty());
    }

    #[test]
    fn finds_a_bare_regtype_cast() {
        let found = find(&parse(
            "CREATE TABLE t (a int CHECK ('x'::regtype IS NOT NULL));",
        ));
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].what, "type or domain");
    }

    #[test]
    fn ignores_types_that_are_not_schema_scoped() {
        assert!(
            find(&parse(
                "CREATE TABLE t (a int DEFAULT ('joe'::regrole)::int);"
            ))
            .is_empty()
        );
        assert!(
            find(&parse(
                "CREATE TABLE t (a int DEFAULT ('s'::regnamespace)::int);"
            ))
            .is_empty()
        );
    }

    #[test]
    fn ignores_ordinary_string_defaults() {
        assert!(
            find(&parse(
                "CREATE TABLE t (a text DEFAULT 'public.not_a_name');"
            ))
            .is_empty()
        );
    }

    #[test]
    fn splits_names_the_way_postgres_does() {
        assert_eq!(name_parts("s"), Some(vec!["s".to_owned()]));
        assert_eq!(
            name_parts("billing.s"),
            Some(vec!["billing".to_owned(), "s".to_owned()])
        );
        // Unquoted parts fold to lower case, exactly as an identifier would.
        assert_eq!(
            name_parts("Billing.S"),
            Some(vec!["billing".to_owned(), "s".to_owned()])
        );
        // A quoted part keeps its case and swallows the dot inside it.
        assert_eq!(
            name_parts("\"my.table\""),
            Some(vec!["my.table".to_owned()])
        );
        assert_eq!(
            name_parts("\"Bill\".\"S.1\""),
            Some(vec!["Bill".to_owned(), "S.1".to_owned()])
        );
        // A doubled quote is one quote.
        assert_eq!(name_parts("\"a\"\"b\""), Some(vec!["a\"b".to_owned()]));
        // An empty quoted identifier is legal in Postgres and stays a part.
        assert_eq!(
            name_parts("\"\".s"),
            Some(vec![String::new(), "s".to_owned()])
        );

        assert_eq!(name_parts("\"unterminated"), None);
        assert_eq!(name_parts(".s"), None);
        assert_eq!(name_parts("s."), None);
        assert_eq!(name_parts(""), None);
    }
}

// ---------------------------------------------------------------------------
// Rewriting (spec §5.4)
// ---------------------------------------------------------------------------

/// Remove the schema qualifier from every object name inside a string literal.
///
/// This is what makes a document work for the schema it targets: pgschema puts
/// that schema's objects into a scratch schema and strips the qualifier from
/// identifiers to match, but a literal keeps whatever it says, and the real
/// schema does not hold the object any more. `nextval('billing.s')` therefore
/// has to become `nextval('s')` in `billing`'s own document — and stay
/// qualified in every other, where `billing`'s objects were left where they
/// are (§5.4).
///
/// The rewrite is uniform per statement rather than per literal, because §4.5
/// admits no cross-schema reference except a foreign key: every name literal
/// in an object belonging to `S` names something in `S`.
///
/// Idempotent, so a literal reached twice — `nextval('s'::regclass)` matches
/// as both a call and a cast — is unchanged the second time.
pub fn dequalify(node: &mut NodeEnum) {
    match node {
        NodeEnum::CreateStmt(create) => {
            for elt in &mut create.table_elts {
                visit(elt);
            }
        }
        NodeEnum::IndexStmt(index) => {
            for param in &mut index.index_params {
                visit(param);
            }
            if let Some(where_clause) = index.where_clause.as_mut() {
                visit(where_clause);
            }
        }
        NodeEnum::AlterTableStmt(alter) => {
            for cmd in &mut alter.cmds {
                visit(cmd);
            }
        }
        NodeEnum::CreateSeqStmt(seq) => {
            for option in &mut seq.options {
                visit(option);
            }
        }
        NodeEnum::CreateDomainStmt(domain) => {
            for constraint in &mut domain.constraints {
                visit(constraint);
            }
        }
        _ => {}
    }
}

/// Walk an expression, rewriting the name literals in it.
///
/// The arms below are every node kind that can hold a sub-expression inside
/// the statements §4.3 admits. Missing one would leave a literal qualified in
/// a document that cannot resolve it, so synthesis checks afterwards that no
/// qualified literal survives and fails loudly rather than emitting one
/// (see [`crate::synth`]).
fn visit(node: &mut Node) {
    let Some(inner) = node.node.as_mut() else {
        return;
    };
    match inner {
        // The two shapes that name an object in a literal. Rewrite, then keep
        // walking: a cast's argument may hold further expressions.
        NodeEnum::TypeCast(cast) => {
            if type_is_qualified_reg(cast)
                && let Some(arg) = cast.arg.as_mut()
            {
                strip(arg);
            }
            if let Some(arg) = cast.arg.as_mut() {
                visit(arg);
            }
        }
        NodeEnum::FuncCall(call) => {
            if call_is_sequence_function(call)
                && let Some(first) = call.args.first_mut()
            {
                strip(first);
            }
            for arg in &mut call.args {
                visit(arg);
            }
            for arg in &mut call.agg_order {
                visit(arg);
            }
            if let Some(filter) = call.agg_filter.as_mut() {
                visit(filter);
            }
        }

        NodeEnum::ColumnDef(column) => {
            if let Some(default) = column.raw_default.as_mut() {
                visit(default);
            }
            for constraint in &mut column.constraints {
                visit(constraint);
            }
        }
        NodeEnum::Constraint(constraint) => {
            if let Some(expr) = constraint.raw_expr.as_mut() {
                visit(expr);
            }
            if let Some(where_clause) = constraint.where_clause.as_mut() {
                visit(where_clause);
            }
            for exclusion in &mut constraint.exclusions {
                visit(exclusion);
            }
            for option in &mut constraint.options {
                visit(option);
            }
        }
        NodeEnum::AlterTableCmd(cmd) => {
            if let Some(def) = cmd.def.as_mut() {
                visit(def);
            }
        }
        NodeEnum::IndexElem(elem) => {
            if let Some(expr) = elem.expr.as_mut() {
                visit(expr);
            }
        }
        NodeEnum::DefElem(elem) => {
            if let Some(arg) = elem.arg.as_mut() {
                visit(arg);
            }
        }

        NodeEnum::AExpr(expr) => {
            if let Some(lexpr) = expr.lexpr.as_mut() {
                visit(lexpr);
            }
            if let Some(rexpr) = expr.rexpr.as_mut() {
                visit(rexpr);
            }
        }
        NodeEnum::BoolExpr(expr) => {
            for arg in &mut expr.args {
                visit(arg);
            }
        }
        NodeEnum::CoalesceExpr(expr) => {
            for arg in &mut expr.args {
                visit(arg);
            }
        }
        NodeEnum::MinMaxExpr(expr) => {
            for arg in &mut expr.args {
                visit(arg);
            }
        }
        NodeEnum::CaseExpr(expr) => {
            if let Some(arg) = expr.arg.as_mut() {
                visit(arg);
            }
            for when in &mut expr.args {
                visit(when);
            }
            if let Some(default) = expr.defresult.as_mut() {
                visit(default);
            }
        }
        NodeEnum::CaseWhen(when) => {
            if let Some(expr) = when.expr.as_mut() {
                visit(expr);
            }
            if let Some(result) = when.result.as_mut() {
                visit(result);
            }
        }
        NodeEnum::NullTest(test) => {
            if let Some(arg) = test.arg.as_mut() {
                visit(arg);
            }
        }
        NodeEnum::BooleanTest(test) => {
            if let Some(arg) = test.arg.as_mut() {
                visit(arg);
            }
        }
        NodeEnum::CollateClause(clause) => {
            if let Some(arg) = clause.arg.as_mut() {
                visit(arg);
            }
        }
        NodeEnum::NamedArgExpr(arg) => {
            if let Some(inner) = arg.arg.as_mut() {
                visit(inner);
            }
        }
        NodeEnum::AIndirection(indirection) => {
            if let Some(arg) = indirection.arg.as_mut() {
                visit(arg);
            }
        }
        NodeEnum::AArrayExpr(array) => {
            for element in &mut array.elements {
                visit(element);
            }
        }
        NodeEnum::RowExpr(row) => {
            for arg in &mut row.args {
                visit(arg);
            }
        }
        NodeEnum::List(list) => {
            for item in &mut list.items {
                visit(item);
            }
        }
        NodeEnum::SortBy(sort) => {
            if let Some(node) = sort.node.as_mut() {
                visit(node);
            }
        }
        _ => {}
    }
}

fn type_is_qualified_reg(cast: &TypeCast) -> bool {
    cast.type_name
        .as_ref()
        .and_then(|t| last_sval(&t.names))
        .is_some_and(|name| QUALIFIED_REG_TYPES.iter().any(|(t, _)| *t == name))
}

fn call_is_sequence_function(call: &FuncCall) -> bool {
    last_sval(&call.funcname).is_some_and(|name| SEQUENCE_FUNCTIONS.contains(&name.as_str()))
}

fn last_sval(nodes: &[Node]) -> Option<String> {
    match nodes.last()?.node.as_ref()? {
        NodeEnum::String(s) => Some(s.sval.clone()),
        _ => None,
    }
}

/// Drop the schema from the innermost string constant of `node`.
fn strip(node: &mut Node) {
    match node.node.as_mut() {
        Some(NodeEnum::AConst(constant)) => {
            if let Some(a_const::Val::Sval(s)) = constant.val.as_mut()
                && let Some(parts) = name_parts(&s.sval)
                && parts.len() > 1
            {
                s.sval = quote_if_needed(parts.last().expect("checked non-empty"));
            }
        }
        Some(NodeEnum::TypeCast(cast)) => {
            if let Some(arg) = cast.arg.as_mut() {
                strip(arg);
            }
        }
        _ => {}
    }
}

/// Re-render one identifier for a name literal.
///
/// [`name_parts`] has already folded an unquoted part to lower case and
/// removed the quoting from a quoted one, so anything that would not survive
/// being read back bare has to be quoted again.
fn quote_if_needed(part: &str) -> String {
    let plain = !part.is_empty()
        && !part.starts_with(|c: char| c.is_ascii_digit())
        && part
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if plain {
        part.to_owned()
    } else {
        format!("\"{}\"", part.replace('"', "\"\""))
    }
}

#[cfg(test)]
mod rewrite_tests {
    use super::*;

    fn parse(sql: &str) -> NodeEnum {
        pg_query::parse(sql).unwrap().protobuf.stmts[0]
            .stmt
            .as_ref()
            .unwrap()
            .node
            .clone()
            .unwrap()
    }

    fn dequalified(sql: &str) -> String {
        let mut node = parse(sql);
        dequalify(&mut node);
        node.deparse().unwrap()
    }

    #[test]
    fn strips_the_schema_from_a_column_default() {
        assert!(
            dequalified("CREATE TABLE billing.t (id int DEFAULT nextval('billing.s'));")
                .contains("nextval('s')")
        );
    }

    #[test]
    fn strips_the_schema_from_the_pg_dump_form() {
        let out =
            dequalified("CREATE TABLE billing.t (id int DEFAULT nextval('billing.s'::regclass));");
        assert!(out.contains("'s'::regclass"), "got: {out}");
    }

    #[test]
    fn is_idempotent() {
        let once = dequalified("CREATE TABLE billing.t (id int DEFAULT nextval('billing.s'));");
        let mut node = parse(&once);
        dequalify(&mut node);
        assert_eq!(node.deparse().unwrap(), once);
    }

    #[test]
    fn requotes_a_name_that_needs_it() {
        let out = dequalified("CREATE TABLE t (id int DEFAULT nextval('billing.\"My Seq\"'));");
        assert!(out.contains(r#"'"My Seq"'"#), "got: {out}");
    }

    #[test]
    fn leaves_an_ordinary_string_default_alone() {
        let out = dequalified("CREATE TABLE t (a text DEFAULT 'public.not_a_name');");
        assert!(out.contains("'public.not_a_name'"), "got: {out}");
    }

    #[test]
    fn reaches_a_literal_nested_in_an_expression() {
        let out = dequalified(
            "CREATE TABLE t (id int DEFAULT coalesce(nextval('billing.s'), (CASE WHEN true \
             THEN nextval('billing.t') ELSE 0 END)::bigint));",
        );
        assert!(out.contains("nextval('s')"), "got: {out}");
        assert!(out.contains("nextval('t')"), "got: {out}");
    }

    #[test]
    fn reaches_a_literal_in_a_check_constraint() {
        let out =
            dequalified("CREATE TABLE t (a int, CONSTRAINT c CHECK (a < nextval('billing.s')));");
        assert!(out.contains("nextval('s')"), "got: {out}");
    }

    #[test]
    fn reaches_a_literal_in_an_index_predicate() {
        let out = dequalified("CREATE INDEX i ON t (a) WHERE a < nextval('billing.s');");
        assert!(out.contains("nextval('s')"), "got: {out}");
    }

    /// The rewrite and the search must agree: anything `find` reports, the
    /// walk must be able to reach. Synthesis relies on that (spec §5.4).
    #[test]
    fn nothing_find_reports_survives_the_rewrite() {
        for sql in [
            "CREATE TABLE billing.t (id int DEFAULT nextval('billing.s'::regclass), b int);",
            "CREATE TABLE billing.t (a int, CONSTRAINT c CHECK (a < nextval('billing.s')));",
            "CREATE TABLE billing.t (a int DEFAULT coalesce(nextval('billing.s'), 0));",
            "CREATE INDEX i ON billing.t (a) WHERE a < nextval('billing.s');",
            "CREATE TABLE billing.t (a int DEFAULT (ARRAY[nextval('billing.s')])[1]);",
        ] {
            let mut node = parse(sql);
            dequalify(&mut node);
            let left: Vec<_> = find(&node)
                .into_iter()
                .filter(|l| name_parts(&l.raw).is_some_and(|p| p.len() > 1))
                .collect();
            assert!(left.is_empty(), "{sql}\nleft qualified: {left:?}");
        }
    }
}
