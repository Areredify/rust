//! Code related to processing overloaded binary and unary operators.

use super::method::MethodCallee;
use super::{FnCtxt, Needs};
use rustc_errors::{self, struct_span_err, Applicability, DiagnosticBuilder};
use rustc_hir as hir;
use rustc_infer::infer::type_variable::{TypeVariableOrigin, TypeVariableOriginKind};
use rustc_middle::ty::adjustment::{
    Adjust, Adjustment, AllowTwoPhase, AutoBorrow, AutoBorrowMutability,
};
use rustc_middle::ty::TyKind::{Adt, Array, Char, FnDef, Never, Ref, Str, Tuple, Uint};
use rustc_middle::ty::{self, suggest_constraining_type_param, Ty, TyCtxt, TypeFoldable};
use rustc_span::symbol::Ident;
use rustc_span::Span;
use rustc_trait_selection::infer::InferCtxtExt;

impl<'a, 'tcx> FnCtxt<'a, 'tcx> {
    /// Checks a `a <op>= b`
    pub fn check_binop_assign(
        &self,
        expr: &'tcx hir::Expr<'tcx>,
        op: hir::BinOp,
        lhs: &'tcx hir::Expr<'tcx>,
        rhs: &'tcx hir::Expr<'tcx>,
    ) -> Ty<'tcx> {
        let (lhs_ty, rhs_ty, return_ty) =
            self.check_overloaded_binop(expr, lhs, rhs, op, IsAssign::Yes);

        let ty =
            if !lhs_ty.is_ty_var() && !rhs_ty.is_ty_var() && is_builtin_binop(lhs_ty, rhs_ty, op) {
                self.enforce_builtin_binop_types(&lhs.span, lhs_ty, &rhs.span, rhs_ty, op);
                self.tcx.mk_unit()
            } else {
                return_ty
            };

        self.check_lhs_assignable(lhs, "E0067", &op.span);

        ty
    }

    /// Checks a potentially overloaded binary operator.
    pub fn check_binop(
        &self,
        expr: &'tcx hir::Expr<'tcx>,
        op: hir::BinOp,
        lhs_expr: &'tcx hir::Expr<'tcx>,
        rhs_expr: &'tcx hir::Expr<'tcx>,
    ) -> Ty<'tcx> {
        let tcx = self.tcx;

        debug!(
            "check_binop(expr.hir_id={}, expr={:?}, op={:?}, lhs_expr={:?}, rhs_expr={:?})",
            expr.hir_id, expr, op, lhs_expr, rhs_expr
        );

        match BinOpCategory::from(op) {
            BinOpCategory::Shortcircuit => {
                // && and || are a simple case.
                self.check_expr_coercable_to_type(lhs_expr, tcx.types.bool);
                let lhs_diverges = self.diverges.get();
                self.check_expr_coercable_to_type(rhs_expr, tcx.types.bool);

                // Depending on the LHS' value, the RHS can never execute.
                self.diverges.set(lhs_diverges);

                tcx.types.bool
            }
            _ => {
                // Otherwise, we always treat operators as if they are
                // overloaded. This is the way to be most flexible w/r/t
                // types that get inferred.
                let (lhs_ty, rhs_ty, return_ty) =
                    self.check_overloaded_binop(expr, lhs_expr, rhs_expr, op, IsAssign::No);

                // Supply type inference hints if relevant. Probably these
                // hints should be enforced during select as part of the
                // `consider_unification_despite_ambiguity` routine, but this
                // more convenient for now.
                //
                // The basic idea is to help type inference by taking
                // advantage of things we know about how the impls for
                // scalar types are arranged. This is important in a
                // scenario like `1_u32 << 2`, because it lets us quickly
                // deduce that the result type should be `u32`, even
                // though we don't know yet what type 2 has and hence
                // can't pin this down to a specific impl.
                if !lhs_ty.is_ty_var()
                    && !rhs_ty.is_ty_var()
                    && is_builtin_binop(lhs_ty, rhs_ty, op)
                {
                    let builtin_return_ty = self.enforce_builtin_binop_types(
                        &lhs_expr.span,
                        lhs_ty,
                        &rhs_expr.span,
                        rhs_ty,
                        op,
                    );
                    self.demand_suptype(expr.span, builtin_return_ty, return_ty);
                }

                return_ty
            }
        }
    }

    fn enforce_builtin_binop_types(
        &self,
        lhs_span: &Span,
        lhs_ty: Ty<'tcx>,
        rhs_span: &Span,
        rhs_ty: Ty<'tcx>,
        op: hir::BinOp,
    ) -> Ty<'tcx> {
        debug_assert!(is_builtin_binop(lhs_ty, rhs_ty, op));

        // Special-case a single layer of referencing, so that things like `5.0 + &6.0f32` work.
        // (See https://github.com/rust-lang/rust/issues/57447.)
        let (lhs_ty, rhs_ty) = (deref_ty_if_possible(lhs_ty), deref_ty_if_possible(rhs_ty));

        let tcx = self.tcx;
        match BinOpCategory::from(op) {
            BinOpCategory::Shortcircuit => {
                self.demand_suptype(*lhs_span, tcx.types.bool, lhs_ty);
                self.demand_suptype(*rhs_span, tcx.types.bool, rhs_ty);
                tcx.types.bool
            }

            BinOpCategory::Shift => {
                // result type is same as LHS always
                lhs_ty
            }

            BinOpCategory::Math | BinOpCategory::Bitwise => {
                // both LHS and RHS and result will have the same type
                self.demand_suptype(*rhs_span, lhs_ty, rhs_ty);
                lhs_ty
            }

            BinOpCategory::Comparison => {
                // both LHS and RHS and result will have the same type
                self.demand_suptype(*rhs_span, lhs_ty, rhs_ty);
                tcx.types.bool
            }
        }
    }

    fn check_overloaded_binop(
        &self,
        expr: &'tcx hir::Expr<'tcx>,
        lhs_expr: &'tcx hir::Expr<'tcx>,
        rhs_expr: &'tcx hir::Expr<'tcx>,
        op: hir::BinOp,
        is_assign: IsAssign,
    ) -> (Ty<'tcx>, Ty<'tcx>, Ty<'tcx>) {
        debug!(
            "check_overloaded_binop(expr.hir_id={}, op={:?}, is_assign={:?})",
            expr.hir_id, op, is_assign
        );

        let lhs_ty = match is_assign {
            IsAssign::No => {
                // Find a suitable supertype of the LHS expression's type, by coercing to
                // a type variable, to pass as the `Self` to the trait, avoiding invariant
                // trait matching creating lifetime constraints that are too strict.
                // e.g., adding `&'a T` and `&'b T`, given `&'x T: Add<&'x T>`, will result
                // in `&'a T <: &'x T` and `&'b T <: &'x T`, instead of `'a = 'b = 'x`.
                let lhs_ty = self.check_expr_with_needs(lhs_expr, Needs::None);
                let fresh_var = self.next_ty_var(TypeVariableOrigin {
                    kind: TypeVariableOriginKind::MiscVariable,
                    span: lhs_expr.span,
                });
                self.demand_coerce(lhs_expr, lhs_ty, fresh_var, AllowTwoPhase::No)
            }
            IsAssign::Yes => {
                // rust-lang/rust#52126: We have to use strict
                // equivalence on the LHS of an assign-op like `+=`;
                // overwritten or mutably-borrowed places cannot be
                // coerced to a supertype.
                self.check_expr_with_needs(lhs_expr, Needs::MutPlace)
            }
        };
        let lhs_ty = self.resolve_vars_with_obligations(lhs_ty);

        // N.B., as we have not yet type-checked the RHS, we don't have the
        // type at hand. Make a variable to represent it. The whole reason
        // for this indirection is so that, below, we can check the expr
        // using this variable as the expected type, which sometimes lets
        // us do better coercions than we would be able to do otherwise,
        // particularly for things like `String + &String`.
        let rhs_ty_var = self.next_ty_var(TypeVariableOrigin {
            kind: TypeVariableOriginKind::MiscVariable,
            span: rhs_expr.span,
        });

        let result = self.lookup_op_method(lhs_ty, &[rhs_ty_var], Op::Binary(op, is_assign));

        // see `NB` above
        let rhs_ty = self.check_expr_coercable_to_type(rhs_expr, rhs_ty_var);
        let rhs_ty = self.resolve_vars_with_obligations(rhs_ty);

        let return_ty = match result {
            Ok(method) => {
                let by_ref_binop = !op.node.is_by_value();
                if is_assign == IsAssign::Yes || by_ref_binop {
                    if let ty::Ref(region, _, mutbl) = method.sig.inputs()[0].kind {
                        let mutbl = match mutbl {
                            hir::Mutability::Not => AutoBorrowMutability::Not,
                            hir::Mutability::Mut => AutoBorrowMutability::Mut {
                                // Allow two-phase borrows for binops in initial deployment
                                // since they desugar to methods
                                allow_two_phase_borrow: AllowTwoPhase::Yes,
                            },
                        };
                        let autoref = Adjustment {
                            kind: Adjust::Borrow(AutoBorrow::Ref(region, mutbl)),
                            target: method.sig.inputs()[0],
                        };
                        self.apply_adjustments(lhs_expr, vec![autoref]);
                    }
                }
                if by_ref_binop {
                    if let ty::Ref(region, _, mutbl) = method.sig.inputs()[1].kind {
                        let mutbl = match mutbl {
                            hir::Mutability::Not => AutoBorrowMutability::Not,
                            hir::Mutability::Mut => AutoBorrowMutability::Mut {
                                // Allow two-phase borrows for binops in initial deployment
                                // since they desugar to methods
                                allow_two_phase_borrow: AllowTwoPhase::Yes,
                            },
                        };
                        let autoref = Adjustment {
                            kind: Adjust::Borrow(AutoBorrow::Ref(region, mutbl)),
                            target: method.sig.inputs()[1],
                        };
                        // HACK(eddyb) Bypass checks due to reborrows being in
                        // some cases applied on the RHS, on top of which we need
                        // to autoref, which is not allowed by apply_adjustments.
                        // self.apply_adjustments(rhs_expr, vec![autoref]);
                        self.tables
                            .borrow_mut()
                            .adjustments_mut()
                            .entry(rhs_expr.hir_id)
                            .or_default()
                            .push(autoref);
                    }
                }
                self.write_method_call(expr.hir_id, method);

                method.sig.output()
            }
            Err(()) => {
                // error types are considered "builtin"
                if !lhs_ty.references_error() && !rhs_ty.references_error() {
                    let source_map = self.tcx.sess.source_map();

                    match is_assign {
                        IsAssign::Yes => {
                            let mut err = struct_span_err!(
                                self.tcx.sess,
                                expr.span,
                                E0368,
                                "binary assignment operation `{}=` cannot be applied to type `{}`",
                                op.node.as_str(),
                                lhs_ty,
                            );
                            err.span_label(
                                lhs_expr.span,
                                format!("cannot use `{}=` on type `{}`", op.node.as_str(), lhs_ty),
                            );
                            let mut suggested_deref = false;
                            if let Ref(_, rty, _) = lhs_ty.kind {
                                if {
                                    self.infcx.type_is_copy_modulo_regions(
                                        self.param_env,
                                        rty,
                                        lhs_expr.span,
                                    ) && self
                                        .lookup_op_method(rty, &[rhs_ty], Op::Binary(op, is_assign))
                                        .is_ok()
                                } {
                                    if let Ok(lstring) = source_map.span_to_snippet(lhs_expr.span) {
                                        let msg = &format!(
                                            "`{}=` can be used on '{}', you can dereference `{}`",
                                            op.node.as_str(),
                                            rty.peel_refs(),
                                            lstring,
                                        );
                                        err.span_suggestion(
                                            lhs_expr.span,
                                            msg,
                                            format!("*{}", lstring),
                                            rustc_errors::Applicability::MachineApplicable,
                                        );
                                        suggested_deref = true;
                                    }
                                }
                            }
                            let missing_trait = match op.node {
                                hir::BinOpKind::Add => Some("std::ops::AddAssign"),
                                hir::BinOpKind::Sub => Some("std::ops::SubAssign"),
                                hir::BinOpKind::Mul => Some("std::ops::MulAssign"),
                                hir::BinOpKind::Div => Some("std::ops::DivAssign"),
                                hir::BinOpKind::Rem => Some("std::ops::RemAssign"),
                                hir::BinOpKind::BitAnd => Some("std::ops::BitAndAssign"),
                                hir::BinOpKind::BitXor => Some("std::ops::BitXorAssign"),
                                hir::BinOpKind::BitOr => Some("std::ops::BitOrAssign"),
                                hir::BinOpKind::Shl => Some("std::ops::ShlAssign"),
                                hir::BinOpKind::Shr => Some("std::ops::ShrAssign"),
                                _ => None,
                            };
                            if let Some(missing_trait) = missing_trait {
                                if op.node == hir::BinOpKind::Add
                                    && self.check_str_addition(
                                        lhs_expr, rhs_expr, lhs_ty, rhs_ty, &mut err, true, op,
                                    )
                                {
                                    // This has nothing here because it means we did string
                                    // concatenation (e.g., "Hello " += "World!"). This means
                                    // we don't want the note in the else clause to be emitted
                                } else if let ty::Param(p) = lhs_ty.kind {
                                    suggest_constraining_param(
                                        self.tcx,
                                        self.body_id,
                                        &mut err,
                                        lhs_ty,
                                        rhs_ty,
                                        missing_trait,
                                        p,
                                        false,
                                    );
                                } else if !suggested_deref {
                                    suggest_impl_missing(&mut err, lhs_ty, &missing_trait);
                                }
                            }
                            err.emit();
                        }
                        IsAssign::No => {
                            let (message, missing_trait, use_output) = match op.node {
                                hir::BinOpKind::Add => (
                                    format!("cannot add `{}` to `{}`", rhs_ty, lhs_ty),
                                    Some("std::ops::Add"),
                                    true,
                                ),
                                hir::BinOpKind::Sub => (
                                    format!("cannot subtract `{}` from `{}`", rhs_ty, lhs_ty),
                                    Some("std::ops::Sub"),
                                    true,
                                ),
                                hir::BinOpKind::Mul => (
                                    format!("cannot multiply `{}` to `{}`", rhs_ty, lhs_ty),
                                    Some("std::ops::Mul"),
                                    true,
                                ),
                                hir::BinOpKind::Div => (
                                    format!("cannot divide `{}` by `{}`", lhs_ty, rhs_ty),
                                    Some("std::ops::Div"),
                                    true,
                                ),
                                hir::BinOpKind::Rem => (
                                    format!("cannot mod `{}` by `{}`", lhs_ty, rhs_ty),
                                    Some("std::ops::Rem"),
                                    true,
                                ),
                                hir::BinOpKind::BitAnd => (
                                    format!("no implementation for `{} & {}`", lhs_ty, rhs_ty),
                                    Some("std::ops::BitAnd"),
                                    true,
                                ),
                                hir::BinOpKind::BitXor => (
                                    format!("no implementation for `{} ^ {}`", lhs_ty, rhs_ty),
                                    Some("std::ops::BitXor"),
                                    true,
                                ),
                                hir::BinOpKind::BitOr => (
                                    format!("no implementation for `{} | {}`", lhs_ty, rhs_ty),
                                    Some("std::ops::BitOr"),
                                    true,
                                ),
                                hir::BinOpKind::Shl => (
                                    format!("no implementation for `{} << {}`", lhs_ty, rhs_ty),
                                    Some("std::ops::Shl"),
                                    true,
                                ),
                                hir::BinOpKind::Shr => (
                                    format!("no implementation for `{} >> {}`", lhs_ty, rhs_ty),
                                    Some("std::ops::Shr"),
                                    true,
                                ),
                                hir::BinOpKind::Eq | hir::BinOpKind::Ne => (
                                    format!(
                                        "binary operation `{}` cannot be applied to type `{}`",
                                        op.node.as_str(),
                                        lhs_ty
                                    ),
                                    Some("std::cmp::PartialEq"),
                                    false,
                                ),
                                hir::BinOpKind::Lt
                                | hir::BinOpKind::Le
                                | hir::BinOpKind::Gt
                                | hir::BinOpKind::Ge => (
                                    format!(
                                        "binary operation `{}` cannot be applied to type `{}`",
                                        op.node.as_str(),
                                        lhs_ty
                                    ),
                                    Some("std::cmp::PartialOrd"),
                                    false,
                                ),
                                _ => (
                                    format!(
                                        "binary operation `{}` cannot be applied to type `{}`",
                                        op.node.as_str(),
                                        lhs_ty
                                    ),
                                    None,
                                    false,
                                ),
                            };
                            let mut err = struct_span_err!(
                                self.tcx.sess,
                                op.span,
                                E0369,
                                "{}",
                                message.as_str()
                            );

                            let mut involves_fn = false;
                            if !lhs_expr.span.eq(&rhs_expr.span) {
                                involves_fn |= self.add_type_neq_err_label(
                                    &mut err,
                                    lhs_expr.span,
                                    lhs_ty,
                                    rhs_ty,
                                    op,
                                    is_assign,
                                );
                                involves_fn |= self.add_type_neq_err_label(
                                    &mut err,
                                    rhs_expr.span,
                                    rhs_ty,
                                    lhs_ty,
                                    op,
                                    is_assign,
                                );
                            }

                            let mut suggested_deref = false;
                            if let Ref(_, rty, _) = lhs_ty.kind {
                                if {
                                    self.infcx.type_is_copy_modulo_regions(
                                        self.param_env,
                                        rty,
                                        lhs_expr.span,
                                    ) && self
                                        .lookup_op_method(rty, &[rhs_ty], Op::Binary(op, is_assign))
                                        .is_ok()
                                } {
                                    if let Ok(lstring) = source_map.span_to_snippet(lhs_expr.span) {
                                        err.help(&format!(
                                            "`{}` can be used on '{}', you can \
                                            dereference `{2}`: `*{2}`",
                                            op.node.as_str(),
                                            rty.peel_refs(),
                                            lstring
                                        ));
                                        suggested_deref = true;
                                    }
                                }
                            }
                            if let Some(missing_trait) = missing_trait {
                                if op.node == hir::BinOpKind::Add
                                    && self.check_str_addition(
                                        lhs_expr, rhs_expr, lhs_ty, rhs_ty, &mut err, false, op,
                                    )
                                {
                                    // This has nothing here because it means we did string
                                    // concatenation (e.g., "Hello " + "World!"). This means
                                    // we don't want the note in the else clause to be emitted
                                } else if let ty::Param(p) = lhs_ty.kind {
                                    suggest_constraining_param(
                                        self.tcx,
                                        self.body_id,
                                        &mut err,
                                        lhs_ty,
                                        rhs_ty,
                                        missing_trait,
                                        p,
                                        use_output,
                                    );
                                } else if !suggested_deref && !involves_fn {
                                    suggest_impl_missing(&mut err, lhs_ty, &missing_trait);
                                }
                            }
                            err.emit();
                        }
                    }
                }
                self.tcx.types.err
            }
        };

        (lhs_ty, rhs_ty, return_ty)
    }

    /// If one of the types is an uncalled function and calling it would yield the other type,
    /// suggest calling the function. Returns `true` if suggestion would apply (even if not given).
    fn add_type_neq_err_label(
        &self,
        err: &mut rustc_errors::DiagnosticBuilder<'_>,
        span: Span,
        ty: Ty<'tcx>,
        other_ty: Ty<'tcx>,
        op: hir::BinOp,
        is_assign: IsAssign,
    ) -> bool /* did we suggest to call a function because of missing parenthesis? */ {
        err.span_label(span, ty.to_string());
        if let FnDef(def_id, _) = ty.kind {
            let source_map = self.tcx.sess.source_map();
            if !self.tcx.has_typeck_tables(def_id) {
                return false;
            }
            // We're emitting a suggestion, so we can just ignore regions
            let fn_sig = *self.tcx.fn_sig(def_id).skip_binder();

            let other_ty = if let FnDef(def_id, _) = other_ty.kind {
                if !self.tcx.has_typeck_tables(def_id) {
                    return false;
                }
                // We're emitting a suggestion, so we can just ignore regions
                self.tcx.fn_sig(def_id).skip_binder().output()
            } else {
                other_ty
            };

            if self
                .lookup_op_method(fn_sig.output(), &[other_ty], Op::Binary(op, is_assign))
                .is_ok()
            {
                if let Ok(snippet) = source_map.span_to_snippet(span) {
                    let (variable_snippet, applicability) = if !fn_sig.inputs().is_empty() {
                        (format!("{}( /* arguments */ )", snippet), Applicability::HasPlaceholders)
                    } else {
                        (format!("{}()", snippet), Applicability::MaybeIncorrect)
                    };

                    err.span_suggestion(
                        span,
                        "you might have forgotten to call this function",
                        variable_snippet,
                        applicability,
                    );
                }
                return true;
            }
        }
        false
    }

    /// Provide actionable suggestions when trying to add two strings with incorrect types,
    /// like `&str + &str`, `String + String` and `&str + &String`.
    ///
    /// If this function returns `true` it means a note was printed, so we don't need
    /// to print the normal "implementation of `std::ops::Add` might be missing" note
    fn check_str_addition(
        &self,
        lhs_expr: &'tcx hir::Expr<'tcx>,
        rhs_expr: &'tcx hir::Expr<'tcx>,
        lhs_ty: Ty<'tcx>,
        rhs_ty: Ty<'tcx>,
        err: &mut rustc_errors::DiagnosticBuilder<'_>,
        is_assign: bool,
        op: hir::BinOp,
    ) -> bool {
        let source_map = self.tcx.sess.source_map();
        let remove_borrow_msg = "String concatenation appends the string on the right to the \
                                 string on the left and may require reallocation. This \
                                 requires ownership of the string on the left";

        let msg = "`to_owned()` can be used to create an owned `String` \
                   from a string reference. String concatenation \
                   appends the string on the right to the string \
                   on the left and may require reallocation. This \
                   requires ownership of the string on the left";

        let is_std_string = |ty| &format!("{:?}", ty) == "std::string::String";

        match (&lhs_ty.kind, &rhs_ty.kind) {
            (&Ref(_, l_ty, _), &Ref(_, r_ty, _)) // &str or &String + &str, &String or &&str
                if (l_ty.kind == Str || is_std_string(l_ty)) && (
                        r_ty.kind == Str || is_std_string(r_ty) ||
                        &format!("{:?}", rhs_ty) == "&&str"
                    ) =>
            {
                if !is_assign { // Do not supply this message if `&str += &str`
                    err.span_label(
                        op.span,
                        "`+` cannot be used to concatenate two `&str` strings",
                    );
                    match source_map.span_to_snippet(lhs_expr.span) {
                        Ok(lstring) => {
                            err.span_suggestion(
                                lhs_expr.span,
                                if lstring.starts_with('&') {
                                    remove_borrow_msg
                                } else {
                                    msg
                                },
                                if lstring.starts_with('&') {
                                    // let a = String::new();
                                    // let _ = &a + "bar";
                                    lstring[1..].to_string()
                                } else {
                                    format!("{}.to_owned()", lstring)
                                },
                                Applicability::MachineApplicable,
                            )
                        }
                        _ => err.help(msg),
                    };
                }
                true
            }
            (&Ref(_, l_ty, _), &Adt(..)) // Handle `&str` & `&String` + `String`
                if (l_ty.kind == Str || is_std_string(l_ty)) && is_std_string(rhs_ty) =>
            {
                err.span_label(
                    op.span,
                    "`+` cannot be used to concatenate a `&str` with a `String`",
                );
                match (
                    source_map.span_to_snippet(lhs_expr.span),
                    source_map.span_to_snippet(rhs_expr.span),
                    is_assign,
                ) {
                    (Ok(l), Ok(r), false) => {
                        let to_string = if l.starts_with('&') {
                            // let a = String::new(); let b = String::new();
                            // let _ = &a + b;
                            l[1..].to_string()
                        } else {
                            format!("{}.to_owned()", l)
                        };
                        err.multipart_suggestion(
                            msg,
                            vec![
                                (lhs_expr.span, to_string),
                                (rhs_expr.span, format!("&{}", r)),
                            ],
                            Applicability::MachineApplicable,
                        );
                    }
                    _ => {
                        err.help(msg);
                    }
                };
                true
            }
            _ => false,
        }
    }

    pub fn check_user_unop(
        &self,
        ex: &'tcx hir::Expr<'tcx>,
        operand_ty: Ty<'tcx>,
        op: hir::UnOp,
    ) -> Ty<'tcx> {
        assert!(op.is_by_value());
        match self.lookup_op_method(operand_ty, &[], Op::Unary(op, ex.span)) {
            Ok(method) => {
                self.write_method_call(ex.hir_id, method);
                method.sig.output()
            }
            Err(()) => {
                let actual = self.resolve_vars_if_possible(&operand_ty);
                if !actual.references_error() {
                    let mut err = struct_span_err!(
                        self.tcx.sess,
                        ex.span,
                        E0600,
                        "cannot apply unary operator `{}` to type `{}`",
                        op.as_str(),
                        actual
                    );
                    err.span_label(
                        ex.span,
                        format!(
                            "cannot apply unary \
                                                    operator `{}`",
                            op.as_str()
                        ),
                    );
                    match actual.kind {
                        Uint(_) if op == hir::UnOp::UnNeg => {
                            err.note("unsigned values cannot be negated");
                        }
                        Str | Never | Char | Tuple(_) | Array(_, _) => {}
                        Ref(_, ref lty, _) if lty.kind == Str => {}
                        _ => {
                            let missing_trait = match op {
                                hir::UnOp::UnNeg => "std::ops::Neg",
                                hir::UnOp::UnNot => "std::ops::Not",
                                hir::UnOp::UnDeref => "std::ops::UnDerf",
                            };
                            suggest_impl_missing(&mut err, operand_ty, &missing_trait);
                        }
                    }
                    err.emit();
                }
                self.tcx.types.err
            }
        }
    }

    fn lookup_op_method(
        &self,
        lhs_ty: Ty<'tcx>,
        other_tys: &[Ty<'tcx>],
        op: Op,
    ) -> Result<MethodCallee<'tcx>, ()> {
        let lang = self.tcx.lang_items();

        let span = match op {
            Op::Binary(op, _) => op.span,
            Op::Unary(_, span) => span,
        };
        let (opname, trait_did) = if let Op::Binary(op, IsAssign::Yes) = op {
            match op.node {
                hir::BinOpKind::Add => ("add_assign", lang.add_assign_trait()),
                hir::BinOpKind::Sub => ("sub_assign", lang.sub_assign_trait()),
                hir::BinOpKind::Mul => ("mul_assign", lang.mul_assign_trait()),
                hir::BinOpKind::Div => ("div_assign", lang.div_assign_trait()),
                hir::BinOpKind::Rem => ("rem_assign", lang.rem_assign_trait()),
                hir::BinOpKind::BitXor => ("bitxor_assign", lang.bitxor_assign_trait()),
                hir::BinOpKind::BitAnd => ("bitand_assign", lang.bitand_assign_trait()),
                hir::BinOpKind::BitOr => ("bitor_assign", lang.bitor_assign_trait()),
                hir::BinOpKind::Shl => ("shl_assign", lang.shl_assign_trait()),
                hir::BinOpKind::Shr => ("shr_assign", lang.shr_assign_trait()),
                hir::BinOpKind::Lt
                | hir::BinOpKind::Le
                | hir::BinOpKind::Ge
                | hir::BinOpKind::Gt
                | hir::BinOpKind::Eq
                | hir::BinOpKind::Ne
                | hir::BinOpKind::And
                | hir::BinOpKind::Or => {
                    span_bug!(span, "impossible assignment operation: {}=", op.node.as_str())
                }
            }
        } else if let Op::Binary(op, IsAssign::No) = op {
            match op.node {
                hir::BinOpKind::Add => ("add", lang.add_trait()),
                hir::BinOpKind::Sub => ("sub", lang.sub_trait()),
                hir::BinOpKind::Mul => ("mul", lang.mul_trait()),
                hir::BinOpKind::Div => ("div", lang.div_trait()),
                hir::BinOpKind::Rem => ("rem", lang.rem_trait()),
                hir::BinOpKind::BitXor => ("bitxor", lang.bitxor_trait()),
                hir::BinOpKind::BitAnd => ("bitand", lang.bitand_trait()),
                hir::BinOpKind::BitOr => ("bitor", lang.bitor_trait()),
                hir::BinOpKind::Shl => ("shl", lang.shl_trait()),
                hir::BinOpKind::Shr => ("shr", lang.shr_trait()),
                hir::BinOpKind::Lt => ("lt", lang.partial_ord_trait()),
                hir::BinOpKind::Le => ("le", lang.partial_ord_trait()),
                hir::BinOpKind::Ge => ("ge", lang.partial_ord_trait()),
                hir::BinOpKind::Gt => ("gt", lang.partial_ord_trait()),
                hir::BinOpKind::Eq => ("eq", lang.eq_trait()),
                hir::BinOpKind::Ne => ("ne", lang.eq_trait()),
                hir::BinOpKind::And | hir::BinOpKind::Or => {
                    span_bug!(span, "&& and || are not overloadable")
                }
            }
        } else if let Op::Unary(hir::UnOp::UnNot, _) = op {
            ("not", lang.not_trait())
        } else if let Op::Unary(hir::UnOp::UnNeg, _) = op {
            ("neg", lang.neg_trait())
        } else {
            bug!("lookup_op_method: op not supported: {:?}", op)
        };

        debug!(
            "lookup_op_method(lhs_ty={:?}, op={:?}, opname={:?}, trait_did={:?})",
            lhs_ty, op, opname, trait_did
        );

        let method = trait_did.and_then(|trait_did| {
            let opname = Ident::from_str(opname);
            self.lookup_method_in_trait(span, opname, trait_did, lhs_ty, Some(other_tys))
        });

        match method {
            Some(ok) => {
                let method = self.register_infer_ok_obligations(ok);
                self.select_obligations_where_possible(false, |_| {});

                Ok(method)
            }
            None => Err(()),
        }
    }
}

// Binary operator categories. These categories summarize the behavior
// with respect to the builtin operationrs supported.
enum BinOpCategory {
    /// &&, || -- cannot be overridden
    Shortcircuit,

    /// <<, >> -- when shifting a single integer, rhs can be any
    /// integer type. For simd, types must match.
    Shift,

    /// +, -, etc -- takes equal types, produces same type as input,
    /// applicable to ints/floats/simd
    Math,

    /// &, |, ^ -- takes equal types, produces same type as input,
    /// applicable to ints/floats/simd/bool
    Bitwise,

    /// ==, !=, etc -- takes equal types, produces bools, except for simd,
    /// which produce the input type
    Comparison,
}

impl BinOpCategory {
    fn from(op: hir::BinOp) -> BinOpCategory {
        match op.node {
            hir::BinOpKind::Shl | hir::BinOpKind::Shr => BinOpCategory::Shift,

            hir::BinOpKind::Add
            | hir::BinOpKind::Sub
            | hir::BinOpKind::Mul
            | hir::BinOpKind::Div
            | hir::BinOpKind::Rem => BinOpCategory::Math,

            hir::BinOpKind::BitXor | hir::BinOpKind::BitAnd | hir::BinOpKind::BitOr => {
                BinOpCategory::Bitwise
            }

            hir::BinOpKind::Eq
            | hir::BinOpKind::Ne
            | hir::BinOpKind::Lt
            | hir::BinOpKind::Le
            | hir::BinOpKind::Ge
            | hir::BinOpKind::Gt => BinOpCategory::Comparison,

            hir::BinOpKind::And | hir::BinOpKind::Or => BinOpCategory::Shortcircuit,
        }
    }
}

/// Whether the binary operation is an assignment (`a += b`), or not (`a + b`)
#[derive(Clone, Copy, Debug, PartialEq)]
enum IsAssign {
    No,
    Yes,
}

#[derive(Clone, Copy, Debug)]
enum Op {
    Binary(hir::BinOp, IsAssign),
    Unary(hir::UnOp, Span),
}

/// Dereferences a single level of immutable referencing.
fn deref_ty_if_possible(ty: Ty<'tcx>) -> Ty<'tcx> {
    match ty.kind {
        ty::Ref(_, ty, hir::Mutability::Not) => ty,
        _ => ty,
    }
}

/// Returns `true` if this is a built-in arithmetic operation (e.g., u32
/// + u32, i16x4 == i16x4) and false if these types would have to be
/// overloaded to be legal. There are two reasons that we distinguish
/// builtin operations from overloaded ones (vs trying to drive
/// everything uniformly through the trait system and intrinsics or
/// something like that):
///
/// 1. Builtin operations can trivially be evaluated in constants.
/// 2. For comparison operators applied to SIMD types the result is
///    not of type `bool`. For example, `i16x4 == i16x4` yields a
///    type like `i16x4`. This means that the overloaded trait
///    `PartialEq` is not applicable.
///
/// Reason #2 is the killer. I tried for a while to always use
/// overloaded logic and just check the types in constants/codegen after
/// the fact, and it worked fine, except for SIMD types. -nmatsakis
fn is_builtin_binop<'tcx>(lhs: Ty<'tcx>, rhs: Ty<'tcx>, op: hir::BinOp) -> bool {
    // Special-case a single layer of referencing, so that things like `5.0 + &6.0f32` work.
    // (See https://github.com/rust-lang/rust/issues/57447.)
    let (lhs, rhs) = (deref_ty_if_possible(lhs), deref_ty_if_possible(rhs));

    match BinOpCategory::from(op) {
        BinOpCategory::Shortcircuit => true,

        BinOpCategory::Shift => {
            lhs.references_error()
                || rhs.references_error()
                || lhs.is_integral() && rhs.is_integral()
        }

        BinOpCategory::Math => {
            lhs.references_error()
                || rhs.references_error()
                || lhs.is_integral() && rhs.is_integral()
                || lhs.is_floating_point() && rhs.is_floating_point()
        }

        BinOpCategory::Bitwise => {
            lhs.references_error()
                || rhs.references_error()
                || lhs.is_integral() && rhs.is_integral()
                || lhs.is_floating_point() && rhs.is_floating_point()
                || lhs.is_bool() && rhs.is_bool()
        }

        BinOpCategory::Comparison => {
            lhs.references_error() || rhs.references_error() || lhs.is_scalar() && rhs.is_scalar()
        }
    }
}

/// If applicable, note that an implementation of `trait` for `ty` may fix the error.
fn suggest_impl_missing(err: &mut DiagnosticBuilder<'_>, ty: Ty<'_>, missing_trait: &str) {
    if let Adt(def, _) = ty.peel_refs().kind {
        if def.did.is_local() {
            err.note(&format!(
                "an implementation of `{}` might \
                be missing for `{}`",
                missing_trait, ty
            ));
        }
    }
}

fn suggest_constraining_param(
    tcx: TyCtxt<'_>,
    body_id: hir::HirId,
    mut err: &mut DiagnosticBuilder<'_>,
    lhs_ty: Ty<'_>,
    rhs_ty: Ty<'_>,
    missing_trait: &str,
    p: ty::ParamTy,
    set_output: bool,
) {
    let hir = tcx.hir();
    let msg = &format!("`{}` might need a bound for `{}`", lhs_ty, missing_trait);
    // Try to find the def-id and details for the parameter p. We have only the index,
    // so we have to find the enclosing function's def-id, then look through its declared
    // generic parameters to get the declaration.
    let def_id = hir.body_owner_def_id(hir::BodyId { hir_id: body_id });
    let generics = tcx.generics_of(def_id);
    let param_def_id = generics.type_param(&p, tcx).def_id;
    if let Some(generics) = param_def_id
        .as_local()
        .map(|id| hir.as_local_hir_id(id))
        .and_then(|id| hir.find(hir.get_parent_item(id)))
        .as_ref()
        .and_then(|node| node.generics())
    {
        let output = if set_output { format!("<Output = {}>", rhs_ty) } else { String::new() };
        suggest_constraining_type_param(
            tcx,
            generics,
            &mut err,
            &format!("{}", lhs_ty),
            &format!("{}{}", missing_trait, output),
            None,
        );
    } else {
        let span = tcx.def_span(param_def_id);
        err.span_label(span, msg);
    }
}
