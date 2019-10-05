use std::{marker::PhantomData, sync::Arc};

use crate::base::{
    ast::TypedIdent,
    fnv::FnvSet,
    merge::{merge, merge_collect, merge_fn, merge_iter},
    pos,
    symbol::Symbol,
    types::{ArcType, Field, TypeEnv, TypeExt},
};

use crate::core::{
    dead_code::{self},
    interpreter::Global,
    Allocator, Alternative, ArenaAllocatable, ArenaExt, CExpr, Closure, CoreExpr, Expr, LetBinding,
    Named, Pattern,
};

pub trait OptimizeEnv: TypeEnv {
    fn find_expr(&self, id: &Symbol) -> Option<Global<CoreExpr>>;
}

pub trait Produce<'a, 'b, P, Input> {
    fn produce_with(input: &'b Input, producer: &mut P) -> Self;
}

impl<'a, 'b, P> Produce<'a, 'b, P, Expr<'b>> for Expr<'a>
where
    P: ExprProducer<'a, 'b>,
{
    fn produce_with(input: &'b Expr<'b>, producer: &mut P) -> Self {
        producer.produce(input).clone()
    }
}

impl<'a, 'b, P> Produce<'a, 'b, P, Expr<'b>> for CExpr<'a>
where
    P: ExprProducer<'a, 'b>,
{
    fn produce_with(input: &'b Expr<'b>, producer: &mut P) -> Self {
        producer.produce(input)
    }
}

impl<'a, 'b, P> Produce<'a, 'b, P, Alternative<'b>> for Alternative<'a>
where
    P: ExprProducer<'a, 'b>,
{
    fn produce_with(input: &'b Alternative<'b>, producer: &mut P) -> Self {
        producer.produce_alt(input)
    }
}

pub trait ExprProducer<'a, 'b> {
    fn new(allocator: &'a Allocator<'a>) -> Self
    where
        Self: Sized;
    fn produce(&mut self, expr: CExpr<'b>) -> CExpr<'a>;
    fn produce_slice(&mut self, expr: &'b [Expr<'b>]) -> &'a [Expr<'a>];
    fn produce_alt(&mut self, alt: &'b Alternative<'b>) -> Alternative<'a>;
}

pub struct SameLifetime<'a>(&'a Allocator<'a>);
impl<'a> ExprProducer<'a, 'a> for SameLifetime<'a> {
    fn new(allocator: &'a Allocator<'a>) -> Self {
        SameLifetime(allocator)
    }
    fn produce(&mut self, expr: CExpr<'a>) -> CExpr<'a> {
        expr
    }
    fn produce_slice(&mut self, expr: &'a [Expr<'a>]) -> &'a [Expr<'a>] {
        expr
    }
    fn produce_alt(&mut self, alt: &'a Alternative<'a>) -> Alternative<'a> {
        alt.clone()
    }
}

impl<'a> Visitor<'a, 'a> for SameLifetime<'a> {
    type Producer = SameLifetime<'a>;

    fn visit_expr(&mut self, expr: &'a Expr<'a>) -> Option<&'a Expr<'a>> {
        Some(self.produce(expr))
    }
    fn detach_allocator(&self) -> Option<&'a Allocator<'a>> {
        Some(self.0)
    }
}

pub struct DifferentLifetime<'a, 'b>(&'a Allocator<'a>, PhantomData<&'b ()>);

impl<'a, 'b> ExprProducer<'a, 'b> for DifferentLifetime<'a, 'b> {
    fn new(allocator: &'a Allocator<'a>) -> Self {
        DifferentLifetime(allocator, PhantomData)
    }
    fn produce(&mut self, expr: CExpr<'b>) -> CExpr<'a> {
        match *expr {
            Expr::Const(ref id, ref span) => {
                self.0.arena.alloc(Expr::Const(id.clone(), span.clone()))
            }
            Expr::Ident(ref id, ref span) => {
                self.0.arena.alloc(Expr::Ident(id.clone(), span.clone()))
            }
            Expr::Data(ref id, args, pos) if args.is_empty() => {
                self.0.arena.alloc(Expr::Data(id.clone(), &[], pos.clone()))
            }
            _ => walk_expr_alloc(self, expr).unwrap(),
        }
    }
    fn produce_slice(&mut self, exprs: &'b [Expr<'b>]) -> &'a [Expr<'a>] {
        self.0
            .arena
            .alloc_fixed(exprs.iter().map(|expr| match *expr {
                Expr::Const(ref id, ref span) => Expr::Const(id.clone(), span.clone()),
                Expr::Ident(ref id, ref span) => Expr::Ident(id.clone(), span.clone()),
                Expr::Data(ref id, args, pos) if args.is_empty() => {
                    Expr::Data(id.clone(), &[], pos.clone())
                }
                _ => walk_expr(self, expr).unwrap(),
            }))
    }
    fn produce_alt(&mut self, alt: &'b Alternative<'b>) -> Alternative<'a> {
        Alternative {
            pattern: alt.pattern.clone(),
            expr: self.produce(alt.expr),
        }
    }
}

impl<'a, 'b> Visitor<'a, 'b> for DifferentLifetime<'a, 'b> {
    type Producer = DifferentLifetime<'a, 'b>;

    fn visit_expr(&mut self, expr: &'b Expr<'b>) -> Option<&'a Expr<'a>> {
        Some(self.produce(expr))
    }

    fn detach_allocator(&self) -> Option<&'a Allocator<'a>> {
        Some(self.0)
    }
}

pub trait Visitor<'a, 'b> {
    type Producer: ExprProducer<'a, 'b>;

    fn visit_expr(&mut self, expr: CExpr<'b>) -> Option<&'a Expr<'a>>;

    fn visit_expr_(&mut self, expr: CExpr<'b>) -> Option<Expr<'a>> {
        self.visit_expr(expr).map(Clone::clone)
    }

    fn visit_binding(&mut self, _: &Symbol) -> Option<Symbol> {
        None
    }

    fn visit_pattern(&mut self, expr: &'b Pattern) -> Option<Pattern> {
        walk_pattern(self, expr)
    }

    fn visit_alt(&mut self, alt: &'b Alternative<'b>) -> Option<Alternative<'a>> {
        let new_expr = self.visit_expr(alt.expr);
        new_expr.map(|expr| Alternative {
            pattern: alt.pattern.clone(),
            expr: expr,
        })
    }

    fn detach_allocator(&self) -> Option<&'a Allocator<'a>>;
    fn allocator(&self) -> &'a Allocator<'a> {
        self.detach_allocator().expect("Allocator")
    }
}

struct RecognizeUnnecessaryAllocation<'a> {
    allocator: &'a Allocator<'a>,
}

impl<'a> Visitor<'a, 'a> for RecognizeUnnecessaryAllocation<'a> {
    type Producer = SameLifetime<'a>;

    fn visit_expr(&mut self, expr: &'a Expr<'a>) -> Option<&'a Expr<'a>> {
        fn make_let<'b>(
            self_: &mut RecognizeUnnecessaryAllocation<'b>,
            fields: &[(TypedIdent<Symbol>, Option<Symbol>)],
            next_expr: &'b Expr<'b>,
            field: &Field<Symbol, ArcType>,
            expr: &'b Expr<'b>,
        ) -> &'b Expr<'b> {
            let pattern_field = fields
                .iter()
                .find(|f| f.0.name.name_eq(&field.name))
                .map(|pattern_field| {
                    pattern_field
                        .1
                        .as_ref()
                        .unwrap_or(&pattern_field.0.name)
                        .clone()
                })
                .unwrap_or_else(|| Symbol::from("dummy"));
            let new_expr = Expr::Let(
                self_.allocator.let_binding_arena.alloc(LetBinding {
                    name: TypedIdent {
                        name: pattern_field.clone(),
                        typ: field.typ.clone(),
                    },
                    expr: Named::Expr(expr),
                    span_start: pos::BytePos::default(),
                }),
                next_expr,
            );
            &*self_.allocator().arena.alloc(new_expr)
        }
        // Avoids boxing data which are destructured immediately after creation
        // match { l, r } with
        // | { l, r } -> ...
        //
        // to
        //
        // let l = x
        // let l = y
        // ...
        match *expr {
            Expr::Match(&Expr::Data(ref id, exprs, ..), alts) if alts.len() == 1 => {
                match alts[0].pattern {
                    Pattern::Record(ref fields) => {
                        debug_assert!(id.typ.row_iter().len() >= fields.len());
                        let next_expr = alts[0].expr;
                        Some(
                            id.typ
                                .row_iter()
                                .zip(exprs)
                                .collect::<Vec<_>>()
                                .into_iter()
                                .rev()
                                .fold(next_expr, |next_expr, (field, expr)| {
                                    make_let(self, fields, next_expr, field, expr)
                                }),
                        )
                    }
                    _ => walk_expr_alloc(self, expr),
                }
            }
            _ => walk_expr_alloc(self, expr),
        }
    }

    fn detach_allocator(&self) -> Option<&'a Allocator<'a>> {
        Some(self.allocator)
    }
}

fn optimize_unnecessary_allocation<'a>(
    allocator: &'a Allocator<'a>,
    expr: &'a Expr<'a>,
) -> &'a Expr<'a> {
    let mut optimizer = RecognizeUnnecessaryAllocation { allocator };
    optimizer.visit_expr(expr).unwrap_or(expr)
}

pub fn optimize<'a>(
    allocator: &'a Arc<Allocator<'a>>,
    env: &'a dyn OptimizeEnv<Type = ArcType>,
    expr: &'a Expr<'a>,
) -> Global<CoreExpr> {
    let expr = optimize_unnecessary_allocation(allocator, expr);

    let pure_symbols = crate::core::purity::purity(expr);

    let mut dep_graph = dead_code::DepGraph::default();
    let used_bindings = dep_graph.used_bindings(expr);
    let cyclic_bindings: FnvSet<_> = dep_graph.cycles().flat_map(|cycle| cycle).collect();

    let expr = dead_code::dead_code_elimination(&used_bindings, &pure_symbols, allocator, expr);

    let costs = crate::core::costs::analyze_costs(&cyclic_bindings, expr);

    let f = |symbol: &Symbol| {
        env.find_expr(symbol)
            .map(crate::core::interpreter::Binding::Expr)
    };
    let mut interpreter = crate::core::interpreter::Compiler::new(allocator, &f)
        .costs(costs)
        .pure_symbols(&pure_symbols);
    let expr = interpreter.compile_expr(expr).ok().unwrap_or(expr);

    let mut dep_graph = dead_code::DepGraph::default();
    let used_bindings = dep_graph.used_bindings(expr);
    let expr = dead_code::dead_code_elimination(&used_bindings, &pure_symbols, allocator, expr);

    Global {
        value: crate::core::freeze_expr(allocator, expr),
        info: Arc::new(interpreter.optimizer_info(allocator)),
    }
}

pub fn walk_expr_alloc<'a, 'b, V>(visitor: &mut V, expr: CExpr<'b>) -> Option<CExpr<'a>>
where
    V: ?Sized + Visitor<'a, 'b>,
    V::Producer: Visitor<'a, 'b, Producer = V::Producer>,
{
    walk_expr(visitor, expr).map(|expr| &*visitor.allocator().arena.alloc(expr))
}

pub fn walk_expr<'a, 'b, V>(visitor: &mut V, expr: CExpr<'b>) -> Option<Expr<'a>>
where
    V: ?Sized + Visitor<'a, 'b>,
    V::Producer: Visitor<'a, 'b, Producer = V::Producer>,
{
    let allocator: Option<&'a Allocator<'a>> = visitor.detach_allocator();
    match *expr {
        Expr::Call(f, args) => {
            let new_f = visitor.visit_expr(f);
            let new_args = merge_slice_produce::<V::Producer, _, _, _>(allocator, args, |expr| {
                visitor.visit_expr_(expr)
            });

            merge_fn(
                &f,
                |e| V::Producer::new(visitor.allocator()).produce(e),
                new_f,
                &args,
                |a| {
                    let a = a
                        .iter()
                        .map(|a| V::Producer::new(visitor.allocator()).produce(a).clone());
                    &*visitor.allocator().arena.alloc_fixed(a)
                },
                new_args,
                Expr::Call,
            )
        }
        Expr::Const(_, _) | Expr::Ident(_, _) => None,
        Expr::Data(ref id, exprs, pos) => {
            merge_slice_produce::<V::Producer, _, _, _>(allocator, exprs, |expr| {
                visitor.visit_expr_(expr)
            })
            .map(|exprs| Expr::Data(id.clone(), exprs, pos))
        }
        Expr::Let(ref bind, expr) => {
            let new_bind = walk_bind(visitor, bind);
            let new_expr = visitor.visit_expr(expr);
            merge_fn(
                bind,
                |bind| walk_bind(&mut V::Producer::new(visitor.allocator()), bind).unwrap(),
                new_bind,
                &expr,
                |e| V::Producer::new(visitor.allocator()).produce(e),
                new_expr,
                Expr::Let,
            )
        }
        Expr::Match(expr, alts) => {
            let new_expr = visitor.visit_expr(expr);
            let new_alts = merge_slice_produce::<V::Producer, _, _, _>(allocator, alts, |alt| {
                visitor.visit_alt(alt)
            });
            merge_fn(
                &expr,
                |e| V::Producer::new(visitor.allocator()).produce(e),
                new_expr,
                &alts,
                |a| {
                    let a = a.iter().map(|a| Alternative {
                        pattern: a.pattern.clone(),
                        expr: V::Producer::new(visitor.allocator()).produce(a.expr),
                    });
                    visitor.allocator().alternative_arena.alloc_fixed(a)
                },
                new_alts,
                Expr::Match,
            )
        }

        Expr::Cast(expr, ref typ) => visitor
            .visit_expr(expr)
            .map(|expr| Expr::Cast(expr, typ.clone())),
    }
}

pub fn walk_pattern<'a, 'b, V>(visitor: &mut V, pattern: &'b Pattern) -> Option<Pattern>
where
    V: ?Sized + Visitor<'a, 'b>,
{
    match pattern {
        Pattern::Ident(id) => visitor.visit_binding(&id.name).map(|name| {
            Pattern::Ident(TypedIdent {
                typ: id.typ.clone(),
                name,
            })
        }),
        Pattern::Literal(_) => None,
        Pattern::Constructor(id, fields) => merge_iter(
            fields,
            |field| {
                visitor.visit_binding(&field.name).map(|name| TypedIdent {
                    typ: field.typ.clone(),
                    name,
                })
            },
            |field| field.clone(),
        )
        .map(|iter| Pattern::Constructor(id.clone(), iter.collect())),
        Pattern::Record(fields) => merge_iter(
            fields,
            |field| {
                visitor
                    .visit_binding(field.1.as_ref().unwrap_or(&field.0.name))
                    .map(|name| (field.0.clone(), Some(name)))
            },
            |e| e.clone(),
        )
        .map(|iter| Pattern::Record(iter.collect())),
    }
}

pub fn walk_named<'a, 'b, V>(visitor: &mut V, bind: &Named<'b>) -> Option<Named<'a>>
where
    V: ?Sized + Visitor<'a, 'b>,
{
    match bind {
        Named::Recursive(closures) => walk_closures(visitor, closures).map(Named::Recursive),
        Named::Expr(bind_expr) => visitor.visit_expr(bind_expr).map(Named::Expr),
    }
}

pub fn walk_bind<'a, 'b, V>(visitor: &mut V, bind: &LetBinding<'b>) -> Option<&'a LetBinding<'a>>
where
    V: ?Sized + Visitor<'a, 'b>,
    V::Producer: Visitor<'a, 'b, Producer = V::Producer>,
{
    let allocator = visitor.detach_allocator();
    let new_named = walk_named(visitor, &bind.expr);
    let new_name = match bind.expr {
        Named::Recursive(..) => None,
        Named::Expr(_) => visitor
            .visit_binding(&bind.name.name)
            .map(|name| TypedIdent {
                typ: bind.name.typ.clone(),
                name,
            }),
    };

    merge_fn(
        &bind.name,
        Clone::clone,
        new_name,
        &bind.expr,
        |e| walk_named(&mut V::Producer::new(allocator.unwrap()), e).unwrap(),
        new_named,
        |name, expr| {
            &*allocator
                .expect("Allocator")
                .let_binding_arena
                .alloc(LetBinding {
                    name,
                    expr,
                    span_start: bind.span_start,
                })
        },
    )
}

pub fn walk_closures<'a, 'b, V>(
    visitor: &mut V,
    closures: &[Closure<'b>],
) -> Option<Vec<Closure<'a>>>
where
    V: ?Sized + Visitor<'a, 'b>,
{
    let allocator = visitor.detach_allocator();
    merge_collect(
        closures,
        |closure| {
            visitor.visit_expr(closure.expr).map(|new_expr| Closure {
                pos: closure.pos,
                name: closure.name.clone(),
                args: closure.args.clone(),
                expr: new_expr,
            })
        },
        |closure| Closure {
            pos: closure.pos,
            name: closure.name.clone(),
            args: closure.args.clone(),
            expr: V::Producer::new(allocator.expect("Allocator")).produce(closure.expr),
        },
    )
}

pub fn merge_slice<'a, T, U>(
    allocator: &'a Allocator<'a>,
    slice: &'a [U],
    action: impl FnMut(&'a U) -> Option<T>,
) -> Option<&'a [T]>
where
    U: ArenaAllocatable<'a> + 'a,
    T: ArenaAllocatable<'a> + Produce<'a, 'a, SameLifetime<'a>, U>,
{
    merge_slice_produce::<SameLifetime, _, _, _>(Some(allocator), slice, action)
}

pub fn merge_slice_produce<'a, 'b, P, T, U, F>(
    allocator: Option<&'a Allocator<'a>>,
    slice: &'b [U],
    action: F,
) -> Option<&'a [T]>
where
    U: ArenaAllocatable<'b> + 'b,
    T: ArenaAllocatable<'a> + Produce<'a, 'b, P, U>,
    P: ExprProducer<'a, 'b>,
    F: FnMut(&'b U) -> Option<T>,
{
    merge_iter(slice, action, |e| {
        T::produce_with(e, &mut P::new(allocator.expect("Allocator")))
    })
    .map(|iter| ArenaAllocatable::alloc_iter_into(iter, allocator.expect("Allocator")))
}

pub fn merge_match<'a>(
    allocator: &'a Allocator<'a>,
    expr: CExpr<'a>,
    new_expr: Option<CExpr<'a>>,
    alts: &'a [Alternative<'a>],
    new_alts: Option<&'a [Alternative<'a>]>,
) -> Option<CExpr<'a>> {
    merge(&expr, new_expr, &alts, new_alts, |expr, alts| {
        &*allocator.arena.alloc(Expr::Match(expr, alts))
    })
}

#[cfg(all(test, feature = "test"))]
pub(crate) mod tests {
    use super::*;

    use base::symbol::Symbols;

    use crate::core::{self, grammar::ExprParser, tests::PatternEq};

    pub(crate) fn check_optimization(
        initial_str: &str,
        expected_str: &str,
        optimize: impl for<'a> FnOnce(&'a Allocator<'a>, CExpr<'a>) -> CExpr<'a>,
    ) {
        let mut symbols = Symbols::new();
        let allocator = core::Allocator::new();

        let initial_expr = allocator.arena.alloc(
            ExprParser::new()
                .parse(&mut symbols, &allocator, initial_str)
                .unwrap(),
        );

        let optimized_expr = optimize(&allocator, initial_expr);

        let expected_expr = ExprParser::new()
            .parse(&mut symbols, &allocator, expected_str)
            .unwrap();
        assert_deq!(PatternEq(optimized_expr), expected_expr);
    }

    #[test]
    fn unnecessary_allocation() {
        let initial_str = r#"
            match { l, r } with
            | { l, r } -> l
            end
            "#;
        let expected_str = r#"
            let l = l
            in
            let r = r
            in
            l
            "#;
        check_optimization(initial_str, expected_str, optimize_unnecessary_allocation);
    }
}
