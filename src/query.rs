use crate::{
    index::{Entry, Kind, Snapshot},
    table::{NO_DIR, Table},
};
use anyhow::{Context, Result, bail};
use regex::bytes::{Regex, RegexBuilder};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Debug)]
enum Expr {
    All,
    And(Vec<Expr>),
    Or(Vec<Expr>),
    Not(Box<Expr>),
    Text {
        regex: Regex,
        path: bool,
    },
    Literal {
        needle: Box<[u8]>,
        path: bool,
        case: bool,
        /// For path literals: which parent directories contain the needle,
        /// computed once per query instead of once per entry.
        dir_hits: Vec<bool>,
    },
    /// Name wildcards with one literal part: `x*`, `*x` and `x`-with-`*`s at
    /// both ends become prefix, suffix or substring checks instead of a regex.
    Affix {
        needle: Box<[u8]>,
        case: bool,
        prefix: bool,
    },
    Kind(Kind),
    Ext(Vec<Vec<u8>>),
    Size(Range),
    Date(Range),
}
#[derive(Debug)]
struct Range {
    min: i128,
    max: i128,
}
impl Range {
    fn contains(&self, n: i128) -> bool {
        n >= self.min && n <= self.max
    }
}
#[derive(Debug, PartialEq)]
enum Token {
    Word(String, bool),
    Open,
    Close,
    Or,
    Not,
}

fn lex(input: &str) -> Result<Vec<Token>> {
    let mut out = Vec::new();
    let mut text = String::new();
    let mut quoted = false;
    let mut literal_prefix = false;
    let flush = |out: &mut Vec<Token>, text: &mut String, literal: &mut bool| {
        if !text.is_empty() {
            out.push(Token::Word(std::mem::take(text), *literal));
        }
        *literal = false;
    };
    for c in input.chars() {
        if c == '"' {
            if !quoted && text.is_empty() {
                literal_prefix = true;
            }
            quoted = !quoted;
            continue;
        }
        if quoted {
            text.push(c);
            continue;
        }
        if c.is_whitespace() {
            flush(&mut out, &mut text, &mut literal_prefix);
            continue;
        }
        // Comparators belong to metadata atoms, not grouping tokens.
        if (c == '<' || c == '>') && (text == "size:" || text == "dm:") {
            text.push(c);
            continue;
        }
        if matches!(c, '<' | '>' | '|' | '!') {
            flush(&mut out, &mut text, &mut literal_prefix);
            out.push(match c {
                '<' => Token::Open,
                '>' => Token::Close,
                '|' => Token::Or,
                _ => Token::Not,
            });
        } else {
            text.push(c);
        }
    }
    if quoted {
        bail!("Unclosed quote");
    }
    flush(&mut out, &mut text, &mut literal_prefix);
    Ok(out)
}
struct Parser {
    tokens: Vec<Token>,
    pos: usize,
    depth: usize,
}
impl Parser {
    // Everything evaluates OR before implicit AND.
    fn and(&mut self) -> Result<Expr> {
        let mut terms = vec![self.or()?];
        while self.pos < self.tokens.len() && self.tokens[self.pos] != Token::Close {
            terms.push(self.or()?);
        }
        Ok(if terms.len() == 1 {
            terms.remove(0)
        } else {
            Expr::And(terms)
        })
    }
    fn or(&mut self) -> Result<Expr> {
        let mut terms = vec![self.atom()?];
        while self.tokens.get(self.pos) == Some(&Token::Or) {
            self.pos += 1;
            terms.push(self.atom()?);
        }
        Ok(if terms.len() == 1 {
            terms.remove(0)
        } else {
            Expr::Or(terms)
        })
    }
    fn atom(&mut self) -> Result<Expr> {
        self.depth += 1;
        if self.depth > 64 {
            bail!("Query nesting exceeds 64 levels");
        }
        let result = match self.tokens.get(self.pos) {
            Some(Token::Not) => {
                self.pos += 1;
                Expr::Not(Box::new(self.atom()?))
            }
            Some(Token::Open) => {
                self.pos += 1;
                let e = self.and()?;
                if self.tokens.get(self.pos) != Some(&Token::Close) {
                    bail!("Missing closing >");
                }
                self.pos += 1;
                e
            }
            Some(Token::Word(text, literal)) => {
                let e = term(text, *literal)?;
                self.pos += 1;
                e
            }
            _ => bail!("Expected a search term near token {}", self.pos + 1),
        };
        self.depth -= 1;
        Ok(result)
    }
}
fn number(text: &str) -> Result<i128> {
    let t = text.to_ascii_lowercase();
    let split = t
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(t.len());
    let n: f64 = t[..split].parse().context("Expected a nonnegative size")?;
    let multiplier: f64 = match &t[split..] {
        "" | "b" => 1.,
        "kb" | "kib" => 1024.,
        "mb" | "mib" => 1024_f64.powi(2),
        "gb" | "gib" => 1024_f64.powi(3),
        "tb" | "tib" => 1024_f64.powi(4),
        unit => bail!("Unknown size unit: {unit}"),
    };
    if !n.is_finite() || n < 0. || n * multiplier > u64::MAX as f64 {
        bail!("Size out of range");
    }
    Ok((n * multiplier) as i128)
}
/// Local calendar days, matching the times shown in the results list. Day
/// ends come from the next local midnight, so 23- and 25-hour DST days work.
fn date(text: &str) -> Result<(i128, i128)> {
    use chrono::TimeZone;
    let today = chrono::Local::now().date_naive();
    let d = match text {
        "today" => today,
        "yesterday" => today - chrono::Duration::days(1),
        _ => chrono::NaiveDate::parse_from_str(text, "%Y-%m-%d")
            .context("Use dm:YYYY-MM-DD, today, or yesterday")?,
    };
    let midnight = |day: chrono::NaiveDate| -> Result<i128> {
        let local = day.and_hms_opt(0, 0, 0).context("Invalid date")?;
        Ok(chrono::Local
            .from_local_datetime(&local)
            .earliest()
            .map_or_else(|| local.and_utc().timestamp(), |t| t.timestamp()) as i128)
    };
    let next = d.succ_opt().context("Date out of range")?;
    Ok((midnight(d)?, midnight(next)? - 1))
}
fn range(text: &str, is_date: bool) -> Result<Range> {
    let val = |s: &str| -> Result<(i128, i128)> {
        if is_date {
            date(s)
        } else {
            let n = number(s)?;
            Ok((n, n))
        }
    };
    let r = if let Some((a, b)) = text.split_once("..") {
        Range {
            min: val(a)?.0,
            max: val(b)?.1,
        }
    } else if let Some(s) = text.strip_prefix(">=") {
        Range {
            min: val(s)?.0,
            max: i128::MAX,
        }
    } else if let Some(s) = text.strip_prefix("<=") {
        Range {
            min: i128::MIN,
            max: val(s)?.1,
        }
    } else if let Some(s) = text.strip_prefix('>') {
        Range {
            min: val(s)?.1 + 1,
            max: i128::MAX,
        }
    } else if let Some(s) = text.strip_prefix('<') {
        Range {
            min: i128::MIN,
            max: val(s)?.0 - 1,
        }
    } else {
        let (min, max) = val(text)?;
        Range { min, max }
    };
    if r.min > r.max {
        bail!("Range starts after its end");
    }
    Ok(r)
}
fn term(input: &str, literal: bool) -> Result<Expr> {
    let mut text = input;
    let mut case = false;
    let mut path = false;
    let mut regex = false;
    if !literal {
        loop {
            if let Some(s) = text.strip_prefix("case:") {
                case = true;
                text = s;
            } else if let Some(s) = text.strip_prefix("path:") {
                path = true;
                text = s;
            } else if let Some(s) = text.strip_prefix("regex:") {
                regex = true;
                text = s;
                break;
            } else {
                break;
            }
        }
        if !regex {
            match text {
                "file:" => return Ok(Expr::Kind(Kind::File)),
                "folder:" => return Ok(Expr::Kind(Kind::Folder)),
                _ => {}
            }
            if let Some((_, _, extensions)) = TYPE_FILTERS.iter().find(|(_, m, _)| *m == text) {
                return Ok(Expr::Ext(
                    extensions
                        .split(';')
                        .map(|e| e.as_bytes().to_vec())
                        .collect(),
                ));
            }
            if let Some(s) = text.strip_prefix("ext:") {
                if s.is_empty() {
                    bail!("ext: requires an extension");
                }
                return Ok(Expr::Ext(
                    s.split(';')
                        .map(|s| s.trim_start_matches('.').to_ascii_lowercase().into_bytes())
                        .collect(),
                ));
            }
            if let Some(s) = text.strip_prefix("size:") {
                return Ok(Expr::Size(range(s, false)?));
            }
            if let Some(s) = text.strip_prefix("dm:") {
                return Ok(Expr::Date(range(s, true)?));
            }
            if text.contains(':') {
                bail!("Unknown modifier; quote filenames containing a colon");
            }
        }
    }
    if text.is_empty() {
        bail!("Modifier needs a value");
    }
    path |= text.contains('/');
    if !regex && !text.contains(['*', '?']) {
        return Ok(Expr::Literal {
            needle: text.as_bytes().into(),
            path,
            case,
            dir_hits: Vec::new(),
        });
    }
    if !regex && !path && !text.contains('?') {
        let inner = text.trim_start_matches('*').trim_end_matches('*');
        let (lead, trail) = (text.starts_with('*'), text.ends_with('*'));
        if !inner.is_empty() && !inner.contains('*') {
            let needle: Box<[u8]> = inner.as_bytes().into();
            match (lead, trail) {
                (true, true) => {
                    return Ok(Expr::Literal {
                        needle,
                        path: false,
                        case,
                        dir_hits: Vec::new(),
                    });
                }
                (false, true) => {
                    return Ok(Expr::Affix {
                        needle,
                        case,
                        prefix: true,
                    });
                }
                (true, false) => {
                    return Ok(Expr::Affix {
                        needle,
                        case,
                        prefix: false,
                    });
                }
                (false, false) => {}
            }
        }
    }
    let pattern = if regex {
        text.to_owned()
    } else if text.contains(['*', '?']) {
        let mut p = String::from("(?s)^");
        for c in text.chars() {
            match c {
                '*' => p.push_str(if path { "[^/]*" } else { ".*" }),
                '?' => p.push_str(if path { "[^/]" } else { "." }),
                _ => p.push_str(&regex::escape(&c.to_string())),
            }
        }
        p.push('$');
        p
    } else {
        regex::escape(text)
    };
    let regex = RegexBuilder::new(&pattern)
        .unicode(false)
        .case_insensitive(!case)
        .size_limit(2_000_000)
        .build()
        .context("Invalid or overly complex regex")?;
    Ok(Expr::Text { regex, path })
}
fn affix(name: &[u8], needle: &[u8], case: bool, prefix: bool) -> bool {
    if name.len() < needle.len() {
        return false;
    }
    let part = if prefix {
        &name[..needle.len()]
    } else {
        &name[name.len() - needle.len()..]
    };
    if case {
        part == needle
    } else {
        part.eq_ignore_ascii_case(needle)
    }
}
fn contains(hay: &[u8], needle: &[u8], case: bool) -> bool {
    if case {
        memchr::memmem::find(hay, needle).is_some()
    } else if hay.len() < needle.len() {
        false
    } else {
        memchr::memchr2_iter(
            needle[0].to_ascii_lowercase(),
            needle[0].to_ascii_uppercase(),
            hay,
        )
        .any(|i| {
            hay.get(i..i + needle.len())
                .is_some_and(|s| s.eq_ignore_ascii_case(needle))
        })
    }
}

/// What a query can ask about one entry, whether it is a materialized
/// [`Entry`] or a row of the compact [`Table`].
trait View {
    fn name(&self) -> &[u8];
    fn kind(&self) -> Kind;
    fn size(&self) -> u64;
    fn modified(&self) -> i64;
    fn path(&mut self) -> &[u8];
    fn path_contains(&mut self, needle: &[u8], case: bool, _dir_hits: &[bool]) -> bool {
        contains(self.path(), needle, case)
    }
}
impl View for &Entry {
    fn name(&self) -> &[u8] {
        Entry::name(self)
    }
    fn kind(&self) -> Kind {
        self.kind
    }
    fn size(&self) -> u64 {
        self.size
    }
    fn modified(&self) -> i64 {
        self.modified
    }
    fn path(&mut self) -> &[u8] {
        &self.path
    }
}
struct RowView<'a> {
    table: &'a Table,
    index: usize,
    scratch: &'a mut Vec<u8>,
    built: bool,
}
impl View for RowView<'_> {
    fn name(&self) -> &[u8] {
        self.table.name(self.index)
    }
    fn kind(&self) -> Kind {
        self.table.row(self.index).kind
    }
    fn size(&self) -> u64 {
        self.table.row(self.index).size
    }
    fn modified(&self) -> i64 {
        self.table.row(self.index).modified
    }
    fn path(&mut self) -> &[u8] {
        if !self.built {
            self.table.write_path(self.index, self.scratch);
            self.built = true;
        }
        self.scratch
    }
    fn path_contains(&mut self, needle: &[u8], case: bool, dir_hits: &[bool]) -> bool {
        let dir = self.table.row(self.index).dir;
        let Some(dir_path) = self.table.dir_path(self.index) else {
            return contains(self.table.name(self.index), needle, case);
        };
        if dir_hits.get(dir as usize) == Some(&true) {
            return true;
        }
        // Remaining matches must touch the separator or the name: check the
        // last needle-1 bytes of the folder, the separator and the name.
        let keep = needle.len().saturating_sub(1).min(dir_path.len());
        self.scratch.clear();
        self.scratch
            .extend_from_slice(&dir_path[dir_path.len() - keep..]);
        if !dir_path.ends_with(b"/") {
            self.scratch.push(b'/');
        }
        self.scratch.extend_from_slice(self.table.name(self.index));
        self.built = false;
        contains(self.scratch, needle, case)
    }
}

impl Expr {
    fn prepare(&mut self, table: &Table) {
        match self {
            Self::And(v) | Self::Or(v) => v.iter_mut().for_each(|x| x.prepare(table)),
            Self::Not(x) => x.prepare(table),
            Self::Literal {
                needle,
                path: true,
                case,
                dir_hits,
            } => {
                *dir_hits = table
                    .dirs()
                    .iter()
                    .map(|d| contains(d, needle, *case))
                    .collect();
            }
            _ => {}
        }
    }
    fn matches(&self, e: &mut impl View) -> bool {
        match self {
            Self::All => true,
            Self::And(v) => v.iter().all(|x| x.matches(e)),
            Self::Or(v) => v.iter().any(|x| x.matches(e)),
            Self::Not(x) => !x.matches(e),
            Self::Text { regex, path } => {
                if *path {
                    regex.is_match(e.path())
                } else {
                    regex.is_match(e.name())
                }
            }
            Self::Literal {
                needle,
                path,
                case,
                dir_hits,
            } => {
                if *path {
                    e.path_contains(needle, *case, dir_hits)
                } else {
                    contains(e.name(), needle, *case)
                }
            }
            Self::Affix {
                needle,
                case,
                prefix,
            } => affix(e.name(), needle, *case, *prefix),
            Self::Kind(k) => e.kind() == *k,
            Self::Ext(v) => {
                e.kind() == Kind::File && {
                    let name = e.name();
                    name.iter().rposition(|b| *b == b'.').is_some_and(|i| {
                        v.iter().any(|ext| name[i + 1..].eq_ignore_ascii_case(ext))
                    })
                }
            }
            Self::Size(r) => e.kind() == Kind::File && r.contains(e.size() as i128),
            Self::Date(r) => r.contains(e.modified() as i128),
        }
    }
}
/// One bit per table row.
struct Bits {
    words: Vec<u64>,
    len: usize,
}
impl Bits {
    fn zeros(len: usize) -> Self {
        Self {
            words: vec![0; len.div_ceil(64)],
            len,
        }
    }
    fn ones(len: usize) -> Self {
        let mut bits = Self {
            words: vec![u64::MAX; len.div_ceil(64)],
            len,
        };
        bits.trim();
        bits
    }
    fn trim(&mut self) {
        if !self.len.is_multiple_of(64)
            && let Some(last) = self.words.last_mut()
        {
            *last &= (1_u64 << (self.len % 64)) - 1;
        }
    }
    fn set(&mut self, i: usize) {
        self.words[i / 64] |= 1 << (i % 64);
    }
    fn get(&self, i: usize) -> bool {
        self.words[i / 64] & (1 << (i % 64)) != 0
    }
    fn and(&mut self, other: &Self) {
        self.words
            .iter_mut()
            .zip(&other.words)
            .for_each(|(a, b)| *a &= b);
    }
    fn or(&mut self, other: &Self) {
        self.words
            .iter_mut()
            .zip(&other.words)
            .for_each(|(a, b)| *a |= b);
    }
    fn invert(&mut self) {
        self.words.iter_mut().for_each(|a| *a = !*a);
        self.trim();
    }
    fn count(&self) -> usize {
        self.words.iter().map(|w| w.count_ones() as usize).sum()
    }
    fn is_empty(&self) -> bool {
        self.words.iter().all(|w| *w == 0)
    }
    fn ones_iter(&self) -> impl Iterator<Item = usize> + '_ {
        self.words.iter().enumerate().flat_map(|(w, &word)| {
            let mut word = word;
            std::iter::from_fn(move || {
                (word != 0).then(|| {
                    let bit = word.trailing_zeros() as usize;
                    word &= word - 1;
                    w * 64 + bit
                })
            })
        })
    }
}

/// How common a byte is in file names; searches start from the rarest
/// byte of the needle so fewer candidate positions need checking.
fn commonness(b: u8) -> u8 {
    match b.to_ascii_lowercase() {
        b'q' | b'z' | b'x' | b'j' => 0,
        b'k' | b'v' => 1,
        b'w' | b'y' | b'b' | b'g' | b'f' | b'0'..=b'9' => 2,
        b'p' | b'h' | b'u' | b'm' | b'c' | b' ' => 3,
        b'd' | b'l' => 4,
        b'r' | b's' | b'n' | b'i' | b'o' => 5,
        b't' | b'a' | b'e' | b'.' | b'-' | b'_' => 6,
        _ => 1,
    }
}

/// Rows whose name contains `needle`, found by scanning the shared name
/// buffer once instead of testing each name separately. Names are stored
/// back to back in row order, so hit positions map to rows with one cursor.
fn name_hits(table: &Table, needle: &[u8], case: bool, cancel: &dyn Fn() -> bool) -> Result<Bits> {
    let (names, _, rows) = table.parts();
    let mut bits = Bits::zeros(rows.len());
    let n = needle.len();
    let mut row = 0;
    let mut skip_to = 0;
    let mut checks = 0_u32;
    let mut hit = |start: usize, bits: &mut Bits| -> Option<usize> {
        // Advance to the row holding `start`, then require the match to end
        // inside that same name.
        while row < rows.len() {
            let (offset, len) = rows[row].name_range();
            let end = offset as usize + len as usize;
            if start < end {
                if start >= offset as usize && start + n <= end {
                    bits.set(row);
                    return Some(end);
                }
                return None;
            }
            row += 1;
        }
        None
    };
    if case {
        let finder = memchr::memmem::Finder::new(needle);
        let mut from = 0;
        while let Some(k) = finder.find(&names[from..]) {
            let start = from + k;
            checks += 1;
            if checks.is_multiple_of(65_536) && cancel() {
                bail!("Query cancelled");
            }
            from = hit(start, &mut bits).unwrap_or(start + 1);
        }
    } else {
        let at = (0..n).min_by_key(|i| commonness(needle[*i])).unwrap_or(0);
        let (lower, upper) = (
            needle[at].to_ascii_lowercase(),
            needle[at].to_ascii_uppercase(),
        );
        for pos in memchr::memchr2_iter(lower, upper, names) {
            checks += 1;
            if checks.is_multiple_of(65_536) && cancel() {
                bail!("Query cancelled");
            }
            let Some(start) = pos.checked_sub(at) else {
                continue;
            };
            if start < skip_to
                || start + n > names.len()
                || !names[start..start + n].eq_ignore_ascii_case(needle)
            {
                continue;
            }
            if let Some(end) = hit(start, &mut bits) {
                skip_to = end;
            }
        }
    }
    Ok(bits)
}

impl Expr {
    /// Evaluates the whole query over a table, one tight pass per term.
    fn eval(&self, table: &Table, cancel: &dyn Fn() -> bool) -> Result<Bits> {
        let len = table.len();
        let per_row = |test: &mut dyn FnMut(usize) -> bool| -> Result<Bits> {
            let mut bits = Bits::zeros(len);
            for i in 0..len {
                if i % 65_536 == 0 && cancel() {
                    bail!("Query cancelled");
                }
                if test(i) {
                    bits.set(i);
                }
            }
            Ok(bits)
        };
        Ok(match self {
            Self::All => Bits::ones(len),
            Self::And(terms) => {
                let mut bits = terms[0].eval(table, cancel)?;
                for term in &terms[1..] {
                    if bits.is_empty() {
                        break;
                    }
                    bits.and(&term.eval(table, cancel)?);
                }
                bits
            }
            Self::Or(terms) => {
                let mut bits = terms[0].eval(table, cancel)?;
                for term in &terms[1..] {
                    bits.or(&term.eval(table, cancel)?);
                }
                bits
            }
            Self::Not(term) => {
                let mut bits = term.eval(table, cancel)?;
                bits.invert();
                bits
            }
            Self::Literal {
                needle,
                path: false,
                case,
                ..
            } => name_hits(table, needle, *case, cancel)?,
            Self::Literal {
                needle,
                path: true,
                case,
                dir_hits,
            } => {
                if needle.contains(&b'/') {
                    // The match may cross the folder/name separator.
                    let mut scratch = Vec::new();
                    per_row(&mut |index| {
                        RowView {
                            table,
                            index,
                            scratch: &mut scratch,
                            built: false,
                        }
                        .path_contains(needle, *case, dir_hits)
                    })?
                } else {
                    // Without a '/', a match lies wholly in the folder or the name.
                    let mut bits = name_hits(table, needle, *case, cancel)?;
                    let (_, _, rows) = table.parts();
                    for (i, row) in rows.iter().enumerate() {
                        if row.dir != NO_DIR && dir_hits[row.dir as usize] {
                            bits.set(i);
                        }
                    }
                    bits
                }
            }
            Self::Text { regex, path: false } => per_row(&mut |i| regex.is_match(table.name(i)))?,
            Self::Text { regex, path: true } => {
                let mut scratch = Vec::new();
                per_row(&mut |i| {
                    table.write_path(i, &mut scratch);
                    regex.is_match(&scratch)
                })?
            }
            Self::Affix {
                needle,
                case,
                prefix,
            } => per_row(&mut |i| affix(table.name(i), needle, *case, *prefix))?,
            Self::Kind(kind) => per_row(&mut |i| table.row(i).kind == *kind)?,
            Self::Ext(extensions) => per_row(&mut |i| {
                table.row(i).kind == Kind::File && {
                    let name = table.name(i);
                    name.iter().rposition(|b| *b == b'.').is_some_and(|dot| {
                        extensions
                            .iter()
                            .any(|ext| name[dot + 1..].eq_ignore_ascii_case(ext))
                    })
                }
            })?,
            Self::Size(range) => per_row(&mut |i| {
                let row = table.row(i);
                row.kind == Kind::File && range.contains(row.size as i128)
            })?,
            Self::Date(range) => per_row(&mut |i| range.contains(table.row(i).modified as i128))?,
        })
    }
}
pub struct Query(Expr);
impl Query {
    pub fn parse(input: &str) -> Result<Self> {
        if input.len() > 4096 {
            bail!("Query exceeds 4096 bytes");
        }
        let tokens = lex(input)?;
        if tokens.is_empty() {
            return Ok(Self(Expr::All));
        }
        let mut p = Parser {
            tokens,
            pos: 0,
            depth: 0,
        };
        let expr = p.and()?;
        if p.pos != p.tokens.len() {
            bail!("Unexpected closing >");
        }
        Ok(Self(expr))
    }
    pub fn matches(&self, entry: &Entry) -> bool {
        self.0.matches(&mut &*entry)
    }
}
/// Type filters: menu label, search macro (as in Everything) and the
/// extensions it stands for. Linux executables rarely have an extension,
/// so there is no executable filter.
pub const TYPE_FILTERS: &[(&str, &str, &str)] = &[
    (
        "Audio",
        "audio:",
        "aac;ac3;aif;aifc;aiff;amr;ape;au;flac;m4a;m4b;mid;midi;mka;mp2;mp3;mpc;oga;ogg;opus;ra;wav;wma;wv",
    ),
    (
        "Compressed",
        "zip:",
        "7z;ace;apk;arj;bz2;cab;cpio;deb;gz;iso;jar;lz;lz4;lzma;rar;rpm;tar;tbz2;tgz;txz;tzst;xz;z;zip;zst",
    ),
    (
        "Document",
        "doc:",
        "csv;doc;docm;docx;dot;dotx;epub;htm;html;key;md;numbers;odg;odp;ods;odt;pages;pdf;pps;ppsx;ppt;pptm;pptx;rtf;tex;txt;xls;xlsb;xlsm;xlsx;xps",
    ),
    (
        "Picture",
        "pic:",
        "arw;avif;bmp;cr2;cr3;dng;gif;heic;heif;ico;jfif;jpe;jpeg;jpg;jxl;nef;orf;png;psd;raf;raw;rw2;svg;tga;tif;tiff;webp;xcf",
    ),
    (
        "Video",
        "video:",
        "3g2;3gp;asf;avi;flv;m2ts;m4v;mkv;mov;mp4;mpeg;mpg;mts;mxf;ogm;ogv;rm;rmvb;ts;vob;webm;wmv",
    ),
];
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
pub enum Sort {
    #[default]
    Name,
    Path,
    Size,
    Modified,
    /// Folders first, then by extension, then by name.
    Type,
}
/// The extension of a file name (after the last dot; none for `.hidden`).
pub fn extension(name: &[u8]) -> &[u8] {
    match name.iter().rposition(|b| *b == b'.') {
        Some(i) if i > 0 => &name[i + 1..],
        _ => &[],
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SearchRequest {
    pub query: String,
    pub sort: Sort,
    pub descending: bool,
    pub limit: usize,
    pub offset: usize,
}
impl Default for SearchRequest {
    fn default() -> Self {
        Self {
            query: String::new(),
            sort: Sort::Name,
            descending: false,
            limit: 1000,
            offset: 0,
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SearchResponse {
    pub entries: Vec<Entry>,
    pub total: usize,
    pub generation: u64,
    pub elapsed_ms: f64,
}
pub fn search(
    snapshot: &Snapshot,
    request: &SearchRequest,
    cancellation: Option<(&AtomicU64, u64)>,
) -> Result<SearchResponse> {
    let started = std::time::Instant::now();
    let cancelled = || cancellation.is_some_and(|(a, n)| a.load(Ordering::Relaxed) != n);
    if cancelled() {
        bail!("Query cancelled");
    }
    let table = &snapshot.entries;
    let mut query = Query::parse(&request.query)?;
    query.0.prepare(table);
    let limit = request.limit.min(10_000);
    let bits = match query.0 {
        Expr::All => None,
        _ => Some(query.0.eval(table, &cancelled)?),
    };
    let wanted = |i: usize| bits.as_ref().is_none_or(|b| b.get(i));
    let total = bits.as_ref().map_or(table.len(), Bits::count);
    let page: Vec<u32> = if total <= request.offset || limit == 0 {
        Vec::new()
    } else {
        match request.sort {
            // Rows are stored in path order and names have a cached order, so
            // these sorts walk an existing order and stop after one page.
            Sort::Name => {
                let order = table.name_order();
                let take = |order: &mut dyn Iterator<Item = u32>| -> Vec<u32> {
                    order
                        .filter(|i| wanted(*i as usize))
                        .skip(request.offset)
                        .take(limit)
                        .collect()
                };
                if request.descending {
                    take(&mut order.iter().rev().copied())
                } else {
                    take(&mut order.iter().copied())
                }
            }
            Sort::Path => {
                let rows = 0..table.len() as u32;
                let order: &mut dyn Iterator<Item = u32> = if request.descending {
                    &mut rows.rev()
                } else {
                    &mut { rows }
                };
                order
                    .filter(|i| wanted(*i as usize))
                    .skip(request.offset)
                    .take(limit)
                    .collect()
            }
            Sort::Size | Sort::Modified | Sort::Type => {
                let mut hits: Vec<u32> = match &bits {
                    Some(bits) => bits.ones_iter().map(|i| i as u32).collect(),
                    None => (0..table.len() as u32).collect(),
                };
                let key = |i: &u32| {
                    let row = table.row(*i as usize);
                    if request.sort == Sort::Size {
                        row.size as i128
                    } else {
                        row.modified as i128
                    }
                };
                let by_type = |a: &u32, b: &u32| {
                    let (a, b) = (*a as usize, *b as usize);
                    let folder = |i: usize| table.row(i).kind != Kind::Folder;
                    let lower =
                        |i: usize| extension(table.name(i)).iter().map(u8::to_ascii_lowercase);
                    folder(a)
                        .cmp(&folder(b))
                        .then_with(|| lower(a).cmp(lower(b)))
                        .then_with(|| table.name(a).cmp(table.name(b)))
                };
                let compare = |a: &u32, b: &u32| {
                    let order = if request.sort == Sort::Type {
                        by_type(a, b)
                    } else {
                        key(a).cmp(&key(b))
                    }
                    .then(a.cmp(b));
                    if request.descending {
                        order.reverse()
                    } else {
                        order
                    }
                };
                // Only order the requested prefix; sorting every match to show
                // one page wastes both latency and CPU.
                let needed = request.offset.saturating_add(limit).min(total);
                if needed < hits.len() {
                    hits.select_nth_unstable_by(needed, compare);
                    hits.truncate(needed);
                }
                hits.sort_unstable_by(compare);
                hits.into_iter().skip(request.offset).take(limit).collect()
            }
        }
    };
    Ok(SearchResponse {
        entries: page.into_iter().map(|i| table.entry(i as usize)).collect(),
        total,
        generation: snapshot.generation,
        elapsed_ms: started.elapsed().as_secs_f64() * 1000.,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::Entry;

    /// The fast table evaluator must agree with the simple per-entry check.
    #[test]
    fn table_evaluation_matches_per_entry_evaluation() {
        let names = [
            "/data/Projects/src/main.rs",
            "/data/Projects/src/lib.RS",
            "/data/Projects/README.md",
            "/data/Projects/readme-old.txt",
            "/data/Projects/notes/abab.txt",
            "/data/Projects/notes/aba",
            "/data/photos/IMG_0001.JPG",
            "/data/photos/a b c.png",
            "/data/photos/source map.json",
            "/data/x.json",
            "/data",
            "/srcfiles/thing",
            "/",
        ];
        let entries: Vec<Entry> = names
            .iter()
            .enumerate()
            .map(|(i, p)| {
                let kind = if p.ends_with("Projects") || *p == "/data" || *p == "/" {
                    Kind::Folder
                } else {
                    Kind::File
                };
                Entry::new(
                    p.as_bytes().to_vec(),
                    kind,
                    (i as u64) << 20,
                    1_700_000_000 + i as i64,
                )
            })
            .collect();
        let table = Table::from_entries(entries);
        let materialized = table.entries();
        for query in [
            "",
            "a",
            "aba",
            "abab",
            "ab",
            "readme",
            "case:readme",
            "case:README",
            "md",
            "*.json",
            "*.JSON",
            "case:*.JSON",
            "main*",
            "*a*",
            "*.rs",
            "ext:rs",
            "pic:",
            "doc: | zip:",
            "file:",
            "folder:",
            "path:src",
            "path:src/",
            "path:/src",
            "path:projects/src/m",
            "s/m",
            "path:data",
            "!a",
            "a | json",
            "<a json> | readme",
            "size:>4mb",
            "a?c",
            "regex:^lib",
            "path:regex:^/data/p",
            "dm:2023-11-14",
            "photos/",
            "sourc",
        ] {
            let mut q = Query::parse(query).unwrap();
            q.0.prepare(&table);
            let bits = q.0.eval(&table, &|| false).unwrap();
            for (i, entry) in materialized.iter().enumerate() {
                assert_eq!(
                    bits.get(i),
                    q.matches(entry),
                    "query {query:?} on {:?}",
                    entry.display_path()
                );
            }
        }
    }

    #[test]
    fn bits_tail_and_iteration() {
        let mut bits = Bits::ones(70);
        assert_eq!(bits.count(), 70);
        bits.invert();
        assert!(bits.is_empty());
        bits.set(3);
        bits.set(69);
        assert_eq!(bits.ones_iter().collect::<Vec<_>>(), [3, 69]);
    }
}
