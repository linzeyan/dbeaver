//! Enough structure to know what a name at the caret would mean.
//!
//! Not a grammar. A grammar is the wrong tool for the job this has: the input
//! is always invalid, because the user is in the middle of typing it, and a
//! parser built to recognise correct SQL spends its life in error recovery and
//! answers "no" on the one input that matters. `SELECT ▮ FROM orders o` is
//! nonsense to the server and is exactly when completion is wanted, and the
//! answer — the columns of `orders` — is written *after* the caret.
//!
//! So what is built here is the smallest tree that carries that answer: a
//! nest of scopes, each holding the relations visible inside it and the names
//! they were given. Everything else about a statement is skipped rather than
//! described. A subquery is a scope inside a scope, a CTE is a name bound in
//! the scope that declares it, and text this does not understand is text it
//! walks past, which is what makes broken input cost nothing.
//!
//! The dialect reaches this only through the token stream. There is no branch
//! on which database is being read, which is the point: `QUALIFY` is a keyword
//! in DuckDB and a column name in PostgreSQL, and that difference is already
//! settled by the time a token gets here.

use crate::dialect::Dialect;
use crate::lex::{Token, TokenKind, tokens};
use crate::script::Span;

/// A relation something can be selected from, as written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    /// The qualifier, when the name was written with one.
    pub schema: Option<String>,
    /// The relation's name, or the alias of a subquery that has no name of its
    /// own. Empty for a subquery nobody named, which is legal and useless.
    pub name: String,
    /// What it answers to in this statement. `None` when it answers to its own
    /// name.
    pub alias: Option<String>,
    /// Whether `name` is a CTE or a derived table rather than something the
    /// catalog knows. Completion asks the catalog about the others and must not
    /// ask about these.
    pub derived: bool,
}

impl Source {
    /// The name this answers to when a column is qualified.
    pub fn handle(&self) -> &str {
        self.alias.as_deref().unwrap_or(&self.name)
    }
}

/// One place names are resolved: a statement, a subquery, a CTE body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scope {
    /// The scope this one is written inside, if any.
    pub parent: Option<usize>,
    /// The characters this scope covers. An unclosed subquery runs to the end
    /// of the statement, which is the right answer while it is being typed.
    pub span: Span,
    /// What can be selected from here, in the order written.
    pub sources: Vec<Source>,
}

/// The scopes of one statement, outermost first.
#[derive(Debug, Clone, Default)]
pub struct Statement {
    pub scopes: Vec<Scope>,
}

impl Statement {
    /// The innermost scope containing `caret`.
    ///
    /// Innermost because that is where a name is looked up first: inside a
    /// subquery, its own FROM list shadows the one outside it.
    pub fn scope_at(&self, caret: u32) -> Option<usize> {
        self.scopes
            .iter()
            .enumerate()
            .filter(|(_, s)| s.span.start <= caret && caret <= s.span.end)
            .map(|(i, _)| i)
            .next_back()
    }

    /// Every source visible at `caret`, innermost scope first.
    ///
    /// A subquery can name the tables of the query around it — that is what a
    /// correlated subquery is — so this walks outwards rather than stopping at
    /// the first scope.
    pub fn sources_at(&self, caret: u32) -> Vec<&Source> {
        let mut out = Vec::new();
        let mut at = self.scope_at(caret);
        while let Some(i) = at {
            out.extend(self.scopes[i].sources.iter());
            at = self.scopes[i].parent;
        }
        out
    }
}

/// The scope tree of the statement covering `span`.
pub fn statement(text: &str, span: Span, dialect: &Dialect) -> Statement {
    let chars: Vec<char> = text.chars().collect();
    scopes(&tokens(text, dialect), &chars, span)
}

/// The same, for a caller that has already lexed the buffer.
pub(crate) fn scopes(all: &[Token], chars: &[char], span: Span) -> Statement {
    let within: Vec<Token> = all
        .iter()
        .copied()
        .filter(|t| t.start >= span.start && t.end <= span.end && !t.kind.is_trivia())
        .collect();
    Walk {
        chars,
        tokens: &within,
        at: 0,
        end: span.end,
        out: Statement::default(),
    }
    .run()
}

struct Walk<'a> {
    chars: &'a [char],
    tokens: &'a [Token],
    at: usize,
    end: u32,
    out: Statement,
}

impl Walk<'_> {
    fn run(mut self) -> Statement {
        let start = self.tokens.first().map(|t| t.start).unwrap_or(self.end);
        let root = self.push_scope(None, start);
        self.body(root);
        self.out
    }

    fn push_scope(&mut self, parent: Option<usize>, start: u32) -> usize {
        self.out.scopes.push(Scope {
            parent,
            span: start..self.end,
            sources: Vec::new(),
        });
        self.out.scopes.len() - 1
    }

    /// The text of a token, as written.
    fn text(&self, token: &Token) -> String {
        self.chars[token.start as usize..token.end as usize]
            .iter()
            .collect()
    }

    /// A token's text with its quoting removed, which is the name the catalog
    /// holds. `"Order"` and `order` are different names and both arrive here as
    /// what is between the delimiters.
    fn name(&self, token: &Token) -> String {
        let raw = self.text(token);
        match token.kind {
            TokenKind::QuotedIdentifier => {
                let inner = &raw[raw.char_indices().nth(1).map_or(raw.len(), |(i, _)| i)
                    ..raw.char_indices().next_back().map_or(0, |(i, _)| i)];
                // A doubled delimiter is one character, the same rule the lexer
                // reads it by.
                match raw.chars().next() {
                    Some('"') => inner.replace("\"\"", "\""),
                    Some('`') => inner.replace("``", "`"),
                    Some('[') => inner.replace("]]", "]"),
                    _ => inner.to_string(),
                }
            }
            _ => raw,
        }
    }

    fn peek(&self, ahead: usize) -> Option<&Token> {
        self.tokens.get(self.at + ahead)
    }

    /// Whether the token `ahead` is the keyword `word`, case-folded.
    fn is_word(&self, ahead: usize, word: &str) -> bool {
        self.peek(ahead).is_some_and(|t| {
            matches!(t.kind, TokenKind::Keyword | TokenKind::Identifier)
                && self.text(t).eq_ignore_ascii_case(word)
        })
    }

    fn is_any(&self, ahead: usize, words: &[&str]) -> bool {
        words.iter().any(|w| self.is_word(ahead, w))
    }

    fn is_punct(&self, ahead: usize, c: char) -> bool {
        self.peek(ahead)
            .is_some_and(|t| t.len() == 1 && self.chars[t.start as usize] == c)
    }

    /// Walks a scope's tokens, collecting what it can and skipping the rest.
    ///
    /// Returns having consumed the `)` that closed this scope, or at the end of
    /// the statement.
    fn body(&mut self, scope: usize) {
        while self.at < self.tokens.len() {
            if self.is_punct(0, ')') {
                self.out.scopes[scope].span.end = self.tokens[self.at].end;
                self.at += 1;
                return;
            }
            if self.is_punct(0, '(') {
                self.paren(scope);
                continue;
            }
            // The words after which a relation is named. `FROM` and `JOIN`
            // introduce lists; `UPDATE` and `INTO` name one, which the same
            // code reads as a list of one.
            if self.is_any(0, &["from", "join", "update", "into"]) {
                self.at += 1;
                self.source_list(scope);
                continue;
            }
            if self.is_word(0, "with") {
                self.at += 1;
                self.cte_list(scope);
                continue;
            }
            self.at += 1;
        }
        self.out.scopes[scope].span.end = self.end;
    }

    /// A parenthesised run. A new scope when it holds a query, and otherwise
    /// just something to walk past with its nesting respected.
    fn paren(&mut self, scope: usize) {
        let open = self.tokens[self.at];
        let query = self.is_any(1, &["select", "with", "values", "table"]);
        self.at += 1;
        if query {
            let inner = self.push_scope(Some(scope), open.end);
            self.body(inner);
        } else {
            // Not a scope, but the parentheses still have to be balanced or a
            // `)` belonging to an argument list would close the query around it.
            let mut depth = 1usize;
            while self.at < self.tokens.len() && depth > 0 {
                if self.is_punct(0, '(') {
                    depth += 1;
                } else if self.is_punct(0, ')') {
                    depth -= 1;
                }
                self.at += 1;
            }
        }
    }

    /// `WITH name AS (…), name AS (…)`, binding each name in `scope`.
    fn cte_list(&mut self, scope: usize) {
        // `WITH RECURSIVE` says nothing about the names.
        if self.is_word(0, "recursive") {
            self.at += 1;
        }
        loop {
            let Some(token) = self.peek(0) else { return };
            if !matches!(
                token.kind,
                TokenKind::Identifier | TokenKind::QuotedIdentifier
            ) {
                return;
            }
            let name = self.name(token);
            self.at += 1;
            // An optional column list, which is not a scope.
            if self.is_punct(0, '(') {
                self.paren(scope);
            }
            if !self.is_word(0, "as") {
                return;
            }
            self.at += 1;
            self.out.scopes[scope].sources.push(Source {
                schema: None,
                name,
                alias: None,
                derived: true,
            });
            if self.is_punct(0, '(') {
                self.paren(scope);
            }
            if !self.is_punct(0, ',') {
                return;
            }
            self.at += 1;
        }
    }

    /// The relations after a `FROM`, `JOIN`, `UPDATE` or `INTO`.
    fn source_list(&mut self, scope: usize) {
        loop {
            if self.is_punct(0, '(') {
                // A derived table. Its own scope, and whatever it is called
                // afterwards is a name in this one.
                self.paren(scope);
                let alias = self.alias(scope);
                self.out.scopes[scope].sources.push(Source {
                    schema: None,
                    name: alias.clone().unwrap_or_default(),
                    alias,
                    derived: true,
                });
            } else if let Some(source) = self.qualified_name(scope) {
                self.out.scopes[scope].sources.push(source);
            } else {
                return;
            }

            // A join reads as another source; anything else ends the list.
            if self.is_punct(0, ',') {
                self.at += 1;
                continue;
            }
            while self.is_any(
                0,
                &[
                    "inner", "outer", "left", "right", "full", "cross", "natural", "lateral",
                ],
            ) {
                self.at += 1;
            }
            if self.is_word(0, "join") {
                self.at += 1;
                continue;
            }
            if self.is_word(0, "on") || self.is_word(0, "using") {
                // The join condition is not a source list, and the sources
                // after it are reached by the loop in `body`.
                return;
            }
            return;
        }
    }

    /// `[schema.]name [[AS] alias]`, or nothing when what is here is not one.
    fn qualified_name(&mut self, scope: usize) -> Option<Source> {
        let first = self.peek(0)?;
        if !matches!(
            first.kind,
            TokenKind::Identifier | TokenKind::QuotedIdentifier
        ) {
            return None;
        }
        let mut schema = None;
        let mut name = self.name(first);
        self.at += 1;

        // Two levels are taken and no more. DuckDB and SQL Server have three,
        // and the catalog level is dropped rather than mistaken for a schema:
        // a wrong schema sends completion to look somewhere that exists and is
        // not where the table is, which is worse than looking in the default.
        while self.is_punct(0, '.') {
            self.at += 1;
            let Some(part) = self.peek(0) else {
                // `sales.` with the relation not typed yet. There is no source
                // here, and inventing one called `sales` would make the schema
                // look like a table that is in scope.
                return None;
            };
            if !matches!(
                part.kind,
                TokenKind::Identifier | TokenKind::QuotedIdentifier
            ) {
                return None;
            }
            schema = Some(name);
            name = self.name(part);
            self.at += 1;
        }

        // A table function's arguments, which are not a source list.
        if self.is_punct(0, '(') {
            self.paren(scope);
        }
        let alias = self.alias(scope);
        Some(Source {
            schema,
            name,
            alias,
            derived: false,
        })
    }

    /// `[AS] name`, when what follows is a name and not the next clause.
    fn alias(&mut self, scope: usize) -> Option<String> {
        if self.is_word(0, "as") {
            self.at += 1;
        }
        let token = self.peek(0)?;
        // A keyword after a source is the next clause, not an alias. The
        // exception a dialect table cannot help with is that some of these are
        // ordinary names elsewhere, which is why this looks at the kind the
        // lexer assigned rather than at a list of its own.
        if !matches!(
            token.kind,
            TokenKind::Identifier | TokenKind::QuotedIdentifier
        ) {
            return None;
        }
        let name = self.name(token);
        self.at += 1;
        // A column list after an alias, as in `t(a, b)`.
        if self.is_punct(0, '(') {
            self.paren(scope);
        }
        Some(name)
    }
}
