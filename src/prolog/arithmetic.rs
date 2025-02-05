use prolog_parser::ast::*;

use prolog::clause_types::*;
use prolog::fixtures::*;
use prolog::forms::*;
use prolog::instructions::*;
use prolog::iterators::*;

use prolog::machine::machine_errors::*;
use prolog::machine::machine_indices::*;

use prolog::ordered_float::*;
use prolog::rug::{Assign, Integer, Rational};
use prolog::rug::ops::PowAssign;

use std::cell::Cell;
use std::cmp::{Ordering, min, max};
use std::f64;
use std::num::FpCategory;
use std::ops::{Add, Sub, Div, Mul, Neg};
use std::rc::Rc;
use std::vec::Vec;

pub struct ArithInstructionIterator<'a> {
    state_stack: Vec<TermIterState<'a>>
}

pub type ArithCont = (Code, Option<ArithmeticTerm>);

impl<'a> ArithInstructionIterator<'a> {
    fn push_subterm(&mut self, lvl: Level, term: &'a Term) {
        self.state_stack.push(TermIterState::subterm_to_state(lvl, term));
    }

    fn new(term: &'a Term) -> Result<Self, ArithmeticError> {
        let state = match term {
            &Term::AnonVar =>
                return Err(ArithmeticError::UninstantiatedVar),
            &Term::Clause(ref cell, ref name, ref terms, ref fixity) =>
                match ClauseType::from(name.clone(), terms.len(), fixity.clone()) {
                    ct @ ClauseType::Named(..) | ct @ ClauseType::Op(..) =>
                        Ok(TermIterState::Clause(Level::Shallow, 0, cell, ct, terms)),
                    ClauseType::Inlined(InlinedClauseType::IsFloat(_)) => {
                        let ct = ClauseType::Named(clause_name!("float"), 1, CodeIndex::default());
                        Ok(TermIterState::Clause(Level::Shallow, 0, cell, ct, terms))
                    },
                    _ => Err(ArithmeticError::NonEvaluableFunctor(Constant::Atom(name.clone(),
                                                                                 fixity.clone()),
                                                                  terms.len()))
                }?,
            &Term::Constant(ref cell, ref cons) =>
                TermIterState::Constant(Level::Shallow, cell, cons),
            &Term::Cons(_, _, _) =>
                return Err(ArithmeticError::NonEvaluableFunctor(atom!("'.'"), 2)),
            &Term::Var(ref cell, ref var) =>
                TermIterState::Var(Level::Shallow, cell, var.clone())
        };

        Ok(ArithInstructionIterator { state_stack: vec![state] })
    }
}

pub enum ArithTermRef<'a> {
    Constant(&'a Constant),
    Op(ClauseName, usize), // name, arity.
    Var(&'a Cell<VarReg>, Rc<Var>)
}

impl<'a> Iterator for ArithInstructionIterator<'a> {
    type Item = Result<ArithTermRef<'a>, ArithmeticError>;

    fn next(&mut self) -> Option<Self::Item> {
        while let Some(iter_state) = self.state_stack.pop() {
            match iter_state {
                TermIterState::AnonVar(_) =>
                    return Some(Err(ArithmeticError::UninstantiatedVar)),
                TermIterState::Clause(lvl, child_num, cell, ct, subterms) => {
                    let arity = subterms.len();

                    if child_num == arity {
                        return Some(Ok(ArithTermRef::Op(ct.name(), arity)));
                    } else {
                        self.state_stack.push(TermIterState::Clause(lvl, child_num + 1, cell, ct, subterms));
                        self.push_subterm(lvl, subterms[child_num].as_ref());
                    }
                },
                TermIterState::Constant(_, _, c) =>
                    return Some(Ok(ArithTermRef::Constant(c))),
                TermIterState::Var(_, cell, var) =>
                    return Some(Ok(ArithTermRef::Var(cell, var.clone()))),
                _ =>
                    return Some(Err(ArithmeticError::NonEvaluableFunctor(atom!("'.'"), 2)))
            };
        }

        None
    }
}

pub struct ArithmeticEvaluator<'a> {
    bindings: &'a AllocVarDict,
    interm: Vec<ArithmeticTerm>,
    interm_c: usize
}

pub trait ArithmeticTermIter<'a> {
    type Iter : Iterator<Item=Result<ArithTermRef<'a>, ArithmeticError>>;

    fn iter(self) -> Result<Self::Iter, ArithmeticError>;
}

impl<'a> ArithmeticTermIter<'a> for &'a Term {
    type Iter = ArithInstructionIterator<'a>;

    fn iter(self) -> Result<Self::Iter, ArithmeticError> {
        ArithInstructionIterator::new(self)
    }
}

impl<'a> ArithmeticEvaluator<'a>
{
    pub fn new(bindings: &'a AllocVarDict, target_int: usize) -> Self {
        ArithmeticEvaluator { bindings, interm: Vec::new(), interm_c: target_int }
    }

    fn get_unary_instr(name: ClauseName, a1: ArithmeticTerm, t: usize)
                       -> Result<ArithmeticInstruction, ArithmeticError>
    {
        match name.as_str() {
            "abs" => Ok(ArithmeticInstruction::Abs(a1, t)),
            "-" => Ok(ArithmeticInstruction::Neg(a1, t)),
            "+" => Ok(ArithmeticInstruction::Plus(a1, t)),
            "cos" => Ok(ArithmeticInstruction::Cos(a1, t)),
            "sin" => Ok(ArithmeticInstruction::Sin(a1, t)),
            "tan" => Ok(ArithmeticInstruction::Tan(a1, t)),
            "log" => Ok(ArithmeticInstruction::Log(a1, t)),
            "exp" => Ok(ArithmeticInstruction::Exp(a1, t)),
            "sqrt" => Ok(ArithmeticInstruction::Sqrt(a1, t)),
            "acos" => Ok(ArithmeticInstruction::ACos(a1, t)),
            "asin" => Ok(ArithmeticInstruction::ASin(a1, t)),
            "atan" => Ok(ArithmeticInstruction::ATan(a1, t)),
            "float" => Ok(ArithmeticInstruction::Float(a1, t)),
            "truncate" => Ok(ArithmeticInstruction::Truncate(a1, t)),
            "round" => Ok(ArithmeticInstruction::Round(a1, t)),
            "ceiling" => Ok(ArithmeticInstruction::Ceiling(a1, t)),
            "floor" => Ok(ArithmeticInstruction::Floor(a1, t)),
            "\\" => Ok(ArithmeticInstruction::BitwiseComplement(a1, t)),
             _  => Err(ArithmeticError::NonEvaluableFunctor(Constant::Atom(name, None), 1))
        }
    }

    fn get_binary_instr(name: ClauseName, a1: ArithmeticTerm, a2: ArithmeticTerm, t: usize)
                        -> Result<ArithmeticInstruction, ArithmeticError>
    {
        match name.as_str() {
            "+"    => Ok(ArithmeticInstruction::Add(a1, a2, t)),
            "-"    => Ok(ArithmeticInstruction::Sub(a1, a2, t)),
            "/"    => Ok(ArithmeticInstruction::Div(a1, a2, t)),
            "//"   => Ok(ArithmeticInstruction::IDiv(a1, a2, t)),
            "max"  => Ok(ArithmeticInstruction::Max(a1, a2, t)),
            "min"  => Ok(ArithmeticInstruction::Min(a1, a2, t)),
            "div"  => Ok(ArithmeticInstruction::IntFloorDiv(a1, a2, t)),
            "rdiv" => Ok(ArithmeticInstruction::RDiv(a1, a2, t)),
            "*"    => Ok(ArithmeticInstruction::Mul(a1, a2, t)),
            "**"   => Ok(ArithmeticInstruction::Pow(a1, a2, t)),
            "^"    => Ok(ArithmeticInstruction::IntPow(a1, a2, t)),
            ">>"   => Ok(ArithmeticInstruction::Shr(a1, a2, t)),
            "<<"   => Ok(ArithmeticInstruction::Shl(a1, a2, t)),
            "/\\"  => Ok(ArithmeticInstruction::And(a1, a2, t)),
            "\\/"  => Ok(ArithmeticInstruction::Or(a1, a2, t)),
            "xor"  => Ok(ArithmeticInstruction::Xor(a1, a2, t)),
            "mod"  => Ok(ArithmeticInstruction::Mod(a1, a2, t)),
            "rem"  => Ok(ArithmeticInstruction::Rem(a1, a2, t)),
            "atan2" => Ok(ArithmeticInstruction::ATan2(a1, a2, t)),
             _     => Err(ArithmeticError::NonEvaluableFunctor(Constant::Atom(name, None), 2))
        }
    }

    fn incr_interm(&mut self) -> usize {
        let temp = self.interm_c;

        self.interm.push(ArithmeticTerm::Interm(temp));
        self.interm_c += 1;

        temp
    }

    fn instr_from_clause(&mut self, name: ClauseName, arity: usize)
                         -> Result<ArithmeticInstruction, ArithmeticError>
    {
        match arity {
            1 => {
                let a1 = self.interm.pop().unwrap();

                let ninterm = if a1.interm_or(0) == 0 {
                    self.incr_interm()
                } else {
                    self.interm.push(a1.clone());
                    a1.interm_or(0)
                };

                Self::get_unary_instr(name, a1, ninterm)
            },
            2 => {
                let a2 = self.interm.pop().unwrap();
                let a1 = self.interm.pop().unwrap();

                let min_interm = min(a1.interm_or(0), a2.interm_or(0));

                let ninterm = if min_interm == 0 {
                    let max_interm = max(a1.interm_or(0), a2.interm_or(0));

                    if max_interm == 0 {
                        self.incr_interm()
                    } else {
                        self.interm.push(ArithmeticTerm::Interm(max_interm));
                        self.interm_c = max_interm + 1;
                        max_interm
                    }
                } else {
                    self.interm.push(ArithmeticTerm::Interm(min_interm));
                    self.interm_c = min_interm + 1;
                    min_interm
                };

                Self::get_binary_instr(name, a1, a2, ninterm)
            },
            _ => Err(ArithmeticError::NonEvaluableFunctor(Constant::Atom(name, None), arity))
        }
    }

    fn push_constant(&mut self, c: &Constant) -> Result<(), ArithmeticError> {
        match c {
            &Constant::Integer(ref n) =>
                self.interm.push(ArithmeticTerm::Number(Number::Integer(n.clone()))),
            &Constant::Float(ref n) =>
                self.interm.push(ArithmeticTerm::Number(Number::Float(n.clone()))),
            &Constant::Rational(ref n) =>
                self.interm.push(ArithmeticTerm::Number(Number::Rational(n.clone()))),
            &Constant::Atom(ref name, _) if name.as_str() == "pi" =>
                self.interm.push(ArithmeticTerm::Number(Number::Float(OrderedFloat(f64::consts::PI)))),
            _ =>
                return Err(ArithmeticError::NonEvaluableFunctor(c.clone(), 0))
        }

        Ok(())
    }

    pub fn eval<Iter>(&mut self, src: Iter) -> Result<ArithCont, ArithmeticError>
        where Iter: ArithmeticTermIter<'a>
    {
        let mut code = vec![];

        for term_ref in src.iter()?
        {
            match term_ref? {
                ArithTermRef::Constant(c) => self.push_constant(c)?,
                ArithTermRef::Var(cell, name) => {
                    let r = if cell.get().norm().reg_num() == 0 {
                        match self.bindings.get(&name) {
                            Some(&VarData::Temp(_, t, _)) if t != 0 => RegType::Temp(t),
                            Some(&VarData::Perm(p)) if p != 0 => RegType::Perm(p),
                            _ => return Err(ArithmeticError::UninstantiatedVar)
                        }
                    } else {
                        cell.get().norm()
                    };

                    self.interm.push(ArithmeticTerm::Reg(r));
                },
                ArithTermRef::Op(name, arity) => {
                    code.push(Line::Arithmetic(self.instr_from_clause(name, arity)?));
                }
            }
        }

        Ok((code, self.interm.pop()))
    }
}

// integer division rounding function -- 9.1.3.1.
pub fn rnd_i<'a>(n: &'a Number) -> RefOrOwned<'a, Integer> {
    match n {
        &Number::Integer(ref n) =>
            RefOrOwned::Borrowed(n),
        &Number::Float(OrderedFloat(f)) =>
            RefOrOwned::Owned(Integer::from_f64(f.floor()).unwrap_or_else(|| Integer::from(0))),
        &Number::Rational(ref r) => {
            let r_ref = r.fract_floor_ref();
            let (mut fract, mut floor) = (Rational::new(), Integer::new());

            (&mut fract, &mut floor).assign(r_ref);
            RefOrOwned::Owned(floor)
        }
    }
}

// floating point rounding function -- 9.1.4.1.
pub fn rnd_f(n: &Number) -> f64 {
    match n {
        &Number::Integer(ref n) => n.to_f64(),
        &Number::Float(OrderedFloat(f)) => f,
        &Number::Rational(ref r) => r.to_f64()
    }
}

// floating point result function -- 9.1.4.2.
pub fn result_f<Round>(n: &Number, round: Round) -> Result<f64, EvalError>
  where Round: Fn(&Number) -> f64
{
    let f = rnd_f(n);
    classify_float(f, round)
}

fn classify_float<Round>(f: f64, round: Round) -> Result<f64, EvalError>
  where Round: Fn(&Number) -> f64
{
    match f.classify() {
        FpCategory::Normal | FpCategory::Zero =>
            Ok(round(&Number::Float(OrderedFloat(f)))),
        FpCategory::Infinite => {
            let f = round(&Number::Float(OrderedFloat(f)));

            if OrderedFloat(f) == OrderedFloat(f64::MAX) {
                Ok(f)
            } else {
                Err(EvalError::FloatOverflow)
            }
        },
        FpCategory::Nan => Err(EvalError::Undefined),
        _ => Ok(round(&Number::Float(OrderedFloat(f))))
    }
}

fn float_i_to_f(n: &Integer) -> Result<f64, EvalError> {
    classify_float(n.to_f64(), rnd_f)
}

fn float_r_to_f(r: &Rational) -> Result<f64, EvalError> {
    classify_float(r.to_f64(), rnd_f)
}

fn add_f(f1: f64, f2: f64) -> Result<OrderedFloat<f64>, EvalError> {
    Ok(OrderedFloat(classify_float(f1 + f2, rnd_f)?))
}

fn mul_f(f1: f64, f2: f64) -> Result<OrderedFloat<f64>, EvalError> {
    Ok(OrderedFloat(classify_float(f1 * f2, rnd_f)?))
}

fn div_f(f1: f64, f2: f64) -> Result<OrderedFloat<f64>, EvalError> {
    if FpCategory::Zero == f2.classify() {
        Err(EvalError::ZeroDivisor)
    } else {
        Ok(OrderedFloat(classify_float(f1 / f2, rnd_f)?))
    }
}

impl Add<Number> for Number {
    type Output = Result<Number, EvalError>;

    fn add(self, rhs: Number) -> Self::Output {
        match (self, rhs) {
            (Number::Integer(n1), Number::Integer(n2)) =>
                Ok(Number::Integer(n1 + n2)), // add_i
            (Number::Integer(n1), Number::Float(OrderedFloat(n2)))
          | (Number::Float(OrderedFloat(n2)), Number::Integer(n1)) =>
                Ok(Number::Float(add_f(float_i_to_f(&n1)?, n2)?)),
            (Number::Integer(n1), Number::Rational(n2))
          | (Number::Rational(n2), Number::Integer(n1)) =>
                Ok(Number::Rational(Rational::from(n1) + n2)),
            (Number::Rational(n1), Number::Float(OrderedFloat(n2)))
          | (Number::Float(OrderedFloat(n2)), Number::Rational(n1)) =>
                Ok(Number::Float(add_f(float_r_to_f(&n1)?, n2)?)),
            (Number::Float(OrderedFloat(f1)), Number::Float(OrderedFloat(f2))) =>
                Ok(Number::Float(add_f(f1, f2)?)),
            (Number::Rational(r1), Number::Rational(r2)) =>
                Ok(Number::Rational(r1 + r2))
        }
    }
}

impl Neg for Number {
    type Output = Number;

    fn neg(self) -> Self::Output {
        match self {
            Number::Integer(n) => Number::Integer(-n),
            Number::Float(OrderedFloat(f)) => Number::Float(OrderedFloat(-f)),
            Number::Rational(r) => Number::Rational(-r)
        }
    }
}

impl Sub<Number> for Number {
    type Output = Result<Number, EvalError>;

    fn sub(self, rhs: Number) -> Self::Output {
        self.add(-rhs)
    }
}

impl Mul<Number> for Number {
    type Output = Result<Number, EvalError>;

    fn mul(self, rhs: Number) -> Self::Output {
        match (self, rhs) {
            (Number::Integer(n1), Number::Integer(n2)) =>
                Ok(Number::Integer(n1 * n2)), // mul_i
            (Number::Integer(n1), Number::Float(OrderedFloat(n2)))
          | (Number::Float(OrderedFloat(n2)), Number::Integer(n1)) =>
                Ok(Number::Float(mul_f(float_i_to_f(&n1)?, n2)?)),
            (Number::Integer(n1), Number::Rational(n2))
          | (Number::Rational(n2), Number::Integer(n1)) =>
                Ok(Number::Rational(Rational::from(n1) * n2)),
            (Number::Rational(n1), Number::Float(OrderedFloat(n2)))
          | (Number::Float(OrderedFloat(n2)), Number::Rational(n1)) =>
                Ok(Number::Float(mul_f(float_r_to_f(&n1)?, n2)?)),
            (Number::Float(OrderedFloat(f1)), Number::Float(OrderedFloat(f2))) =>
                Ok(Number::Float(mul_f(f1, f2)?)),
            (Number::Rational(r1), Number::Rational(r2)) =>
                Ok(Number::Rational(r1 * r2))
        }
    }
}

impl Div<Number> for Number {
    type Output = Result<Number, EvalError>;

    fn div(self, rhs: Number) -> Self::Output {
        match (self, rhs) {
            (Number::Integer(n1), Number::Integer(n2)) =>
                Ok(Number::Float(div_f(float_i_to_f(&n1)?, float_i_to_f(&n2)?)?)),
            (Number::Integer(n1), Number::Float(OrderedFloat(n2))) =>
                Ok(Number::Float(div_f(float_i_to_f(&n1)?, n2)?)),
            (Number::Float(OrderedFloat(n2)), Number::Integer(n1)) =>
                Ok(Number::Float(div_f(n2, float_i_to_f(&n1)?)?)),
            (Number::Integer(n1), Number::Rational(n2)) =>
                Ok(Number::Float(div_f(float_i_to_f(&n1)?, float_r_to_f(&n2)?)?)),
            (Number::Rational(n2), Number::Integer(n1)) =>
                Ok(Number::Float(div_f(float_r_to_f(&n2)?, float_i_to_f(&n1)?)?)),
            (Number::Rational(n1), Number::Float(OrderedFloat(n2))) =>
                Ok(Number::Float(div_f(float_r_to_f(&n1)?, n2)?)),
            (Number::Float(OrderedFloat(n2)), Number::Rational(n1)) =>
                Ok(Number::Float(div_f(n2, float_r_to_f(&n1)?)?)),
            (Number::Float(OrderedFloat(f1)), Number::Float(OrderedFloat(f2))) =>
                Ok(Number::Float(div_f(f1, f2)?)),
            (Number::Rational(r1), Number::Rational(r2)) =>
                Ok(Number::Float(div_f(float_r_to_f(&r1)?, float_r_to_f(&r2)?)?))
        }
    }
}

impl PartialOrd for Number {
    fn partial_cmp(&self, rhs: &Number) -> Option<Ordering> {
        match (self, rhs) {
            (&Number::Integer(ref n1), &Number::Integer(ref n2)) =>
                Some(n1.cmp(n2)),
            (&Number::Integer(_), Number::Float(_)) =>
                Some(Ordering::Greater),
            (&Number::Float(_), &Number::Integer(_)) =>
                Some(Ordering::Less),
            (&Number::Integer(_), &Number::Rational(_)) =>
                Some(Ordering::Greater),
            (&Number::Rational(_), &Number::Integer(_)) =>
                Some(Ordering::Less),
            (&Number::Rational(_), Number::Float(_)) =>
                Some(Ordering::Greater),
            (&Number::Float(_), &Number::Rational(_)) =>
                Some(Ordering::Less),
            (&Number::Float(f1), &Number::Float(f2)) =>
                Some(f1.cmp(&f2)),
            (&Number::Rational(ref r1), &Number::Rational(ref r2)) =>
                Some(r1.cmp(&r2))
        }
    }
}

impl Ord for Number {
    fn cmp(&self, rhs: &Number) -> Ordering {
        match (self, rhs) {
            (&Number::Integer(ref n1), &Number::Integer(ref n2)) =>
                n1.cmp(n2),
            (&Number::Integer(_), Number::Float(_)) =>
                Ordering::Greater,
            (&Number::Float(_), &Number::Integer(_)) =>
                Ordering::Less,
            (&Number::Integer(_), &Number::Rational(_)) =>
                Ordering::Greater,
            (&Number::Rational(_), &Number::Integer(_)) =>
                Ordering::Less,
            (&Number::Rational(_), Number::Float(_)) =>
                Ordering::Greater,
            (&Number::Float(_), &Number::Rational(_)) =>
                Ordering::Less,
            (&Number::Float(f1), &Number::Float(f2)) =>
                f1.cmp(&f2),
            (&Number::Rational(ref r1), &Number::Rational(ref r2)) =>
                r1.cmp(&r2)
        }
    }
}

// Computes n ^ power. Ignores the sign of power.
pub fn binary_pow(mut n: Integer, power: Integer) -> Integer
{
    let mut power = power.abs();

    if power == 0 {
        return Integer::from(1);
    }

    let mut oddand = Integer::from(1);

    while power > 1 {
        if power.is_odd() {
            oddand *= &n;
        }

        n.pow_assign(2);
        power >>= 1;
    }

    n * oddand
}
