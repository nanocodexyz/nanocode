use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    fmt::Write as _,
    rc::Rc,
    sync::atomic::{AtomicU64, Ordering},
};

pub const KIWI_VERSION: &str = "1.5.1";

const NEAR_ZERO_EPS: f64 = 1.0e-8;
const STRENGTH_MAX: f64 = 1000.0;
const STRONG_SCALE: f64 = 1_000_000.0;
const MEDIUM_SCALE: f64 = 1000.0;
const STREAM_PRECISION: i32 = 6;

pub const REQUIRED: f64 = create(STRENGTH_MAX, STRENGTH_MAX, STRENGTH_MAX, 1.0);
pub const STRONG: f64 = create(1.0, 0.0, 0.0, 1.0);
pub const MEDIUM: f64 = create(0.0, 1.0, 0.0, 1.0);
pub const WEAK: f64 = create(0.0, 0.0, 1.0, 1.0);

static CREATED: AtomicU64 = AtomicU64::new(0);

fn identity() -> u64 {
    CREATED.fetch_add(1, Ordering::Relaxed)
}

const fn min(a: f64, b: f64) -> f64 {
    if b < a { b } else { a }
}

const fn max(a: f64, b: f64) -> f64 {
    if a < b { b } else { a }
}

pub const fn create(a: f64, b: f64, c: f64, w: f64) -> f64 {
    let mut result = 0.0;
    result += max(0.0, min(STRENGTH_MAX, a * w)) * STRONG_SCALE;
    result += max(0.0, min(STRENGTH_MAX, b * w)) * MEDIUM_SCALE;
    result += max(0.0, min(STRENGTH_MAX, c * w));
    result
}

pub const fn clip(value: f64) -> f64 {
    max(0.0, min(REQUIRED, value))
}

pub fn near_zero(value: f64) -> bool {
    if value < 0.0 {
        -value < NEAR_ZERO_EPS
    } else {
        value < NEAR_ZERO_EPS
    }
}

pub fn general(value: f64) -> String {
    if value.is_nan() {
        return if value.is_sign_negative() {
            "-nan"
        } else {
            "nan"
        }
        .to_owned();
    }
    if value.is_infinite() {
        return if value < 0.0 { "-inf" } else { "inf" }.to_owned();
    }
    if value == 0.0 {
        return if value.is_sign_negative() { "-0" } else { "0" }.to_owned();
    }
    let scientific = format!("{:.*e}", (STREAM_PRECISION - 1) as usize, value);
    let (mantissa, exponent) = scientific
        .split_once('e')
        .expect("an exponent format has an exponent");
    let exponent: i32 = exponent.parse().expect("an exponent is an integer");
    if !(-4..STREAM_PRECISION).contains(&exponent) {
        let sign = if exponent < 0 { '-' } else { '+' };
        format!("{}e{sign}{:02}", trim(mantissa), exponent.unsigned_abs())
    } else {
        let fixed = format!("{:.*}", (STREAM_PRECISION - 1 - exponent) as usize, value);
        trim(&fixed).to_owned()
    }
}

fn trim(digits: &str) -> &str {
    if digits.contains('.') {
        digits.trim_end_matches('0').trim_end_matches('.')
    } else {
        digits
    }
}

#[derive(Debug)]
pub struct VariableData {
    identity: u64,
    name: RefCell<String>,
    value: Cell<f64>,
}

pub type Variable = Rc<VariableData>;

pub fn variable(name: String) -> Variable {
    Rc::new(VariableData {
        identity: identity(),
        name: RefCell::new(name),
        value: Cell::new(0.0),
    })
}

impl VariableData {
    pub fn name(&self) -> String {
        self.name.borrow().clone()
    }

    pub fn set_name(&self, name: String) {
        *self.name.borrow_mut() = name;
    }

    pub const fn value(&self) -> f64 {
        self.value.get()
    }

    pub const fn identity(&self) -> u64 {
        self.identity
    }
}

#[derive(Clone, Debug)]
pub struct Term {
    pub variable: Variable,
    pub coefficient: f64,
}

#[derive(Clone, Debug)]
pub struct Expression {
    pub terms: Vec<Term>,
    pub constant: f64,
}

impl Expression {
    fn value(&self) -> f64 {
        let mut result = self.constant;
        for term in &self.terms {
            result += term.coefficient * term.variable.value();
        }
        result
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RelationalOperator {
    Le,
    Ge,
    Eq,
}

#[derive(Debug)]
pub struct ConstraintData {
    identity: u64,
    expression: Expression,
    strength: f64,
    op: RelationalOperator,
}

pub type Constraint = Rc<ConstraintData>;

pub fn constraint(expression: &Expression, op: RelationalOperator, strength: f64) -> Constraint {
    let mut vars: BTreeMap<u64, (Variable, f64)> = BTreeMap::new();
    for term in &expression.terms {
        vars.entry(term.variable.identity)
            .or_insert_with(|| (term.variable.clone(), 0.0))
            .1 += term.coefficient;
    }
    let terms = vars
        .into_values()
        .map(|(variable, coefficient)| Term {
            variable,
            coefficient,
        })
        .collect();
    Rc::new(ConstraintData {
        identity: identity(),
        expression: Expression {
            terms,
            constant: expression.constant,
        },
        strength: clip(strength),
        op,
    })
}

pub fn restrengthen(other: &ConstraintData, strength: f64) -> Constraint {
    Rc::new(ConstraintData {
        identity: identity(),
        expression: other.expression.clone(),
        strength: clip(strength),
        op: other.op,
    })
}

impl ConstraintData {
    pub const fn op(&self) -> RelationalOperator {
        self.op
    }

    pub const fn strength(&self) -> f64 {
        self.strength
    }

    pub fn violated(&self) -> bool {
        match self.op {
            RelationalOperator::Eq => !near_zero(self.expression.value()),
            RelationalOperator::Ge => self.expression.value() < 0.0,
            RelationalOperator::Le => self.expression.value() > 0.0,
        }
    }
}

#[derive(Clone, Debug)]
struct AssocVector<K, V> {
    entries: Vec<(K, V)>,
}

impl<K: Ord + Copy, V> AssocVector<K, V> {
    const fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    fn search(&self, key: &K) -> Result<usize, usize> {
        self.entries.binary_search_by(|(entry, _)| entry.cmp(key))
    }

    fn find(&self, key: &K) -> Option<usize> {
        self.search(key).ok()
    }

    fn get(&self, key: &K) -> Option<&V> {
        self.find(key).map(|index| &self.entries[index].1)
    }

    fn get_mut(&mut self, key: &K) -> Option<&mut V> {
        self.find(key).map(|index| &mut self.entries[index].1)
    }

    fn insert(&mut self, key: K, value: V) {
        match self.search(&key) {
            Ok(index) => self.entries[index].1 = value,
            Err(index) => self.entries.insert(index, (key, value)),
        }
    }

    fn remove_at(&mut self, index: usize) -> (K, V) {
        self.entries.remove(index)
    }

    fn remove(&mut self, key: &K) {
        if let Some(index) = self.find(key) {
            self.entries.remove(index);
        }
    }

    fn clear(&mut self) {
        self.entries.clear();
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    External,
    Slack,
    Error,
    Dummy,
}

#[derive(Clone, Copy, Debug)]
struct Symbol {
    id: u64,
    kind: Kind,
}

impl PartialEq for Symbol {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl Eq for Symbol {}

impl PartialOrd for Symbol {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Symbol {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.id.cmp(&other.id)
    }
}

impl Symbol {
    const fn pivotable(self) -> bool {
        matches!(self.kind, Kind::Slack | Kind::Error)
    }

    fn dump(self, out: &mut String) {
        let letter = match self.kind {
            Kind::External => 'v',
            Kind::Slack => 's',
            Kind::Error => 'e',
            Kind::Dummy => 'd',
        };
        let _ = write!(out, "{letter}{}", self.id);
    }
}

#[derive(Clone, Debug)]
struct Row {
    cells: AssocVector<Symbol, f64>,
    constant: f64,
}

impl Row {
    const fn new(constant: f64) -> Self {
        Self {
            cells: AssocVector::new(),
            constant,
        }
    }

    fn add(&mut self, value: f64) -> f64 {
        self.constant += value;
        self.constant
    }

    fn insert_symbol(&mut self, symbol: Symbol, coefficient: f64) {
        let index = match self.cells.search(&symbol) {
            Ok(index) => index,
            Err(index) => {
                self.cells.entries.insert(index, (symbol, 0.0));
                index
            }
        };
        let cell = &mut self.cells.entries[index].1;
        *cell += coefficient;
        if near_zero(*cell) {
            self.cells.remove_at(index);
        }
    }

    fn insert_row(&mut self, other: &Self, coefficient: f64) {
        self.constant += other.constant * coefficient;
        let mine = std::mem::take(&mut self.cells.entries);
        let mut merged = Vec::with_capacity(mine.len() + other.cells.entries.len());
        let mut mine = mine.into_iter().peekable();
        for &(symbol, value) in &other.cells.entries {
            let coeff = value * coefficient;
            while let Some(entry) = mine.next_if(|(entry, _)| *entry < symbol) {
                merged.push(entry);
            }
            let current = mine
                .next_if(|(entry, _)| *entry == symbol)
                .map_or(0.0, |(_, current)| current);
            let sum = current + coeff;
            if !near_zero(sum) {
                merged.push((symbol, sum));
            }
        }
        merged.extend(mine);
        self.cells.entries = merged;
    }

    fn remove(&mut self, symbol: Symbol) {
        self.cells.remove(&symbol);
    }

    fn reverse_sign(&mut self) {
        self.constant = -self.constant;
        for (_, cell) in &mut self.cells.entries {
            *cell = -*cell;
        }
    }

    fn solve_for(&mut self, symbol: Symbol) {
        let index = self
            .cells
            .find(&symbol)
            .expect("a row is solved for a symbol it holds");
        let coeff = -1.0 / self.cells.remove_at(index).1;
        self.constant *= coeff;
        for (_, cell) in &mut self.cells.entries {
            *cell *= coeff;
        }
    }

    fn solve_for_pair(&mut self, lhs: Symbol, rhs: Symbol) {
        self.insert_symbol(lhs, -1.0);
        self.solve_for(rhs);
    }

    fn coefficient_for(&self, symbol: Symbol) -> f64 {
        self.cells.get(&symbol).copied().unwrap_or(0.0)
    }

    fn substitute(&mut self, symbol: Symbol, row: &Self) {
        if let Some(index) = self.cells.find(&symbol) {
            let coefficient = self.cells.remove_at(index).1;
            self.insert_row(row, coefficient);
        }
    }

    fn dump(&self, out: &mut String) {
        for &(symbol, coefficient) in &self.cells.entries {
            let _ = write!(out, " + {} * ", general(coefficient));
            symbol.dump(out);
        }
        out.push('\n');
    }
}

#[derive(Clone, Copy, Debug)]
struct Tag {
    marker: Symbol,
    other: Option<Symbol>,
}

#[derive(Debug)]
struct EditInfo {
    variable: Variable,
    tag: Tag,
    constraint: Constraint,
    constant: f64,
}

#[derive(Debug)]
pub enum Exception {
    DuplicateConstraint,
    UnsatisfiableConstraint,
    UnknownConstraint,
    DuplicateEditVariable,
    UnknownEditVariable,
    BadRequiredStrength,
    InternalSolverError(&'static str),
}

#[derive(Clone, Copy)]
enum Objective {
    Objective,
    Artificial,
}

#[derive(Debug)]
pub struct Solver {
    cns: AssocVector<u64, (Constraint, Tag)>,
    rows: AssocVector<Symbol, Row>,
    vars: AssocVector<u64, (Variable, Symbol)>,
    edits: AssocVector<u64, EditInfo>,
    infeasible_rows: Vec<Symbol>,
    objective: Row,
    artificial: Option<Row>,
    id_tick: u64,
}

impl Default for Solver {
    fn default() -> Self {
        Self {
            cns: AssocVector::new(),
            rows: AssocVector::new(),
            vars: AssocVector::new(),
            edits: AssocVector::new(),
            infeasible_rows: Vec::new(),
            objective: Row::new(0.0),
            artificial: None,
            id_tick: 1,
        }
    }
}

impl Solver {
    pub fn add_constraint(&mut self, constraint: &Constraint) -> Result<(), Exception> {
        if self.cns.find(&constraint.identity).is_some() {
            return Err(Exception::DuplicateConstraint);
        }
        let (mut row, tag) = self.create_row(constraint);
        let mut subject = Self::choose_subject(&row, &tag);
        if subject.is_none() && Self::all_dummies(&row) {
            if !near_zero(row.constant) {
                return Err(Exception::UnsatisfiableConstraint);
            }
            subject = Some(tag.marker);
        }
        match subject {
            None => {
                if !self.add_with_artificial_variable(&row)? {
                    return Err(Exception::UnsatisfiableConstraint);
                }
            }
            Some(subject) => {
                row.solve_for(subject);
                self.substitute(subject, &row);
                self.rows.insert(subject, row);
            }
        }
        self.cns
            .insert(constraint.identity, (constraint.clone(), tag));
        self.optimize(Objective::Objective)
    }

    pub fn remove_constraint(&mut self, constraint: &Constraint) -> Result<(), Exception> {
        let index = self
            .cns
            .find(&constraint.identity)
            .ok_or(Exception::UnknownConstraint)?;
        let (_, (_, tag)) = self.cns.remove_at(index);
        self.remove_constraint_effects(constraint, &tag);
        if let Some(index) = self.rows.find(&tag.marker) {
            self.rows.remove_at(index);
        } else {
            let index = self
                .marker_leaving_row(tag.marker)
                .ok_or(Exception::InternalSolverError("failed to find leaving row"))?;
            let (leaving, mut row) = self.rows.remove_at(index);
            row.solve_for_pair(leaving, tag.marker);
            self.substitute(tag.marker, &row);
        }
        self.optimize(Objective::Objective)
    }

    pub fn has_constraint(&self, constraint: &Constraint) -> bool {
        self.cns.find(&constraint.identity).is_some()
    }

    pub fn add_edit_variable(
        &mut self,
        variable: &Variable,
        strength: f64,
    ) -> Result<(), Exception> {
        if self.edits.find(&variable.identity).is_some() {
            return Err(Exception::DuplicateEditVariable);
        }
        let strength = clip(strength);
        if strength == REQUIRED {
            return Err(Exception::BadRequiredStrength);
        }
        let expression = Expression {
            terms: vec![Term {
                variable: variable.clone(),
                coefficient: 1.0,
            }],
            constant: 0.0,
        };
        let cn = constraint(&expression, RelationalOperator::Eq, strength);
        self.add_constraint(&cn)?;
        let tag = self
            .cns
            .get(&cn.identity)
            .expect("an added constraint is in the solver")
            .1;
        self.edits.insert(
            variable.identity,
            EditInfo {
                variable: variable.clone(),
                tag,
                constraint: cn,
                constant: 0.0,
            },
        );
        Ok(())
    }

    pub fn remove_edit_variable(&mut self, variable: &Variable) -> Result<(), Exception> {
        let constraint = self
            .edits
            .get(&variable.identity)
            .ok_or(Exception::UnknownEditVariable)?
            .constraint
            .clone();
        self.remove_constraint(&constraint)?;
        self.edits.remove(&variable.identity);
        Ok(())
    }

    pub fn has_edit_variable(&self, variable: &Variable) -> bool {
        self.edits.find(&variable.identity).is_some()
    }

    pub fn suggest_value(&mut self, variable: &Variable, value: f64) -> Result<(), Exception> {
        let index = self
            .edits
            .find(&variable.identity)
            .ok_or(Exception::UnknownEditVariable)?;
        self.suggest(index, value);
        self.dual_optimize()
    }

    fn suggest(&mut self, index: usize, value: f64) {
        let info = &mut self.edits.entries[index].1;
        let delta = value - info.constant;
        info.constant = value;
        let tag = info.tag;
        if let Some(row) = self.rows.get_mut(&tag.marker) {
            if row.add(-delta) < 0.0 {
                self.infeasible_rows.push(tag.marker);
            }
            return;
        }
        if let Some(other) = tag.other
            && let Some(row) = self.rows.get_mut(&other)
        {
            if row.add(delta) < 0.0 {
                self.infeasible_rows.push(other);
            }
            return;
        }
        for (symbol, row) in &mut self.rows.entries {
            let coeff = row.coefficient_for(tag.marker);
            if coeff != 0.0 && row.add(delta * coeff) < 0.0 && symbol.kind != Kind::External {
                self.infeasible_rows.push(*symbol);
            }
        }
    }

    pub fn update_variables(&self) {
        for (_, (variable, symbol)) in &self.vars.entries {
            let value = self.rows.get(symbol).map_or(0.0, |row| row.constant);
            variable.value.set(value);
        }
    }

    pub fn reset(&mut self) {
        self.rows.clear();
        self.cns.clear();
        self.vars.clear();
        self.edits.clear();
        self.infeasible_rows.clear();
        self.objective = Row::new(0.0);
        self.artificial = None;
        self.id_tick = 1;
    }

    const fn symbol(&mut self, kind: Kind) -> Symbol {
        let symbol = Symbol {
            id: self.id_tick,
            kind,
        };
        self.id_tick += 1;
        symbol
    }

    fn var_symbol(&mut self, variable: &Variable) -> Symbol {
        if let Some((_, symbol)) = self.vars.get(&variable.identity) {
            return *symbol;
        }
        let symbol = self.symbol(Kind::External);
        self.vars
            .insert(variable.identity, (variable.clone(), symbol));
        symbol
    }

    fn create_row(&mut self, constraint: &ConstraintData) -> (Row, Tag) {
        let expr = &constraint.expression;
        let mut row = Row::new(expr.constant);
        for term in &expr.terms {
            if !near_zero(term.coefficient) {
                let symbol = self.var_symbol(&term.variable);
                if let Some(basic) = self.rows.get(&symbol) {
                    row.insert_row(basic, term.coefficient);
                } else {
                    row.insert_symbol(symbol, term.coefficient);
                }
            }
        }
        let tag = match constraint.op {
            RelationalOperator::Le | RelationalOperator::Ge => {
                let coeff = if constraint.op == RelationalOperator::Le {
                    1.0
                } else {
                    -1.0
                };
                let slack = self.symbol(Kind::Slack);
                row.insert_symbol(slack, coeff);
                let other = if constraint.strength < REQUIRED {
                    let error = self.symbol(Kind::Error);
                    row.insert_symbol(error, -coeff);
                    self.objective.insert_symbol(error, constraint.strength);
                    Some(error)
                } else {
                    None
                };
                Tag {
                    marker: slack,
                    other,
                }
            }
            RelationalOperator::Eq => {
                if constraint.strength < REQUIRED {
                    let errplus = self.symbol(Kind::Error);
                    let errminus = self.symbol(Kind::Error);
                    row.insert_symbol(errplus, -1.0);
                    row.insert_symbol(errminus, 1.0);
                    self.objective.insert_symbol(errplus, constraint.strength);
                    self.objective.insert_symbol(errminus, constraint.strength);
                    Tag {
                        marker: errplus,
                        other: Some(errminus),
                    }
                } else {
                    let dummy = self.symbol(Kind::Dummy);
                    row.insert_symbol(dummy, 1.0);
                    Tag {
                        marker: dummy,
                        other: None,
                    }
                }
            }
        };
        if row.constant < 0.0 {
            row.reverse_sign();
        }
        (row, tag)
    }

    fn choose_subject(row: &Row, tag: &Tag) -> Option<Symbol> {
        if let Some(&(symbol, _)) = row
            .cells
            .entries
            .iter()
            .find(|(symbol, _)| symbol.kind == Kind::External)
        {
            return Some(symbol);
        }
        if tag.marker.pivotable() && row.coefficient_for(tag.marker) < 0.0 {
            return Some(tag.marker);
        }
        tag.other
            .filter(|other| other.pivotable() && row.coefficient_for(*other) < 0.0)
    }

    fn add_with_artificial_variable(&mut self, row: &Row) -> Result<bool, Exception> {
        let art = self.symbol(Kind::Slack);
        self.rows.insert(art, row.clone());
        self.artificial = Some(row.clone());
        self.optimize(Objective::Artificial)?;
        let success = self
            .artificial
            .take()
            .is_some_and(|artificial| near_zero(artificial.constant));
        if let Some(index) = self.rows.find(&art) {
            let (_, mut row) = self.rows.remove_at(index);
            if row.cells.entries.is_empty() {
                return Ok(success);
            }
            let Some(entering) = Self::any_pivotable_symbol(&row) else {
                return Ok(false);
            };
            row.solve_for_pair(art, entering);
            self.substitute(entering, &row);
            self.rows.insert(entering, row);
        }
        for (_, row) in &mut self.rows.entries {
            row.remove(art);
        }
        self.objective.remove(art);
        Ok(success)
    }

    fn substitute(&mut self, symbol: Symbol, row: &Row) {
        for (basic, entry) in &mut self.rows.entries {
            entry.substitute(symbol, row);
            if basic.kind != Kind::External && entry.constant < 0.0 {
                self.infeasible_rows.push(*basic);
            }
        }
        self.objective.substitute(symbol, row);
        if let Some(artificial) = &mut self.artificial {
            artificial.substitute(symbol, row);
        }
    }

    fn optimize(&mut self, objective: Objective) -> Result<(), Exception> {
        loop {
            let row = match objective {
                Objective::Objective => &self.objective,
                Objective::Artificial => self
                    .artificial
                    .as_ref()
                    .expect("the artificial objective lives while it is optimized"),
            };
            let Some(entering) = Self::entering_symbol(row) else {
                return Ok(());
            };
            let index = self
                .leaving_row(entering)
                .ok_or(Exception::InternalSolverError(
                    "The objective is unbounded.",
                ))?;
            let (leaving, mut row) = self.rows.remove_at(index);
            row.solve_for_pair(leaving, entering);
            self.substitute(entering, &row);
            self.rows.insert(entering, row);
        }
    }

    fn dual_optimize(&mut self) -> Result<(), Exception> {
        while let Some(leaving) = self.infeasible_rows.pop() {
            let Some(index) = self.rows.find(&leaving) else {
                continue;
            };
            let constant = self.rows.entries[index].1.constant;
            if !near_zero(constant) && constant < 0.0 {
                let entering = self
                    .dual_entering_symbol(&self.rows.entries[index].1)
                    .ok_or(Exception::InternalSolverError("Dual optimize failed."))?;
                let (_, mut row) = self.rows.remove_at(index);
                row.solve_for_pair(leaving, entering);
                self.substitute(entering, &row);
                self.rows.insert(entering, row);
            }
        }
        Ok(())
    }

    fn entering_symbol(objective: &Row) -> Option<Symbol> {
        objective
            .cells
            .entries
            .iter()
            .find(|(symbol, coefficient)| symbol.kind != Kind::Dummy && *coefficient < 0.0)
            .map(|(symbol, _)| *symbol)
    }

    fn dual_entering_symbol(&self, row: &Row) -> Option<Symbol> {
        let mut entering = None;
        let mut ratio = f64::MAX;
        for &(symbol, coefficient) in &row.cells.entries {
            if coefficient > 0.0 && symbol.kind != Kind::Dummy {
                let coeff = self.objective.coefficient_for(symbol);
                let r = coeff / coefficient;
                if r < ratio {
                    ratio = r;
                    entering = Some(symbol);
                }
            }
        }
        entering
    }

    fn any_pivotable_symbol(row: &Row) -> Option<Symbol> {
        row.cells
            .entries
            .iter()
            .map(|(symbol, _)| *symbol)
            .find(|symbol| symbol.pivotable())
    }

    fn leaving_row(&self, entering: Symbol) -> Option<usize> {
        let mut ratio = f64::MAX;
        let mut found = None;
        for (index, (symbol, row)) in self.rows.entries.iter().enumerate() {
            if symbol.kind != Kind::External {
                let temp = row.coefficient_for(entering);
                if temp < 0.0 {
                    let temp_ratio = -row.constant / temp;
                    if temp_ratio < ratio {
                        ratio = temp_ratio;
                        found = Some(index);
                    }
                }
            }
        }
        found
    }

    fn marker_leaving_row(&self, marker: Symbol) -> Option<usize> {
        let mut r1 = f64::MAX;
        let mut r2 = f64::MAX;
        let mut first = None;
        let mut second = None;
        let mut third = None;
        for (index, (symbol, row)) in self.rows.entries.iter().enumerate() {
            let c = row.coefficient_for(marker);
            if c == 0.0 {
                continue;
            }
            if symbol.kind == Kind::External {
                third = Some(index);
            } else if c < 0.0 {
                let r = -row.constant / c;
                if r < r1 {
                    r1 = r;
                    first = Some(index);
                }
            } else {
                let r = row.constant / c;
                if r < r2 {
                    r2 = r;
                    second = Some(index);
                }
            }
        }
        first.or(second).or(third)
    }

    fn remove_constraint_effects(&mut self, cn: &ConstraintData, tag: &Tag) {
        if tag.marker.kind == Kind::Error {
            self.remove_marker_effects(tag.marker, cn.strength);
        }
        if let Some(other) = tag.other
            && other.kind == Kind::Error
        {
            self.remove_marker_effects(other, cn.strength);
        }
    }

    fn remove_marker_effects(&mut self, marker: Symbol, strength: f64) {
        if let Some(row) = self.rows.get(&marker) {
            self.objective.insert_row(row, -strength);
        } else {
            self.objective.insert_symbol(marker, -strength);
        }
    }

    fn all_dummies(row: &Row) -> bool {
        row.cells
            .entries
            .iter()
            .all(|(symbol, _)| symbol.kind == Kind::Dummy)
    }

    pub fn dumps(&self) -> String {
        let mut out = String::new();
        out.push_str("Objective\n---------\n");
        self.objective.dump(&mut out);
        out.push_str("\nTableau\n-------\n");
        for (symbol, row) in &self.rows.entries {
            symbol.dump(&mut out);
            out.push_str(" | ");
            row.dump(&mut out);
        }
        out.push_str("\nInfeasible\n----------\n");
        for symbol in &self.infeasible_rows {
            symbol.dump(&mut out);
            out.push('\n');
        }
        out.push_str("\nVariables\n---------\n");
        for (_, (variable, symbol)) in &self.vars.entries {
            let _ = write!(out, "{} = ", variable.name.borrow());
            symbol.dump(&mut out);
            out.push('\n');
        }
        out.push_str("\nEdit Variables\n--------------\n");
        for (_, info) in &self.edits.entries {
            let _ = writeln!(out, "{}", info.variable.name.borrow());
        }
        out.push_str("\nConstraints\n-----------\n");
        for (_, (cn, _)) in &self.cns.entries {
            for term in &cn.expression.terms {
                let _ = write!(
                    out,
                    "{} * {} + ",
                    general(term.coefficient),
                    term.variable.name.borrow()
                );
            }
            out.push_str(&general(cn.expression.constant));
            out.push_str(match cn.op {
                RelationalOperator::Le => " <= 0 ",
                RelationalOperator::Ge => " >= 0 ",
                RelationalOperator::Eq => " == 0 ",
            });
            let _ = writeln!(out, " | strength = {}", general(cn.strength));
        }
        out.push_str("\n\n");
        out
    }
}
