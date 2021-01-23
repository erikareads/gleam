// Gleam Parser
//
// Terminology:
//   Expression Unit:
//     Essentially a thing that goes between operators.
//     Int, Bool, function call, "{" expression-sequence "}", case x {}, ..etc
//
//   Expression:
//     One or more Expression Units separated by an operator
//
//   Binding:
//     (let|assert|try) name (:TypeAnnotation)? = Expression
//
//   Expression Sequence:
//     * One or more Expressions
//     * A Binding followed by at least one more Expression Sequences
//
// Naming Conventions:
//   parse_x
//      Parse a specific part of the grammar, not erroring if it cannot.
//      Generally returns `Result<Option<A>, ParseError>`, note the inner Option
//
//   expect_x
//      Parse a generic or specific part of the grammar, erroring if it cannot.
//      Generally returns `Result<A, ParseError>`, note no inner Option
//
//   maybe_x
//      Parse a generic part of the grammar. Returning `None` if it cannot.
//      Returns `Some(x)` and advances the token stream if it can.
//
// Operator Precedence Parsing:
//   Needs to take place in expressions and in clause guards.
//   It is accomplished using the Simple Precedence Parser algoithm.
//   See: https://en.wikipedia.org/wiki/Simple_precedence_parser
//
//   It relies or the operator grammar being in the general form:
//   e ::= expr op expr | expr
//   Which just means that exprs and operators always alternate, starting with an expr
//
//   The gist of the algorithm is:
//   Create 2 stacks, one to hold expressions, and one to hold un-reduced operators.
//   While consuming the input stream, if an expression is encountered add it to the top
//   of the expression stack. If an operator is encountered, compare its precedence to the
//   top of the operator stack and perform the appropriate action, which is either using an
//   operator to reduce 2 expressions on the top of the expression stack or put it on the top
//   of the operator stack. When the end of the input is reached, attempt to reduce all of the
//   expressions down to a single expression(or no expression) using the remaining operators
//   on the operator stack. If there are any operators left, or more than 1 expression left
//   this is a syntax error. But the implementation here shouldn't need to handle that case
//   as the outer parser ensures the correct structure.
//
pub mod error;
pub mod extra;
pub mod lexer;
mod token;
use crate::ast::{
    Arg, ArgNames, BinOp, BindingKind, BitStringSegment, BitStringSegmentOption, CallArg, Clause,
    ClauseGuard, Constant, ExternalFnArg, HasLocation, Module, Pattern, RecordConstructor,
    RecordConstructorArg, RecordUpdateSpread, SrcSpan, Statement, TypeAst, UnqualifiedImport,
    UntypedArg, UntypedClause, UntypedClauseGuard, UntypedConstant, UntypedExpr,
    UntypedExternalFnArg, UntypedModule, UntypedPattern, UntypedRecordUpdateArg, UntypedStatement,
    CAPTURE_VARIABLE,
};
use crate::error::fatal_compiler_bug;
use crate::parse::extra::ModuleExtra;
use error::{LexicalError, ParseError, ParseErrorType};
use lexer::{LexResult, Spanned};
use std::cmp::Ordering;
use std::str::FromStr;
use token::Tok;
#[cfg(test)]
mod tests;

//
// Public Interface
//
pub fn parse_module(src: &str) -> Result<(UntypedModule, ModuleExtra), ParseError> {
    let lex = lexer::make_tokenizer(src);
    let mut parser = Parser::new(lex);
    let module = parser.parse_module()?;
    Ok((module, parser.extra))
}

//
// Test Interface
//
#[cfg(test)]
pub fn parse_expression_sequence(src: &str) -> Result<UntypedExpr, ParseError> {
    let lex = lexer::make_tokenizer(src);
    let mut parser = Parser::new(lex);
    let expr = parser.parse_expression_seq();
    let expr = parser.ensure_no_errors(expr)?;
    if let Some((e, _)) = expr {
        Ok(e)
    } else {
        parse_error(ParseErrorType::ExpectedExpr, SrcSpan { start: 0, end: 0 })
    }
}

//
// Parser
//
pub struct Parser<T: Iterator<Item = LexResult>> {
    tokens: T,
    lex_errors: Vec<LexicalError>,
    tok0: Option<Spanned>,
    tok1: Option<Spanned>,
    extra: ModuleExtra,
}
impl<T> Parser<T>
where
    T: Iterator<Item = LexResult>,
{
    pub fn new(input: T) -> Self {
        let mut parser = Parser {
            tokens: input,
            lex_errors: vec![],
            tok0: None,
            tok1: None,
            extra: ModuleExtra::new(),
        };
        let _ = parser.next_tok();
        let _ = parser.next_tok();
        parser
    }

    fn parse_module(&mut self) -> Result<UntypedModule, ParseError> {
        let statements = Parser::series_of(self, &Parser::parse_statement, None);
        let statements = self.ensure_no_errors(statements)?;
        Ok(Module {
            name: vec![],
            documentation: vec![],
            type_info: (),
            statements,
        })
    }

    // The way the parser is currenly implemented, it cannot exit immediately while advancing
    // the token stream upon seing a LexError. That is to avoid having to put `?` all over the
    // place and instead we collect LexErrors in `self.lex_errors` and attempt to continue parsing.
    // Once parsing has returned we want to surface an error in the order:
    // 1) LexError, 2) ParseError, 3) More Tokens Left
    fn ensure_no_errors<A>(
        &mut self,
        parse_result: Result<A, ParseError>,
    ) -> Result<A, ParseError> {
        self.lex_errors.reverse();
        if let Some(error) = self.lex_errors.pop() {
            // Lex errors first
            let location = error.location;
            parse_error(
                ParseErrorType::LexError { error },
                SrcSpan {
                    start: location,
                    end: location,
                },
            )
        } else if parse_result.is_err() {
            // Then parse errors
            parse_result
        } else if let Some((start, _, end)) = self.next_tok() {
            // there are still more tokens
            let expected = vec!["An import, const, type, or function.".to_string()];
            parse_error(
                ParseErrorType::UnexpectedToken { expected },
                SrcSpan { start, end },
            )
        } else {
            // no errors
            parse_result
        }
    }

    fn parse_statement(&mut self) -> Result<Option<UntypedStatement>, ParseError> {
        let statement = match (self.tok0.take(), self.tok1.as_ref()) {
            // Imports
            (Some((_, Tok::Import, _)), _) => {
                let _ = self.next_tok();
                self.parse_import()
            }
            // Module Constants
            (Some((_, Tok::Const, _)), _) => {
                let _ = self.next_tok();
                self.parse_module_const(false)
            }
            (Some((_, Tok::Pub, _)), Some((_, Tok::Const, _))) => {
                let _ = self.next_tok();
                let _ = self.next_tok();
                self.parse_module_const(true)
            }

            // External Type
            (Some((start, Tok::External, _)), Some((_, Tok::Type, _))) => {
                let _ = self.next_tok();
                let _ = self.next_tok();
                self.parse_external_type(start, false)
            }
            // External Function
            (Some((start, Tok::External, _)), Some((_, Tok::Fn, _))) => {
                let _ = self.next_tok();
                let _ = self.next_tok();
                self.parse_external_fn(start, false)
            }
            // Pub External type or fn
            (Some((start, Tok::Pub, _)), Some((_, Tok::External, _))) => {
                let _ = self.next_tok();
                let _ = self.next_tok();
                match self.next_tok() {
                    Some((_, Tok::Type, _)) => self.parse_external_type(start, true),
                    Some((_, Tok::Fn, _)) => self.parse_external_fn(start, true),
                    _ => {
                        self.next_tok_unexpected(vec!["A type or function definition".to_string()])?
                    }
                }
            }

            // function
            (Some((start, Tok::Fn, _)), _) => {
                let _ = self.next_tok();
                self.parse_function(start, false, false)
            }
            (Some((start, Tok::Pub, _)), Some((_, Tok::Fn, _))) => {
                let _ = self.next_tok();
                let _ = self.next_tok();
                self.parse_function(start, true, false)
            }

            // Custom Types, and Type Aliases
            (Some((start, Tok::Type, _)), _) => {
                let _ = self.next_tok();
                self.parse_custom_type(start, false, false)
            }
            (Some((start, Tok::Pub, _)), Some((_, Tok::Opaque, _))) => {
                let _ = self.next_tok();
                let _ = self.next_tok();
                let _ = self.expect_one(&Tok::Type)?;
                self.parse_custom_type(start, true, true)
            }
            (Some((start, Tok::Pub, _)), Some((_, Tok::Type, _))) => {
                let _ = self.next_tok();
                let _ = self.next_tok();
                self.parse_custom_type(start, true, false)
            }

            (t0, _) => {
                self.tok0 = t0;
                Ok(None)
            }
        };
        tracing::trace!("Statement Parsed: {:?}", statement);
        statement
    }

    //
    // Parse Expressions
    //

    // examples:
    //   unit
    //   unit op unit
    //   unit op unit pipe unit(call)
    //   unit op unit pipe unit(call) pipe unit(call)
    fn parse_expression(&mut self) -> Result<Option<UntypedExpr>, ParseError> {
        // uses the simple operator parser algorithm
        let mut opstack = vec![];
        let mut estack = vec![];
        let mut last_op_start = 0;
        let mut last_op_end = 0;
        loop {
            if let Some(unit) = self.parse_expression_unit()? {
                estack.push(unit)
            } else if estack.is_empty() {
                return Ok(None);
            } else {
                return parse_error(
                    ParseErrorType::OpNakedRight,
                    SrcSpan {
                        start: last_op_start,
                        end: last_op_end,
                    },
                );
            }

            if let Some((op_s, t, op_e)) = self.tok0.take() {
                if let Some(p) = precedence(&t) {
                    // Is Op
                    let _ = self.next_tok();
                    last_op_start = op_s;
                    last_op_end = op_e;
                    let _ = handle_op(
                        Some(((op_s, t, op_e), p)),
                        &mut opstack,
                        &mut estack,
                        &do_reduce_expression,
                    )?;
                } else {
                    // Is not Op
                    self.tok0 = Some((op_s, t, op_e));
                    break;
                }
            } else {
                break;
            }
        }

        handle_op(None, &mut opstack, &mut estack, &do_reduce_expression)
    }

    // examples:
    //   1
    //   "one"
    //   True
    //   fn() { "hi" }
    //   unit().unit().unit()
    //   A(a.., label: tuple(1))
    //   { expression_sequence }
    fn parse_expression_unit(&mut self) -> Result<Option<UntypedExpr>, ParseError> {
        let mut expr = match self.tok0.take() {
            Some((start, Tok::String { value }, end)) => {
                let _ = self.next_tok();
                UntypedExpr::String {
                    location: SrcSpan { start, end },
                    value,
                }
            }
            Some((start, Tok::Int { value }, end)) => {
                let _ = self.next_tok();
                UntypedExpr::Int {
                    location: SrcSpan { start, end },
                    value,
                }
            }

            Some((start, Tok::Float { value }, end)) => {
                let _ = self.next_tok();
                UntypedExpr::Float {
                    location: SrcSpan { start, end },
                    value,
                }
            }
            Some((start, Tok::ListNil, end)) => {
                let _ = self.next_tok();

                UntypedExpr::ListNil {
                    location: SrcSpan { start, end },
                }
            }
            // var lower_name and UpName
            Some((start, Tok::Name { name }, end)) | Some((start, Tok::UpName { name }, end)) => {
                let _ = self.next_tok();
                UntypedExpr::Var {
                    location: SrcSpan { start, end },
                    name,
                }
            }
            Some((start, Tok::Todo, mut end)) => {
                let _ = self.next_tok();
                let mut label = None;
                if self.maybe_one(&Tok::Lpar).is_some() {
                    let (_, l, _) = self.expect_string()?;
                    label = Some(l);
                    let (_, e) = self.expect_one(&Tok::Rpar)?;
                    end = e;
                }
                UntypedExpr::Todo {
                    location: SrcSpan { start, end },
                    label,
                }
            }
            Some((start, Tok::Tuple, _)) => {
                let _ = self.next_tok();
                let _ = self.expect_one(&Tok::Lpar)?;
                let elems = Parser::series_of(self, &Parser::parse_expression, Some(&Tok::Comma))?;
                let (_, end) = self.expect_one(&Tok::Rpar)?;
                UntypedExpr::Tuple {
                    location: SrcSpan { start, end },
                    elems,
                }
            }
            // list
            Some((start, Tok::Lsqb, _)) => {
                let _ = self.next_tok();
                let elems = Parser::series_of(self, &Parser::parse_expression, Some(&Tok::Comma))?;
                let mut tail0 = None;
                if self.maybe_one(&Tok::DotDot).is_some() {
                    tail0 = self.parse_expression()?;
                    let _ = self.maybe_one(&Tok::Comma);
                }
                let (_, end) = self.expect_one(&Tok::Rsqb)?;
                let tail = tail0.unwrap_or(UntypedExpr::ListNil {
                    location: SrcSpan { start, end },
                });

                elems
                    .into_iter()
                    .rev()
                    .fold(tail, |t, h| UntypedExpr::ListCons {
                        location: t.location(),
                        head: Box::new(h),
                        tail: Box::new(t),
                    })
            }
            // Bitstring
            Some((start, Tok::LtLt, _)) => {
                let _ = self.next_tok();
                let segments = Parser::series_of(
                    self,
                    &|s| {
                        Parser::parse_bit_string_segment(
                            s,
                            &Parser::parse_expression_unit,
                            &Parser::expect_expression,
                            &bit_string_expr_int,
                        )
                    },
                    Some(&Tok::Comma),
                )?;
                let (_, end) = self.expect_one(&Tok::GtGt)?;
                UntypedExpr::BitString {
                    location: SrcSpan { start, end },
                    segments,
                }
            }
            Some((start, Tok::Fn, _)) => {
                let _ = self.next_tok();
                match self.parse_function(start, false, true)? {
                    Some(Statement::Fn {
                        location,
                        args,
                        body,
                        return_annotation,
                        ..
                    }) => UntypedExpr::Fn {
                        location,
                        is_capture: false,
                        args,
                        body: Box::new(body),
                        return_annotation,
                    },

                    _ => {
                        // this isn't just none, it could also be Some(UntypedExpr::..)
                        return self
                            .next_tok_unexpected(vec!["An opening parenthesis.".to_string()]);
                    }
                }
            }

            // expression group  "{" "}"
            Some((start, Tok::Lbrace, _)) => {
                let _ = self.next_tok();
                let expr = self.parse_expression_seq()?;
                let (_, end) = self.expect_one(&Tok::Rbrace)?;
                if let Some((expr, _)) = expr {
                    expr
                } else {
                    return parse_error(ParseErrorType::NoExpression, SrcSpan { start, end });
                }
            }

            // case
            Some((start, Tok::Case, case_e)) => {
                let _ = self.next_tok();
                let subjects =
                    Parser::series_of(self, &Parser::parse_expression, Some(&Tok::Comma))?;
                let _ = self.expect_one(&Tok::Lbrace)?;
                let clauses = Parser::series_of(self, &Parser::parse_case_clause, None)?;
                let (_, end) = self.expect_one(&Tok::Rbrace)?;
                if subjects.is_empty() {
                    return parse_error(
                        ParseErrorType::ExpectedExpr,
                        SrcSpan { start, end: case_e },
                    );
                } else if clauses.is_empty() {
                    return parse_error(ParseErrorType::NoCaseClause, SrcSpan { start, end });
                } else {
                    UntypedExpr::Case {
                        location: SrcSpan { start, end },
                        subjects,
                        clauses,
                    }
                }
            }

            // helpful error on possibly trying to group with "("
            Some((start, Tok::Lpar, _)) => {
                return parse_error(ParseErrorType::ExprLparStart, SrcSpan { start, end: start });
            }
            t0 => {
                self.tok0 = t0;
                return Ok(None);
            }
        };

        // field access and call can stack up
        loop {
            if let Some((_, _)) = self.maybe_one(&Tok::Dot) {
                let start = expr.location().start;
                // field access
                match self.tok0.take() {
                    // tuple access
                    Some((_, Tok::Int { value }, end)) => {
                        let _ = self.next_tok();
                        let v = value.replace("_", "");
                        if let Ok(index) = u64::from_str(&v) {
                            expr = UntypedExpr::TupleIndex {
                                location: SrcSpan { start, end },
                                index,
                                tuple: Box::new(expr),
                            }
                        } else {
                            return parse_error(
                                ParseErrorType::InvalidTupleAccess,
                                SrcSpan { start, end },
                            );
                        }
                    }

                    Some((_, Tok::Name { name: label }, end)) => {
                        let _ = self.next_tok();
                        expr = UntypedExpr::FieldAccess {
                            location: SrcSpan { start, end },
                            label,
                            container: Box::new(expr),
                        }
                    }

                    Some((_, Tok::UpName { name: label }, end)) => {
                        let _ = self.next_tok();
                        expr = UntypedExpr::FieldAccess {
                            location: SrcSpan { start, end },
                            label,
                            container: Box::new(expr),
                        }
                    }

                    t0 => {
                        self.tok0 = t0;
                        return self.next_tok_unexpected(vec![
                            "A positive integer or a field name.".to_string(),
                        ]);
                    }
                }
            } else if self.maybe_one(&Tok::Lpar).is_some() {
                let start = expr.location().start.clone();
                if let Some((dot_s, _)) = self.maybe_one(&Tok::DotDot) {
                    // Record update
                    let (_, name, name_e) = self.expect_name()?;
                    let spread = RecordUpdateSpread {
                        name,
                        location: SrcSpan {
                            start: dot_s,
                            end: name_e,
                        },
                    };
                    let mut args = vec![];
                    if self.maybe_one(&Tok::Comma).is_some() {
                        args = Parser::series_of(
                            self,
                            &Parser::parse_record_update_arg,
                            Some(&Tok::Comma),
                        )?;
                    }
                    let (_, end) = self.expect_one(&Tok::Rpar)?;

                    expr = UntypedExpr::RecordUpdate {
                        location: SrcSpan { start, end },
                        constructor: Box::new(expr),
                        spread,
                        args,
                    };
                } else {
                    // Call
                    let args = self.parse_fn_args()?;
                    let (_, end) = self.expect_one(&Tok::Rpar)?;
                    match make_call(expr, args, start, end) {
                        Ok(e) => expr = e,
                        Err(_) => {
                            return parse_error(
                                ParseErrorType::TooManyArgHoles,
                                SrcSpan { start, end },
                            )
                        }
                    }
                }
            } else {
                // done
                break;
            }
        }

        Ok(Some(expr))
    }

    // examples:
    //   let pattern = expr
    //   assert pattern: Type = expr
    //   try pattern = expr
    //   expr expr
    //   expr
    //
    //   In order to parse an expr sequence, you must try to parse an expr, if it is a binding
    //   you MUST parse another expr, if it is some other expr, you MAY parse another expr
    fn parse_expression_seq(&mut self) -> Result<Option<(UntypedExpr, usize)>, ParseError> {
        // binding
        if let Some((start, kind)) = self.maybe_binding_start() {
            let pattern = if let Some(p) = self.parse_pattern()? {
                p
            } else {
                return self.next_tok_unexpected(vec![
                    "A pattern".to_string(),
                    "See: https://gleam.run/book/tour/patterns".to_string(),
                ])?;
            };
            let annotation = self.parse_type_annotation(&Tok::Colon, false)?;
            let (eq_s, eq_e) = self.expect_one(&Tok::Equal)?;
            let value = self.parse_expression()?;
            let then = self.parse_expression_seq()?;
            match (value, then) {
                (Some(value), Some((then, seq_end))) => Ok(Some((
                    UntypedExpr::Let {
                        location: SrcSpan {
                            start,
                            end: value.location().end,
                        },
                        value: Box::new(value),
                        pattern,
                        annotation,
                        kind,
                        then: Box::new(then),
                    },
                    seq_end,
                ))),

                (None, _) => parse_error(
                    ParseErrorType::ExpectedValue,
                    SrcSpan {
                        start: eq_s,
                        end: eq_e,
                    },
                ),
                (Some(val), None) => parse_error(
                    ParseErrorType::ExprTailBinding,
                    SrcSpan {
                        start,
                        end: val.location().end,
                    },
                ),
            }
        } else if let Some(mut expr) = self.parse_expression()? {
            while let Some((e, _)) = self.parse_expression_seq()? {
                expr = UntypedExpr::Seq {
                    first: Box::new(expr),
                    then: Box::new(e),
                }
            }
            let end = expr.location().end;
            Ok(Some((expr, end)))
        } else {
            Ok(None)
        }
    }

    // let, assert, or try
    fn maybe_binding_start(&mut self) -> Option<(usize, BindingKind)> {
        match self.tok0 {
            Some((start, Tok::Let, _)) => {
                let _ = self.next_tok();
                Some((start, BindingKind::Let))
            }
            Some((start, Tok::Assert, _)) => {
                let _ = self.next_tok();
                Some((start, BindingKind::Assert))
            }
            Some((start, Tok::Try, _)) => {
                let _ = self.next_tok();
                Some((start, BindingKind::Try))
            }
            _ => None,
        }
    }

    // The left side of an "=" or a "->"
    fn parse_pattern(&mut self) -> Result<Option<UntypedPattern>, ParseError> {
        let pattern = match self.tok0.take() {
            // Pattern::Var or Pattern::Constructor start
            Some((start, Tok::Name { name }, end)) => {
                let _ = self.next_tok();
                if self.maybe_one(&Tok::Dot).is_some() {
                    self.expect_constructor_pattern(Some((start, name, end)))?
                } else {
                    Pattern::Var {
                        location: SrcSpan { start, end },
                        name,
                    }
                }
            }
            // Constructor
            Some((start, tok @ Tok::UpName { .. }, end)) => {
                self.tok0 = Some((start, tok, end));
                self.expect_constructor_pattern(None)?
            }
            Some((start, Tok::DiscardName { name }, end)) => {
                let _ = self.next_tok();
                Pattern::Discard {
                    location: SrcSpan { start, end },
                    name,
                }
            }
            Some((start, Tok::String { value }, end)) => {
                let _ = self.next_tok();
                Pattern::String {
                    location: SrcSpan { start, end },
                    value,
                }
            }
            Some((start, Tok::Int { value }, end)) => {
                let _ = self.next_tok();
                Pattern::Int {
                    location: SrcSpan { start, end },
                    value,
                }
            }
            Some((start, Tok::Float { value }, end)) => {
                let _ = self.next_tok();
                Pattern::Float {
                    location: SrcSpan { start, end },
                    value,
                }
            }
            Some((start, Tok::ListNil, end)) => {
                let _ = self.next_tok();
                Pattern::Nil {
                    location: SrcSpan { start, end },
                }
            }
            Some((start, Tok::Tuple, _)) => {
                let _ = self.next_tok();
                let _ = self.expect_one(&Tok::Lpar)?;
                let elems = Parser::series_of(self, &Parser::parse_pattern, Some(&Tok::Comma))?;
                let (_, end) = self.expect_one(&Tok::Rpar)?;
                Pattern::Tuple {
                    location: SrcSpan { start, end },
                    elems,
                }
            }
            // Bitstring
            Some((start, Tok::LtLt, _)) => {
                let _ = self.next_tok();
                let segments = Parser::series_of(
                    self,
                    &|s| {
                        Parser::parse_bit_string_segment(
                            s,
                            &Parser::parse_pattern,
                            &Parser::expect_bit_string_pattern_segment_arg,
                            &bit_string_pattern_int,
                        )
                    },
                    Some(&Tok::Comma),
                )?;
                let (_, end) = self.expect_one(&Tok::GtGt)?;
                Pattern::BitString {
                    location: SrcSpan { start, end },
                    segments,
                }
            }
            // List
            Some((start, Tok::Lsqb, _)) => {
                let _ = self.next_tok();
                let elems = Parser::series_of(self, &Parser::parse_pattern, Some(&Tok::Comma))?;
                let tail = if let Some((_, Tok::DotDot, _)) = self.tok0 {
                    let _ = self.next_tok();
                    let pat = self.parse_pattern()?;
                    let _ = self.maybe_one(&Tok::Comma);
                    Some(pat)
                } else {
                    None
                };
                let (_, rsqb_e) = self.expect_one(&Tok::Rsqb)?;
                let tail_pattern = match tail {
                    // There is a tail and it has a Pattern::Var or Pattern::Discard
                    Some(Some(pat @ Pattern::Var { .. }))
                    | Some(Some(pat @ Pattern::Discard { .. })) => pat,
                    // There is a tail and but it has no content, implicit discard
                    Some(Some(pat)) => {
                        return parse_error(ParseErrorType::InvalidTailPattern, pat.location())
                    }
                    Some(None) => Pattern::Discard {
                        location: SrcSpan {
                            start: rsqb_e - 1,
                            end: rsqb_e,
                        },
                        name: "_".to_string(),
                    },
                    // No tail specified
                    None => Pattern::Nil {
                        location: SrcSpan {
                            start: rsqb_e - 1,
                            end: rsqb_e,
                        },
                    },
                };

                elems
                    .into_iter()
                    .rev()
                    .fold(tail_pattern, |a, e| Pattern::Cons {
                        location: e.location(),
                        head: Box::new(e),
                        tail: Box::new(a),
                    })
                    .put_list_cons_location_start(start)
            }

            // No pattern
            t0 => {
                self.tok0 = t0;
                return Ok(None);
            }
        };

        if let Some((_, Tok::As, _)) = self.tok0 {
            let _ = self.next_tok();
            let (_, name, _) = self.expect_name()?;
            Ok(Some(Pattern::Let {
                name,
                pattern: Box::new(pattern),
            }))
        } else {
            Ok(Some(pattern))
        }
    }

    // examples:
    //   pattern -> expr
    //   pattern, pattern if -> expr
    //   pattern, pattern | pattern, pattern if -> expr
    fn parse_case_clause(&mut self) -> Result<Option<UntypedClause>, ParseError> {
        let patterns = self.parse_patterns()?;
        if let Some(lead) = &patterns.first() {
            let mut alternative_patterns = vec![];
            loop {
                if None == self.maybe_one(&Tok::Vbar) {
                    break;
                }
                alternative_patterns.push(self.parse_patterns()?);
            }
            let guard = self.parse_case_clause_guard(false)?;
            let (arr_s, arr_e) = self.expect_one(&Tok::RArrow)?;
            let then = self.parse_expression()?;
            if let Some(then) = then {
                Ok(Some(Clause {
                    location: SrcSpan {
                        start: lead.location().start,
                        end: then.location().end,
                    },
                    pattern: patterns,
                    alternative_patterns,
                    guard,
                    then,
                }))
            } else {
                parse_error(
                    ParseErrorType::ExpectedExpr,
                    SrcSpan {
                        start: arr_s,
                        end: arr_e,
                    },
                )
            }
        } else {
            Ok(None)
        }
    }
    fn parse_patterns(&mut self) -> Result<Vec<UntypedPattern>, ParseError> {
        Parser::series_of(self, &Parser::parse_pattern, Some(&Tok::Comma))
    }

    // examples:
    //   if a
    //   if a < b
    //   if a < b || b < c
    fn parse_case_clause_guard(
        &mut self,
        nested: bool,
    ) -> Result<Option<UntypedClauseGuard>, ParseError> {
        if self.maybe_one(&Tok::If).is_some() || nested {
            let mut opstack = vec![];
            let mut estack = vec![];
            let mut last_op_start = 0;
            let mut last_op_end = 0;
            loop {
                if let Some(unit) = self.parse_case_clause_guard_unit()? {
                    estack.push(unit)
                } else if estack.is_empty() {
                    return Ok(None);
                } else {
                    return parse_error(
                        ParseErrorType::OpNakedRight,
                        SrcSpan {
                            start: last_op_start,
                            end: last_op_end,
                        },
                    );
                }

                if let Some((op_s, t, op_e)) = self.tok0.take() {
                    if let Some(p) = &t.guard_precedence() {
                        // Is Op
                        let _ = self.next_tok();
                        last_op_start = op_s;
                        last_op_end = op_e;
                        let _ = handle_op(
                            Some(((op_s, t, op_e), *p)),
                            &mut opstack,
                            &mut estack,
                            &do_reduce_clause_guard,
                        )?;
                    } else {
                        // Is not Op
                        self.tok0 = Some((op_s, t, op_e));
                        break;
                    }
                }
            }

            handle_op(None, &mut opstack, &mut estack, &do_reduce_clause_guard)
        } else {
            Ok(None)
        }
    }

    // examples
    // a
    // 1
    // a.1
    // { a }
    // a || b
    // a < b || b < c
    fn parse_case_clause_guard_unit(&mut self) -> Result<Option<UntypedClauseGuard>, ParseError> {
        match self.tok0.take() {
            Some((start, Tok::Name { name }, end)) => {
                let _ = self.next_tok();
                if let Some((dot_s, _)) = self.maybe_one(&Tok::Dot) {
                    match self.next_tok() {
                        Some((_, Tok::Int { value }, int_e)) => {
                            let v = value.replace("_", "");
                            if let Ok(index) = u64::from_str(&v) {
                                Ok(Some(ClauseGuard::TupleIndex {
                                    location: SrcSpan {
                                        start: dot_s,
                                        end: int_e,
                                    },
                                    index,
                                    typ: (),
                                    tuple: Box::new(ClauseGuard::Var {
                                        location: SrcSpan { start, end },
                                        typ: (),
                                        name,
                                    }),
                                }))
                            } else {
                                parse_error(
                                    ParseErrorType::InvalidTupleAccess,
                                    SrcSpan { start, end },
                                )
                            }
                        }

                        Some((start, _, end)) => {
                            parse_error(ParseErrorType::InvalidTupleAccess, SrcSpan { start, end })
                        }
                        _ => self.next_tok_unexpected(vec!["A positive integer".to_string()]),
                    }
                } else {
                    Ok(Some(ClauseGuard::Var {
                        location: SrcSpan { start, end },
                        typ: (),
                        name,
                    }))
                }
            }
            Some((_, Tok::Lbrace, _)) => {
                // Nested guard expression
                let _ = self.next_tok();
                let guard = self.parse_case_clause_guard(true);
                let _ = self.expect_one(&Tok::Rbrace)?;
                guard
            }
            t0 => {
                self.tok0 = t0;
                if let Some(const_val) = self.parse_const_value()? {
                    // Constant
                    Ok(Some(ClauseGuard::Constant(const_val)))
                } else {
                    Ok(None)
                }
            }
        }
    }

    // examples:
    //   UpName( args )
    fn expect_constructor_pattern(
        &mut self,
        module: Option<(usize, String, usize)>,
    ) -> Result<UntypedPattern, ParseError> {
        let (mut start, name, end) = self.expect_upname()?;
        let (args, with_spread, end) = self.parse_constructor_pattern_args(end)?;
        if let Some((s, _, _)) = module {
            start = s;
        }
        Ok(Pattern::Constructor {
            location: SrcSpan { start, end },
            args,
            module: module.map(|(_, n, _)| n),
            name,
            with_spread,
            constructor: (),
        })
    }

    // examples:
    //   ( args )
    fn parse_constructor_pattern_args(
        &mut self,
        upname_end: usize,
    ) -> Result<(Vec<CallArg<UntypedPattern>>, bool, usize), ParseError> {
        if self.maybe_one(&Tok::Lpar).is_some() {
            let args = Parser::series_of(
                self,
                &Parser::parse_constructor_pattern_arg,
                Some(&Tok::Comma),
            )?;
            let with_spread = self.maybe_one(&Tok::DotDot).is_some();
            if with_spread {
                let _ = self.maybe_one(&Tok::Comma);
            }
            let (_, end) = self.expect_one(&Tok::Rpar)?;
            Ok((args, with_spread, end))
        } else {
            Ok((vec![], false, upname_end))
        }
    }

    // examples:
    //   a: <pattern>
    //   <pattern>
    fn parse_constructor_pattern_arg(
        &mut self,
    ) -> Result<Option<CallArg<UntypedPattern>>, ParseError> {
        match (self.tok0.take(), self.tok1.take()) {
            // named arg
            (Some((start, Tok::Name { name }, _)), Some((col_s, Tok::Colon, col_e))) => {
                let _ = self.next_tok();
                let _ = self.next_tok();
                if let Some(value) = self.parse_pattern()? {
                    Ok(Some(CallArg {
                        location: SrcSpan {
                            start,
                            end: value.location().end,
                        },
                        label: Some(name),
                        value,
                    }))
                } else {
                    parse_error(
                        ParseErrorType::ExpectedPattern,
                        SrcSpan {
                            start: col_s,
                            end: col_e,
                        },
                    )
                }
            }
            // unnamed arg
            (t0, t1) => {
                self.tok0 = t0;
                self.tok1 = t1;
                if let Some(value) = self.parse_pattern()? {
                    Ok(Some(CallArg {
                        location: value.location(),
                        label: None,
                        value,
                    }))
                } else {
                    Ok(None)
                }
            }
        }
    }

    // examples:
    //   a: expr
    fn parse_record_update_arg(&mut self) -> Result<Option<UntypedRecordUpdateArg>, ParseError> {
        if let Some((start, label, _)) = self.maybe_name() {
            let _ = self.expect_one(&Tok::Colon)?;
            let value = self.parse_expression()?;
            if let Some(value) = value {
                Ok(Some(UntypedRecordUpdateArg {
                    label,
                    location: SrcSpan {
                        start,
                        end: value.location().end,
                    },
                    value,
                }))
            } else {
                self.next_tok_unexpected(vec!["An expression".to_string()])
            }
        } else {
            Ok(None)
        }
    }

    //
    // Parse Functions
    //

    // Starts after "fn"
    //
    // examples:
    //   fn a(name: String) -> String { .. }
    //   pub fn a(name name: String) -> String { .. }
    fn parse_function(
        &mut self,
        start: usize,
        public: bool,
        is_anon: bool,
    ) -> Result<Option<UntypedStatement>, ParseError> {
        let mut name = String::new();
        if !is_anon {
            let (_, n, _) = self.expect_name()?;
            name = n;
        }
        let _ = self.expect_one(&Tok::Lpar)?;
        let args = Parser::series_of(self, &Parser::parse_fn_param, Some(&Tok::Comma))?;
        let (_, rpar_e) = self.expect_one(&Tok::Rpar)?;
        let return_annotation = self.parse_type_annotation(&Tok::RArrow, false)?;
        let _ = self.expect_one(&Tok::Lbrace)?;
        if let Some((body, _)) = self.parse_expression_seq()? {
            let (_, rbr_e) = self.expect_one(&Tok::Rbrace)?;
            let end = return_annotation
                .as_ref()
                .map(|l| l.location().end)
                .unwrap_or_else(|| if is_anon { rbr_e } else { rpar_e });
            Ok(Some(Statement::Fn {
                doc: None,
                location: SrcSpan { start, end },
                end_location: rbr_e - 1,
                public,
                name,
                args,
                body,
                return_type: (),
                return_annotation,
            }))
        } else {
            self.next_tok_unexpected(vec!["The body of a function".to_string()])
        }
    }

    // Starts after "fn"
    //
    // examples:
    //   external fn a(String) -> String = "x" "y"
    //   pub external fn a(name: String) -> String = "x" "y"
    fn parse_external_fn(
        &mut self,
        start: usize,
        public: bool,
    ) -> Result<Option<UntypedStatement>, ParseError> {
        let (_, name, _) = self.expect_name()?;
        let _ = self.expect_one(&Tok::Lpar)?;
        let args = Parser::series_of(self, &Parser::parse_external_fn_param, Some(&Tok::Comma))?;
        let _ = self.expect_one(&Tok::Rpar)?;
        let (arr_s, arr_e) = self.expect_one(&Tok::RArrow)?;
        let return_annotation = self.parse_type(false)?;
        let _ = self.expect_one(&Tok::Equal)?;
        let (_, module, _) = self.expect_string()?;
        let (_, fun, end) = self.expect_string()?;

        if let Some(retrn) = return_annotation {
            Ok(Some(Statement::ExternalFn {
                doc: None,
                location: SrcSpan { start, end },
                public,
                name,
                args,
                module,
                fun,
                retrn,
                return_type: (),
            }))
        } else {
            parse_error(
                ParseErrorType::ExpectedType,
                SrcSpan {
                    start: arr_s,
                    end: arr_e,
                },
            )
        }
    }

    // Parse a single external function definition param
    //
    // examples:
    //   A
    //   a: A
    fn parse_external_fn_param(&mut self) -> Result<Option<UntypedExternalFnArg>, ParseError> {
        let mut start = 0;
        let mut label = None;
        let mut end = 0;
        match (self.tok0.take(), self.tok1.take()) {
            (Some((s, Tok::Name { name }, _)), Some((_, Tok::Colon, e))) => {
                let _ = self.next_tok();
                let _ = self.next_tok();
                start = s;
                label = Some(name);
                end = e;
            }

            (t0, t1) => {
                self.tok0 = t0;
                self.tok1 = t1;
            }
        };
        match (&label, self.parse_type(false)?) {
            (None, None) => Ok(None),
            (Some(_), None) => {
                parse_error(ParseErrorType::ExpectedType, SrcSpan { start: end, end })
            }
            (None, Some(annotation)) => Ok(Some(ExternalFnArg {
                location: annotation.location(),
                label: None,
                annotation,
                typ: (),
            })),

            (Some(_), Some(annotation)) => {
                let end = annotation.location().end;
                Ok(Some(ExternalFnArg {
                    location: SrcSpan { start, end },
                    label,
                    annotation,
                    typ: (),
                }))
            }
        }
    }

    // Parse a single function definition param
    //
    // examples:
    //   _
    //   a
    //   a a
    //   a _
    //   a _:A
    //   a a:A
    fn parse_fn_param(&mut self) -> Result<Option<UntypedArg>, ParseError> {
        let (start, names, mut end) = match (self.tok0.take(), self.tok1.take()) {
            // labeled discard
            (
                Some((start, Tok::Name { name: label }, _)),
                Some((_, Tok::DiscardName { name }, end)),
            ) => {
                let _ = self.next_tok();
                let _ = self.next_tok();
                (start, ArgNames::LabelledDiscard { name, label }, end)
            }
            // discard
            (Some((start, Tok::DiscardName { name }, end)), t1) => {
                self.tok1 = t1;
                let _ = self.next_tok();
                (start, ArgNames::Discard { name }, end)
            }
            // labeled name
            (Some((start, Tok::Name { name: label }, _)), Some((_, Tok::Name { name }, end))) => {
                let _ = self.next_tok();
                let _ = self.next_tok();
                (start, ArgNames::NamedLabelled { name, label }, end)
            }
            // name
            (Some((start, Tok::Name { name }, end)), t1) => {
                self.tok1 = t1;
                let _ = self.next_tok();
                (start, ArgNames::Named { name }, end)
            }
            (t0, t1) => {
                self.tok0 = t0;
                self.tok1 = t1;
                return Ok(None);
            }
        };
        let annotation = if let Some(a) = self.parse_type_annotation(&Tok::Colon, false)? {
            end = a.location().end;
            Some(a)
        } else {
            None
        };
        Ok(Some(Arg {
            location: SrcSpan { start, end },
            typ: (),
            names,
            annotation,
        }))
    }

    // Parse function call arguments, no parens
    //
    // examples:
    //   _
    //   expr, expr
    //   a: _, expr
    //   a: expr, _, b: _
    fn parse_fn_args(&mut self) -> Result<Vec<ParserArg>, ParseError> {
        let args = Parser::series_of(self, &Parser::parse_fn_arg, Some(&Tok::Comma))?;
        Ok(args)
    }

    // Parse a single function call arg
    //
    // examples:
    //   _
    //   expr
    //   a: _
    //   a: expr
    fn parse_fn_arg(&mut self) -> Result<Option<ParserArg>, ParseError> {
        let mut start = 0;
        let label = match (self.tok0.take(), self.tok1.as_ref()) {
            (Some((s, Tok::Name { name }, _)), Some((_, Tok::Colon, _))) => {
                let _ = self.next_tok();
                let _ = self.next_tok();
                start = s;
                Some(name)
            }
            (t0, _) => {
                self.tok0 = t0;
                None
            }
        };

        if let Some(value) = self.parse_expression()? {
            let mut location = value.location();
            if label.is_some() {
                location.start = start
            };
            Ok(Some(ParserArg::Arg(CallArg {
                label,
                location,
                value,
            })))
        } else if let Some((start, _, end)) = self.maybe_discard_name() {
            let mut location = SrcSpan { start, end };
            if label.is_some() {
                location.start = start
            };
            Ok(Some(ParserArg::Hole { location, label }))
        } else {
            Ok(None)
        }
    }

    // Starts after "type"
    //
    // examples:
    //   external type A
    //   pub external type A(a, b, c)
    fn parse_external_type(
        &mut self,
        start: usize,
        public: bool,
    ) -> Result<Option<UntypedStatement>, ParseError> {
        let (_, name, args, end) = self.expect_type_name()?;
        Ok(Some(Statement::ExternalType {
            location: SrcSpan { start, end },
            public,
            name,
            args,
            doc: None,
        }))
    }

    //
    // Parse Custom Types
    //

    // examples:
    //   type A { A }
    //   type A { A(String) }
    //   type A(a) { A(a: b) }
    //   type A(a) { A(String, a: b) }
    fn parse_custom_type(
        &mut self,
        start: usize,
        public: bool,
        opaque: bool,
    ) -> Result<Option<UntypedStatement>, ParseError> {
        let (_, name, parameters, end) = self.expect_type_name()?;
        if self.maybe_one(&Tok::Lbrace).is_some() {
            // Custom Type
            let constructors = Parser::series_of(
                self,
                &|p| {
                    if let Some((c_s, c_n, c_e)) = Parser::maybe_upname(p) {
                        let (args, args_e) = Parser::parse_type_constructor_args(p)?;
                        let end = args_e.max(c_e);
                        Ok(Some(RecordConstructor {
                            location: SrcSpan { start: c_s, end },
                            name: c_n,
                            args,
                            documentation: None,
                        }))
                    } else {
                        Ok(None)
                    }
                },
                // No separator
                None,
            )?;
            let _ = self.expect_one(&Tok::Rbrace)?;
            if constructors.is_empty() {
                parse_error(ParseErrorType::NoConstructors, SrcSpan { start, end })
            } else {
                Ok(Some(Statement::CustomType {
                    doc: None,
                    location: SrcSpan { start, end },
                    public,
                    opaque,
                    name,
                    parameters,
                    constructors,
                    typed_parameters: vec![],
                }))
            }
        } else if let Some((eq_s, eq_e)) = self.maybe_one(&Tok::Equal) {
            // Type Alias
            if !opaque {
                if let Some(t) = self.parse_type(false)? {
                    let type_end = t.location().end;
                    Ok(Some(Statement::TypeAlias {
                        doc: None,
                        location: SrcSpan {
                            start,
                            end: type_end,
                        },
                        public,
                        alias: name,
                        args: parameters,
                        resolved_type: t,
                        typ: (),
                    }))
                } else {
                    parse_error(
                        ParseErrorType::ExpectedType,
                        SrcSpan {
                            start: eq_s,
                            end: eq_e,
                        },
                    )
                }
            } else {
                parse_error(ParseErrorType::OpaqueTypeAlias, SrcSpan { start, end })
            }
        } else {
            // Stared defining a custom type or type alias, didn't supply any {} or =
            parse_error(ParseErrorType::NoConstructors, SrcSpan { start, end })
        }
    }

    // examples:
    //   A
    //   A(one, two)
    fn expect_type_name(&mut self) -> Result<(usize, String, Vec<String>, usize), ParseError> {
        let (start, upname, end) = self.expect_upname()?;
        if self.maybe_one(&Tok::Lpar).is_some() {
            let args = Parser::series_of(self, &|p| Ok(Parser::maybe_name(p)), Some(&Tok::Comma))?;
            let (_, par_e) = self.expect_one(&Tok::Rpar)?;
            let args2 = args.into_iter().map(|(_, a, _)| a).collect();
            Ok((start, upname, args2, par_e))
        } else {
            Ok((start, upname, vec![], end))
        }
    }

    // examples:
    //   *no args*
    //   ()
    //   (a, b)
    fn parse_type_constructor_args(
        &mut self,
    ) -> Result<(Vec<RecordConstructorArg<()>>, usize), ParseError> {
        if self.maybe_one(&Tok::Lpar).is_some() {
            let args = Parser::series_of(
                self,
                &|p| match (p.tok0.take(), p.tok1.take()) {
                    (Some((start, Tok::Name { name }, _)), Some((_, Tok::Colon, end))) => {
                        let _ = Parser::next_tok(p);
                        let _ = Parser::next_tok(p);
                        match Parser::parse_type(p, false)? {
                            Some(type_ast) => Ok(Some(RecordConstructorArg {
                                label: Some(name),
                                ast: type_ast,
                                location: SrcSpan { start, end },
                                typ: (),
                            })),
                            None => {
                                parse_error(ParseErrorType::ExpectedType, SrcSpan { start, end })
                            }
                        }
                    }
                    (t0, t1) => {
                        p.tok0 = t0;
                        p.tok1 = t1;
                        match Parser::parse_type(p, false)? {
                            Some(type_ast) => {
                                let type_location = type_ast.location();
                                Ok(Some(RecordConstructorArg {
                                    label: None,
                                    ast: type_ast,
                                    location: type_location,
                                    typ: (),
                                }))
                            }
                            None => Ok(None),
                        }
                    }
                },
                Some(&Tok::Comma),
            )?;
            let (_, end) = self.expect_one(&Tok::Rpar)?;
            Ok((args, end))
        } else {
            Ok((vec![], 0))
        }
    }

    //
    // Parse Type Annotations
    //

    // examples:
    //   :a
    //   :Int
    //   :Result(a, _)
    //   :Result(Result(a, e), tuple(_, String))
    fn parse_type_annotation(
        &mut self,
        start_tok: &Tok,
        for_const: bool,
    ) -> Result<Option<TypeAst>, ParseError> {
        if let Some((start, end)) = self.maybe_one(start_tok) {
            match self.parse_type(for_const) {
                Ok(None) => parse_error(ParseErrorType::ExpectedType, SrcSpan { start, end }),
                other => other,
            }
        } else {
            Ok(None)
        }
    }

    // Parse the type part of a type annotation, same as `parse_type_annotation` minus the ":"
    fn parse_type(&mut self, for_const: bool) -> Result<Option<TypeAst>, ParseError> {
        match self.tok0.take() {
            // Type hole
            Some((start, Tok::DiscardName { name }, end)) => {
                let _ = self.next_tok();
                Ok(Some(TypeAst::Hole {
                    location: SrcSpan { start, end },
                    name,
                }))
            }

            // Tuple
            Some((start, Tok::Tuple, end)) => {
                let _ = self.next_tok();
                let _ = self.expect_one(&Tok::Lpar)?;
                let elems = self.parse_types(for_const)?;
                let _ = self.expect_one(&Tok::Rpar)?;
                Ok(Some(TypeAst::Tuple {
                    location: SrcSpan { start, end },
                    elems,
                }))
            }

            // Function
            Some((start, Tok::Fn, end)) => {
                let _ = self.next_tok();
                if for_const {
                    parse_error(ParseErrorType::NotConstType, SrcSpan { start, end })
                } else {
                    let _ = self.expect_one(&Tok::Lpar)?;
                    let args = Parser::series_of(
                        self,
                        &|x| Parser::parse_type(x, for_const),
                        Some(&Tok::Comma),
                    )?;
                    let _ = self.expect_one(&Tok::Rpar)?;
                    let (arr_s, arr_e) = self.expect_one(&Tok::RArrow)?;
                    let retrn = self.parse_type(for_const)?;
                    if let Some(retrn) = retrn {
                        Ok(Some(TypeAst::Fn {
                            location: SrcSpan {
                                start,
                                end: retrn.location().end,
                            },
                            retrn: Box::new(retrn),
                            args,
                        }))
                    } else {
                        parse_error(
                            ParseErrorType::ExpectedType,
                            SrcSpan {
                                start: arr_s,
                                end: arr_e,
                            },
                        )
                    }
                }
            }

            // Constructor function
            Some((start, Tok::UpName { name }, end)) => {
                let _ = self.next_tok();
                self.parse_type_name_finish(for_const, start, None, name, end)
            }

            // Constructor Module or type Variable
            Some((start, Tok::Name { name: mod_name }, end)) => {
                let _ = self.next_tok();
                if self.maybe_one(&Tok::Dot).is_some() {
                    let (_, upname, upname_e) = self.expect_upname()?;
                    self.parse_type_name_finish(for_const, start, Some(mod_name), upname, upname_e)
                } else if for_const {
                    return parse_error(ParseErrorType::NotConstType, SrcSpan { start, end });
                } else {
                    Ok(Some(TypeAst::Var {
                        location: SrcSpan { start, end },
                        name: mod_name,
                    }))
                }
            }

            t0 => {
                self.tok0 = t0;
                Ok(None)
            }
        }
    }

    // Parse the '( ... )' of a type name
    fn parse_type_name_finish(
        &mut self,
        for_const: bool,
        start: usize,
        module: Option<String>,
        name: String,
        end: usize,
    ) -> Result<Option<TypeAst>, ParseError> {
        if self.maybe_one(&Tok::Lpar).is_some() {
            let args = self.parse_types(for_const)?;
            let (_, par_e) = self.expect_one(&Tok::Rpar)?;
            Ok(Some(TypeAst::Constructor {
                location: SrcSpan { start, end: par_e },
                module,
                name,
                args,
            }))
        } else {
            Ok(Some(TypeAst::Constructor {
                location: SrcSpan { start, end },
                module,
                name,
                args: vec![],
            }))
        }
    }

    // For parsing a comma separated "list" of types, for tuple, constructor, and function
    fn parse_types(&mut self, for_const: bool) -> Result<Vec<TypeAst>, ParseError> {
        let elems = Parser::series_of(
            self,
            &|p| Parser::parse_type(p, for_const),
            Some(&Tok::Comma),
        )?;
        Ok(elems)
    }

    //
    // Parse Imports
    //

    // examples:
    //   import a
    //   import a/b
    //   import a/b.{c}
    //   import a/b.{c as d} as e
    fn parse_import(&mut self) -> Result<Option<UntypedStatement>, ParseError> {
        let mut start = 0;
        let mut end;
        let mut module = vec![];
        // Gather module names
        loop {
            let (s, name, e) = self.expect_name()?;
            if module.is_empty() {
                start = s;
            }
            module.push(name);
            end = e;

            // Ueful error for : import a/.{b}
            if let Some((s, _)) = self.maybe_one(&Tok::SlashDot) {
                return parse_error(
                    ParseErrorType::ExpectedName,
                    SrcSpan {
                        start: s + 1,
                        end: s + 1,
                    },
                );
            }

            // break if there's no trailing slash
            if self.maybe_one(&Tok::Slash).is_none() {
                break;
            }
        }

        // Gather imports
        let mut unqualified = vec![];
        if self.maybe_one(&Tok::Dot).is_some() {
            let _ = self.expect_one(&Tok::Lbrace)?;
            unqualified = self.parse_unqualified_imports()?;
            let _ = self.expect_one(&Tok::Rbrace)?;
        }

        // Parse as_name
        let mut as_name = None;
        if self.maybe_one(&Tok::As).is_some() {
            as_name = Some(self.expect_name()?.1)
        }

        Ok(Some(Statement::Import {
            location: SrcSpan { start, end },
            unqualified,
            module,
            as_name,
        }))
    }

    // [Name (as Name)? | UpName (as Name)? ](, [Name (as Name)? | UpName (as Name)?])*,?
    fn parse_unqualified_imports(&mut self) -> Result<Vec<UnqualifiedImport>, ParseError> {
        let mut imports = vec![];
        loop {
            // parse imports
            match self.tok0.take() {
                Some((start, Tok::Name { name }, end))
                | Some((start, Tok::UpName { name }, end)) => {
                    let _ = self.next_tok();
                    let location = SrcSpan { start, end };
                    let mut import = UnqualifiedImport {
                        name,
                        location,
                        as_name: None,
                    };
                    if self.maybe_one(&Tok::As).is_some() {
                        let (_, as_name, _) = self.expect_name()?;
                        import.as_name = Some(as_name)
                    }
                    imports.push(import)
                }
                t0 => {
                    self.tok0 = t0;
                    break;
                }
            }
            // parse comma
            match self.tok0 {
                Some((_, Tok::Comma, _)) => {
                    let _ = self.next_tok();
                }
                _ => break,
            }
        }
        Ok(imports)
    }

    //
    // Parse Constants
    //

    // examples:
    //   const a = 1
    //   const a:Int = 1
    //   pub const a:Int = 1
    fn parse_module_const(&mut self, public: bool) -> Result<Option<UntypedStatement>, ParseError> {
        let (start, name, _) = self.expect_name()?;

        let annotation = self.parse_type_annotation(&Tok::Colon, true)?;

        let (eq_s, eq_e) = self.expect_one(&Tok::Equal)?;
        if let Some(value) = self.parse_const_value()? {
            Ok(Some(Statement::ModuleConstant {
                doc: None,
                location: SrcSpan { start, end: 0 },
                public,
                name,
                annotation,
                value: Box::new(value),
                typ: (),
            }))
        } else {
            parse_error(
                ParseErrorType::NoValueAfterEqual,
                SrcSpan {
                    start: eq_s,
                    end: eq_e,
                },
            )
        }
    }

    // examples:
    //   1
    //   "hi"
    //   True
    //   [1,2,3]
    fn parse_const_value(&mut self) -> Result<Option<UntypedConstant>, ParseError> {
        match self.tok0.take() {
            Some((start, Tok::String { value }, end)) => {
                let _ = self.next_tok();
                Ok(Some(Constant::String {
                    value,
                    location: SrcSpan { start, end },
                }))
            }

            Some((start, Tok::Float { value }, end)) => {
                let _ = self.next_tok();
                Ok(Some(Constant::Float {
                    value,
                    location: SrcSpan { start, end },
                }))
            }

            Some((start, Tok::Int { value }, end)) => {
                let _ = self.next_tok();
                Ok(Some(Constant::Int {
                    value,
                    location: SrcSpan { start, end },
                }))
            }

            Some((start, Tok::Tuple, _)) => {
                let _ = self.next_tok();
                let _ = self.expect_one(&Tok::Lpar)?;
                let elements =
                    Parser::series_of(self, &Parser::parse_const_value, Some(&Tok::Comma))?;
                let (_, end) = self.expect_one(&Tok::Rpar)?;
                Ok(Some(Constant::Tuple {
                    elements,
                    location: SrcSpan { start, end },
                }))
            }

            Some((start, Tok::Lsqb, _)) => {
                let _ = self.next_tok();
                let elements =
                    Parser::series_of(self, &Parser::parse_const_value, Some(&Tok::Comma))?;
                let (_, end) = self.expect_one(&Tok::Rsqb)?;
                Ok(Some(Constant::List {
                    elements,
                    location: SrcSpan { start, end },
                    typ: (),
                }))
            }
            // Bitstring
            Some((start, Tok::LtLt, _)) => {
                let _ = self.next_tok();
                let segments = Parser::series_of(
                    self,
                    &|s| {
                        Parser::parse_bit_string_segment(
                            s,
                            &Parser::parse_const_value,
                            &Parser::expect_const_int,
                            &bit_string_const_int,
                        )
                    },
                    Some(&Tok::Comma),
                )?;
                let (_, end) = self.expect_one(&Tok::GtGt)?;
                Ok(Some(Constant::BitString {
                    location: SrcSpan { start, end },
                    segments,
                }))
            }

            Some((start, Tok::UpName { name }, end)) => {
                let _ = self.next_tok();
                self.parse_const_record_finish(start, None, name, end)
            }

            Some((start, Tok::Name { name }, end)) => {
                let _ = self.next_tok();
                if self.expect_one(&Tok::Dot).is_ok() {
                    let (_, upname, end) = self.expect_upname()?;
                    self.parse_const_record_finish(start, Some(name), upname, end)
                } else {
                    parse_error(ParseErrorType::NotConstType, SrcSpan { start, end })
                }
            }

            // Helpful error for []
            Some((start, Tok::ListNil, end)) => {
                parse_error(ParseErrorType::ListNilNotAllowed, SrcSpan { start, end })
            }

            // Helpful error for fn
            Some((start, Tok::Fn, end)) => {
                parse_error(ParseErrorType::NotConstType, SrcSpan { start, end })
            }

            t0 => {
                self.tok0 = t0;
                Ok(None)
            }
        }
    }

    // Parse the '( .. )' of a const type constructor
    fn parse_const_record_finish(
        &mut self,
        start: usize,
        module: Option<String>,
        name: String,
        end: usize,
    ) -> Result<Option<UntypedConstant>, ParseError> {
        if self.maybe_one(&Tok::Lpar).is_some() {
            let args = Parser::series_of(self, &Parser::parse_const_record_arg, Some(&Tok::Comma))?;
            let (_, par_e) = self.expect_one(&Tok::Rpar)?;
            Ok(Some(Constant::Record {
                location: SrcSpan { start, end: par_e },
                module,
                name,
                args,
                tag: (),
                typ: (),
            }))
        } else {
            Ok(Some(Constant::Record {
                location: SrcSpan { start, end },
                module,
                name,
                args: vec![],
                tag: (),
                typ: (),
            }))
        }
    }

    // examples:
    //  name: const
    //  const
    fn parse_const_record_arg(&mut self) -> Result<Option<CallArg<UntypedConstant>>, ParseError> {
        let name = match (self.tok0.take(), self.tok1.as_ref()) {
            // Named arg
            (Some((start, Tok::Name { name }, end)), Some((_, Tok::Colon, _))) => {
                let _ = self.next_tok();
                let _ = self.next_tok();
                Some((start, name, end))
            }

            // Unnamed arg
            (t0, _) => {
                self.tok0 = t0;
                None
            }
        };

        if let Some(value) = self.parse_const_value()? {
            if let Some((start, label, _)) = name {
                Ok(Some(CallArg {
                    location: SrcSpan {
                        start,
                        end: value.location().end,
                    },
                    value,
                    label: Some(label),
                }))
            } else {
                Ok(Some(CallArg {
                    location: value.location(),
                    value,
                    label: None,
                }))
            }
        } else if name.is_some() {
            self.next_tok_unexpected(vec!["a constant value".to_string()])?
        } else {
            Ok(None)
        }
    }

    //
    // Bit String parsing
    //

    // The structure is roughly the same for pattern, const, and expr
    // thats why these functions take functions
    //
    // patern (: option)?
    fn parse_bit_string_segment<A>(
        &mut self,
        value_parser: &impl Fn(&mut Self) -> Result<Option<A>, ParseError>,
        arg_parser: &impl Fn(&mut Self) -> Result<A, ParseError>,
        to_int_segment: &impl Fn(String, usize, usize) -> A,
    ) -> Result<Option<BitStringSegment<A, ()>>, ParseError>
    where
        A: HasLocation,
    {
        if let Some(value) = value_parser(self)? {
            let options = if self.maybe_one(&Tok::Colon).is_some() {
                Parser::series_of(
                    self,
                    &|s| Parser::parse_bit_string_segment_option(s, &arg_parser, &to_int_segment),
                    Some(&Tok::Minus),
                )?
            } else {
                vec![]
            };
            let end = options
                .last()
                .map(|o| o.location().end)
                .unwrap_or_else(|| value.location().end);
            Ok(Some(BitStringSegment {
                location: SrcSpan {
                    start: value.location().start,
                    end,
                },
                value: Box::new(value),
                typ: (),
                options,
            }))
        } else {
            Ok(None)
        }
    }

    // examples:
    //   1
    //   size(1)
    //   size(five)
    //   utf8
    fn parse_bit_string_segment_option<A>(
        &mut self,
        arg_parser: &impl Fn(&mut Self) -> Result<A, ParseError>,
        to_int_segment: &impl Fn(String, usize, usize) -> A,
    ) -> Result<Option<BitStringSegmentOption<A>>, ParseError> {
        match self.next_tok() {
            // named segment
            Some((start, Tok::Name { name }, end)) => {
                if self.maybe_one(&Tok::Lpar).is_some() {
                    // named function segment
                    let value = arg_parser(self)?;
                    let (_, end) = self.expect_one(&Tok::Rpar)?;
                    match name.as_str() {
                        "unit" => Ok(Some(BitStringSegmentOption::Unit {
                            location: SrcSpan { start, end },
                            value: Box::new(value),
                            short_form: false,
                        })),

                        "size" => Ok(Some(BitStringSegmentOption::Size {
                            location: SrcSpan { start, end },
                            value: Box::new(value),
                            short_form: false,
                        })),
                        _ => parse_error(
                            ParseErrorType::InvalidBitStringSegment,
                            SrcSpan { start, end },
                        ),
                    }
                } else {
                    str_to_bit_string_segment_option(&name, SrcSpan { start, end })
                        .ok_or(ParseError {
                            error: ParseErrorType::InvalidBitStringSegment,
                            location: SrcSpan { start, end },
                        })
                        .map(|v| Some(v))
                }
            }
            // int segment
            Some((start, Tok::Int { value }, end)) => Ok(Some(BitStringSegmentOption::Size {
                location: SrcSpan { start, end },
                value: Box::new(to_int_segment(value, start, end)),
                short_form: true,
            })),
            // invalid
            _ => self.next_tok_unexpected(vec![
                "A valid bitstring segment type".to_string(),
                "See: https://gleam.run/book/tour/bit-strings.html".to_string(),
            ]),
        }
    }

    fn expect_bit_string_pattern_segment_arg(&mut self) -> Result<UntypedPattern, ParseError> {
        match self.next_tok() {
            Some((start, Tok::Name { name }, end)) => Ok(Pattern::VarCall {
                location: SrcSpan { start, end },
                name,
                typ: (),
            }),
            Some((start, Tok::Int { value }, end)) => Ok(Pattern::Int {
                location: SrcSpan { start, end },
                value,
            }),
            _ => self.next_tok_unexpected(vec!["A variable name or an integer".to_string()]),
        }
    }

    fn expect_const_int(&mut self) -> Result<UntypedConstant, ParseError> {
        match self.next_tok() {
            Some((start, Tok::Int { value }, end)) => Ok(Constant::Int {
                location: SrcSpan { start, end },
                value,
            }),
            _ => self.next_tok_unexpected(vec!["A variable name or an integer".to_string()]),
        }
    }

    fn expect_expression(&mut self) -> Result<UntypedExpr, ParseError> {
        if let Some(e) = self.parse_expression()? {
            Ok(e)
        } else {
            self.next_tok_unexpected(vec!["An expression".to_string()])
        }
    }

    //
    // Parse Helpers
    //

    // Expect a particular token, advances the token stream
    fn expect_one(&mut self, wanted: &Tok) -> Result<(usize, usize), ParseError> {
        match self.maybe_one(wanted) {
            Some((start, end)) => Ok((start, end)),
            None => self.next_tok_unexpected(vec![wanted.to_string()]),
        }
    }

    // Expect a Name else a token dependent helpful error
    fn expect_name(&mut self) -> Result<(usize, String, usize), ParseError> {
        let t = self.next_tok();
        match t {
            Some((start, tok, end)) => {
                if let Tok::Name { name } = tok {
                    Ok((start, name, end))
                } else if let Tok::UpName { .. } = tok {
                    parse_error(ParseErrorType::IncorrectName, SrcSpan { start, end })
                } else if let Tok::DiscardName { .. } = tok {
                    parse_error(ParseErrorType::IncorrectName, SrcSpan { start, end })
                } else if is_reserved_word(tok) {
                    parse_error(
                        ParseErrorType::UnexpectedReservedWord,
                        SrcSpan { start, end },
                    )
                } else {
                    parse_error(ParseErrorType::ExpectedName, SrcSpan { start, end })
                }
            }
            None => parse_error(ParseErrorType::UnexpectedEOF, SrcSpan { start: 0, end: 0 }),
        }
    }

    // Expect an UpName else a token dependent helpful error
    fn expect_upname(&mut self) -> Result<(usize, String, usize), ParseError> {
        let t = self.next_tok();
        match t {
            Some((start, tok, end)) => {
                if let Tok::Name { .. } = tok {
                    parse_error(ParseErrorType::IncorrectUpName, SrcSpan { start, end })
                } else if let Tok::UpName { name } = tok {
                    Ok((start, name, end))
                } else if let Tok::DiscardName { .. } = tok {
                    parse_error(ParseErrorType::IncorrectUpName, SrcSpan { start, end })
                } else {
                    parse_error(ParseErrorType::ExpectedUpName, SrcSpan { start, end })
                }
            }
            None => parse_error(ParseErrorType::UnexpectedEOF, SrcSpan { start: 0, end: 0 }),
        }
    }

    // Expect a String else error
    fn expect_string(&mut self) -> Result<(usize, String, usize), ParseError> {
        match self.next_tok() {
            Some((start, Tok::String { value }, end)) => Ok((start, value, end)),
            _ => self.next_tok_unexpected(vec!["a string".to_string()]),
        }
    }

    // If the next token matches the requested, consume it and return (start, end)
    fn maybe_one(&mut self, tok: &Tok) -> Option<(usize, usize)> {
        match self.tok0.take() {
            Some((s, t, e)) if t == *tok => {
                let _ = self.next_tok();
                Some((s, e))
            }

            t0 => {
                self.tok0 = t0;
                None
            }
        }
    }

    // Parse a series by repeating a parser, and possibly a separator
    fn series_of<A>(
        &mut self,
        parser: &impl Fn(&mut Self) -> Result<Option<A>, ParseError>,
        sep: Option<&Tok>,
    ) -> Result<Vec<A>, ParseError> {
        let mut results = vec![];
        while let Some(result) = parser(self)? {
            results.push(result);
            if let Some(sep) = sep {
                if self.maybe_one(sep).is_none() {
                    break;
                }
                // Helpful error if extra separator
                if let Some((start, end)) = self.maybe_one(sep) {
                    return parse_error(ParseErrorType::ExtraSeparator, SrcSpan { start, end });
                }
            }
        }

        Ok(results)
    }

    // If next token is a Name, consume it and return relevant info, otherwise, return none
    fn maybe_name(&mut self) -> Option<(usize, String, usize)> {
        match self.tok0.take() {
            Some((s, Tok::Name { name }, e)) => {
                let _ = self.next_tok();
                Some((s, name, e))
            }
            t0 => {
                self.tok0 = t0;
                None
            }
        }
    }

    // if next token is an UpName, consume it and return relevant info, otherwise, return none
    fn maybe_upname(&mut self) -> Option<(usize, String, usize)> {
        match self.tok0.take() {
            Some((s, Tok::UpName { name }, e)) => {
                let _ = self.next_tok();
                Some((s, name, e))
            }
            t0 => {
                self.tok0 = t0;
                None
            }
        }
    }

    // if next token is a DiscardName, consume it and return relevant info, otherwise, return none
    fn maybe_discard_name(&mut self) -> Option<(usize, String, usize)> {
        match self.tok0.take() {
            Some((s, Tok::DiscardName { name }, e)) => {
                let _ = self.next_tok();
                Some((s, name, e))
            }
            t0 => {
                self.tok0 = t0;
                None
            }
        }
    }

    // Error on the next token or EOF
    fn next_tok_unexpected<A>(&mut self, expected: Vec<String>) -> Result<A, ParseError> {
        match self.next_tok() {
            None => parse_error(ParseErrorType::UnexpectedEOF, SrcSpan { start: 0, end: 0 }),

            Some((start, _, end)) => parse_error(
                ParseErrorType::UnexpectedToken { expected },
                SrcSpan { start, end },
            ),
        }
    }

    // Moving the token stream forward
    // returns old tok0
    fn next_tok(&mut self) -> Option<Spanned> {
        let t = self.tok0.take();
        let mut nxt;
        loop {
            match self.tokens.next() {
                // gather and skip extra
                Some(Ok((s, Tok::EmptyLine, _))) => {
                    self.extra.empty_lines.push(s);
                }
                Some(Ok((start, Tok::CommentNormal, end))) => {
                    self.extra.comments.push(SrcSpan { start, end });
                }
                Some(Ok((start, Tok::CommentDoc, end))) => {
                    self.extra.doc_comments.push(SrcSpan { start, end });
                }
                Some(Ok((start, Tok::CommentModule, end))) => {
                    self.extra.module_comments.push(SrcSpan { start, end });
                }

                // die on lex error
                Some(Err(err)) => {
                    nxt = None;
                    self.lex_errors.push(err);
                    break;
                }

                Some(Ok(tok)) => {
                    nxt = Some(tok);
                    break;
                }
                None => {
                    nxt = None;
                    break;
                }
            }
        }
        self.tok0 = self.tok1.take();
        self.tok1 = nxt.take();
        t
    }
}

// Operator Precedence Parsing
//
// Higher number means higher precedence.
// All operators are left associative.

// Simple-Precedence-Parser, handle seeing an operator or end
fn handle_op<A>(
    next_op: Option<(Spanned, u8)>,
    opstack: &mut Vec<(Spanned, u8)>,
    estack: &mut Vec<A>,
    do_reduce: &impl Fn(Spanned, &mut Vec<A>),
) -> Result<Option<A>, ParseError> {
    let mut next_op = next_op;
    loop {
        match (opstack.pop(), next_op.take()) {
            (None, None) => {
                if let Some(fin) = estack.pop() {
                    if estack.is_empty() {
                        return Ok(Some(fin));
                    } else {
                        fatal_compiler_bug("Expression not fully reduced.")
                    }
                } else {
                    return Ok(None);
                }
            }

            (None, Some(op)) => {
                opstack.push(op);
                break;
            }

            (Some((op, _)), None) => do_reduce(op, estack),

            (Some((opl, pl)), Some((opr, pr))) => {
                match pl.cmp(&pr) {
                    // all ops are left associative
                    Ordering::Greater | Ordering::Equal => {
                        do_reduce(opl, estack);
                        next_op = Some((opr, pr));
                    }
                    Ordering::Less => {
                        opstack.push((opl, pl));
                        opstack.push((opr, pr));
                        break;
                    }
                }
            }
        }
    }
    Ok(None)
}

fn precedence(t: &Tok) -> Option<u8> {
    if t == &Tok::Pipe {
        return Some(5);
    };
    match tok_to_binop(t) {
        Some(b) => Some(b.precedence()),
        None => None,
    }
}

fn tok_to_binop(t: &Tok) -> Option<BinOp> {
    match *t {
        Tok::VbarVbar => Some(BinOp::Or),
        Tok::AmperAmper => Some(BinOp::And),
        Tok::EqualEqual => Some(BinOp::Eq),
        Tok::NotEqual => Some(BinOp::NotEq),
        Tok::Less => Some(BinOp::LtInt),
        Tok::LessEqual => Some(BinOp::LtEqInt),
        Tok::Greater => Some(BinOp::GtInt),
        Tok::GreaterEqual => Some(BinOp::GtEqInt),
        Tok::LessDot => Some(BinOp::LtFloat),
        Tok::LessEqualDot => Some(BinOp::LtEqFloat),
        Tok::GreaterDot => Some(BinOp::GtFloat),
        Tok::GreaterEqualDot => Some(BinOp::GtEqFloat),
        Tok::Plus => Some(BinOp::AddInt),
        Tok::Minus => Some(BinOp::SubInt),
        Tok::PlusDot => Some(BinOp::AddFloat),
        Tok::MinusDot => Some(BinOp::SubFloat),
        Tok::Percent => Some(BinOp::ModuloInt),
        Tok::Star => Some(BinOp::MultInt),
        Tok::StarDot => Some(BinOp::MultFloat),
        Tok::Slash => Some(BinOp::DivInt),
        Tok::SlashDot => Some(BinOp::DivFloat),
        _ => None,
    }
}
// Simple-Precedence-Parser, perform reduction for expression
fn do_reduce_expression(op: Spanned, estack: &mut Vec<UntypedExpr>) {
    match (estack.pop(), estack.pop()) {
        (Some(er), Some(el)) => {
            let new_e = expr_op_reduction(op, el, er);
            estack.push(new_e);
        }
        _ => fatal_compiler_bug("Tried to reduce without 2 expressions"),
    }
}

// Simple-Precedence-Parser, perform reduction for clause guard
fn do_reduce_clause_guard(op: Spanned, estack: &mut Vec<UntypedClauseGuard>) {
    match (estack.pop(), estack.pop()) {
        (Some(er), Some(el)) => {
            let new_e = clause_guard_reduction(op, el, er);
            estack.push(new_e);
        }
        _ => fatal_compiler_bug("Tried to reduce without 2 guards"),
    }
}

fn expr_op_reduction(op: Spanned, l: UntypedExpr, r: UntypedExpr) -> UntypedExpr {
    if op.1 == Tok::Pipe {
        UntypedExpr::Pipe {
            location: SrcSpan {
                start: op.0,
                end: r.location().end,
            },
            left: Box::new(l),
            right: Box::new(r),
        }
    } else if let Some(bin_op) = tok_to_binop(&op.1) {
        UntypedExpr::BinOp {
            location: SrcSpan {
                start: l.location().start,
                end: r.location().end,
            },
            name: bin_op,
            left: Box::new(l),
            right: Box::new(r),
        }
    } else {
        fatal_compiler_bug("Token could not be converted to binop.")
    }
}

fn clause_guard_reduction(
    op: Spanned,
    l: UntypedClauseGuard,
    r: UntypedClauseGuard,
) -> UntypedClauseGuard {
    let location = SrcSpan {
        start: l.location().start,
        end: r.location().end,
    };
    let left = Box::new(l);
    let right = Box::new(r);
    match op.1 {
        Tok::VbarVbar => ClauseGuard::Or {
            location,
            left,
            right,
        },

        Tok::AmperAmper => ClauseGuard::And {
            location,
            left,
            right,
        },

        Tok::EqualEqual => ClauseGuard::Equals {
            location,
            left,
            right,
        },

        Tok::NotEqual => ClauseGuard::NotEquals {
            location,
            left,
            right,
        },

        Tok::Greater => ClauseGuard::GtInt {
            location,
            left,
            right,
        },

        Tok::GreaterEqual => ClauseGuard::GtEqInt {
            location,
            left,
            right,
        },

        Tok::Less => ClauseGuard::LtInt {
            location,
            left,
            right,
        },

        Tok::LessEqual => ClauseGuard::LtEqInt {
            location,
            left,
            right,
        },

        Tok::GreaterDot => ClauseGuard::GtFloat {
            location,
            left,
            right,
        },

        Tok::GreaterEqualDot => ClauseGuard::GtEqFloat {
            location,
            left,
            right,
        },

        Tok::LessDot => ClauseGuard::LtFloat {
            location,
            left,
            right,
        },

        Tok::LessEqualDot => ClauseGuard::LtEqFloat {
            location,
            left,
            right,
        },

        _ => fatal_compiler_bug("Token could not be converted to Guard Op."),
    }
}

// Bitstring Parse Helpers
//
// Bitstrings in patterns, guards, and expressions have a very similar structure
// but need specific types. These are helpers for that. There is probably a
// rustier way to do this :)
fn bit_string_pattern_int(value: String, start: usize, end: usize) -> UntypedPattern {
    Pattern::Int {
        location: SrcSpan { start, end },
        value,
    }
}

fn bit_string_expr_int(value: String, start: usize, end: usize) -> UntypedExpr {
    UntypedExpr::Int {
        location: SrcSpan { start, end },
        value,
    }
}

fn bit_string_const_int(value: String, start: usize, end: usize) -> UntypedConstant {
    Constant::Int {
        location: SrcSpan { start, end },
        value,
    }
}

fn str_to_bit_string_segment_option<A>(
    lit: &str,
    location: SrcSpan,
) -> Option<BitStringSegmentOption<A>> {
    match lit {
        "binary" => Some(BitStringSegmentOption::Binary { location }),
        "bytes" => Some(BitStringSegmentOption::Binary { location }),
        "int" => Some(BitStringSegmentOption::Integer { location }),
        "float" => Some(BitStringSegmentOption::Float { location }),
        "bit_string" => Some(BitStringSegmentOption::BitString { location }),
        "bits" => Some(BitStringSegmentOption::BitString { location }),
        "utf8" => Some(BitStringSegmentOption::UTF8 { location }),
        "utf16" => Some(BitStringSegmentOption::UTF16 { location }),
        "utf32" => Some(BitStringSegmentOption::UTF32 { location }),
        "utf8_codepoint" => Some(BitStringSegmentOption::UTF8Codepoint { location }),
        "utf16_codepoint" => Some(BitStringSegmentOption::UTF16Codepoint { location }),
        "utf32_codepoint" => Some(BitStringSegmentOption::UTF32Codepoint { location }),
        "signed" => Some(BitStringSegmentOption::Signed { location }),
        "unsigned" => Some(BitStringSegmentOption::Unsigned { location }),
        "big" => Some(BitStringSegmentOption::Big { location }),
        "little" => Some(BitStringSegmentOption::Little { location }),
        "native" => Some(BitStringSegmentOption::Native { location }),
        _ => None,
    }
}

//
// Error Helpers
//
fn parse_error<T>(error: ParseErrorType, location: SrcSpan) -> Result<T, ParseError> {
    Err(ParseError { error, location })
}

//
// Misc Helpers
//

// useful for checking if a user tried to enter a reserved word as a name
fn is_reserved_word(tok: Tok) -> bool {
    matches![
        tok,
        Tok::As
            | Tok::Assert
            | Tok::Case
            | Tok::Const
            | Tok::External
            | Tok::Fn
            | Tok::If
            | Tok::Import
            | Tok::Let
            | Tok::Opaque
            | Tok::Pub
            | Tok::Todo
            | Tok::Try
            | Tok::Tuple
            | Tok::Type
    ]
}

// Parsing a function call into the appropriate structure
pub enum ParserArg {
    Arg(CallArg<UntypedExpr>),
    Hole {
        location: SrcSpan,
        label: Option<String>,
    },
}

pub fn make_call(
    fun: UntypedExpr,
    args: Vec<ParserArg>,
    start: usize,
    end: usize,
) -> Result<UntypedExpr, ParseError> {
    let mut num_holes = 0;
    let args = args
        .into_iter()
        .map(|a| match a {
            ParserArg::Arg(arg) => arg,
            ParserArg::Hole { location, label } => {
                num_holes += 1;
                CallArg {
                    label,
                    location,
                    value: UntypedExpr::Var {
                        location,
                        name: CAPTURE_VARIABLE.to_string(),
                    },
                }
            }
        })
        .collect();
    let call = UntypedExpr::Call {
        location: SrcSpan { start, end },
        fun: Box::new(fun),
        args,
    };
    match num_holes {
        // A normal call
        0 => Ok(call),

        // An anon function using the capture syntax run(_, 1, 2)
        1 => Ok(UntypedExpr::Fn {
            location: call.location(),
            is_capture: true,
            args: vec![Arg {
                location: SrcSpan { start: 0, end: 0 },
                annotation: None,
                names: ArgNames::Named {
                    name: CAPTURE_VARIABLE.to_string(),
                },
                typ: (),
            }],
            body: Box::new(call),
            return_annotation: None,
        }),

        _ => parse_error(ParseErrorType::TooManyArgHoles, call.location()),
    }
}
