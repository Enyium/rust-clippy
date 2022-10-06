use clippy_utils::diagnostics::span_lint;
use clippy_utils::higher::IfLetOrMatch;
use clippy_utils::ty::is_type_diagnostic_item;
use clippy_utils::visitors::{for_each_expr, Descend};
use clippy_utils::{meets_msrv, msrvs, peel_blocks};
use if_chain::if_chain;
use rustc_data_structures::fx::FxHashSet;
use rustc_hir::{Expr, ExprKind, MatchSource, Pat, PatKind, QPath, Stmt, StmtKind};
use rustc_lint::{LateContext, LateLintPass, LintContext};
use rustc_middle::lint::in_external_macro;
use rustc_semver::RustcVersion;
use rustc_session::{declare_tool_lint, impl_lint_pass};
use rustc_span::symbol::sym;
use std::ops::ControlFlow;

declare_clippy_lint! {
    /// ### What it does
    ///
    /// Warn of cases where `let...else` could be used
    ///
    /// ### Why is this bad?
    ///
    /// `let...else` provides a standard construct for this pattern
    /// that people can easily recognize. It's also more compact.
    ///
    /// ### Example
    ///
    /// ```rust
    /// # let w = Some(0);
    /// let v = if let Some(v) = w { v } else { return };
    /// ```
    ///
    /// Could be written:
    ///
    /// ```rust
    /// # #![feature(let_else)]
    /// # fn main () {
    /// # let w = Some(0);
    /// let Some(v) = w else { return };
    /// # }
    /// ```
    #[clippy::version = "1.67.0"]
    pub MANUAL_LET_ELSE,
    pedantic,
    "manual implementation of a let...else statement"
}

pub struct ManualLetElse {
    msrv: Option<RustcVersion>,
}

impl ManualLetElse {
    #[must_use]
    pub fn new(msrv: Option<RustcVersion>) -> Self {
        Self { msrv }
    }
}

impl_lint_pass!(ManualLetElse => [MANUAL_LET_ELSE]);

impl<'tcx> LateLintPass<'tcx> for ManualLetElse {
    fn check_stmt(&mut self, cx: &LateContext<'_>, stmt: &'tcx Stmt<'tcx>) {
        let if_let_or_match = if_chain! {
            if meets_msrv(self.msrv, msrvs::LET_ELSE);
            if !in_external_macro(cx.sess(), stmt.span);
            if let StmtKind::Local(local) = stmt.kind;
            if let Some(init) = local.init;
            if init.span.ctxt() == stmt.span.ctxt();
            if let Some(if_let_or_match) = IfLetOrMatch::parse(cx, init);
            then {
                if_let_or_match
            } else {
                return;
            }
        };

        match if_let_or_match {
            IfLetOrMatch::IfLet(_let_expr, let_pat, if_then, if_else) => if_chain! {
                if expr_is_simple_identity(let_pat, if_then);
                if let Some(if_else) = if_else;
                if expr_diverges(cx, if_else);
                then {
                    span_lint(
                        cx,
                        MANUAL_LET_ELSE,
                        stmt.span,
                        "this could be rewritten as `let else`",
                    );
                }
            },
            IfLetOrMatch::Match(_match_expr, arms, source) => {
                if source != MatchSource::Normal {
                    return;
                }
                // Any other number than two arms doesn't (neccessarily)
                // have a trivial mapping to let else.
                if arms.len() != 2 {
                    return;
                }
                // We iterate over both arms, trying to find one that is an identity,
                // one that diverges. Our check needs to work regardless of the order
                // of both arms.
                let mut found_identity_arm = false;
                let mut found_diverging_arm = false;
                for arm in arms {
                    // Guards don't give us an easy mapping to let else
                    if arm.guard.is_some() {
                        return;
                    }
                    if expr_is_simple_identity(arm.pat, arm.body) {
                        found_identity_arm = true;
                    } else if expr_diverges(cx, arm.body) && pat_allowed_for_else(cx, arm.pat) {
                        found_diverging_arm = true;
                    }
                }
                if !(found_identity_arm && found_diverging_arm) {
                    return;
                }
                span_lint(cx, MANUAL_LET_ELSE, stmt.span, "this could be rewritten as `let else`");
            },
        }
    }

    extract_msrv_attr!(LateContext);
}

fn expr_diverges(cx: &LateContext<'_>, expr: &'_ Expr<'_>) -> bool {
    fn is_never(cx: &LateContext<'_>, expr: &'_ Expr<'_>) -> bool {
        if let Some(ty) = cx.typeck_results().expr_ty_opt(expr) {
            return ty.is_never();
        }
        false
    }
    // We can't just call is_never on expr and be done, because the type system
    // sometimes coerces the ! type to something different before we can get
    // our hands on it. So instead, we do a manual search. We do fall back to
    // is_never in some places when there is no better alternative.
    for_each_expr(expr, |ex| {
        match ex.kind {
            ExprKind::Continue(_) | ExprKind::Break(_, _) | ExprKind::Ret(_) => ControlFlow::Break(()),
            ExprKind::Call(call, _) => {
                if is_never(cx, ex) || is_never(cx, call) {
                    return ControlFlow::Break(());
                }
                ControlFlow::Continue(Descend::Yes)
            },
            ExprKind::MethodCall(..) => {
                if is_never(cx, ex) {
                    return ControlFlow::Break(());
                }
                ControlFlow::Continue(Descend::Yes)
            },
            ExprKind::If(if_expr, if_then, if_else) => {
                let else_diverges = if_else.map_or(false, |ex| expr_diverges(cx, ex));
                let diverges = expr_diverges(cx, if_expr) || (else_diverges && expr_diverges(cx, if_then));
                if diverges {
                    return ControlFlow::Break(());
                }
                ControlFlow::Continue(Descend::No)
            },
            ExprKind::Match(match_expr, match_arms, _) => {
                let diverges =
                    expr_diverges(cx, match_expr) || match_arms.iter().all(|arm| expr_diverges(cx, arm.body));
                if diverges {
                    return ControlFlow::Break(());
                }
                ControlFlow::Continue(Descend::No)
            },

            // Don't continue into loops or labeled blocks, as they are breakable,
            // and we'd have to start checking labels.
            ExprKind::Block(_, Some(_)) | ExprKind::Loop(..) => ControlFlow::Continue(Descend::No),

            // Default: descend
            _ => ControlFlow::Continue(Descend::Yes),
        }
    })
    .is_some()
}

fn pat_allowed_for_else(cx: &LateContext<'_>, pat: &'_ Pat<'_>) -> bool {
    // Check whether the pattern contains any bindings, as the
    // binding might potentially be used in the body.
    // TODO: only look for *used* bindings.
    let mut has_bindings = false;
    pat.each_binding_or_first(&mut |_, _, _, _| has_bindings = true);
    if has_bindings {
        return false;
    }

    // Check whether any possibly "unknown" patterns are included,
    // because users might not know which values some enum has.
    // Well-known enums are excepted, as we assume people know them.
    // We do a deep check, to be able to disallow Err(En::Foo(_))
    // for usage of the En::Foo variant, as we disallow En::Foo(_),
    // but we allow Err(_).
    let typeck_results = cx.typeck_results();
    let mut has_disallowed = false;
    pat.walk_always(|pat| {
        // Only do the check if the type is "spelled out" in the pattern
        if !matches!(
            pat.kind,
            PatKind::Struct(..) | PatKind::TupleStruct(..) | PatKind::Path(..)
        ) {
            return;
        };
        let ty = typeck_results.pat_ty(pat);
        // Option and Result are allowed, everything else isn't.
        if !(is_type_diagnostic_item(cx, ty, sym::Option) || is_type_diagnostic_item(cx, ty, sym::Result)) {
            has_disallowed = true;
        }
    });
    !has_disallowed
}

/// Checks if the passed block is a simple identity referring to bindings created by the pattern
fn expr_is_simple_identity(pat: &'_ Pat<'_>, expr: &'_ Expr<'_>) -> bool {
    // We support patterns with multiple bindings and tuples, like:
    //   let ... = if let (Some(foo), bar) = g() { (foo, bar) } else { ... }
    let peeled = peel_blocks(expr);
    let paths = match peeled.kind {
        ExprKind::Tup(exprs) | ExprKind::Array(exprs) => exprs,
        ExprKind::Path(_) => std::slice::from_ref(peeled),
        _ => return false,
    };
    let mut pat_bindings = FxHashSet::default();
    pat.each_binding_or_first(&mut |_ann, _hir_id, _sp, ident| {
        pat_bindings.insert(ident);
    });
    if pat_bindings.len() < paths.len() {
        return false;
    }
    for path in paths {
        if_chain! {
            if let ExprKind::Path(QPath::Resolved(_ty, path)) = path.kind;
            if let [path_seg] = path.segments;
            then {
                if !pat_bindings.remove(&path_seg.ident) {
                    return false;
                }
            } else {
                return false;
            }
        }
    }
    true
}
