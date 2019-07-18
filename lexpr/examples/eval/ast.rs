use std::{
    fmt::{self, Write},
    iter,
    rc::Rc,
};

use gc::{Gc, GcCell};
use lexpr::sexp;
use log::debug;
use smallvec::{smallvec, SmallVec};

use crate::{eval, value::Value};

#[derive(Debug)]
pub enum Params {
    Any(Box<str>),
    Exact(Vec<Box<str>>),
    AtLeast(Vec<Box<str>>, Box<str>),
}

#[derive(Debug)]
struct EnvFrame {
    idents: Vec<Box<str>>,
    rec_bodies: Vec<lexpr::Value>,
}

impl EnvFrame {
    fn new(idents: Vec<Box<str>>) -> Self {
        EnvFrame {
            idents,
            rec_bodies: Vec::new(),
        }
    }
}

#[derive(Default, Debug, Clone)]
pub struct EnvIndex(usize, usize);

impl EnvIndex {
    pub fn level(&self) -> usize {
        self.0
    }

    pub fn slot(&self) -> usize {
        self.1
    }
}

#[derive(Debug)]
pub struct EnvStack {
    frames: Vec<EnvFrame>,
}

impl EnvStack {
    pub fn initial<T>(idents: impl IntoIterator<Item = T>) -> Self
    where
        T: Into<Box<str>>,
    {
        EnvStack {
            frames: vec![EnvFrame {
                idents: idents.into_iter().map(Into::into).collect(),
                rec_bodies: Vec::new(),
            }],
        }
    }

    pub fn with_pushed<T, F>(&mut self, params: &Params, f: F) -> Result<(Vec<Rc<Ast>>, T), Value>
    where
        F: FnOnce(&mut Self) -> Result<T, Value>,
    {
        match params {
            Params::Any(ident) => self.frames.push(EnvFrame::new(vec![ident.clone()])),
            Params::Exact(idents) => self
                .frames
                .push(EnvFrame::new(idents.into_iter().cloned().collect())),
            Params::AtLeast(idents, rest) => {
                let mut idents: Vec<_> = idents.into_iter().cloned().collect();
                idents.push(rest.clone());
                self.frames.push(EnvFrame::new(idents));
            }
        }
        debug!("extended env-stack {:?}", self);
        match f(self) {
            Ok(v) => {
                let bodies = self.last_frame_mut().rec_bodies.split_off(0);
                let bodies = bodies
                    .into_iter()
                    .map(|b| Ok(Rc::new(Ast::expr(&b, self, NonTail)?)))
                    .collect::<Result<_, Value>>()?;
                self.pop();
                debug!("done with extended env-stack {:?} -> {:?}", self, bodies);
                Ok((bodies, v))
            }
            Err(e) => {
                self.pop();
                Err(e)
            }
        }
    }

    fn pop(&mut self) {
        assert!(self.frames.len() > 1);
        self.frames.pop().unwrap();
    }

    pub fn lookup(&self, name: &str) -> Option<EnvIndex> {
        for (level, frame) in self.frames.iter().rev().enumerate() {
            if let Some(i) = frame.idents.iter().position(|ident| ident.as_ref() == name) {
                return Some(EnvIndex(level, i));
            }
        }
        None
    }

    pub fn bind_rec(&mut self, name: &str, body: lexpr::Value) {
        let last = self.frames.len() - 1;
        self.frames[last].idents.push(name.into());
        self.frames[last].rec_bodies.push(body);
        debug!("bound {} recursively -> {:?}", name, self);
    }

    fn last_frame_mut(&mut self) -> &mut EnvFrame {
        let last = self.frames.len() - 1;
        &mut self.frames[last]
    }

    pub fn resolve_rec(&mut self, env: Gc<GcCell<eval::Env>>) -> Result<(), Value> {
        let pos = env
            .borrow_mut()
            .init_rec(self.last_frame_mut().rec_bodies.len());
        let bodies = self.last_frame_mut().rec_bodies.split_off(0);
        for (i, body) in bodies.into_iter().enumerate() {
            let value = eval(Rc::new(Ast::expr(&body, self, NonTail)?), env.clone())?;
            env.borrow_mut().resolve_rec(pos + i, value);
        }
        Ok(())
    }
}

impl Params {
    pub fn new(v: &lexpr::Value) -> Result<Self, SyntaxError> {
        use lexpr::Value::*;
        match v {
            Null => Ok(Params::Exact(vec![])),
            Cons(cell) => match cell.to_ref_vec() {
                (params, Null) => Ok(Params::Exact(param_list(&params)?)),
                (params, rest) => Ok(Params::AtLeast(param_list(&params)?, param_rest(rest)?)),
            },
            _ => Ok(Params::Any(param_rest(v)?)),
        }
    }

    /// Form the values for argument vector
    pub fn values(&self, args: SmallVec<[Value; 8]>) -> Result<SmallVec<[Value; 8]>, Value> {
        match self {
            Params::Any(_) => Ok(smallvec![Value::list(args)]),
            Params::Exact(names) => {
                if names.len() != args.len() {
                    Err(make_error!(
                        "parameter length mismatch; got ({}), expected ({})",
                        ShowSlice(&args),
                        ShowSlice(names)
                    ))
                } else {
                    Ok(args)
                }
            }
            Params::AtLeast(names, _) => {
                if names.len() > args.len() {
                    Err(make_error!(
                        "too few parameters; got ({}), expected ({})",
                        ShowSlice(&args),
                        ShowSlice(names)
                    ))
                } else {
                    let (named, rest) = args.split_at(names.len());
                    let values = named
                        .into_iter()
                        .cloned()
                        .chain(iter::once(Value::list(rest.into_iter().cloned()).into()))
                        .collect();
                    Ok(values)
                }
            }
        }
    }

    pub fn bind(
        &self,
        args: SmallVec<[Value; 8]>,
        parent: Gc<GcCell<eval::Env>>,
    ) -> Result<Gc<GcCell<eval::Env>>, Value> {
        Ok(Gc::new(GcCell::new(eval::Env::new(
            parent,
            self.values(args)?,
        ))))
    }
}

fn param_list(params: &[&lexpr::Value]) -> Result<Vec<Box<str>>, SyntaxError> {
    params
        .into_iter()
        .map(|p| {
            p.as_symbol()
                .ok_or(SyntaxError::ExpectedSymbol)
                .map(Into::into)
        })
        .collect()
}

fn param_rest(rest: &lexpr::Value) -> Result<Box<str>, SyntaxError> {
    rest.as_symbol()
        .ok_or(SyntaxError::ExpectedSymbol)
        .map(Into::into)
}

#[derive(Debug)]
pub enum Ast {
    Datum(lexpr::Value),
    Lambda {
        params: Rc<Params>,
        body: Rc<Ast>,
    },
    If {
        cond: Rc<Ast>,
        consequent: Rc<Ast>,
        alternative: Rc<Ast>,
    },
    LetRec {
        bound_exprs: Vec<Rc<Ast>>,
        exprs: Vec<Rc<Ast>>,
    },
    Apply {
        op: Rc<Ast>,
        operands: Vec<Rc<Ast>>,
    },
    TailCall {
        op: Rc<Ast>,
        operands: Vec<Rc<Ast>>,
    },
    EnvRef(EnvIndex),
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum TailPosition {
    Tail,
    NonTail,
}

use TailPosition::*;

impl Ast {
    pub fn expr(
        expr: &lexpr::Value,
        stack: &mut EnvStack,
        tail: TailPosition,
    ) -> Result<Ast, Value> {
        debug!("forming AST for {} in {:?}", expr, stack);
        match expr {
            lexpr::Value::Null => Err(make_error!("empty application")),
            lexpr::Value::Nil => Err(make_error!("#nil unsupported")),
            lexpr::Value::Char(_) => Err(make_error!("characters are currently unsupported")),
            lexpr::Value::Keyword(_) => Err(make_error!("keywords are currently unsupported")),
            lexpr::Value::Bytes(_) => Err(make_error!("byte vectors are currently unsupported")),
            lexpr::Value::Vector(_) => Err(make_error!("vectors are currently unsupported")),
            lexpr::Value::Bool(_) => Ok(Ast::Datum(expr.clone())),
            lexpr::Value::Number(_) => Ok(Ast::Datum(expr.clone())),
            lexpr::Value::String(_) => Ok(Ast::Datum(expr.clone())),
            lexpr::Value::Symbol(ident) => stack
                .lookup(ident)
                .map(Ast::EnvRef)
                .ok_or_else(|| make_error!("unbound identifier `{}'", ident)),
            lexpr::Value::Cons(cell) => {
                let (first, rest) = cell.as_pair();
                match first.as_symbol() {
                    Some("quote") => {
                        let args = proper_list(rest).map_err(syntax_error)?;
                        if args.len() != 1 {
                            return Err(make_error!("`quote' expects a single form"));
                        }
                        Ok(Ast::Datum(args[0].clone()))
                    }
                    Some("lambda") => {
                        let args = proper_list(rest).map_err(syntax_error)?;
                        if args.len() < 2 {
                            return Err(make_error!("`lambda` expects at least two forms"));
                        }
                        Ast::lambda(args[0], &args[1..], stack)
                    }
                    Some("define") => {
                        return Err(make_error!("`define` not allowed in expression context"));
                    }
                    Some("if") => {
                        let args = proper_list(rest).map_err(syntax_error)?;
                        if args.len() != 3 {
                            return Err(make_error!("`if` expects at exactly three forms"));
                        }
                        Ok(Ast::If {
                            cond: Ast::expr(&args[0], stack, NonTail)?.into(),
                            consequent: Ast::expr(&args[1], stack, tail)?.into(),
                            alternative: Ast::expr(&args[2], stack, tail)?.into(),
                        })
                    }
                    _ => {
                        let arg_exprs = proper_list(rest).map_err(syntax_error)?;
                        let op = Ast::expr(first, stack, NonTail)?.into();
                        let operands = arg_exprs
                            .into_iter()
                            .map(|arg| Ok(Ast::expr(arg, stack, NonTail)?.into()))
                            .collect::<Result<Vec<Rc<Ast>>, Value>>()?;
                        if tail == Tail {
                            Ok(Ast::TailCall { op, operands })
                        } else {
                            Ok(Ast::Apply { op, operands })
                        }
                    }
                }
            }
        }
    }

    pub fn definition(
        expr: &lexpr::Value,
        stack: &mut EnvStack,
        tail: TailPosition,
    ) -> Result<Option<Ast>, Value> {
        // Check for definition, return `Ok(None)` if found
        match expr {
            lexpr::Value::Cons(cell) => {
                let (first, rest) = cell.as_pair();
                match first.as_symbol() {
                    Some("define") => {
                        let args = proper_list(rest).map_err(syntax_error)?;
                        if args.len() < 2 {
                            return Err(make_error!("`define` expects at least two forms"));
                        }
                        match args[0] {
                            lexpr::Value::Symbol(ident) => {
                                if args.len() != 2 {
                                    return Err(make_error!(
                                        "`define` for variable expects one value form"
                                    ));
                                }
                                stack.bind_rec(ident, args[1].clone()); // TODO: clone
                                return Ok(None);
                            }
                            lexpr::Value::Cons(cell) => {
                                let ident = cell.car().as_symbol().ok_or_else(|| {
                                    make_error!("invalid use of `define': non-identifier")
                                })?;
                                let body =
                                    lexpr::Value::list(args[1..].into_iter().map(|e| (*e).clone()));
                                let lambda = lexpr::Value::cons(
                                    sexp!(lambda),
                                    lexpr::Value::cons(cell.cdr().clone(), body),
                                );
                                stack.bind_rec(ident, lambda);
                                return Ok(None);
                            }
                            _ => return Err(make_error!("invalid `define' form")),
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
        // Otherwise, it must be an expression
        Ok(Some(Ast::expr(expr, stack, tail)?))
    }

    fn let_rec(
        params: &Params,
        exprs: &[&lexpr::Value],
        stack: &mut EnvStack,
        tail: TailPosition,
    ) -> Result<Self, Value> {
        let (bound_exprs, body_exprs) = stack.with_pushed(params, |stack| -> Result<_, Value> {
            let mut body_exprs = Vec::with_capacity(exprs.len());
            let mut definitions = true;
            for (i, expr) in exprs.into_iter().enumerate() {
                let tail = if i + 1 == exprs.len() { tail } else { NonTail };
                if definitions {
                    if let Some(ast) = Ast::definition(expr, stack, tail)? {
                        body_exprs.push(Rc::new(ast));
                        definitions = false;
                    }
                } else {
                    body_exprs.push(Rc::new(Ast::expr(expr, stack, tail)?));
                }
            }
            Ok(body_exprs)
        })?;
        Ok(Ast::LetRec {
            bound_exprs,
            exprs: body_exprs,
        })
    }

    fn lambda(
        params: &lexpr::Value,
        body: &[&lexpr::Value],
        stack: &mut EnvStack,
    ) -> Result<Self, Value> {
        let params = Params::new(params).map_err(syntax_error)?;
        let body = Rc::new(Ast::let_rec(&params, body, stack, Tail)?);
        Ok(Ast::Lambda {
            params: Rc::new(params),
            body,
        })
    }
}

#[derive(Debug)]
pub enum SyntaxError {
    ExpectedSymbol,
    ImproperList(Vec<lexpr::Value>, lexpr::Value),
    NonList(lexpr::Value),
}

impl fmt::Display for SyntaxError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            SyntaxError::ExpectedSymbol => write!(f, "expected symbol"),
            SyntaxError::ImproperList(elts, tail) => {
                write!(f, "improper list `({} . {})'", ShowSlice(&elts), tail)
            }
            SyntaxError::NonList(value) => write!(f, "non-list `{}'", value),
        }
    }
}

struct ShowSlice<'a, T>(&'a [T]);

impl<'a, T> fmt::Display for ShowSlice<'a, T>
where
    T: fmt::Display,
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        for (i, value) in self.0.iter().enumerate() {
            value.fmt(f)?;
            if i + 1 < self.0.len() {
                f.write_char(' ')?;
            }
        }
        Ok(())
    }
}

impl std::error::Error for SyntaxError {}

fn proper_list(expr: &lexpr::Value) -> Result<Vec<&lexpr::Value>, SyntaxError> {
    match expr {
        lexpr::Value::Cons(cell) => match cell.to_ref_vec() {
            (args, tail) => {
                if tail != &lexpr::Value::Null {
                    Err(SyntaxError::ImproperList(
                        args.into_iter().cloned().collect(),
                        tail.clone(),
                    ))
                } else {
                    Ok(args)
                }
            }
        },
        lexpr::Value::Null => Ok(Vec::new()),
        value => Err(SyntaxError::NonList(value.clone())),
    }
}

fn syntax_error(e: SyntaxError) -> Value {
    make_error!("syntax error: {}", e)
}