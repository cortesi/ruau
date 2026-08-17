//! Small AST helpers shared by checker, query, and generation code.

use ruau_syntax::Expr;

pub fn ungroup_expr(mut expr: &Expr) -> &Expr {
    while let Expr::Group { expr: inner, .. } = expr {
        expr = inner;
    }
    expr
}
