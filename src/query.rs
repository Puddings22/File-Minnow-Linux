use crate::{
    fold::fold,
    index::{Entry, Kind, Snapshot},
    table::{NO_DATE, NO_DIR, Table},
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
        /// The needle as typed (for `case:`), and its folded form, which is
        /// what case- and accent-insensitive matching compares.
        needle: Needle,
        path: bool,
        case: bool,
        /// `wholeword:`: the match may not touch letters or digits.
        whole: bool,
        /// For path literals: which parent directories contain the needle,
        /// computed once per query instead of once per entry.
        dir_hits: Vec<bool>,
    },
    /// Name wildcards with one literal part: `x*`, `*x` and `x`-with-`*`s at
    /// both ends become prefix, suffix or substring checks instead of a regex.
    Affix {
        needle: Needle,
        case: bool,
        prefix: bool,
    },
    Kind(Kind),
    Ext(Vec<Vec<u8>>),
    Size(Range),
    Date(Range),
    /// `dc:` creation date.
    Created(Range),
    /// `dupe:` results that share a name, a size or their contents with
    /// another result of the other terms. Evaluated after them.
    Dupe(DupeBy),
    /// `empty:` folders with nothing indexed inside.
    Empty,
    /// `parent:<folder>` entries directly inside the folder; `dir` ids found in `prepare`.
    Parent {
        path: Vec<u8>,
        dirs: Vec<bool>,
    },
    /// `child:<name>` folders with an entry of that name; rows found in `prepare`.
    Child {
        needle: Needle,
        rows: Vec<bool>,
    },
    /// `content:<text>` files whose contents contain the text. Slow: each
    /// candidate file is read, so it is evaluated last, only on the rows the
    /// other terms left.
    Content {
        needle: Needle,
        case: bool,
    },
}
/// What `dupe:` compares: `dupe:` and `dupe:name` the name, `dupe:size`
/// the size, `dupe:content` the size and then the contents; combined with
/// `;` (`dupe:name;size`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct DupeBy {
    name: bool,
    size: bool,
    content: bool,
}
impl DupeBy {
    fn parse(text: &str) -> Result<Self> {
        let mut by = Self::default();
        for part in text.split(';').map(str::trim).filter(|p| !p.is_empty()) {
            match part.to_ascii_lowercase().as_str() {
                "name" => by.name = true,
                "size" => by.size = true,
                "content" | "contents" => by.content = true,
                other => bail!("dupe: compares name, size or content, not “{other}”"),
            }
        }
        if by == Self::default() {
            by.name = true;
        }
        Ok(by)
    }
}
/// Rows of `rows` that share what `by` compares with another of them.
/// Contents are compared only within groups of equal size: first the
/// start of each file, then whole files where the starts agree.
fn duplicates(
    table: &Table,
    mut rows: Vec<usize>,
    by: DupeBy,
    cancel: &dyn Fn() -> bool,
) -> Result<Bits> {
    let mut bits = Bits::zeros(table.len());
    let sized = by.size || by.content;
    if sized {
        rows.retain(|&i| table.row(i).kind == Kind::File && table.row(i).size > 0);
    }
    let key = |i: usize| {
        (
            by.name.then(|| table.name(i)),
            sized.then(|| table.row(i).size),
        )
    };
    rows.sort_unstable_by(|&a, &b| key(a).cmp(&key(b)).then(a.cmp(&b)));
    let hasher = (
        std::collections::hash_map::RandomState::new(),
        std::collections::hash_map::RandomState::new(),
    );
    let mut buffer = vec![0_u8; 1 << 16];
    for group in rows.chunk_by(|&a, &b| key(a) == key(b)) {
        if group.len() < 2 {
            continue;
        }
        if !by.content {
            group.iter().for_each(|&i| bits.set(i));
            continue;
        }
        // The start of each file, then (if the files are larger) all of it.
        let mut candidates = vec![group.to_vec()];
        for whole in [false, true] {
            let mut next = Vec::new();
            for rows in candidates {
                if whole && table.row(rows[0]).size <= buffer.len() as u64 {
                    next.push(rows);
                    continue;
                }
                let mut digests = Vec::with_capacity(rows.len());
                for i in rows {
                    if cancel() {
                        bail!("Query cancelled");
                    }
                    if let Some(digest) = digest(table, i, whole, &hasher, &mut buffer, cancel)? {
                        digests.push((digest, i));
                    }
                }
                digests.sort_unstable();
                for same in digests.chunk_by(|a, b| a.0 == b.0) {
                    if same.len() > 1 {
                        next.push(same.iter().map(|(_, i)| *i).collect());
                    }
                }
            }
            candidates = next;
        }
        for rows in candidates {
            rows.iter().for_each(|&i| bits.set(i));
        }
    }
    Ok(bits)
}
/// A 128-bit hash of the start of a file, or of all of it. None when the
/// file cannot be read.
fn digest(
    table: &Table,
    i: usize,
    whole: bool,
    hasher: &(
        std::collections::hash_map::RandomState,
        std::collections::hash_map::RandomState,
    ),
    buffer: &mut [u8],
    cancel: &dyn Fn() -> bool,
) -> Result<Option<u128>> {
    use std::hash::{BuildHasher, Hasher};
    use std::io::Read;
    use std::os::unix::ffi::OsStrExt;
    let path = table.path(i);
    let Ok(mut file) = std::fs::File::open(std::ffi::OsStr::from_bytes(&path)) else {
        return Ok(None);
    };
    let (mut a, mut b) = (hasher.0.build_hasher(), hasher.1.build_hasher());
    let mut chunks = 0_u32;
    loop {
        let read = match file.read(buffer) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return Ok(None),
        };
        a.write(&buffer[..read]);
        b.write(&buffer[..read]);
        if !whole {
            break;
        }
        chunks += 1;
        if chunks.is_multiple_of(64) && cancel() {
            bail!("Query cancelled");
        }
    }
    Ok(Some(u128::from(a.finish()) << 64 | u128::from(b.finish())))
}
/// Files larger than this are not read for `content:`.
const CONTENT_MAX_BYTES: u64 = 16 << 20;
/// Whether a file's contents contain `needle` (ASCII case-insensitive
/// unless `case`), reading into a reusable buffer.
fn file_contains(
    path: &std::path::Path,
    size: u64,
    needle: &Needle,
    case: bool,
    buffer: &mut Vec<u8>,
) -> bool {
    use std::io::Read;
    if size > CONTENT_MAX_BYTES || needle.raw.is_empty() {
        return false;
    }
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    buffer.clear();
    if file
        .by_ref()
        .take(CONTENT_MAX_BYTES)
        .read_to_end(buffer)
        .is_err()
    {
        return false;
    }
    contains_bytes(buffer, needle.bytes(case), case, false)
}
#[derive(Debug)]
struct Needle {
    raw: Box<[u8]>,
    folded: Box<[u8]>,
}
impl Needle {
    fn new(text: &str) -> Self {
        Self {
            raw: text.as_bytes().into(),
            folded: fold(text.as_bytes()).into(),
        }
    }
    /// The bytes compared against unfolded text: exact for `case:`,
    /// otherwise the folded form (which the scanner compares ignoring ASCII case).
    fn bytes(&self, case: bool) -> &[u8] {
        if case { &self.raw } else { &self.folded }
    }
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
        if (c == '<' || c == '>') && matches!(text.as_str(), "size:" | "dm:" | "dc:") {
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
/// A date or period in local time, matching the times shown in the
/// results list, as the first and last second it covers. Calendar periods
/// (`today`, `thisweek`, `2026`, `2026-10`, `2026-10-15`) run from local
/// midnight to midnight, so 23- and 25-hour DST days work; weeks start on
/// Monday. `last7days` (or `past7days`) is a rolling period ending now.
fn date(text: &str) -> Result<(i128, i128)> {
    use chrono::{Datelike, NaiveDate, TimeZone};
    const HELP: &str = "Use a date (2026-10-15, 2026-10, 2026), today, yesterday, \
        thisweek, lastweek, thismonth, lastmonth, thisyear, lastyear or last7days";
    let now = chrono::Local::now();
    let today = now.date_naive();
    let text = text.to_ascii_lowercase();
    let midnight = |day: NaiveDate| -> Result<i128> {
        let local = day.and_hms_opt(0, 0, 0).context("Invalid date")?;
        Ok(chrono::Local
            .from_local_datetime(&local)
            .earliest()
            .map_or_else(|| local.and_utc().timestamp(), |t| t.timestamp()) as i128)
    };
    let span = |first: Option<NaiveDate>, next: Option<NaiveDate>| -> Result<(i128, i128)> {
        let (first, next) = (
            first.context("Date out of range")?,
            next.context("Date out of range")?,
        );
        Ok((midnight(first)?, midnight(next)? - 1))
    };
    let month = |year: i32, month: u32| {
        let first = NaiveDate::from_ymd_opt(year, month, 1);
        span(
            first,
            first.and_then(|d| d.checked_add_months(chrono::Months::new(1))),
        )
    };
    let year = |year: i32| {
        span(
            NaiveDate::from_ymd_opt(year, 1, 1),
            NaiveDate::from_ymd_opt(year + 1, 1, 1),
        )
    };
    let monday = today - chrono::Duration::days(i64::from(today.weekday().num_days_from_monday()));
    let day = chrono::Duration::days(1);
    let week = chrono::Duration::weeks(1);
    match text.as_str() {
        "today" => return span(Some(today), today.succ_opt()),
        "yesterday" => return span(today.pred_opt(), Some(today)),
        "thisweek" => return span(Some(monday), Some(monday + week)),
        "lastweek" => return span(Some(monday - week), Some(monday)),
        "thismonth" => return month(today.year(), today.month()),
        "lastmonth" => {
            let last = today.with_day(1).and_then(|d| d.pred_opt()).context(HELP)?;
            return month(last.year(), last.month());
        }
        "thisyear" => return year(today.year()),
        "lastyear" => return year(today.year() - 1),
        _ => {}
    }
    if let Some(rest) = text
        .strip_prefix("last")
        .or_else(|| text.strip_prefix("past"))
    {
        let digits = rest.chars().take_while(char::is_ascii_digit).count();
        let count: u32 = rest[..digits].parse().context(HELP)?;
        let end = now.timestamp() as i128;
        let start = match rest[digits..].trim_end_matches('s') {
            "min" | "minute" => now - chrono::Duration::minutes(i64::from(count)),
            "hour" => now - chrono::Duration::hours(i64::from(count)),
            "day" => now - day * count as i32,
            "week" => now - week * count as i32,
            "month" => now
                .checked_sub_months(chrono::Months::new(count))
                .context("Date out of range")?,
            "year" => now
                .checked_sub_months(chrono::Months::new(count.saturating_mul(12)))
                .context("Date out of range")?,
            _ => bail!(HELP),
        };
        return Ok((start.timestamp() as i128, end));
    }
    let parts: Vec<&str> = text.split('-').collect();
    let number = |s: &str| {
        s.parse::<u32>()
            .ok()
            .filter(|_| s.chars().all(|c| c.is_ascii_digit()))
    };
    match parts.as_slice() {
        [y] if y.len() == 4 => year(number(y).context(HELP)? as i32),
        [y, m] if y.len() == 4 => month(
            number(y).context(HELP)? as i32,
            number(m).filter(|m| (1..=12).contains(m)).context(HELP)?,
        ),
        _ => {
            let d = NaiveDate::parse_from_str(&text, "%Y-%m-%d").context(HELP)?;
            span(Some(d), d.succ_opt())
        }
    }
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
    let mut whole = false;
    if !literal {
        loop {
            if let Some(s) = text.strip_prefix("case:") {
                case = true;
                text = s;
            } else if let Some(s) = text.strip_prefix("path:") {
                path = true;
                text = s;
            } else if let Some(s) = text
                .strip_prefix("wholeword:")
                .or_else(|| text.strip_prefix("ww:"))
            {
                whole = true;
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
                "dupe:" => return Ok(Expr::Dupe(DupeBy::parse("")?)),
                "empty:" => return Ok(Expr::Empty),
                _ => {}
            }
            if let Some(s) = text
                .strip_prefix("parent:")
                .or_else(|| text.strip_prefix("infolder:"))
            {
                if s.is_empty() {
                    bail!("parent: requires a folder path");
                }
                let mut path = s.as_bytes().to_vec();
                while path.len() > 1 && path.ends_with(b"/") {
                    path.pop();
                }
                return Ok(Expr::Parent {
                    path,
                    dirs: Vec::new(),
                });
            }
            if let Some(s) = text.strip_prefix("dupe:") {
                return Ok(Expr::Dupe(DupeBy::parse(s)?));
            }
            if let Some(s) = text.strip_prefix("child:") {
                if s.is_empty() {
                    bail!("child: requires a file or folder name");
                }
                return Ok(Expr::Child {
                    needle: Needle::new(s),
                    rows: Vec::new(),
                });
            }
            if let Some(s) = text.strip_prefix("dc:") {
                return Ok(Expr::Created(range(s, true)?));
            }
            if let Some(s) = text.strip_prefix("content:") {
                if s.is_empty() {
                    bail!("content: requires text to look for");
                }
                return Ok(Expr::Content {
                    needle: Needle::new(s),
                    case,
                });
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
            if text.starts_with(':') {
                bail!(
                    "Choose a search operator from the list, or quote names that contain a colon"
                );
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
            needle: Needle::new(text),
            path,
            case,
            whole,
            dir_hits: Vec::new(),
        });
    }
    if !regex && !path && !text.contains('?') {
        let inner = text.trim_start_matches('*').trim_end_matches('*');
        let (lead, trail) = (text.starts_with('*'), text.ends_with('*'));
        if !inner.is_empty() && !inner.contains('*') {
            let needle = Needle::new(inner);
            match (lead, trail) {
                (true, true) => {
                    return Ok(Expr::Literal {
                        needle,
                        path: false,
                        case,
                        whole,
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
fn affix_bytes(name: &[u8], needle: &[u8], case: bool, prefix: bool) -> bool {
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
/// Prefix or suffix test; accent-insensitive for names with non-ASCII bytes.
fn affix(name: &[u8], needle: &Needle, case: bool, prefix: bool) -> bool {
    affix_bytes(name, needle.bytes(case), case, prefix)
        || (!case && !name.is_ascii() && affix_bytes(&fold(name), &needle.folded, false, prefix))
}
/// Positions where `needle` occurs in `hay`, ignoring ASCII case unless `case`.
fn occurrences<'a>(
    hay: &'a [u8],
    needle: &'a [u8],
    case: bool,
) -> impl Iterator<Item = usize> + 'a {
    let (first, upper) = if case || needle.is_empty() {
        (
            needle.first().copied().unwrap_or(0),
            needle.first().copied().unwrap_or(0),
        )
    } else {
        (
            needle[0].to_ascii_lowercase(),
            needle[0].to_ascii_uppercase(),
        )
    };
    memchr::memchr2_iter(first, upper, hay).filter(move |&i| {
        hay.get(i..i + needle.len()).is_some_and(|s| {
            if case {
                s == needle
            } else {
                s.eq_ignore_ascii_case(needle)
            }
        })
    })
}
fn contains_bytes(hay: &[u8], needle: &[u8], case: bool, whole: bool) -> bool {
    if needle.is_empty() {
        return true;
    }
    if !whole {
        return if case {
            memchr::memmem::find(hay, needle).is_some()
        } else {
            occurrences(hay, needle, false).next().is_some()
        };
    }
    occurrences(hay, needle, case).any(|i| {
        let word = |b: u8| b.is_ascii_alphanumeric() || b >= 0x80;
        let before = i > 0 && word(hay[i - 1]);
        let after = hay.get(i + needle.len()).is_some_and(|b| word(*b));
        !before && !after
    })
}
/// Substring test; accent-insensitive for text with non-ASCII bytes.
fn contains(hay: &[u8], needle: &Needle, case: bool, whole: bool) -> bool {
    contains_bytes(hay, needle.bytes(case), case, whole)
        || (!case && !hay.is_ascii() && contains_bytes(&fold(hay), &needle.folded, false, whole))
}

/// What a query can ask about one entry, whether it is a materialized
/// [`Entry`] or a row of the compact [`Table`].
trait View {
    fn name(&self) -> &[u8];
    fn kind(&self) -> Kind;
    fn size(&self) -> u64;
    fn modified(&self) -> i64;
    fn created(&self) -> i64;
    fn path(&mut self) -> &[u8];
    fn path_contains(
        &mut self,
        needle: &Needle,
        case: bool,
        whole: bool,
        _dir_hits: &[bool],
    ) -> bool {
        contains(self.path(), needle, case, whole)
    }
    /// The parent folder path, without a trailing slash.
    fn parent_path(&mut self) -> Option<Vec<u8>> {
        let path = self.path();
        if path == b"/" {
            return None;
        }
        Some(match path.iter().rposition(|b| *b == b'/') {
            Some(0) => b"/".to_vec(),
            Some(i) => path[..i].to_vec(),
            None => Vec::new(),
        })
    }
    /// Table-wide facts (duplicates, folder contents) are only known to rows.
    fn row(&self) -> Option<usize> {
        None
    }
    fn empty_folder(&self) -> bool {
        false
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
    fn created(&self) -> i64 {
        self.created
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
    fn created(&self) -> i64 {
        self.table.row(self.index).created
    }
    fn path(&mut self) -> &[u8] {
        if !self.built {
            self.table.write_path(self.index, self.scratch);
            self.built = true;
        }
        self.scratch
    }
    fn parent_path(&mut self) -> Option<Vec<u8>> {
        self.table.dir_path(self.index).map(|d| d.to_vec())
    }
    fn row(&self) -> Option<usize> {
        Some(self.index)
    }
    fn empty_folder(&self) -> bool {
        self.table.is_empty_folder(self.index)
    }
    fn path_contains(
        &mut self,
        needle: &Needle,
        case: bool,
        whole: bool,
        dir_hits: &[bool],
    ) -> bool {
        let dir = self.table.row(self.index).dir;
        let Some(dir_path) = self.table.dir_path(self.index) else {
            return contains(self.table.name(self.index), needle, case, whole);
        };
        if !whole && dir_hits.get(dir as usize) == Some(&true) {
            return true;
        }
        if whole {
            return contains(self.path(), needle, case, true);
        }
        // Remaining matches must touch the separator or the name: check the
        // last needle-1 bytes of the folder, the separator and the name.
        let keep = needle.raw.len().saturating_sub(1).min(dir_path.len());
        self.scratch.clear();
        self.scratch
            .extend_from_slice(&dir_path[dir_path.len() - keep..]);
        if !dir_path.ends_with(b"/") {
            self.scratch.push(b'/');
        }
        self.scratch.extend_from_slice(self.table.name(self.index));
        self.built = false;
        contains(self.scratch, needle, case, false)
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
                ..
            } => {
                *dir_hits = table
                    .dirs()
                    .iter()
                    .map(|d| contains(d, needle, *case, false))
                    .collect();
            }
            Self::Parent { path, dirs } => {
                *dirs = table.dirs().iter().map(|d| d == path.as_slice()).collect();
            }
            Self::Child { needle, rows } => {
                let dir_rows = table.dir_rows();
                let mut parents = vec![false; table.len()];
                for i in 0..table.len() {
                    let dir = table.row(i).dir;
                    if dir != NO_DIR
                        && dir_rows[dir as usize] != u32::MAX
                        && name_is(table.name(i), needle)
                    {
                        parents[dir_rows[dir as usize] as usize] = true;
                    }
                }
                *rows = parents;
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
                whole,
                dir_hits,
            } => {
                if *path {
                    e.path_contains(needle, *case, *whole, dir_hits)
                } else {
                    contains(e.name(), needle, *case, *whole)
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
            Self::Size(r) => sized(e.kind()) && r.contains(e.size() as i128),
            Self::Date(r) => r.contains(e.modified() as i128),
            Self::Created(r) => e.created() != NO_DATE && r.contains(e.created() as i128),
            // Duplicates are a property of a set of results, not of one entry.
            Self::Dupe(_) => false,
            Self::Empty => e.empty_folder(),
            Self::Parent { path, .. } => e.parent_path().as_deref() == Some(path.as_slice()),
            Self::Child { rows, .. } => e.row().is_some_and(|i| rows.get(i) == Some(&true)),
            Self::Content { needle, case } => {
                use std::os::unix::ffi::OsStrExt;
                e.kind() == Kind::File && {
                    let size = e.size();
                    let path = std::path::PathBuf::from(std::ffi::OsStr::from_bytes(e.path()));
                    file_contains(&path, size, needle, *case, &mut Vec::new())
                }
            }
        }
    }
    /// Terms evaluated after the others of an AND, on their results:
    /// reading files is slow, and duplicates are among those results.
    fn evaluated_last(&self) -> bool {
        matches!(self, Self::Content { .. } | Self::Dupe(_))
    }
}
/// Files have a size; folders carry the total of the files inside them.
fn sized(kind: Kind) -> bool {
    matches!(kind, Kind::File | Kind::Folder)
}
/// Whole-name match, ignoring case and accents.
fn name_is(name: &[u8], needle: &Needle) -> bool {
    name.eq_ignore_ascii_case(&needle.folded) || (!name.is_ascii() && fold(name) == *needle.folded)
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

/// Scans one buffer of back-to-back entries for `needle` and calls `found`
/// with the index of each entry that contains it. Hit positions map to
/// entries with one forward cursor, so the whole buffer is read once.
fn scan(
    buffer: &[u8],
    entries: usize,
    range: impl Fn(usize) -> (usize, usize),
    needle: &[u8],
    case: bool,
    cancel: &dyn Fn() -> bool,
    mut found: impl FnMut(usize),
) -> Result<()> {
    let n = needle.len();
    if n == 0 || entries == 0 {
        return Ok(());
    }
    let mut entry = 0;
    let mut skip_to = 0;
    let mut checks = 0_u32;
    let mut hit = |start: usize, found: &mut dyn FnMut(usize)| -> Option<usize> {
        // Advance to the entry holding `start`, then require the match to
        // end inside that same entry.
        while entry < entries {
            let (offset, end) = range(entry);
            if start < end {
                if start >= offset && start + n <= end {
                    found(entry);
                    return Some(end);
                }
                return None;
            }
            entry += 1;
        }
        None
    };
    if case {
        let finder = memchr::memmem::Finder::new(needle);
        let mut from = 0;
        while let Some(k) = finder.find(&buffer[from..]) {
            let start = from + k;
            checks += 1;
            if checks.is_multiple_of(65_536) && cancel() {
                bail!("Query cancelled");
            }
            from = hit(start, &mut found).unwrap_or(start + 1);
        }
    } else {
        let at = (0..n).min_by_key(|i| commonness(needle[*i])).unwrap_or(0);
        let (lower, upper) = (
            needle[at].to_ascii_lowercase(),
            needle[at].to_ascii_uppercase(),
        );
        for pos in memchr::memchr2_iter(lower, upper, buffer) {
            checks += 1;
            if checks.is_multiple_of(65_536) && cancel() {
                bail!("Query cancelled");
            }
            let Some(start) = pos.checked_sub(at) else {
                continue;
            };
            if start < skip_to
                || start + n > buffer.len()
                || !buffer[start..start + n].eq_ignore_ascii_case(needle)
            {
                continue;
            }
            if let Some(end) = hit(start, &mut found) {
                skip_to = end;
            }
        }
    }
    Ok(())
}

/// Rows whose name contains `needle`: one pass over the shared names buffer
/// and, unless `case:`, one over the folded names of non-ASCII rows.
fn name_hits(
    table: &Table,
    needle: &Needle,
    case: bool,
    whole: bool,
    cancel: &dyn Fn() -> bool,
) -> Result<Bits> {
    let (names, _, rows) = table.parts();
    let mut bits = Bits::zeros(rows.len());
    scan(
        names,
        rows.len(),
        |i| {
            let (offset, len) = rows[i].name_range();
            (offset as usize, offset as usize + len as usize)
        },
        needle.bytes(case),
        case,
        cancel,
        |i| bits.set(i),
    )?;
    if !case {
        let folded = table.folded();
        scan(
            &folded.bytes,
            folded.len(),
            |k| folded.range(k),
            &needle.folded,
            false,
            cancel,
            |k| bits.set(folded.rows[k] as usize),
        )?;
    }
    if whole {
        // Keep only the rows where an occurrence stands as a whole word.
        let hits: Vec<usize> = bits.ones_iter().collect();
        bits = Bits::zeros(rows.len());
        for i in hits {
            if contains(table.name(i), needle, case, true) {
                bits.set(i);
            }
        }
    }
    Ok(bits)
}

impl Expr {
    /// Evaluates the whole query over a table, one tight pass per term.
    fn eval(&self, table: &Table, cancel: &dyn Fn() -> bool) -> Result<Bits> {
        self.eval_within(table, None, cancel)
    }
    /// Like `eval`, but a term that reads files only looks at `candidates`.
    fn eval_within(
        &self,
        table: &Table,
        candidates: Option<&Bits>,
        cancel: &dyn Fn() -> bool,
    ) -> Result<Bits> {
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
                // Cheap terms first; terms that read files then only test
                // the rows those left.
                let (slow, fast): (Vec<&Expr>, Vec<&Expr>) =
                    terms.iter().partition(|t| t.evaluated_last());
                let mut bits = match fast.first() {
                    Some(first) => first.eval_within(table, candidates, cancel)?,
                    None => Bits::ones(len),
                };
                for term in fast.iter().skip(1) {
                    if bits.is_empty() {
                        break;
                    }
                    bits.and(&term.eval_within(table, candidates, cancel)?);
                }
                for term in slow {
                    if bits.is_empty() {
                        break;
                    }
                    bits.and(&term.eval_within(table, Some(&bits), cancel)?);
                }
                bits
            }
            Self::Or(terms) => {
                let mut bits = terms[0].eval_within(table, candidates, cancel)?;
                for term in &terms[1..] {
                    bits.or(&term.eval_within(table, candidates, cancel)?);
                }
                bits
            }
            Self::Not(term) => {
                let mut bits = term.eval_within(table, candidates, cancel)?;
                bits.invert();
                bits
            }
            Self::Content { needle, case } => {
                let mut bits = Bits::zeros(len);
                let mut buffer = Vec::new();
                let mut path = Vec::new();
                let rows: Vec<usize> = match candidates {
                    Some(c) => c.ones_iter().collect(),
                    None => (0..len).collect(),
                };
                for i in rows {
                    if cancel() {
                        bail!("Query cancelled");
                    }
                    let row = table.row(i);
                    if row.kind != Kind::File || row.size > CONTENT_MAX_BYTES {
                        continue;
                    }
                    table.write_path(i, &mut path);
                    use std::os::unix::ffi::OsStrExt;
                    let file = std::path::Path::new(std::ffi::OsStr::from_bytes(&path));
                    if file_contains(file, row.size, needle, *case, &mut buffer) {
                        bits.set(i);
                    }
                }
                bits
            }
            Self::Literal {
                needle,
                path: false,
                case,
                whole,
                ..
            } => name_hits(table, needle, *case, *whole, cancel)?,
            Self::Literal {
                needle,
                path: true,
                case,
                whole,
                dir_hits,
            } => {
                if *whole || needle.raw.contains(&b'/') {
                    // The match may cross the folder/name separator.
                    let mut scratch = Vec::new();
                    per_row(&mut |index| {
                        RowView {
                            table,
                            index,
                            scratch: &mut scratch,
                            built: false,
                        }
                        .path_contains(needle, *case, *whole, dir_hits)
                    })?
                } else {
                    // Without a '/', a match lies wholly in the folder or the name.
                    let mut bits = name_hits(table, needle, *case, false, cancel)?;
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
                sized(row.kind) && range.contains(row.size as i128)
            })?,
            Self::Date(range) => per_row(&mut |i| range.contains(table.row(i).modified as i128))?,
            Self::Created(range) => per_row(&mut |i| {
                let created = table.row(i).created;
                created != NO_DATE && range.contains(created as i128)
            })?,
            Self::Child { rows, .. } => per_row(&mut |i| rows.get(i) == Some(&true))?,
            Self::Dupe(by) => {
                let rows: Vec<usize> = match candidates {
                    Some(c) => c.ones_iter().collect(),
                    None => (0..len).collect(),
                };
                duplicates(table, rows, *by, cancel)?
            }
            Self::Empty => per_row(&mut |i| table.is_empty_folder(i))?,
            Self::Parent { dirs, .. } => per_row(&mut |i| {
                let dir = table.row(i).dir;
                dir != NO_DIR && dirs.get(dir as usize) == Some(&true)
            })?,
        })
    }
}
pub struct Query(Expr);
impl Query {
    /// The folded text of the positive literal terms (for highlighting
    /// matches in results); `case:` terms keep their case.
    pub fn highlight_terms(&self) -> Vec<Vec<u8>> {
        fn walk(expr: &Expr, out: &mut Vec<Vec<u8>>) {
            match expr {
                Expr::And(v) | Expr::Or(v) => v.iter().for_each(|x| walk(x, out)),
                Expr::Literal { needle, .. } | Expr::Affix { needle, .. } => {
                    out.push(needle.folded.to_vec())
                }
                _ => {}
            }
        }
        let mut out = Vec::new();
        walk(&self.0, &mut out);
        out.sort();
        out.dedup();
        out.retain(|t| !t.is_empty());
        out
    }
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
    Created,
    /// The number of entries inside a folder.
    Items,
    /// Times the file was opened or launched through File Minnow. Needs the
    /// run counts; without them every result compares equal (name order).
    RunCount,
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
    /// Run counts aligned with `entries`, from the service's counter store;
    /// empty when the search was run without one.
    #[serde(default)]
    pub counts: Vec<u32>,
}
pub fn search(
    snapshot: &Snapshot,
    request: &SearchRequest,
    cancellation: Option<(&AtomicU64, u64)>,
    run_counts: Option<&crate::run_counts::RunCounts>,
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
    // Keyed by raw path bytes so neither the comparator nor the page build
    // converts index paths. Cloning the (small) counter store once per query
    // keeps the sort free of repeated locking.
    let counts_map: std::collections::HashMap<Vec<u8>, u32> =
        run_counts.map_or_else(std::collections::HashMap::new, |c| c.snapshot());
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
            Sort::Size | Sort::Modified | Sort::Type | Sort::Created | Sort::Items => {
                let mut hits: Vec<u32> = match &bits {
                    Some(bits) => bits.ones_iter().map(|i| i as u32).collect(),
                    None => (0..table.len() as u32).collect(),
                };
                let key = |i: &u32| {
                    let row = table.row(*i as usize);
                    match request.sort {
                        Sort::Size => row.size as i128,
                        Sort::Created => row.created as i128,
                        Sort::Items => i128::from(row.items),
                        _ => row.modified as i128,
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
                        .then_with(|| crate::table::name_cmp(table.name(a), table.name(b)))
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
            Sort::RunCount => {
                let mut hits: Vec<u32> = match &bits {
                    Some(bits) => bits.ones_iter().map(|i| i as u32).collect(),
                    None => (0..table.len() as u32).collect(),
                };
                // The counter is not in the index, so pay one path lookup per
                // match up front; comparisons then touch only the small
                // counter table. Without counts everything is zero and the
                // row order (path order) decides, like an empty column.
                let count: std::collections::HashMap<u32, u32> = hits
                    .iter()
                    .filter_map(|i| counts_map.get(&table.path(*i as usize)).map(|c| (*i, *c)))
                    .collect();
                let compare = |a: &u32, b: &u32| {
                    let order = count
                        .get(a)
                        .copied()
                        .unwrap_or(0)
                        .cmp(&count.get(b).copied().unwrap_or(0))
                        .then(a.cmp(b));
                    if request.descending {
                        order.reverse()
                    } else {
                        order
                    }
                };
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
    let counts: Vec<u32> = if counts_map.is_empty() {
        Vec::new()
    } else {
        page.iter()
            .map(|i| {
                counts_map
                    .get(&table.path(*i as usize))
                    .copied()
                    .unwrap_or(0)
            })
            .collect()
    };
    Ok(SearchResponse {
        entries: page.into_iter().map(|i| table.entry(i as usize)).collect(),
        total,
        generation: snapshot.generation,
        elapsed_ms: started.elapsed().as_secs_f64() * 1000.,
        counts,
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
            "/data/Projects/Știință.txt",
            "/data/photos/École/été.jpg",
            "/data/photos/École",
            "/data/Übung/straße.txt",
        ];
        let entries: Vec<Entry> = names
            .iter()
            .enumerate()
            .map(|(i, p)| {
                let kind = if p.ends_with("Projects")
                    || p.ends_with("École")
                    || *p == "/data"
                    || *p == "/"
                {
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
            "dc:2023-11-14",
            "photos/",
            "sourc",
            "stiinta",
            "ȘTIINȚĂ",
            "ecole",
            "path:ecole",
            "path:école/",
            "ete",
            "strasse",
            "ss*",
            "*ta.txt",
            "case:Știință",
            "wholeword:main",
            "ww:mai",
            "ww:path:src",
            "ww:data/Projects",
            "parent:/data/photos/",
            "infolder:/data",
            "size:>4mb",
            "folder: size:>0",
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

    #[test]
    fn date_words_and_periods() {
        use chrono::{Datelike, Local, NaiveDate, TimeZone};
        let at = |y, m, d| {
            Local
                .from_local_datetime(
                    &NaiveDate::from_ymd_opt(y, m, d)
                        .unwrap()
                        .and_hms_opt(0, 0, 0)
                        .unwrap(),
                )
                .earliest()
                .unwrap()
                .timestamp() as i128
        };
        assert_eq!(date("2026").unwrap(), (at(2026, 1, 1), at(2027, 1, 1) - 1));
        assert_eq!(
            date("2024-02").unwrap(),
            (at(2024, 2, 1), at(2024, 3, 1) - 1)
        );
        assert_eq!(
            date("2026-10-15").unwrap(),
            (at(2026, 10, 15), at(2026, 10, 16) - 1)
        );
        let today = Local::now().date_naive();
        let now = Local::now().timestamp() as i128;
        let (start, end) = date("today").unwrap();
        assert!(start <= now && now <= end);
        for word in ["thisweek", "thismonth", "THISYEAR"] {
            let (start, end) = date(word).unwrap();
            assert!(start <= now && now <= end, "{word}");
        }
        let (week_start, _) = date("thisweek").unwrap();
        assert_eq!(date("lastweek").unwrap().1, week_start - 1);
        let (month_start, _) = date("thismonth").unwrap();
        assert_eq!(date("lastmonth").unwrap().1, month_start - 1);
        assert_eq!(date("lastyear").unwrap().0, at(today.year() - 1, 1, 1));
        let (start, end) = date("last7days").unwrap();
        assert!(
            (end - start - 7 * 86400).abs() <= 3600,
            "a DST change shifts by an hour at most"
        );
        assert!((end - now).abs() <= 2);
        assert!(date("past2hours").unwrap().0 >= now - 2 * 3600 - 2);
        for bad in ["lastfortnight", "last7", "2026-13", "20261", "someday"] {
            assert!(date(bad).is_err(), "{bad}");
        }
        // Periods combine with comparisons and ranges.
        let q = Query::parse("dm:>=thismonth").unwrap();
        let recent = Entry::new(b"/a".to_vec(), Kind::File, 1, now as i64);
        let old = Entry::new(b"/b".to_vec(), Kind::File, 1, (month_start - 1) as i64);
        assert!(q.matches(&recent) && !q.matches(&old));
        assert!(Query::parse("dc:2025..lastmonth").is_ok());
    }
}
