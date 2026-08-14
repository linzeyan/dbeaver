//! One pass over a buffer, naming every construct in it.
//!
//! Positions are counted in **characters**, from zero. That is the unit
//! `DbError::statement_position` reports in — one-based, so the conversion is
//! a subtraction — and the unit a Swift `String.unicodeScalars` index is, which
//! is what the editor needs to place a caret. Bytes would be cheaper and would
//! put every offset after an accented letter in the wrong place.
//!
//! The scan never fails. An unterminated quote runs to the end of the buffer
//! rather than reporting an error, because the user is mid-keystroke and what
//! follows an opening quote really is inside a string until proven otherwise;
//! treating a semicolon inside the half-written literal as a boundary would run
//! half a string as though it were a statement.

use crate::dialect::{Dialect, DoubleQuote, Parameter};

/// What one run of characters turned out to be.
///
/// The set is the splitter's and the highlighter's together, which is why
/// `Identifier` and `Keyword` are separate categories of what is lexically the
/// same thing: the splitter does not care and the editor paints only one of
/// them, and separating them here means the word list is consulted once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    /// `;`, and only where it separates — not one inside a literal.
    Terminator,
    Keyword,
    Identifier,
    /// `"name"`, `` `name` ``, `[name]`.
    QuotedIdentifier,
    /// Any string literal, prefix and all.
    String,
    /// `$tag$ … $tag$`.
    DollarQuoted,
    Number,
    Comment,
    /// `$1`, `?`, `@name`, `:name`.
    Parameter,
    Whitespace,
    /// Operators, punctuation, and anything else a character can be.
    Other,
}

impl TokenKind {
    /// Whether this is something the server ignores.
    ///
    /// The one place the distinction matters is deciding whether a run of text
    /// between two semicolons is a statement at all: a chunk holding only these
    /// has nothing in it to run.
    pub fn is_trivia(self) -> bool {
        matches!(self, TokenKind::Whitespace | TokenKind::Comment)
    }
}

/// One construct, and where it is. `start..end` in characters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Token {
    pub kind: TokenKind,
    pub start: u32,
    pub end: u32,
}

impl Token {
    pub fn len(&self) -> u32 {
        self.end - self.start
    }

    pub fn is_empty(&self) -> bool {
        self.end == self.start
    }
}

/// Every token in `text`, in order, covering every character exactly once.
///
/// Trivia included. A highlighter wants a fraction of these and a parser wants
/// all of them, and producing the full stream once is what stops the two from
/// having separate opinions about where a string ends — which they would, the
/// first time one of them was fixed.
pub fn tokens(text: &str, dialect: &Dialect) -> Vec<Token> {
    Lexer {
        chars: text.chars().collect(),
        dialect,
        at: 0,
    }
    .run()
}

struct Lexer<'a> {
    chars: Vec<char>,
    dialect: &'a Dialect,
    at: usize,
}

impl Lexer<'_> {
    fn run(mut self) -> Vec<Token> {
        // Roughly one token per three characters in real SQL once whitespace is
        // counted. A starting guess, not a bound.
        let mut out = Vec::with_capacity(self.chars.len() / 3 + 1);
        while self.at < self.chars.len() {
            let start = self.at;
            let kind = self.step();
            debug_assert!(self.at > start, "the scan must always advance");
            out.push(Token {
                kind,
                start: start as u32,
                end: self.at as u32,
            });
        }
        out
    }

    /// One construct, consuming it. The order of these tests is the grammar's
    /// and cannot be shuffled.
    fn step(&mut self) -> TokenKind {
        let c = self.chars[self.at];

        if c == ';' {
            self.at += 1;
            return TokenKind::Terminator;
        }
        if c.is_whitespace() {
            while self.at < self.chars.len() && self.chars[self.at].is_whitespace() {
                self.at += 1;
            }
            return TokenKind::Whitespace;
        }
        if c == '-' && self.peek(1) == Some('-') {
            return self.line_comment();
        }
        if c == '#' && self.dialect.hash_line_comments {
            return self.line_comment();
        }
        if c == '/' && self.peek(1) == Some('*') {
            return self.block_comment();
        }
        if c == '\'' {
            self.string('\'', self.dialect.backslash_escapes);
            return TokenKind::String;
        }
        if c == '"' {
            return match self.dialect.double_quote {
                DoubleQuote::Identifier => {
                    self.string('"', false);
                    TokenKind::QuotedIdentifier
                }
                DoubleQuote::String => {
                    self.string('"', self.dialect.backslash_escapes);
                    TokenKind::String
                }
            };
        }
        if c == '`' && self.dialect.backtick_identifiers {
            self.string('`', false);
            return TokenKind::QuotedIdentifier;
        }
        if c == '[' && self.dialect.bracket_identifiers {
            return self.bracket_identifier();
        }
        // Before the word test, because `$` continues an identifier as well as
        // opening a body and only this knows which.
        if c == '$'
            && let Some(kind) = self.dollar()
        {
            return kind;
        }
        if let Some(kind) = self.parameter() {
            return kind;
        }
        // Before the word test, because both can start on a digit and only one
        // of them is reached with the digit first.
        if self.number() {
            return TokenKind::Number;
        }
        if is_identifier_char(c) {
            return self.word();
        }
        self.at += 1;
        TokenKind::Other
    }

    fn peek(&self, ahead: usize) -> Option<char> {
        self.chars.get(self.at + ahead).copied()
    }

    fn line_comment(&mut self) -> TokenKind {
        while self.at < self.chars.len() && self.chars[self.at] != '\n' {
            self.at += 1;
        }
        TokenKind::Comment
    }

    fn block_comment(&mut self) -> TokenKind {
        let mut depth = 0usize;
        while self.at < self.chars.len() {
            if self.chars[self.at] == '/' && self.peek(1) == Some('*') {
                depth += 1;
                self.at += 2;
                // A dialect whose comments do not nest sees every later `/*` as
                // ordinary text inside the comment, so the first `*/` closes it.
                if !self.dialect.nested_block_comments {
                    depth = 1;
                }
            } else if self.chars[self.at] == '*' && self.peek(1) == Some('/') {
                self.at += 2;
                depth -= 1;
                if depth == 0 {
                    return TokenKind::Comment;
                }
            } else {
                self.at += 1;
            }
        }
        TokenKind::Comment
    }

    /// Consumes a run delimited by `quote`, doubling included, ending at the
    /// close or at the end of the buffer.
    fn string(&mut self, quote: char, escapes: bool) {
        self.at += 1;
        while self.at < self.chars.len() {
            let c = self.chars[self.at];
            if escapes && c == '\\' {
                self.at += 2;
                continue;
            }
            if c == quote {
                // Doubled is one embedded delimiter, which is how both a
                // literal and an identifier carry their own.
                if self.peek(1) == Some(quote) {
                    self.at += 2;
                    continue;
                }
                self.at += 1;
                return;
            }
            self.at += 1;
        }
        self.at = self.chars.len();
    }

    /// `[name]`, whose only escape is `]]`. There is no backslash inside one.
    fn bracket_identifier(&mut self) -> TokenKind {
        self.at += 1;
        while self.at < self.chars.len() {
            if self.chars[self.at] == ']' {
                if self.peek(1) == Some(']') {
                    self.at += 2;
                    continue;
                }
                self.at += 1;
                return TokenKind::QuotedIdentifier;
            }
            self.at += 1;
        }
        TokenKind::QuotedIdentifier
    }

    /// `$tag$ … $tag$`, or nothing when the `$` opens nothing.
    ///
    /// Two things make this more than a search for the next `$…$`. A tag may
    /// not begin with a digit, which is what keeps `$1` a placeholder rather
    /// than the start of a body running to the end of the script. And `$` is a
    /// legal identifier continuation, so `a$b$c` is one name to the server and
    /// has to be one here — hence the look back before the dollar.
    fn dollar(&mut self) -> Option<TokenKind> {
        if !self.dialect.dollar_quoting {
            return None;
        }
        let open = self.at;
        if open > 0 && is_identifier_char(self.chars[open - 1]) {
            return None;
        }
        let mut i = open + 1;
        while i < self.chars.len() && is_tag_char(self.chars[i], i == open + 1) {
            i += 1;
        }
        if self.chars.get(i) != Some(&'$') {
            return None;
        }

        let tag = &self.chars[open..=i];
        let mut j = i + 1;
        while j + tag.len() <= self.chars.len() {
            if self.chars[j..j + tag.len()] == *tag {
                self.at = j + tag.len();
                return Some(TokenKind::DollarQuoted);
            }
            j += 1;
        }
        // An unclosed body runs to the end, for the same reason an unclosed
        // quote does.
        self.at = self.chars.len();
        Some(TokenKind::DollarQuoted)
    }

    /// A placeholder in one of this dialect's spellings, if one starts here.
    fn parameter(&mut self) -> Option<TokenKind> {
        let c = self.chars[self.at];
        for spelling in self.dialect.parameters {
            let taken = match spelling {
                Parameter::Question if c == '?' => 1,
                Parameter::DollarNumber if c == '$' => self.run_after(|c| c.is_ascii_digit()),
                Parameter::AtName if c == '@' => self.run_after(is_identifier_char),
                Parameter::ColonName if c == ':' => {
                    // `::` is PostgreSQL's cast, not a placeholder, and the
                    // dialects with `:name` all have it. Both colons have to be
                    // ruled out: rejecting only the first leaves the second one
                    // reading `::int` as a placeholder called `int`.
                    let cast = self.peek(1) == Some(':')
                        || (self.at > 0 && self.chars[self.at - 1] == ':');
                    if cast {
                        0
                    } else {
                        self.run_after(is_identifier_char)
                    }
                }
                _ => 0,
            };
            if taken > 1 {
                self.at += taken;
                return Some(TokenKind::Parameter);
            }
            if taken == 1 {
                self.at += 1;
                return Some(TokenKind::Parameter);
            }
        }
        None
    }

    /// How many characters a sigil and the run of `accept` after it occupy, or
    /// zero when nothing follows the sigil.
    fn run_after(&self, accept: impl Fn(char) -> bool) -> usize {
        let mut n = 1;
        while self.chars.get(self.at + n).copied().is_some_and(&accept) {
            n += 1;
        }
        if n == 1 { 0 } else { n }
    }

    /// A numeric literal starting here, consumed. False when what is here is
    /// not one.
    fn number(&mut self) -> bool {
        let c = self.chars[self.at];
        if c.is_ascii_digit() {
            // Non-decimal literals and `_` as a group separator. `0x` with no
            // digits after it is not a number to the server either, but it is
            // not anything else either, so painting the half-typed form beats
            // letting it flicker.
            if c == '0' && self.peek(1).is_some_and(is_radix_mark) {
                self.at += 2;
                while self
                    .peek(0)
                    .is_some_and(|c| c.is_ascii_hexdigit() || c == '_')
                {
                    self.at += 1;
                }
                return true;
            }
        } else if c != '.' || !self.peek(1).is_some_and(|c| c.is_ascii_digit()) {
            // `.5` is a number; `t.x` is not, and neither is a bare `.`.
            return false;
        }

        let mut seen_point = false;
        while self.at < self.chars.len() {
            let c = self.chars[self.at];
            if c.is_ascii_digit() || c == '_' {
                self.at += 1;
            } else if c == '.' && !seen_point {
                seen_point = true;
                self.at += 1;
            } else if (c == 'e' || c == 'E') && self.exponent().is_some() {
                self.at = self.exponent().unwrap();
                return true;
            } else {
                break;
            }
        }
        true
    }

    /// One past the exponent beginning at the current `e`, or nothing when the
    /// `e` begins an identifier instead — `1e` is the number 1 followed by a
    /// column called `e`, and `1e+` is that plus an operator.
    fn exponent(&self) -> Option<usize> {
        let mut k = self.at + 1;
        if matches!(self.chars.get(k), Some('+' | '-')) {
            k += 1;
        }
        if !self.chars.get(k).is_some_and(|c| c.is_ascii_digit()) {
            return None;
        }
        while self.chars.get(k).is_some_and(|c| c.is_ascii_digit()) {
            k += 1;
        }
        Some(k)
    }

    /// A run of identifier characters, which is a keyword, a string with a
    /// letter prefix, or a name.
    fn word(&mut self) -> TokenKind {
        let start = self.at;
        while self.peek(0).is_some_and(is_identifier_char) {
            self.at += 1;
        }

        // `E'…'`, `N'…'`, `x'…'`: the prefix belongs to the literal, so the
        // word just read is not a word at all. Only when the whole word is the
        // prefix — `someN'x'` is a name followed by a string.
        if self.at == start + 1 && self.peek(0) == Some('\'') {
            let prefix = self.chars[start].to_ascii_lowercase();
            let escapes = self.dialect.escape_string_prefix && prefix == 'e';
            if escapes || self.dialect.string_prefixes.contains(&prefix) {
                self.string('\'', escapes || self.dialect.backslash_escapes);
                return TokenKind::String;
            }
        }

        if self.chars[start..self.at]
            .iter()
            .all(|c| c.is_ascii_alphanumeric() || *c == '_')
        {
            let word: String = self.chars[start..self.at]
                .iter()
                .map(|c| c.to_ascii_lowercase())
                .collect();
            if self.dialect.is_keyword(&word) {
                return TokenKind::Keyword;
            }
        }
        TokenKind::Identifier
    }
}

/// Whether `c` can continue an unquoted identifier. `$` is in the set: that is
/// what makes `a$b$c` a single name.
fn is_identifier_char(c: char) -> bool {
    c == '_' || c == '$' || c.is_ascii_alphanumeric() || (c as u32) >= 0x80
}

fn is_tag_char(c: char, first: bool) -> bool {
    if c == '_' || (c as u32) >= 0x80 || c.is_ascii_alphabetic() {
        return true;
    }
    !first && c.is_ascii_digit()
}

fn is_radix_mark(c: char) -> bool {
    matches!(c, 'x' | 'X' | 'o' | 'O' | 'b' | 'B')
}
