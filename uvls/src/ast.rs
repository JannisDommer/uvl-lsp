use crate::check::ErrorInfo;
use crate::parse::*;
use crate::semantic::FileID;
use crate::util::{lsp_range, node_range};
use enumflags2::bitflags;
use hashbrown::HashMap;
use itertools::Itertools;
use log::info;
use ropey::Rope;
use std::borrow::{Borrow, Cow};
use std::hash::Hash;
use std::path::Component;
use tokio::time::Instant;
use tower_lsp::lsp_types::{DiagnosticSeverity, Url};
use tree_sitter::{Node, Tree, TreeCursor};
use ustr::Ustr;
//Easy to work with AST parsing and util.
//The AST is stored as an ECS like structure
//This allows fast queries over all features groups etc.
//Features, Attributes, Imports and Directories are stored in a typed radix tree.
//The radix tree is represented via a maps (sym0,name,ty) -> sym1
//where ty is the type of sym1. Using this representation lowers total
//memory consumption by a nice 20%
//TODO recheck if the radix tree is still required
//TODO check if an index tree could be used
//TODO Should be split into multiple files

pub fn uri_to_path(uri: &Url) -> Option<Vec<Ustr>> {
    let mut p = uri.to_file_path().ok()?;
    p.set_extension("");
    p.components()
        .filter_map(|c| match c {
            Component::Normal(os) => os.to_str().map(|s| Some(s.into())),
            _ => None,
        })
        .collect()
}

pub fn insert_multi<K, V>(map: &mut HashMap<K, Vec<V>>, k: K, v: V)
where
    K: Hash + Eq,
{
    if let Some(old) = map.get_mut(&k) {
        old.push(v)
    } else {
        map.insert(k, vec![v]);
    }
}
//Basic Ast components
pub type Span = std::ops::Range<usize>;
#[derive(Clone, Debug)]
pub struct SymbolSpan {
    pub name: Ustr,
    pub span: Span,
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Path {
    pub names: Vec<Ustr>,
    pub spans: Vec<Span>,
}

impl Path {
    pub fn append(&self, arg: &SymbolSpan) -> Path {
        let mut new = self.clone();
        new.names.push(arg.name);
        new.spans.push(arg.span.clone());
        new
    }
    pub fn len(&self) -> usize {
        self.names.len()
    }
    pub fn range(&self) -> Span {
        if !self.spans.is_empty() {
            self.spans[0].start..self.spans.last().unwrap().end
        } else {
            0..0
        }
    }
    pub fn segment(&self, offset: usize) -> usize {
        self.spans
            .iter()
            .take_while(|i| i.start < offset)
            .count()
            .saturating_sub(1)
    }
    pub fn to_string(&self) -> String {
        self.names.iter().map(|i| i.as_str()).join(".")
    }
}

//Type definitions for symboles

#[bitflags]
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Type {
    String,
    Real,
    Vector,
    Attributes,
    Bool,
    Void,
    Namespace,
}

#[derive(Clone, Debug)]
pub enum GroupMode {
    Or,
    Alternative,
    Optional,
    Mandatory,
    Cardinality(Cardinality),
}
#[derive(Clone, Debug)]
pub enum Cardinality {
    From(usize),
    Range(usize, usize),
    Max(usize),
    Any,
}
#[derive(Clone, Debug)]
pub enum LanguageLevelMajor {
    SAT,
    SMT,
}
#[derive(Clone, Debug)]
pub enum LanguageLevelSMT {
    Any,
    FeatureCardinality,
    Aggregate,
}
#[derive(Clone, Debug)]
pub enum LanguageLevelSAT {
    Any,
    GroupCardinality,
}
#[derive(Clone, Debug)]
pub enum LanguageLevel {
    SAT(Vec<LanguageLevelSAT>),
    SMT(Vec<LanguageLevelSMT>),
}

#[derive(Clone, Debug)]
struct LanguageLevelDecl {
    lang_lvl: LanguageLevel,
    span: Span,
}
#[derive(Clone, Debug)]
pub struct Feature {
    pub name: SymbolSpan,
    pub cardinality: Option<Cardinality>,
    pub ty: Type,
}
#[derive(Clone, Debug)]
pub struct Import {
    pub path: Path,
    pub alias: Option<SymbolSpan>,
}
#[derive(Clone, Debug)]
pub struct Namespace {
    pub prefix: Path,
}
#[derive(Clone, Debug)]
pub struct Group {
    pub mode: GroupMode,
    pub span: Span,
}
#[derive(Clone, Debug)]
pub struct Reference {
    pub path: Path,
}
#[derive(Clone, Debug)]
pub struct Attribute {
    pub name: SymbolSpan,
    pub value: ValueDecl,
    pub depth: u32,
}
#[derive(Clone, Debug)]
pub struct Dir {
    pub name: Ustr,
    pub depth: u32,
}

#[derive(Clone, Debug)]
pub enum Value {
    Void,
    Number(f64),
    String(String),
    Vector,
    Bool(bool),
    Attributes,
}

#[derive(Clone, Debug)]
pub struct ValueDecl {
    pub value: Value,
    pub span: Span,
}

impl Default for Value {
    fn default() -> Self {
        Value::Void
    }
}

#[derive(Clone, Debug)]
pub enum NumericOP {
    Add,
    Sub,
    Div,
    Mul,
}

impl NumericOP {
    pub fn parse(op: &str) -> Option<Self> {
        match op {
            "+" => Some(NumericOP::Add),
            "-" => Some(NumericOP::Sub),
            "*" => Some(NumericOP::Mul),
            "/" => Some(NumericOP::Div),
            _ => None,
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LogicOP {
    And,
    Or,
    Implies,
    Equiv,
}

impl LogicOP {
    pub fn parse(op: &str) -> Option<Self> {
        match op {
            "&" => Some(LogicOP::And),
            "|" => Some(LogicOP::Or),
            "=>" => Some(LogicOP::Implies),
            "<=>" => Some(LogicOP::Equiv),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
pub enum AggregateOP {
    Avg,
    Sum,
}

impl AggregateOP {
    pub fn parse(source: &str, op: Node) -> Option<Self> {
        match &source[op.byte_range()] {
            "avg" => Some(AggregateOP::Avg),
            "sum" => Some(AggregateOP::Sum),
            _ => None,
        }
    }
    pub fn from_str(v: &str) -> Option<Self> {
        match v {
            "avg" => Some(AggregateOP::Avg),
            "sum" => Some(AggregateOP::Sum),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EquationOP {
    Greater,
    Smaller,
    Equal,
}

impl EquationOP {
    pub fn parse(op: &str) -> Option<Self> {
        match op {
            ">" => Some(Self::Greater),
            "<" => Some(Self::Smaller),
            "==" => Some(Self::Equal),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
pub enum Constraint {
    Constant(bool),
    Equation {
        op: EquationOP,
        lhs: Box<ExprDecl>,
        rhs: Box<ExprDecl>,
    },
    Logic {
        op: LogicOP,
        lhs: Box<ConstraintDecl>,
        rhs: Box<ConstraintDecl>,
    },
    Ref(Symbol),
    Not(Box<ConstraintDecl>),
}

#[derive(Clone, Debug)]
pub struct ConstraintDecl {
    pub content: Constraint,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub enum Expr {
    Number(f64),
    String(String),
    Ref(Symbol),
    Binary {
        op: NumericOP,
        rhs: Box<ExprDecl>,
        lhs: Box<ExprDecl>,
    },
    Aggregate {
        op: AggregateOP,
        context: Option<Symbol>,
        query: Path,
    },
    Len(Box<ExprDecl>),
}
#[derive(Clone, Debug)]
pub struct ExprDecl {
    pub content: Expr,
    pub span: Span,
}
//A symbole reprensents a entity in some document
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, enum_kinds::EnumKind)]
#[enum_kind(SymbolKind, derive(Hash))]
pub enum Symbol {
    Feature(usize),
    Constraint(usize),
    Attribute(usize),
    Reference(usize),
    Group(usize),
    Import(usize),
    LangLvl(usize),
    Dir(usize),

    Root,
}
impl Symbol {
    pub fn offset(&self) -> usize {
        match self {
            Self::Feature(id)
            | Self::Constraint(id)
            | Self::Attribute(id)
            | Self::Reference(id)
            | Self::Group(id)
            | Self::LangLvl(id)
            | Self::Dir(id)
            | Self::Import(id) => *id,
            _ => panic!(),
        }
    }
}
//1->N parent child relation
#[derive(Default, Debug, Clone)]
struct TreeMap {
    children: HashMap<Symbol, Vec<Symbol>>,
    parent: HashMap<Symbol, Symbol>,
}
impl TreeMap {
    fn insert(&mut self, parent: Symbol, child: Symbol) {
        insert_multi(&mut self.children, parent, child);
        self.parent.insert(child, parent);
    }
}
//Ast container each symbole kind lifes in its own vector
#[derive(Clone, Debug, Default)]
struct Ast {
    namespace: Option<Path>,
    includes: Vec<LanguageLevelDecl>,
    import: Vec<Import>,
    features: Vec<Feature>,
    constraints: Vec<ConstraintDecl>,
    attributes: Vec<Attribute>,
    references: Vec<Reference>,
    groups: Vec<Group>,
    dirs: Vec<Dir>,
    structure: TreeMap,
    //The index is stored as a typed radix tree
    index: HashMap<(Symbol, Ustr, SymbolKind), Symbol>,
}
impl Ast {
    pub fn import_prefix(&self, sym: Symbol) -> &[Ustr] {
        match sym {
            Symbol::Import(i) => {
                let im = &self.import[i];
                if let Some(alias) = im.alias.as_ref() {
                    std::slice::from_ref(&alias.name)
                } else {
                    &im.path.names
                }
            }
            _ => unimplemented!(),
        }
    }
    //call f for each child under sym and prefix
    fn lookup<F: FnMut(Symbol)>(&self, sym: Symbol, prefix: Ustr, mut f: F) {
        match sym {
            Symbol::Root => {
                if let Some(&dst) = self.index.get(&(sym, prefix, SymbolKind::Import)) {
                    f(dst);
                }
                if let Some(&dst) = self.index.get(&(sym, prefix, SymbolKind::Dir)) {
                    f(dst);
                }
                if let Some(&dst) = self.index.get(&(sym, prefix, SymbolKind::Feature)) {
                    f(dst);
                }
            }
            Symbol::Feature(..) => {
                if let Some(&dst) = self.index.get(&(sym, prefix, SymbolKind::Attribute)) {
                    f(dst);
                }
            }
            Symbol::Attribute(..) => {
                if let Some(&dst) = self.index.get(&(sym, prefix, SymbolKind::Attribute)) {
                    f(dst);
                }
            }
            Symbol::Dir(..) => {
                if let Some(&dst) = self.index.get(&(sym, prefix, SymbolKind::Import)) {
                    f(dst);
                }
                if let Some(&dst) = self.index.get(&(sym, prefix, SymbolKind::Dir)) {
                    f(dst);
                }
            }
            _ => {}
        }
    }
    fn name(&self, sym: Symbol) -> Option<Ustr> {
        match sym {
            Symbol::Feature(i) => Some(self.features[i].name.name),
            Symbol::Attribute(i) => Some(self.attributes[i].name.name),
            Symbol::Import(i) => {
                if let Some(alias) = self.import[i].alias.as_ref() {
                    Some(alias.name)
                } else {
                    self.import[i].path.names.last().cloned()
                }
            }
            Symbol::Dir(i) => Some(self.dirs[i].name),
            _ => None,
        }
    }
    fn lsp_range(&self, sym: Symbol, source: &Rope) -> Option<tower_lsp::lsp_types::Range> {
        self.span(sym).and_then(|s| lsp_range(s, source))
    }
    fn span(&self, sym: Symbol) -> Option<Span> {
        match sym {
            Symbol::Feature(i) => Some(self.features[i].name.span.clone()),
            Symbol::Attribute(i) => Some(self.attributes[i].name.span.clone()),
            Symbol::Import(i) => {
                let import = &self.import[i];
                if let Some(alias) = import.alias.as_ref() {
                    Some(import.path.range().start..alias.span.end)
                } else {
                    Some(import.path.range())
                }
            }
            Symbol::Reference(i) => Some(self.references[i].path.range()),
            Symbol::Group(i) => Some(self.groups[i].span.clone()),
            Symbol::Constraint(i) => Some(self.constraints[i].span.clone()),
            Symbol::LangLvl(i) => Some(self.includes[i].span.clone()),
            _ => None,
        }
    }
    fn children(&self, sym: Symbol) -> impl Iterator<Item = Symbol> + DoubleEndedIterator + '_ {
        self.structure
            .children
            .get(&sym)
            .into_iter()
            .flat_map(|v| v.iter().cloned())
    }
    fn all_imports(&self) -> impl Iterator<Item = Symbol> + DoubleEndedIterator {
        (0..self.import.len()).map(Symbol::Import)
    }
    fn all_features(&self) -> impl Iterator<Item = Symbol> {
        (0..self.features.len()).map(Symbol::Feature)
    }
    fn all_attributes(&self) -> impl Iterator<Item = Symbol> {
        (0..self.attributes.len()).map(Symbol::Attribute)
    }
    fn all_references(&self) -> impl Iterator<Item = Symbol> {
        (0..self.references.len()).map(Symbol::Reference)
    }
    fn all_constraints(&self) -> impl Iterator<Item = Symbol> {
        (0..self.constraints.len()).map(Symbol::Constraint)
    }
    fn all_lang_lvls(&self) -> impl Iterator<Item = Symbol> {
        (0..self.includes.len()).map(Symbol::LangLvl)
    }
    fn find(&self, offset: usize) -> Option<Symbol> {
        self.all_imports()
            .chain(self.all_features())
            .chain(self.all_attributes())
            .chain(self.all_references())
            .find(|s| self.span(*s).unwrap().contains(&offset))
    }
}

pub trait Visitor<'a> {
    fn cursor(&self) -> &TreeCursor<'a>;
    fn cursor_mut(&mut self) -> &mut TreeCursor<'a>;
    fn source(&self) -> &Rope;
    fn push_err_raw(&mut self, err: ErrorInfo);
    fn skip_extra(&mut self) -> bool {
        loop {
            if !self.node().is_extra() && !self.node().is_error() {
                return true;
            }
            if !self.cursor_mut().goto_next_sibling() {
                return false;
            }
        }
    }
    fn goto_first_child(&mut self) -> bool {
        if self.cursor_mut().goto_first_child() {
            if self.skip_extra() {
                true
            } else {
                self.goto_parent();
                false
            }
        } else {
            false
        }
    }
    fn goto_named(&mut self) -> bool {
        loop {
            if self.node().is_named() {
                return true;
            }
            if !self.goto_next_sibling() {
                return false;
            }
        }
    }
    fn goto_next_sibling(&mut self) -> bool {
        self.cursor_mut().goto_next_sibling() && self.skip_extra()
    }
    fn goto_parent(&mut self) {
        self.cursor_mut().goto_parent();
    }
    fn kind(&self) -> &str {
        self.cursor().node().kind()
    }
    fn node(&self) -> Node<'a> {
        self.cursor().node()
    }
    fn child_by_name(&self, name: &str) -> Option<Node<'a>> {
        self.node().child_by_field_name(name)
    }
    fn goto_next_kind(&mut self, kind: &str) -> bool {
        loop {
            if !self.goto_next_sibling() {
                return false;
            }
            if self.kind() == kind {
                return true;
            }
        }
    }
    fn goto_field(&mut self, name: &str) -> bool {
        loop {
            if self
                .cursor()
                .field_name()
                .map(|f| f == name)
                .unwrap_or(false)
            {
                return true;
            }
            if !self.goto_next_sibling() {
                return false;
            }
        }
    }
    fn goto_kind(&mut self, name: &str) -> bool {
        loop {
            if self.kind() == name {
                return true;
            }
            if !self.goto_next_sibling() {
                return false;
            }
        }
    }
    fn push_error<T: Into<String>>(&mut self, w: u32, error: T) {
        self.push_err_raw(ErrorInfo {
            location: node_range(self.node(), self.source()),
            severity: DiagnosticSeverity::ERROR,
            weight: w,
            msg: error.into(),
        });
    }
    fn push_error_node<T: Into<String>>(&mut self, node: Node, w: u32, error: T) {
        self.push_err_raw(ErrorInfo {
            location: node_range(node, self.source()),
            severity: DiagnosticSeverity::ERROR,
            weight: w,
            msg: error.into(),
        });
    }
}
//Utilitu function, tree-sitter uses cursor to traverse trees, this function is a scope that
//gurantees to "go down" call f and later "go up" one level. It also protects against stack
//overflow
pub fn visit_children<'a, F, T, V>(state: &mut V, mut f: F) -> T
where
    V: Visitor<'a>,
    F: FnMut(&mut V) -> T,
    T: Default,
{
    if state.goto_first_child() {
        if stacker::remaining_stack().unwrap() <= 32 * 1024 {
            info!("In the red zone");
        }
        let out = stacker::maybe_grow(32 * 1024, 1024 * 1024, || f(state));
        state.goto_parent();
        out
    } else {
        T::default()
    }
}

pub fn visit_children_arg<'a, A, F, T, V>(state: &mut V, arg: A, mut f: F) -> T
where
    V: Visitor<'a>,
    F: FnMut(&mut V, A) -> T,
    T: Default,
{
    if state.goto_first_child() {
        let out = stacker::maybe_grow(32 * 1024, 1024 * 1024, || f(state, arg));
        state.goto_parent();
        out
    } else {
        T::default()
    }
}
//Loop over all sibiblings
pub fn visit_siblings<'a, F: FnMut(&mut V), V: Visitor<'a>>(state: &mut V, mut f: F) {
    loop {
        f(state);
        if !state.goto_next_sibling() {
            break;
        }
    }
}

//While parsing we keep a mutable state to store entities and errors
#[derive(Clone)]
struct VisitorState<'a> {
    errors: Vec<ErrorInfo>,
    cursor: TreeCursor<'a>,
    ast: Ast,
    source: &'a Rope,
}
impl<'a> Visitor<'a> for VisitorState<'a> {
    fn cursor(&self) -> &TreeCursor<'a> {
        &self.cursor
    }
    fn cursor_mut(&mut self) -> &mut TreeCursor<'a> {
        &mut self.cursor
    }
    fn source(&self) -> &Rope {
        self.source
    }
    fn push_err_raw(&mut self, err: ErrorInfo) {
        self.errors.push(err);
    }
}
impl<'a> VisitorState<'a> {
    fn add_constraint(&mut self, constraint: ConstraintDecl, scope: Symbol) -> Symbol {
        self.ast.constraints.push(constraint);
        let sym = Symbol::Constraint(self.ast.constraints.len() - 1);
        self.push_child(scope, sym);
        sym
    }
    fn add_ref(&mut self, path: Path, scope: Symbol) -> Symbol {
        self.ast.references.push(Reference { path });
        let sym = Symbol::Reference(self.ast.references.len() - 1);
        self.push_child(scope, sym);
        sym
    }
    fn add_ref_direct(&mut self, path: Path) -> Symbol {
        self.ast.references.push(Reference { path });

        Symbol::Reference(self.ast.references.len() - 1)
    }
    //create the import tree map and the general search index for name resolution
    fn connect(&mut self) {
        for i in self.ast.all_imports() {
            let path = self.ast.import_prefix(i).to_vec();
            let mut node = Symbol::Root;
            for k in 0..path.len() - 1 {
                let dir_name = path[k];
                if let Some(dir) = self.ast.index.get(&(node, dir_name, SymbolKind::Dir)) {
                    node = *dir;
                } else {
                    let sym = Symbol::Dir(self.ast.dirs.len());
                    self.ast.dirs.push(Dir {
                        name: dir_name,
                        depth: k as u32 + 1,
                    });
                    self.push_child(node, sym);
                    self.ast
                        .index
                        .insert((node, dir_name, SymbolKind::Dir), sym);
                    node = sym;
                }
            }
            self.push_child(node, i);
            if let Some(old) = self
                .ast
                .index
                .insert((node, *path.last().unwrap(), SymbolKind::Import), i)
            {
                self.errors.push(ErrorInfo {
                    location: self.ast.lsp_range(i, self.source).unwrap(),
                    severity: DiagnosticSeverity::ERROR,
                    weight: 20,
                    msg: format!(
                        "duplicate import already defined in line {}",
                        self.ast.lsp_range(old, self.source).unwrap().start.line
                    ),
                });
            }
        }
        //Create name index for features and attributes
        let mut stack = vec![(Symbol::Root, Symbol::Root, 0)];
        while let Some((node, scope, depth)) = stack.pop() {
            let new_scope = if let Some(name) = self.ast.name(node) {
                match node {
                    Symbol::Feature(..) => {
                        if let Some(old) = self
                            .ast
                            .index
                            .insert((Symbol::Root, name, SymbolKind::Feature), node)
                        {
                            self.errors.push(ErrorInfo {
                                location: self.ast.lsp_range(node, self.source).unwrap(),
                                severity: DiagnosticSeverity::ERROR,
                                weight: 20,
                                msg: "duplicate feature".to_string(),
                            });
                            self.errors.push(ErrorInfo {
                                location: self.ast.lsp_range(old, self.source).unwrap(),
                                severity: DiagnosticSeverity::ERROR,
                                weight: 20,
                                msg: "duplicate feature".to_string(),
                            })
                        }
                        node
                    }
                    Symbol::Attribute(i) => {
                        if let Some(old) = self
                            .ast
                            .index
                            .insert((scope, name, SymbolKind::Attribute), node)
                        {
                            self.errors.push(ErrorInfo {
                                location: self.ast.lsp_range(node, self.source).unwrap(),
                                severity: DiagnosticSeverity::ERROR,
                                weight: 20,
                                msg: "duplicate attribute".to_string(),
                            });
                            self.errors.push(ErrorInfo {
                                location: self.ast.lsp_range(old, self.source).unwrap(),
                                severity: DiagnosticSeverity::ERROR,
                                weight: 20,
                                msg: "duplicate attribute".to_string(),
                            });
                        };
                        self.ast.attributes[i].depth = depth;
                        node
                    }
                    _ => scope,
                }
            } else {
                scope
            };
            for i in self.ast.children(node) {
                stack.push((i, new_scope, depth + 1));
            }
        }
        for i in self.ast.children(Symbol::Root) {
            if matches!(i, Symbol::Feature(..)) {
                if self
                    .ast
                    .index
                    .get(&(Symbol::Root, self.ast.name(i).unwrap(), SymbolKind::Dir))
                    .is_some()
                {
                    self.errors.push(ErrorInfo {
                        location: self.ast.lsp_range(i, self.source).unwrap(),
                        severity: DiagnosticSeverity::ERROR,
                        weight: 20,
                        msg: "name already defined as import directory".to_string(),
                    });
                }
                if self
                    .ast
                    .index
                    .get(&(Symbol::Root, self.ast.name(i).unwrap(), SymbolKind::Import))
                    .is_some()
                {
                    self.errors.push(ErrorInfo {
                        location: self.ast.lsp_range(i, self.source).unwrap(),
                        severity: DiagnosticSeverity::ERROR,
                        weight: 20,
                        msg: "name already defined as import".to_string(),
                    });
                }
            }
        }
    }
    fn push_child(&mut self, parent: Symbol, child: Symbol) {
        self.ast.structure.insert(parent, child);
    }
    fn header(&self) -> Option<Node<'a>> {
        self.node().child_by_field_name("header")
    }
    fn push_error_blk<T: Into<String>>(&mut self, w: u32, error: T) {
        self.errors.push(ErrorInfo {
            location: node_range(self.header().unwrap(), self.source),
            severity: DiagnosticSeverity::ERROR,
            weight: w,
            msg: error.into(),
        });
    }
}
impl<'b> SymbolSlice for VisitorState<'b> {
    fn slice_raw(&self, node: Span) -> Cow<'_, str> {
        self.source.byte_slice(node).into()
    }
}
//Parsing the AST from a tree-sitter green tree
fn opt_name(state: &mut VisitorState) -> Option<SymbolSpan> {
    if state.kind() == "name" {
        if state.node().is_missing() {
            Some(SymbolSpan {
                name: "__MISSING__".into(),
                span: state.node().byte_range(),
            })
        } else {
            Some(SymbolSpan {
                name: state.name(state.node()),
                span: state.node().byte_range(),
            })
        }
    } else {
        None
    }
}
fn opt_path(state: &mut VisitorState) -> Option<Path> {
    if state.kind() == "name" {
        opt_name(state).map(|name| Path {
            names: vec![name.name],
            spans: vec![name.span],
        })
    } else if state.kind() == "path" {
        if state.child_by_name("tail").is_some() {
            state.push_error(10, "tailing dot not supported");
        }
        visit_children(state, |state| {
            let mut p = Path::default();
            loop {
                if let Some(name) = opt_name(state) {
                    p.names.push(name.name);
                    p.spans.push(name.span);
                }
                if !state.goto_next_sibling() {
                    break;
                }
            }
            Some(p)
        })
    } else {
        None
    }
}
fn check_simple_blk(state: &mut VisitorState, kind: &str) {
    match state.cursor.field_name() {
        Some("cardinality") => state.push_error(30, format!("{} may not have a cardinality", kind)),
        Some("attribs") => state.push_error(30, format!("{} may not have a any attributes", kind)),
        Some("child") => state.push_error(30, format!("{} may not have a any children", kind)),
        _ => {}
    }
}

fn check_no_extra_blk(state: &mut VisitorState, kind: &str) {
    match state.cursor.field_name() {
        Some("cardinality") => state.push_error(30, format!("{} may not have a cardinality", kind)),
        Some("attribs") => state.push_error(30, format!("{} may not have a any attributes", kind)),
        _ => {}
    }
}

fn visit_namespace(state: &mut VisitorState) {
    loop {
        check_simple_blk(state, "namespace");
        if state.kind() == "namespace" {
            visit_children(state, |state| {
                state.goto_field("name");
                if state.ast.namespace.is_none() {
                    state.ast.namespace = opt_path(state);
                }
            });
        }
        if !state.goto_next_sibling() {
            break;
        }
    }
}
fn opt_smt_minor(state: &mut VisitorState) -> Option<LanguageLevelSMT> {
    match state.kind() {
        "*" => Some(LanguageLevelSMT::Any),
        "feature-cardinality" => Some(LanguageLevelSMT::FeatureCardinality),
        "aggregate-function" => Some(LanguageLevelSMT::Aggregate),
        "group-cardinality" => {
            state.push_error(30, "not allowed under SMT");
            None
        }
        _ => {
            state.push_error(30, "unknown SMT level");
            None
        }
    }
}
fn opt_sat_minor(state: &mut VisitorState) -> Option<LanguageLevelSAT> {
    match state.kind() {
        "*" => Some(LanguageLevelSAT::Any),
        "group-cardinality" => Some(LanguageLevelSAT::GroupCardinality),
        "feature-cardinality" | "aggregate-function" => {
            state.push_error(30, "not allowed under SAT");
            None
        }
        _ => {
            state.push_error(30, "unknown SAT level");
            None
        }
    }
}
fn opt_major_lang_lvl(state: &mut VisitorState) -> Option<LanguageLevel> {
    match state.node().kind() {
        "SMT-level" => Some(LanguageLevel::SMT(vec![])),
        "SAT-level" => Some(LanguageLevel::SAT(vec![])),
        _ => {
            state.push_error(30, "unknown major language level");
            None
        }
    }
}
fn opt_lang_lvl(state: &mut VisitorState) -> Option<LanguageLevel> {
    let mut out = None;
    loop {
        if state.kind() == "major_lvl" {
            if out.is_some() {
                state.push_error(30, "duplicate major level, please pick a minor level");
                return None;
            } else {
                out = Some(visit_children(state, opt_major_lang_lvl)?);
            }
        }
        if state.kind() == "minor_lvl" {
            if let Some(major) = out.as_mut() {
                match major {
                    LanguageLevel::SMT(v) => {
                        if let Some(lvl) = visit_children(state, opt_smt_minor) {
                            v.push(lvl);
                        } else {
                            return None;
                        }
                    }
                    LanguageLevel::SAT(v) => {
                        if let Some(lvl) = visit_children(state, opt_sat_minor) {
                            v.push(lvl);
                        } else {
                            return None;
                        }
                    }
                }
            } else {
                state.push_error(30, "missing major level, please specify SMT or SAT level");
                return None;
            }
        }
        if state.kind() == "name" {
            state.push_error(30, "unknown language level");
            return None;
        }
        if !state.goto_next_sibling() {
            break;
        }
    }
    out
}
fn visit_lang_lvl(state: &mut VisitorState) {
    loop {
        check_simple_blk(state, "");
        if state.kind() == "lang_lvl" {
            if let Some(lvl) = visit_children(state, opt_lang_lvl) {
                state.ast.includes.push(LanguageLevelDecl {
                    lang_lvl: lvl,
                    span: state.node().byte_range(),
                });
            }
        }
        if !state.goto_next_sibling() {
            break;
        }
    }
}
fn visit_include(state: &mut VisitorState) {
    loop {
        check_no_extra_blk(state, "include");
        if state.kind() == "blk" {
            match state.header().unwrap().kind() {
                "lang_lvl" => visit_children(state, visit_lang_lvl),
                "ref" => state.push_error_blk(
                    30,
                    "unknown language level start with SMT-level or SAT-level",
                ),
                _ => {
                    state.push_error_blk(40, "expected a language level");
                }
            }
        }
        if !state.goto_next_sibling() {
            break;
        }
    }
}
fn visit_import_decl(state: &mut VisitorState) {
    loop {
        check_simple_blk(state, "import");
        if let Some(name) = opt_path(state) {
            state.ast.import.push(Import {
                path: name,
                alias: None,
            })
        } else if state.kind() == "ref" {
            visit_children(state, |state| {
                state.goto_field("path");
                let path = opt_path(state)?;
                let alias = if state.goto_field("alias") {
                    opt_name(state)
                } else {
                    None
                };
                state.ast.import.push(Import { path, alias });
                Some(())
            });
        }

        if !state.goto_next_sibling() {
            break;
        }
    }
}
fn visit_import(state: &mut VisitorState) {
    loop {
        check_no_extra_blk(state, "imports");
        if state.kind() == "blk" {
            match state.header().unwrap().kind() {
                "name" | "ref" => visit_children(state, visit_import_decl),
                "incomplete_ref" => {
                    state.push_error_blk(40, "incomplete import, please specify an alias");
                }
                _ => {
                    state.push_error_blk(40, "expected a import declaration");
                }
            }
        }
        if !state.goto_next_sibling() {
            break;
        }
    }
}

fn opt_int(node: Node, state: &mut VisitorState) -> Option<usize> {
    if let Ok(i) = state.slice(node).parse() {
        Some(i)
    } else {
        state.push_error_node(node, 20, "cant parse integer");
        None
    }
}
fn opt_cardinality(node: Node, state: &mut VisitorState) -> Option<Cardinality> {
    let begin = node.child_by_field_name("begin");
    let end = node.child_by_field_name("end");
    match (begin, end.map(|n| n.kind())) {
        (Some(begin), Some("int")) => Some(Cardinality::Range(
            opt_int(begin, state)?,
            opt_int(end.unwrap(), state)?,
        )),
        (Some(begin), Some("*")) => Some(Cardinality::From(opt_int(begin, state)?)),
        (None, Some("int")) => Some(Cardinality::Max(opt_int(end.unwrap(), state)?)),
        (_, _) => Some(Cardinality::Any),
    }
}

fn opt_number(state: &mut VisitorState) -> Option<f64> {
    if let Ok(num) = state.slice(state.node()).parse() {
        Some(num)
    } else {
        state.push_error(40, "failed to parse number");
        None
    }
}
fn opt_numeric_op(node: Node) -> Option<NumericOP> {
    match node.kind() {
        "+" => Some(NumericOP::Add),
        "-" => Some(NumericOP::Sub),
        "*" => Some(NumericOP::Mul),
        "/" => Some(NumericOP::Div),
        _ => None,
    }
}
fn opt_aggreate_op(state: &mut VisitorState) -> Option<AggregateOP> {
    match state.slice(state.child_by_name("op")?).borrow() {
        "sum" => Some(AggregateOP::Sum),
        "avg" => Some(AggregateOP::Avg),
        _ => {
            state.push_error(30, "unknown aggregate function");
            None
        }
    }
}
fn opt_function_args(state: &mut VisitorState) -> Option<Vec<Path>> {
    visit_children(state, |state| {
        let mut args = Vec::new();
        loop {
            match state.kind() {
                "name" => {}
                "path" => args.push(opt_path(state).unwrap()),
                _ => {
                    if state.node().is_named() {
                        state.push_error(30, "expected a reference");
                        return None;
                    }
                }
            }
            if !state.goto_next_sibling() {
                break;
            }
        }
        Some(args)
    })
}

fn opt_aggregate(state: &mut VisitorState) -> Option<Expr> {
    let op = opt_aggreate_op(state)?;
    if state.child_by_name("tail").is_some() {
        state.push_error(10, "tailing comma not allowed");
    }
    let args = opt_function_args(state)?;
    match args.len() {
        0 => {
            state.push_error(30, "missing arguments");
            None
        }
        1 => Some(Expr::Aggregate {
            op,
            query: args[0].clone(),
            context: None,
        }),
        2 => Some(Expr::Aggregate {
            op,
            query: args[1].clone(),
            context: Some(state.add_ref_direct(args[0].clone())),
        }),
        _ => {
            state.push_error(30, "to many arguments");
            None
        }
    }
}
fn opt_numeric(state: &mut VisitorState) -> Option<ExprDecl> {
    let span = state.node().parent()?.byte_range();
    state.goto_named();
    match state.kind() {
        "path" => {
            let path = opt_path(state)?;
            Some(Expr::Ref(state.add_ref_direct(path)))
        }

        "number" => Some(Expr::Number(opt_number(state)?)),
        "string" => Some(Expr::String(opt_string(state)?)),
        "binary_expr" => {
            let op = state.child_by_name("op").unwrap();
            visit_children(state, |state| {
                if let Some(op) = opt_numeric_op(op) {
                    state.goto_field("lhs");
                    let lhs = opt_numeric(state)?;
                    state.goto_field("rhs");
                    let rhs = opt_numeric(state)?;
                    Some(Expr::Binary {
                        op,
                        lhs: Box::new(lhs),
                        rhs: Box::new(rhs),
                    })
                } else {
                    state.push_error_node(
                        state.node().parent().unwrap(),
                        40,
                        "found a constraint, expected a expression",
                    );
                    None
                }
            })
        }
        "nested_expr" => visit_children(state, opt_numeric).map(|c| c.content),
        "function" => match state.slice(state.child_by_name("op")?).borrow() {
            "sum" | "avg" => opt_aggregate(state),
            "len" => {
                if state.child_by_name("tail").is_some() {
                    state.push_error(10, "tailing comma not allowed");
                }
                visit_children(state, |state| {
                    if state.goto_field("arg") {
                        info!("{:?}", state.node());
                        let out = Some(Expr::Len(opt_numeric(state)?.into()));
                        if state.goto_next_sibling() && state.goto_field("arg") {
                            state.push_error(30, "expected exactly one argument");
                        }

                        out
                    } else {
                        state.push_error(30, "missing argument");
                        None
                    }
                })
            }
            _ => {
                state.push_error(30, "unknown function");
                None
            }
        },
        _ => {
            state.push_error(40, "found a constraint, expected a expression");
            None
        }
    }
    .map(|content| ExprDecl { span, content })
}
fn opt_logic_op(node: Node) -> Option<LogicOP> {
    match node.kind() {
        "&" => Some(LogicOP::And),
        "|" => Some(LogicOP::Or),
        "=>" => Some(LogicOP::Implies),
        "<=>" => Some(LogicOP::Equiv),
        _ => None,
    }
}

fn opt_equation(node: Node) -> Option<EquationOP> {
    match node.kind() {
        "==" => Some(EquationOP::Equal),
        ">" => Some(EquationOP::Greater),
        "<" => Some(EquationOP::Smaller),
        _ => None,
    }
}

fn opt_constraint(state: &mut VisitorState) -> Option<ConstraintDecl> {
    let span = state.node().parent()?.byte_range();
    state.goto_named();
    match state.kind() {
        "path" | "name" => {
            let path = opt_path(state)?;
            Some(Constraint::Ref(state.add_ref_direct(path)))
        }
        "bool" => Some(Constraint::Constant(visit_children(state, opt_bool))),
        "unary_expr" => {
            let op = state.child_by_name("op").unwrap().kind();
            visit_children(state, |state| {
                state.goto_field("lhs");
                match op {
                    "!" => opt_constraint(state).map(|c| Constraint::Not(Box::new(c))),
                    _ => None,
                }
            })
        }
        "nested_expr" => visit_children(state, opt_constraint).map(|c| c.content),
        "binary_expr" => {
            let op = state.child_by_name("op").unwrap();
            visit_children(state, |state| {
                if let Some(op) = opt_logic_op(op) {
                    state.goto_field("lhs");
                    let lhs = opt_constraint(state)?;
                    state.goto_field("rhs");
                    let rhs = opt_constraint(state)?;
                    Some(Constraint::Logic {
                        op,
                        lhs: Box::new(lhs),
                        rhs: Box::new(rhs),
                    })
                } else if let Some(op) = opt_equation(op) {
                    state.goto_field("lhs");
                    let lhs = opt_numeric(state)?;
                    state.goto_field("rhs");
                    let rhs = opt_numeric(state)?;
                    Some(Constraint::Equation {
                        op,
                        lhs: Box::new(lhs),
                        rhs: Box::new(rhs),
                    })
                } else {
                    state.push_error_node(
                        state.node().parent().unwrap(),
                        40,
                        "expected a constraint found a expression",
                    );
                    None
                }
            })
        }
        _ => {
            state.push_error(40, "expected a constraint found a expression");
            None
        }
    }
    .map(|content| ConstraintDecl { span, content })
}
fn visit_constraint(state: &mut VisitorState, parent: Symbol) {
    if let Some(cons) = opt_constraint(state) {
        state.add_constraint(cons, parent);
    }
}
fn opt_bool(state: &mut VisitorState) -> bool {
    match state.kind() {
        "true" => true,
        "false" => false,
        _ => false,
    }
}
fn opt_string(state: &mut VisitorState) -> Option<String> {
    if state.kind() == "string" {
        visit_children(state, |state| {
            state.goto_kind("string_content");
            Some(
                state
                    .source
                    .slice_raw(state.node().byte_range())
                    .to_string(),
            )
        })
    } else {
        None
    }
}
fn opt_attrib_expr(state: &mut VisitorState) -> Option<Value> {
    state.goto_named();
    match state.kind() {
        "number" => Some(Value::Number(opt_number(state)?)),
        "bool" => Some(Value::Bool(visit_children(state, opt_bool))),
        "string" => Some(Value::String(opt_string(state)?)),
        "path" => {
            state.push_error(30, "attribute references are not supported");
            None
        }
        "binary_expr" | "nested_expr" | "aggregate" | "unary_expr" => {
            state.push_error(30, "composit atttribute values are not supported");
            None
        }
        _ => None,
    }
}
fn opt_value(state: &mut VisitorState) -> Value {
    match state.kind() {
        "vector" => Value::Vector, //We dont parse vectors since they seem unsed
        "attributes" => Value::Attributes,
        "attrib_expr" => visit_children(state, opt_attrib_expr).unwrap_or_default(),
        _ => Value::Void,
    }
}

fn visit_attribute_value(state: &mut VisitorState, parent: Symbol) {
    state.goto_field("name");
    let name = opt_name(state).unwrap();
    let sym = Symbol::Attribute(state.ast.attributes.len());
    state.push_child(parent, sym);
    state.goto_field("value");
    let value = opt_value(state);
    let has_children = matches!(&value, Value::Attributes);
    state.ast.attributes.push(Attribute {
        name,
        value: ValueDecl {
            value,
            span: state.node().byte_range(),
        },
        depth: 0,
    });
    if has_children {
        visit_children_arg(state, sym, visit_attributes);
    }
}
fn visit_constraint_list(state: &mut VisitorState, parent: Symbol) {
    debug_assert!(state.node().parent().unwrap().kind() == "attribute_constraints");
    loop {
        if state.kind() == "constraint" {
            visit_children_arg(state, parent, visit_constraint);
        }
        if !state.goto_next_sibling() {
            break;
        }
    }
}
fn visit_attributes(state: &mut VisitorState, parent: Symbol) {
    debug_assert!(state.node().parent().unwrap().kind() == "attributes");
    loop {
        match state.kind() {
            "attribute_constraints" => {
                if state.child_by_name("tail").is_some() {
                    state.push_error(10, "tailing comma unsupported");
                }
                visit_children_arg(state, parent, visit_constraint_list);
            }
            "attribute_constraint" => {
                visit_children(state, |state| {
                    debug_assert!(state.goto_kind("constraint"));
                    visit_children_arg(state, parent, visit_constraint);
                });
            }
            "attribute_value" => {
                visit_children_arg(state, parent, visit_attribute_value);
            }
            _ => {}
        }
        if !state.goto_next_sibling() {
            break;
        }
    }
}

fn visit_feature(state: &mut VisitorState, parent: Symbol, name: SymbolSpan, ty: Type) {
    debug_assert!(state.node().parent().unwrap().kind() == "blk");
    match parent {
        Symbol::Feature(..) => {
            state.push_error(40, "features have to be separated by groups");
        }
        _ => {}
    }
    let feature = Feature {
        name,
        ty,
        cardinality: state
            .node()
            .parent()
            .unwrap()
            .child_by_field_name("cardinality")
            .and_then(|n| opt_cardinality(n, state)),
    };
    let sym = Symbol::Feature(state.ast.features.len());
    state.ast.features.push(feature);
    state.push_child(parent, sym);
    loop {
        match state.kind() {
            "attributes" => {
                visit_children_arg(state, sym, visit_attributes);
            }
            "blk" => {
                visit_children_arg(state, sym, visit_blk_decl);
            }
            _ => {}
        }
        if !state.goto_next_sibling() {
            break;
        }
    }
}

fn visit_ref(state: &mut VisitorState, parent: Symbol, path: Path) {
    debug_assert!(state.node().parent().unwrap().kind() == "blk");
    match parent {
        Symbol::Feature(..) => {
            state.push_error(40, "features have to be separated by groups");
        }
        _ => {}
    }
    state.add_ref(path, parent);
    loop {
        check_simple_blk(state, "references");
        if !state.goto_next_sibling() {
            break;
        }
    }
}
fn visit_group(state: &mut VisitorState, parent: Symbol, mode: GroupMode) {
    debug_assert!(state.node().parent().unwrap().kind() == "blk");
    match parent {
        Symbol::Group(..) => {
            state.push_error(40, "groups have to be separated by features");
        }
        Symbol::Root => {
            state.push_error(40, "groups have to be contained by features");
        }
        _ => {}
    }
    let sym = Symbol::Group(state.ast.groups.len());
    state.push_child(parent, sym);
    state.ast.groups.push(Group {
        mode,
        span: state.node().byte_range(),
    });
    loop {
        check_no_extra_blk(state, "group");
        if state.kind() == "blk" {
            visit_children_arg(state, sym, visit_blk_decl);
        }
        if !state.goto_next_sibling() {
            break;
        }
    }
}
fn visit_blk_decl(state: &mut VisitorState, parent: Symbol) {
    debug_assert!(state.node().parent().unwrap().kind() == "blk");
    state.goto_field("header");
    match state.kind() {
        "name" => {
            let name = opt_name(state).unwrap();
            visit_feature(state, parent, name, Type::Bool);
        }
        "typed_feature" => {
            let (name, ty) = visit_children(state, |state| {
                state.goto_field("type");
                let ty = match &*state.slice_raw(state.node().byte_range()) {
                    "Integer" | "Real" => Type::Real,
                    "String" => Type::String,
                    "Boolean" => Type::Bool,
                    _ => {
                        state.push_error(30, "unknown type, interpreting as boolean");
                        Type::Bool
                    }
                };
                state.goto_field("name");
                Some((opt_name(state).unwrap(), ty))
            })
            .unwrap();
            visit_feature(state, parent, name, ty);
        }
        "ref" => {
            let path = visit_children(state, |state| {
                state.goto_field("path");
                let path = opt_path(state);
                if state.goto_field("alias") {
                    state.push_error(30, "imported features may not have an alias");
                }
                path
            })
            .unwrap();
            visit_ref(state, parent, path);
        }
        "group_mode" => {
            let mode = match state.node().child(0).unwrap().kind() {
                "mandatory" => GroupMode::Mandatory,
                "or" => GroupMode::Or,
                "optional" => GroupMode::Optional,
                "alternative" => GroupMode::Alternative,
                _ => GroupMode::Mandatory,
            };
            visit_group(state, parent, mode);
        }
        "cardinality" => {
            let card = opt_cardinality(state.node(), state).unwrap_or(Cardinality::Any);
            visit_group(state, parent, GroupMode::Cardinality(card));
        }
        _ => {
            state.push_error(40, "expected a feature or group declaration");
        }
    }
}
fn visit_features(state: &mut VisitorState) {
    debug_assert!(state.node().parent().unwrap().kind() == "blk");
    loop {
        check_no_extra_blk(state, "features");
        if state.kind() == "blk" {
            visit_children_arg(state, Symbol::Root, visit_blk_decl);
        }
        if !state.goto_next_sibling() {
            break;
        }
    }
}
fn visit_constraint_decl(state: &mut VisitorState) {
    loop {
        check_simple_blk(state, "constraints");
        match state.kind() {
            "constraint" | "ref" => visit_children_arg(state, Symbol::Root, visit_constraint),
            "name" => visit_constraint(state, Symbol::Root),
            _ => {}
        }
        if state.kind() == "ref" {
            if let Some(alias) = state.child_by_name("alias") {
                state.push_error_node(alias, 30, "alias not allowed here");
            }
        }
        if !state.goto_next_sibling() {
            break;
        }
    }
}
fn visit_constraints(state: &mut VisitorState) {
    loop {
        check_no_extra_blk(state, "constraints");
        if state.kind() == "blk" {
            let header = state.header().unwrap();
            match header.kind() {
                "constraint" | "name" | "ref" => {
                    visit_children(state, visit_constraint_decl);
                }
                _ => {
                    state.push_error(40, "expected a constraint");
                }
            }
        }
        if !state.goto_next_sibling() {
            break;
        }
    }
}
fn visit_top_lvl(state: &mut VisitorState) {
    let mut top_level_order: Vec<Node> = Vec::new();
    loop {
        if state.kind() == "blk" {
            let header = state.header().unwrap();
            top_level_order.push(header);
            match header.kind() {
                "namespace" => visit_children(state, visit_namespace),
                "include" => visit_children(state, visit_include),
                "imports" => visit_children(state, visit_import),
                "features" => visit_children(state, visit_features),
                "constraints" => visit_children(state, visit_constraints),
                "incomplete_namespace" => {
                    state.push_error_blk(60, "incomplete namespace");
                    top_level_order.pop();
                }
                _ => {
                    state.push_error_blk(60,"only namspaces, imports, includes, features and constraints are allowed here");
                    visit_children(state, visit_features);
                    top_level_order.pop();
                }
            }
        }
        if !state.goto_next_sibling() {
            break;
        }
    }
    let fixed_order = ["namespace", "include", "imports", "features", "constraints"];
    for i in 1..top_level_order.iter().len() {
        let k = fixed_order
            .iter()
            .enumerate()
            .find(|name| name.1 == &top_level_order[i - 1].kind())
            .unwrap()
            .0;
        let w = fixed_order
            .iter()
            .enumerate()
            .find(|name| name.1 == &top_level_order[i].kind())
            .unwrap()
            .0;
        if k == w {
            state.push_error_node(
                top_level_order[i],
                50,
                format!("duplicate {} section", top_level_order[i].kind()),
            );
        }
        if k > w {
            state.push_error_node(
                top_level_order[i],
                50,
                format!(
                    "{} section comes before the {} section",
                    top_level_order[i - 1].kind(),
                    top_level_order[i].kind()
                ),
            );
        }
    }
}
//visits all valid children of a tree-sitter (green tree) recursively to translate them into the
//AST(red tree)
pub fn visit_root(source: Rope, tree: Tree, uri: Url, timestamp: Instant) -> AstDocument {
    let (ast, errors) = {
        let mut state = VisitorState {
            errors: Vec::new(),
            cursor: tree.walk(),
            ast: Default::default(),
            source: &source,
        };
        visit_children(&mut state, visit_top_lvl);
        state.connect();
        (state.ast, state.errors)
    };
    let mut path = uri_to_path(&uri).unwrap();
    if let Some(ns) = ast.namespace.as_ref() {
        let len = path.len().saturating_sub(ns.names.len());
        path.truncate(len);
        path.extend_from_slice(&ns.names);
    }
    AstDocument {
        id: FileID::from_uri(&uri),
        path,
        uri,
        ast,
        source,
        tree,
        timestamp,
        errors,
    }
}
//Combines the AST with metadata, this is also a public interface to the AST.
#[derive(Clone, Debug)]
pub struct AstDocument {
    ast: Ast,
    pub source: Rope,
    pub tree: Tree,
    pub timestamp: Instant,
    pub errors: Vec<ErrorInfo>,
    pub path: Vec<Ustr>,
    pub uri: Url,
    pub id: FileID,
}
impl AstDocument {
    pub fn parent(&self, sym: Symbol, merge_root_features: bool) -> Option<Symbol> {
        if merge_root_features && matches!(sym, Symbol::Feature(..)) {
            Some(Symbol::Root)
        } else {
            self.ast.structure.parent.get(&sym).cloned()
        }
    }
    pub fn scope(&self, mut sym: Symbol) -> Symbol {
        while let Some(p) = self.parent(sym, true) {
            match sym {
                Symbol::Feature(..) => return sym,
                Symbol::Root => return sym,
                _ => {}
            }
            sym = p;
        }
        Symbol::Root
    }
    pub fn name(&self, sym: Symbol) -> Option<Ustr> {
        self.ast.name(sym)
    }
    pub fn all_lang_lvls(&self) -> impl Iterator<Item = Symbol> {
        self.ast.all_lang_lvls()
    }
    pub fn all_imports(&self) -> impl Iterator<Item = Symbol> + DoubleEndedIterator {
        self.ast.all_imports()
    }
    pub fn all_features(&self) -> impl Iterator<Item = Symbol> {
        self.ast.all_features()
    }
    pub fn all_attributes(&self) -> impl Iterator<Item = Symbol> {
        self.ast.all_attributes()
    }
    pub fn all_references(&self) -> impl Iterator<Item = Symbol> {
        self.ast.all_references()
    }
    pub fn all_constraints(&self) -> impl Iterator<Item = Symbol> {
        self.ast.all_constraints()
    }
    pub fn lang_lvl(&self, sym: Symbol) -> Option<&LanguageLevel> {
        if let Symbol::LangLvl(i) = sym {
            Some(&self.ast.includes[i].lang_lvl)
        } else {
            None
        }
    }
    pub fn group_mode(&self, sym: Symbol) -> Option<GroupMode> {
        match sym {
            Symbol::Group(id) => Some(self.ast.groups[id].mode.clone()),
            _ => None,
        }
    }
    pub fn constraint(&self, sym: Symbol) -> Option<&ConstraintDecl> {
        match sym {
            Symbol::Constraint(id) => Some(&self.ast.constraints[id]),
            _ => None,
        }
    }
    pub fn constraints(&self) -> &[ConstraintDecl] {
        &self.ast.constraints
    }

    pub fn imports(&self) -> &[Import] {
        &self.ast.import
    }

    pub fn value(&self, sym: Symbol) -> Option<&Value> {
        match sym {
            Symbol::Attribute(id) => Some(&self.ast.attributes[id].value.value),
            _ => None,
        }
    }
    pub fn direct_children(
        &self,
        sym: Symbol,
    ) -> impl Iterator<Item = Symbol> + DoubleEndedIterator + '_ {
        self.ast
            .structure
            .children
            .get(&sym)
            .into_iter()
            .flat_map(|i| i.iter())
            .cloned()
    }
    pub fn lsp_range(&self, sym: Symbol) -> Option<tower_lsp::lsp_types::Range> {
        self.ast.lsp_range(sym, &self.source)
    }

    pub fn span(&self, sym: Symbol) -> Option<Span> {
        self.ast.span(sym)
    }
    pub fn namespace(&self) -> Option<&Path> {
        self.ast.namespace.as_ref()
    }
    pub fn path(&self, sym: Symbol) -> &[Ustr] {
        match sym {
            Symbol::Import(i) => &self.ast.import[i].path.names,
            Symbol::Reference(i) => &self.ast.references[i].path.names,
            _ => unimplemented!(),
        }
    }
    pub fn import_prefix(&self, sym: Symbol) -> &[Ustr] {
        self.ast.import_prefix(sym)
    }
    pub fn depth(&self, sym: Symbol) -> u32 {
        match sym {
            Symbol::Feature(..) => 1,
            Symbol::Import(i) => self.ast.import[i].path.names.len() as u32,
            Symbol::Dir(i) => self.ast.dirs[i].depth,
            Symbol::Attribute(i) => self.ast.attributes[i].depth,
            _ => 0,
        }
    }
    //Find all symboles under root with prefix path.
    //Search branches can be aborted with a filter
    pub fn lookup<'a, F: Fn(Symbol) -> bool + 'a>(
        &'a self,
        root: Symbol,
        path: &'a [Ustr],
        filter: F,
    ) -> impl Iterator<Item = Symbol> + 'a {
        let mut stack = vec![(root, path)];
        std::iter::from_fn(move || loop {
            let (cur, base) = stack.pop()?;
            if base.is_empty() {
                return Some(cur);
            }
            self.ast.lookup(cur, base[0], |dst| {
                if filter(dst) {
                    stack.push((dst, &base[1..]));
                }
            })
        })
    }
    pub fn lookup_import<'a>(
        &'a self,
        path: &'a [Ustr],
    ) -> impl Iterator<Item = (Symbol, &'a [Ustr])> {
        let mut stack = vec![(Symbol::Root, path)];
        std::iter::from_fn(move || loop {
            let (cur, base) = stack.pop()?;

            if base.is_empty() {
                if matches!(cur, Symbol::Import(..)) {
                    return Some((cur, base));
                }
            }
            self.ast.lookup(cur, base[0], |dst| {
                if matches!(dst, Symbol::Dir(..) | Symbol::Import(..)) {
                    stack.push((dst, &base[1..]));
                }
            });
            if matches!(cur, Symbol::Import(..)) {
                return Some((cur, base));
            }
        })
    }
    //Also track the binding for path
    pub fn lookup_with_binding<'a, F: Fn(Symbol) -> bool + 'a>(
        &'a self,
        root: Symbol,
        path: &'a [Ustr],
        filter: F,
    ) -> impl Iterator<Item = Vec<Symbol>> + 'a {
        let mut stack = vec![(root, path, vec![])];
        std::iter::from_fn(move || loop {
            let (cur, base, bind) = stack.pop()?;
            if base.is_empty() {
                return Some(bind);
            }
            self.ast.lookup(cur, base[0], |dst| {
                if filter(dst) {
                    stack.push((dst, &base[1..], [bind.as_slice(), &[dst]].concat()));
                }
            })
        })
    }
    //prefix of sym from root
    pub fn prefix(&self, mut sym: Symbol) -> Vec<Ustr> {
        if matches!(sym, Symbol::Import(..)) {
            return self.ast.import_prefix(sym).into();
        }
        let mut out = Vec::new();
        loop {
            if let Some(name) = self.ast.name(sym) {
                out.push(name);
            }
            if let Some(p) = self.ast.structure.parent.get(&sym) {
                if matches!(p, Symbol::Feature(..)) {
                    break;
                }
                sym = *p;
            } else {
                break;
            }
        }
        out
    }
    pub fn type_of(&self, sym: Symbol) -> Option<Type> {
        match sym {
            Symbol::Root => Some(Type::Namespace),
            Symbol::Feature(i) => Some(self.ast.features[i].ty),
            Symbol::Attribute(i) => match &self.ast.attributes[i].value.value {
                Value::Void => Some(Type::Void),
                Value::Vector => Some(Type::Vector),
                Value::Bool(..) => Some(Type::Bool),
                Value::Attributes => Some(Type::Attributes),
                Value::String(..) => Some(Type::String),
                Value::Number(..) => Some(Type::Real),
            },
            Symbol::Import(..) => Some(Type::Namespace),
            Symbol::Dir(..) => Some(Type::Namespace),
            _ => None,
        }
    }
    pub fn find(&self, offset: usize) -> Option<Symbol> {
        self.ast.find(offset)
    }
    //All children under root, when merge_root_features sub features are ignored
    pub fn visit_named_children<F: FnMut(Symbol, &[Ustr]) -> bool>(
        &self,
        root: Symbol,
        merge_root_features: bool,
        mut f: F,
    ) {
        self.visit_named_children_depth(root, merge_root_features, |sym, prefix, _| f(sym, prefix))
    }

    pub fn visit_named_children_depth<F: FnMut(Symbol, &[Ustr], usize) -> bool>(
        &self,
        root: Symbol,
        merge_root_features: bool,
        mut f: F,
    ) {
        let mut stack: Vec<(Symbol, usize)> = vec![(root, 0)];
        let mut prefix = vec![];
        while let Some((cur, depth)) = stack.pop() {
            prefix.truncate(depth.saturating_sub(1));
            let mut explore = true;
            if let Some(name) = self.name(cur) {
                if cur != root {
                    prefix.push(name);
                    explore = f(cur, &prefix, depth);
                }
            }
            if explore {
                for i in self.ast.children(cur).rev() {
                    if merge_root_features
                        && !matches!(i, Symbol::Attribute(..))
                        && !matches!(root, Symbol::Root)
                    {
                        continue;
                    }
                    match i {
                        Symbol::Feature(..) => {
                            stack.push((i, 1));
                        }
                        Symbol::Attribute(..) | Symbol::Dir(..) | Symbol::Import(..) => {
                            stack.push((i, depth + 1));
                        }
                        _ => {
                            stack.push((i, depth));
                        }
                    }
                }
            }
        }
    }
    pub fn visit_children<F: FnMut(Symbol) -> bool>(
        &self,
        root: Symbol,
        merge_root_features: bool,
        mut f: F,
    ) {
        self.visit_children_depth(root, merge_root_features, |sym, _| f(sym));
    }
    pub fn visit_children_depth<F: FnMut(Symbol, u32) -> bool>(
        &self,
        root: Symbol,
        merge_root_features: bool,
        mut f: F,
    ) {
        let mut stack = vec![(root, 0)];
        while let Some((cur, depth)) = stack.pop() {
            let mut explore = true;
            if cur != root {
                explore = f(cur, depth);
            }
            if explore {
                for i in self.ast.children(cur).rev() {
                    if merge_root_features
                        && matches!(i, Symbol::Feature(..))
                        && !matches!(root, Symbol::Root)
                    {
                        continue;
                    }
                    stack.push((i, depth + 1));
                }
            }
        }
    }
    pub fn visit_attributes<'a, F: FnMut(Symbol, Symbol, &[Ustr])>(&self, root: Symbol, mut f: F) {
        assert!(matches!(root, Symbol::Feature(..) | Symbol::Root));
        let mut owner = root;
        let mut under_feature = 0;
        self.visit_named_children(root, false, |i, prefix| match i {
            Symbol::Feature(..) => {
                owner = i;
                under_feature = 1;
                true
            }
            Symbol::Attribute(..) => {
                f(owner, i, &prefix[under_feature..]);
                true
            }
            _ => false,
        });
    }
}
