//! Frontmatter: a Markdown file's leading `---` YAML block, parsed defensively.
//!
//! Resource files come from repositories aim did not write (and, under SSH, from another host),
//! so parsing is bounded and never expands anything:
//!
//! - the block is at most [`MAX_FRONTMATTER`] bytes, nests at most [`MAX_DEPTH`] deep and holds
//!   at most [`MAX_NODES`] nodes;
//! - YAML anchors and aliases are refused: `yaml-rust2`'s loader expands an alias by copying the
//!   anchored subtree (the "billion laughs" shape), so only its event parser is used and the tree
//!   is built here;
//! - scalars stay strings: `name: yes` is the string `yes`, never a boolean, and `1234` is the
//!   string `1234`; a plain `~`, `null` or nothing is [`Value::Null`];
//! - a key may appear once.

use yaml_rust2::parser::{Event, Parser};
use yaml_rust2::scanner::TScalarStyle;

/// Largest frontmatter block parsed, in bytes.
pub const MAX_FRONTMATTER: usize = 16 * 1024;
/// Deepest nesting of lists and maps.
pub const MAX_DEPTH: usize = 8;
/// Most YAML nodes (scalars, lists, maps) in one block.
pub const MAX_NODES: usize = 1024;

/// A frontmatter value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Value {
    /// `~`, `null` or nothing.
    Null,
    /// Any scalar, as written.
    Str(String),
    /// A list.
    Seq(Vec<Value>),
    /// A map, in document order.
    Map(Vec<(String, Value)>),
}

impl Value {
    /// What kind of value this is, for messages.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Null => "empty",
            Self::Str(_) => "a string",
            Self::Seq(_) => "a list",
            Self::Map(_) => "a map",
        }
    }
}

/// A file split at its frontmatter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Split<'a> {
    /// The file has no frontmatter: all of it is the body.
    None(&'a str),
    /// The frontmatter's YAML and the body after it.
    Found {
        /// The YAML between the fences.
        yaml: &'a str,
        /// Everything after the closing fence.
        body: &'a str,
    },
    /// A `---` opens a block that never closes.
    Unterminated,
}

/// Splits `text` at a leading `---` … `---` (or `...`) block. A UTF-8 byte-order mark is skipped.
#[must_use]
pub fn split(text: &str) -> Split<'_> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut lines = text.split_inclusive('\n');
    let Some(first) = lines.next() else {
        return Split::None(text);
    };
    if first.trim_end() != "---" {
        return Split::None(text);
    }
    let start = first.len();
    let mut at = start;
    for line in lines {
        let end = at.saturating_add(line.len());
        let fence = line.trim_end();
        if fence == "---" || fence == "..." {
            let (Some(yaml), Some(body)) = (text.get(start..at), text.get(end..)) else {
                return Split::Unterminated;
            };
            return Split::Found { yaml, body };
        }
        at = end;
    }
    Split::Unterminated
}

enum Frame {
    Seq(Vec<Value>),
    Map(Vec<(String, Value)>, Option<String>),
}

/// Parses a frontmatter block into its top-level map (empty for an empty block).
///
/// # Errors
/// A message for a diagnostic: malformed YAML, a block over the bounds, an anchor or alias, a
/// non-scalar or duplicate key, or a top level that is not a map.
pub fn parse(yaml: &str) -> Result<Vec<(String, Value)>, String> {
    if yaml.len() > MAX_FRONTMATTER {
        return Err(format!("frontmatter is {} bytes; at most {MAX_FRONTMATTER} are read", yaml.len()));
    }
    let mut parser = Parser::new_from_str(yaml);
    let mut stack: Vec<Frame> = Vec::new();
    let mut root: Option<Value> = None;
    let mut nodes = 0_usize;
    loop {
        let (event, mark) = parser.next_token().map_err(|e| format!("invalid YAML at line {}: {}", e.marker().line(), e.info()))?;
        let line = mark.line();
        match event {
            Event::StreamEnd => break,
            Event::Nothing | Event::StreamStart | Event::DocumentStart | Event::DocumentEnd => continue,
            Event::Alias(_) => return Err(format!("line {line}: YAML aliases are not supported")),
            Event::Scalar(text, style, anchor, _) => {
                refuse_anchor(anchor, line)?;
                let value = if style == TScalarStyle::Plain && matches!(text.as_str(), "" | "~" | "null" | "Null" | "NULL") {
                    Value::Null
                } else {
                    Value::Str(text)
                };
                place(&mut stack, &mut root, value, line)?;
            }
            Event::SequenceStart(anchor, _) | Event::MappingStart(anchor, _) => {
                refuse_anchor(anchor, line)?;
                if stack.len() >= MAX_DEPTH {
                    return Err(format!("line {line}: nested deeper than {MAX_DEPTH} levels"));
                }
                stack.push(if matches!(event, Event::SequenceStart(..)) { Frame::Seq(Vec::new()) } else { Frame::Map(Vec::new(), None) });
            }
            Event::SequenceEnd | Event::MappingEnd => {
                let value = match stack.pop() {
                    Some(Frame::Seq(items)) => Value::Seq(items),
                    Some(Frame::Map(entries, _)) => Value::Map(entries),
                    None => return Err(format!("line {line}: unbalanced YAML")),
                };
                place(&mut stack, &mut root, value, line)?;
            }
        }
        nodes = nodes.saturating_add(1);
        if nodes > MAX_NODES {
            return Err(format!("more than {MAX_NODES} YAML nodes"));
        }
    }
    match root {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Map(entries)) => Ok(entries),
        Some(other) => Err(format!("frontmatter must be a map of fields, not {}", other.kind())),
    }
}

fn refuse_anchor(anchor: usize, line: usize) -> Result<(), String> {
    if anchor == 0 { Ok(()) } else { Err(format!("line {line}: YAML anchors are not supported")) }
}

/// Puts a finished value where it belongs: the root, a list item, a map key or a map value.
fn place(stack: &mut [Frame], root: &mut Option<Value>, value: Value, line: usize) -> Result<(), String> {
    match stack.last_mut() {
        None => {
            if root.is_some() {
                return Err(format!("line {line}: more than one YAML document"));
            }
            *root = Some(value);
        }
        Some(Frame::Seq(items)) => items.push(value),
        Some(Frame::Map(entries, pending)) => {
            if let Some(key) = pending.take() {
                entries.push((key, value));
                return Ok(());
            }
            let Value::Str(key) = value else {
                return Err(format!("line {line}: a key must be a plain string, not {}", value.kind()));
            };
            if entries.iter().any(|(k, _)| *k == key) {
                return Err(format!("line {line}: duplicate key `{key}`"));
            }
            *pending = Some(key);
        }
    }
    Ok(())
}

/// A frontmatter's fields, taken one at a time so the unused ones can be reported.
#[derive(Clone, Debug, Default)]
pub struct Fields {
    entries: Vec<(String, Value)>,
}

impl Fields {
    /// Wraps parsed fields.
    #[must_use]
    pub const fn new(entries: Vec<(String, Value)>) -> Self {
        Self { entries }
    }

    /// Takes field `key`.
    pub fn take(&mut self, key: &str) -> Option<Value> {
        let at = self.entries.iter().position(|(k, _)| k == key)?;
        Some(self.entries.remove(at).1)
    }

    /// Takes a string field (`None` when absent or empty).
    ///
    /// # Errors
    /// When the field is a list or a map.
    pub fn string(&mut self, key: &str) -> Result<Option<String>, String> {
        match self.take(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Str(text)) => Ok(Some(text)),
            Some(other) => Err(format!("`{key}` must be a string, not {}", other.kind())),
        }
    }

    /// Takes a list of strings, written as a YAML list or as one string separated by commas (or,
    /// without commas, by whitespace): `Read, Grep`, `Read Grep` and `[Read, Grep]` are equal.
    ///
    /// # Errors
    /// When the field is a map, or a list holding something other than strings.
    pub fn list(&mut self, key: &str) -> Result<Option<Vec<String>>, String> {
        match self.take(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Str(text)) => Ok(Some(split_list(&text))),
            Some(Value::Seq(items)) => items
                .into_iter()
                .map(|item| match item {
                    Value::Str(text) => Ok(text.trim().to_owned()),
                    other => Err(format!("`{key}` must list strings, not {}", other.kind())),
                })
                .filter(|item| item.as_ref().map_or(true, |text| !text.is_empty()))
                .collect::<Result<Vec<_>, _>>()
                .map(Some),
            Some(other) => Err(format!("`{key}` must be a list, not {}", other.kind())),
        }
    }

    /// The names of the fields not taken, in document order.
    #[must_use]
    pub fn rest(&self) -> Vec<String> {
        self.entries.iter().map(|(k, _)| k.clone()).collect()
    }
}

/// Splits `Read, Grep` or `Read Grep` into names.
#[must_use]
pub fn split_list(text: &str) -> Vec<String> {
    let parts: Vec<&str> = if text.contains(',') { text.split(',').collect() } else { text.split_whitespace().collect() };
    parts.into_iter().map(str::trim).filter(|p| !p.is_empty()).map(str::to_owned).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(text: &str) -> Value {
        Value::Str(text.to_owned())
    }

    #[test]
    fn splits_at_the_fences() {
        assert_eq!(split("---\nname: a\n---\nbody\n"), Split::Found { yaml: "name: a\n", body: "body\n" });
        assert_eq!(split("\u{feff}---\r\nname: a\r\n...\r\nbody"), Split::Found { yaml: "name: a\r\n", body: "body" });
        assert_eq!(split("# Title\n---\n"), Split::None("# Title\n---\n"));
        assert_eq!(split("---\nname: a\n"), Split::Unterminated);
        assert_eq!(split(""), Split::None(""));
    }

    #[test]
    fn scalars_stay_strings() {
        let fields = parse("name: yes\nnumber: 1234\nempty:\ntilde: ~\nquoted: 'null'\nfolded: >\n  one\n  two\n").unwrap_or_default();
        assert_eq!(
            fields,
            vec![
                ("name".into(), s("yes")),
                ("number".into(), s("1234")),
                ("empty".into(), Value::Null),
                ("tilde".into(), Value::Null),
                ("quoted".into(), s("null")),
                ("folded".into(), s("one two\n")),
            ]
        );
    }

    #[test]
    fn lists_and_maps_nest() {
        let fields = parse("tools: [Read, Grep]\nmetadata:\n  owner: me\n  tags:\n    - a\n").unwrap_or_default();
        assert_eq!(
            fields,
            vec![
                ("tools".into(), Value::Seq(vec![s("Read"), s("Grep")])),
                ("metadata".into(), Value::Map(vec![("owner".into(), s("me")), ("tags".into(), Value::Seq(vec![s("a")]))])),
            ]
        );
    }

    #[test]
    fn refuses_what_it_does_not_bound() {
        for (yaml, why) in [
            ("a: &x [1]\nb: *x\n", "anchors"),
            ("a: *x\n", "invalid YAML"),
            ("a: 1\na: 2\n", "duplicate key"),
            ("[a, b]\n", "must be a map"),
            ("? [a]\n: b\n", "plain string"),
            ("a: [[[[[[[[[1]]]]]]]]]\n", "nested deeper"),
            ("a: 'unterminated\n", "invalid YAML"),
        ] {
            let error = parse(yaml).err().unwrap_or_default();
            assert!(error.contains(why), "{yaml:?}: {error}");
        }
        let many = format!("a: [{}]\n", vec!["1"; MAX_NODES].join(", "));
        assert!(parse(&many).err().unwrap_or_default().contains("nodes"));
        let big = format!("a: '{}'\n", "x".repeat(MAX_FRONTMATTER));
        assert!(parse(&big).err().unwrap_or_default().contains("bytes"));
        assert_eq!(parse("").unwrap_or_default(), Vec::new());
    }

    #[test]
    fn lists_come_in_three_spellings() {
        let mut fields = Fields::new(parse("a: Read, Grep\nb: Read Grep\nc: [Read, Grep]\nd: {x: 1}\n").unwrap_or_default());
        for key in ["a", "b", "c"] {
            assert_eq!(fields.list(key), Ok(Some(vec!["Read".to_owned(), "Grep".to_owned()])), "{key}");
        }
        assert!(fields.list("d").is_err());
        assert_eq!(fields.string("absent"), Ok(None));
        assert!(fields.rest().is_empty());
    }
}
