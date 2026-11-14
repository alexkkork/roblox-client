//! Bounded, snapshot-only implementation of Instance:QueryDescendants.
use super::{WorldRef, get_property, instance};
use mlua::{Lua, Table, Value};
use serde_json::{Value as Json, json};

#[derive(Clone)]
enum Filter {
    All,
    Class(String),
    Name(String),
    Tag(String),
    Property(String, Option<Json>),
    Attribute(String, Option<Json>),
    Not(Vec<Vec<Step>>),
    Has(Vec<Vec<Step>>),
}
#[derive(Clone)]
struct Step {
    direct: bool,
    filters: Vec<Filter>,
}

struct Parser {
    chars: Vec<char>,
    at: usize,
    filters: usize,
    steps: usize,
}
impl Parser {
    fn new(text: &str) -> Result<Self, String> {
        if text.is_empty() || text.len() > 4096 || text.contains('\0') {
            return Err("selector must be nonempty UTF-8 of at most 4096 bytes without NUL".into());
        }
        Ok(Self {
            chars: text.chars().collect(),
            at: 0,
            filters: 0,
            steps: 0,
        })
    }
    fn peek(&self) -> Option<char> {
        self.chars.get(self.at).copied()
    }
    fn ws(&mut self) {
        while self.peek().is_some_and(char::is_whitespace) {
            self.at += 1;
        }
    }
    fn err<T>(&self) -> Result<T, String> {
        Err(format!(
            "invalid or unsupported selector near byte {}",
            self.at + 1
        ))
    }
    fn ident_char(c: char) -> bool {
        c.is_alphanumeric() || matches!(c, '_' | '-') || !c.is_ascii()
    }
    fn identifier(&mut self) -> Result<String, String> {
        let start = self.at;
        while self.peek().is_some_and(Self::ident_char) {
            self.at += 1;
        }
        if start == self.at {
            return self.err();
        }
        Ok(self.chars[start..self.at].iter().collect())
    }
    fn quoted(&mut self) -> Result<String, String> {
        let q = self
            .peek()
            .ok_or_else(|| format!("invalid selector near byte {}", self.at + 1))?;
        self.at += 1;
        let mut out = String::new();
        while let Some(c) = self.peek() {
            self.at += 1;
            if c == q {
                return Ok(out);
            }
            if c == '\\' {
                let Some(e) = self.peek() else {
                    return self.err();
                };
                self.at += 1;
                out.push(match e {
                    'n' => '\n',
                    'r' => '\r',
                    't' => '\t',
                    '\\' | '\'' | '"' => e,
                    _ => return self.err(),
                });
            } else {
                out.push(c)
            }
        }
        self.err()
    }
    fn literal(&mut self) -> Result<Json, String> {
        match self.peek() {
            Some('\'' | '"') => return self.quoted().map(Json::String),
            Some(_) => {}
            None => return self.err(),
        };
        let start = self.at;
        while self
            .peek()
            .is_some_and(|c| !c.is_whitespace() && c != ']' && c != ')' && c != ',')
        {
            self.at += 1;
        }
        let t: String = self.chars[start..self.at].iter().collect();
        if t == "true" {
            return Ok(json!(true));
        }
        if t == "false" {
            return Ok(json!(false));
        }
        if t == "nil" || t.is_empty() {
            return self.err();
        }
        if let Ok(n) = t.parse::<f64>() {
            if n.is_finite() {
                return Ok(json!(n));
            }
            return self.err();
        }
        if t.chars().all(Self::ident_char) {
            Ok(json!(t))
        } else {
            self.err()
        }
    }
    fn filter(&mut self, depth: usize) -> Result<Filter, String> {
        self.filters += 1;
        if self.filters > 128 {
            return Err("selector filter limit exceeded".into());
        }
        match self.peek() {
            Some('*') => {
                self.at += 1;
                Ok(Filter::All)
            }
            Some('.') | Some('#') => {
                let ch = self.peek().unwrap();
                self.at += 1;
                let name = self.identifier()?;
                Ok(if ch == '.' {
                    Filter::Tag(name)
                } else {
                    Filter::Name(name)
                })
            }
            Some('[') => {
                self.at += 1;
                self.ws();
                let attr = if self.peek() == Some('$') {
                    self.at += 1;
                    true
                } else {
                    false
                };
                let name = self.identifier()?;
                self.ws();
                let val = if self.peek() == Some('=') {
                    self.at += 1;
                    self.ws();
                    Some(self.literal()?)
                } else {
                    None
                };
                self.ws();
                if self.peek() != Some(']') || (!attr && val.is_none()) {
                    return self.err();
                }
                self.at += 1;
                Ok(if attr {
                    Filter::Attribute(name, val)
                } else {
                    Filter::Property(name, val)
                })
            }
            Some(':') => {
                self.at += 1;
                let name = self.identifier()?;
                if !matches!(name.as_str(), "not" | "has") || self.peek() != Some('(') {
                    return self.err();
                }
                self.at += 1;
                let nested = self.list(depth + 1, Some(')'))?;
                if self.peek() != Some(')') {
                    return self.err();
                }
                self.at += 1;
                Ok(if name == "not" {
                    Filter::Not(nested)
                } else {
                    Filter::Has(nested)
                })
            }
            _ => self.identifier().map(Filter::Class),
        }
    }
    fn chain(&mut self, depth: usize) -> Result<Vec<Step>, String> {
        let mut out = Vec::new();
        let mut direct = false;
        if self.peek() == Some('>') {
            self.at += 1;
            if self.peek() == Some('>') {
                self.at += 1;
                direct = false
            } else {
                direct = true
            }
            self.ws()
        }
        loop {
            self.steps += 1;
            if self.steps > 64 {
                return Err("selector step limit exceeded".into());
            }
            if self.peek().is_none() || matches!(self.peek(), Some(',' | ')' | '>')) {
                return self.err();
            }
            let mut step = Step {
                direct,
                filters: vec![self.filter(depth)?],
            };
            while matches!(self.peek(), Some('.' | '#' | '[' | ':')) {
                step.filters.push(self.filter(depth)?)
            }
            out.push(step);
            self.ws();
            if self.peek() != Some('>') {
                break;
            }
            self.at += 1;
            direct = true;
            if self.peek() == Some('>') {
                self.at += 1;
                direct = false
            }
            self.ws()
        }
        Ok(out)
    }
    fn list(&mut self, depth: usize, stop: Option<char>) -> Result<Vec<Vec<Step>>, String> {
        if depth > 8 {
            return Err("selector nesting limit exceeded".into());
        }
        let mut out = Vec::new();
        self.ws();
        loop {
            out.push(self.chain(depth)?);
            self.ws();
            if self.peek() == Some(',') {
                self.at += 1;
                self.ws();
                continue;
            }
            if stop.is_some() && self.peek() != stop {
                return self.err();
            }
            break;
        }
        Ok(out)
    }
    fn parse(mut self) -> Result<Vec<Vec<Step>>, String> {
        let list = self.list(0, None)?;
        if self.at != self.chars.len() {
            return self.err();
        }
        Ok(list)
    }
}

struct Eval<'a> {
    ctx: &'a WorldRef,
    parents: std::collections::HashMap<String, String>,
    children: std::collections::HashMap<String, Vec<String>>,
    work: usize,
    matches: std::collections::HashMap<(String, String, usize, usize), bool>,
}
impl Eval<'_> {
    fn tick(&mut self) -> Result<(), String> {
        self.work += 1;
        if self.work > 2_000_000 {
            Err("selector work limit exceeded".into())
        } else {
            Ok(())
        }
    }
    fn descendants(&mut self, root: &str) -> Result<Vec<String>, String> {
        let mut out = Vec::new();
        let mut seen = std::collections::HashSet::from([root.to_owned()]);
        let mut pending = Vec::new();
        if let Some(children) = self.children.get(root) {
            pending.extend(children.iter().rev().cloned())
        }
        while let Some(id) = pending.pop() {
            self.tick()?;
            if !seen.insert(id.clone()) {
                continue;
            }
            out.push(id.clone());
            if out.len() > 100_000 {
                return Err("descendant limit exceeded".into());
            }
            if let Some(children) = self.children.get(&id) {
                pending.extend(children.iter().rev().cloned())
            }
        }
        Ok(out)
    }
    fn match_filter(&mut self, id: &str, f: &Filter, scope: &str) -> Result<bool, String> {
        self.tick()?;
        let (class, name, tags, attrs_known, attrs) = {
            let w = self.ctx.borrow();
            let Some(n) = w.nodes.get(id) else {
                return Ok(false);
            };
            (
                n.class_name.clone(),
                n.name.clone(),
                n.collection_tags.clone(),
                n.attributes_known,
                n.attributes.clone(),
            )
        };
        Ok(match f {
            Filter::All => true,
            Filter::Class(c) => super::class_chain(&class).iter().any(|p| p == c),
            Filter::Name(n) => &name == n,
            Filter::Tag(t) => tags.iter().any(|x| x == t),
            Filter::Property(key, expected) => {
                let key = super::canonical_member(&class, key);
                match get_property(self.ctx, id, &key) {
                    Ok(value) => expected.as_ref().is_none_or(|x| json_equal(&value, x)),
                    Err(_) if property_is_known(&class, &key) => {
                        return Err("query property is unavailable in this snapshot".into());
                    }
                    Err(_) => false,
                }
            }
            Filter::Attribute(key, expected) => match attrs.get(key) {
                Some(v) if !v.is_null() => expected.as_ref().is_none_or(|x| json_equal(v, x)),
                Some(_) | None if !attrs_known => {
                    return Err("query attribute state is unavailable in this snapshot".into());
                }
                _ => false,
            },
            Filter::Not(list) => !self.match_list(id, list, scope)?,
            Filter::Has(list) => {
                let ids = self.descendants(id)?;
                let mut found = false;
                for child in ids {
                    if self.match_list(&child, list, id)? {
                        found = true;
                        break;
                    }
                }
                found
            }
        })
    }
    fn match_chain(
        &mut self,
        id: &str,
        chain: &[Step],
        idx: usize,
        scope: &str,
    ) -> Result<bool, String> {
        self.tick()?;
        let key = (
            id.to_owned(),
            scope.to_owned(),
            chain.as_ptr() as usize,
            idx,
        );
        if let Some(result) = self.matches.get(&key) {
            return Ok(*result);
        }
        let result = self.evaluate_chain(id, chain, idx, scope)?;
        if self.matches.len() < 131_072 {
            self.matches.insert(key, result);
        }
        Ok(result)
    }
    fn evaluate_chain(
        &mut self,
        id: &str,
        chain: &[Step],
        idx: usize,
        scope: &str,
    ) -> Result<bool, String> {
        if id == scope {
            return Ok(false);
        }
        for f in &chain[idx].filters {
            if !self.match_filter(id, f, scope)? {
                return Ok(false);
            }
        }
        let parent = self.parents.get(id).cloned();
        if idx == 0 {
            return Ok(!chain[0].direct || parent.as_deref() == Some(scope));
        }
        if chain[idx].direct {
            return match parent {
                Some(p) if p != scope => self.match_chain(&p, chain, idx - 1, scope),
                _ => Ok(false),
            };
        }
        let mut p = parent;
        let mut seen = std::collections::HashSet::new();
        while let Some(current) = p {
            if current == scope || !seen.insert(current.clone()) {
                break;
            }
            if self.match_chain(&current, chain, idx - 1, scope)? {
                return Ok(true);
            }
            p = self.parents.get(&current).cloned();
            self.tick()?
        }
        Ok(false)
    }
    fn match_list(&mut self, id: &str, list: &[Vec<Step>], scope: &str) -> Result<bool, String> {
        for chain in list {
            if self.match_chain(id, chain, chain.len() - 1, scope)? {
                return Ok(true);
            }
        }
        Ok(false)
    }
}
fn json_equal(a: &Json, b: &Json) -> bool {
    if let (Some(x), Some(y)) = (a.as_f64(), b.as_f64()) {
        (x - y).abs() <= f64::EPSILON
    } else {
        a == b
    }
}
fn property_is_known(class: &str, key: &str) -> bool {
    let classes = super::constructors().get("classes");
    let lower = key.to_ascii_lowercase();
    let mut current = Some(class);
    let mut seen = std::collections::HashSet::new();
    while let Some(c) = current {
        if !seen.insert(c.to_owned()) {
            break;
        }
        if let Some(desc) = classes.and_then(|v| v.get(c)) {
            if desc
                .get("properties")
                .and_then(|p| p.get(key).or_else(|| p.get(&lower)))
                .is_some()
            {
                return true;
            }
            current = desc.get("parent").and_then(Json::as_str)
        } else {
            current = super::class_parents()
                .get(c)
                .and_then(|p| p.first())
                .map(String::as_str)
        }
    }
    false
}

pub(super) fn query(lua: &Lua, ctx: &WorldRef, root: &str, selector: &str) -> mlua::Result<Table> {
    let list = Parser::new(selector)
        .and_then(Parser::parse)
        .map_err(|e| mlua::Error::RuntimeError(format!("QueryDescendants failed: {e}")))?;
    let (order, live, relations) = {
        let w = ctx.borrow();
        let live = w
            .nodes
            .iter()
            .filter(|(_, n)| !n.destroyed)
            .map(|(id, _)| id.clone())
            .collect::<std::collections::HashSet<_>>();
        let relations = w
            .node_order
            .iter()
            .filter_map(|id| {
                w.nodes
                    .get(id)
                    .filter(|n| !n.destroyed)
                    .and_then(|n| n.parent.clone().map(|p| (id.clone(), p)))
            })
            .collect::<Vec<_>>();
        (w.node_order.clone(), live, relations)
    };
    let work = order.len();
    if work > 2_000_000 {
        return Err(mlua::Error::RuntimeError(
            "QueryDescendants failed: selector work limit exceeded".into(),
        ));
    }
    let (mut parents, mut children) = (
        std::collections::HashMap::new(),
        std::collections::HashMap::<String, Vec<String>>::new(),
    );
    for (id, parent) in relations {
        if live.contains(&parent) {
            parents.insert(id.clone(), parent.clone());
            children.entry(parent).or_default().push(id);
        }
    }
    let mut eval = Eval {
        ctx,
        parents,
        children,
        work,
        matches: std::collections::HashMap::new(),
    };
    let ids = if live.contains(root) {
        eval.descendants(root)
            .map_err(|e| mlua::Error::RuntimeError(format!("QueryDescendants failed: {e}")))?
    } else {
        Vec::new()
    };
    let mut selected = Vec::new();
    for id in ids {
        if eval
            .match_list(&id, &list, root)
            .map_err(|e| mlua::Error::RuntimeError(format!("QueryDescendants failed: {e}")))?
        {
            selected.push(id)
        }
    }
    let table = lua.create_table_with_capacity(selected.len(), 0)?;
    for (i, id) in selected.iter().enumerate() {
        table.raw_set(i + 1, instance(lua, ctx, id)?)?
    }
    Ok(table)
}
