// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The dotenv files that `env.load_file` and `env.read` read.
//!
//! A dotenv file holds one assignment per line, `NAME=value`, optionally after
//! `export` and whitespace. Blank lines and lines whose first non-blank character
//! is `#` are skipped. A value is either unquoted, the rest of the line with
//! surrounding whitespace removed, or quoted:
//!
//! * single quotes keep every character literally, up to the closing quote;
//! * double quotes support the escapes `\n`, `\t`, `\"` and `\\`, and no other.
//!
//! After a closing quote only whitespace may follow. A name starts with a letter or
//! `_` and continues with letters, digits, and `_`. There is no `${VAR}` expansion:
//! `$` is an ordinary character. When a name is assigned twice, the later value
//! wins. Any other line is a syntax error that names its line, and it fails the
//! task.

use std::collections::BTreeMap;

/// A line of a dotenv file that is not blank, not a comment, and not an
/// assignment in a supported form.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("line {line}: {message}")]
pub struct DotenvError {
    /// The line, counting from 1.
    pub line: usize,
    /// What is wrong with it.
    pub message: String,
}

/// Parse the dotenv `text` into its variables.
///
/// # Errors
///
/// Returns [`DotenvError`] for the first line that is not blank, not a comment,
/// and not an assignment in a supported form.
pub fn parse(text: &str) -> Result<BTreeMap<String, String>, DotenvError> {
    let mut vars = BTreeMap::new();
    for (line, content) in (1..).zip(text.lines()) {
        if let Some((name, value)) =
            parse_line(content).map_err(|message| DotenvError { line, message })?
        {
            vars.insert(name, value);
        }
    }
    Ok(vars)
}

/// Parse one line: `None` for a blank line or a comment, the name and value of an
/// assignment, or what is wrong with the line.
fn parse_line(line: &str) -> Result<Option<(String, String)>, String> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return Ok(None);
    }
    let assignment = line
        .strip_prefix("export")
        .filter(|rest| rest.starts_with([' ', '\t']))
        .map_or(line, str::trim_start);
    let Some((name, value)) = assignment.split_once('=') else {
        return Err(format!(
            "expected NAME=value, a comment starting with #, or a blank line, not `{line}`"
        ));
    };
    if !is_valid_name(name) {
        return Err(format!(
            "`{name}` is not a valid variable name: a name starts with a letter or _ and \
             continues with letters, digits and _, with no space before ="
        ));
    }
    Ok(Some((name.to_owned(), parse_value(value)?)))
}

/// Whether `name` is a variable name: a letter or `_`, then letters, digits, and
/// `_`.
fn is_valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Parse the value after `=`: unquoted, single-quoted, or double-quoted.
fn parse_value(value: &str) -> Result<String, String> {
    let value = value.trim();
    let mut chars = value.chars();
    let (text, rest) = match chars.next() {
        Some('\'') => {
            let inner = chars.as_str();
            let Some((text, rest)) = inner.split_once('\'') else {
                return Err("the single-quoted value has no closing '".to_owned());
            };
            (text.to_owned(), rest)
        },
        Some('"') => parse_double_quoted(chars.as_str())?,
        _ => return Ok(value.to_owned()),
    };
    if rest.trim().is_empty() {
        Ok(text)
    } else {
        Err(format!(
            "unexpected `{}` after the closing quote",
            rest.trim()
        ))
    }
}

/// Parse a double-quoted value after its opening quote, returning the value and
/// the text after the closing quote.
fn parse_double_quoted(inner: &str) -> Result<(String, &str), String> {
    let mut text = String::new();
    let mut chars = inner.chars();
    loop {
        match chars.next() {
            None => return Err("the double-quoted value has no closing \"".to_owned()),
            Some('"') => return Ok((text, chars.as_str())),
            Some('\\') => {
                match chars.next() {
                    Some('n') => text.push('\n'),
                    Some('t') => text.push('\t'),
                    Some('"') => text.push('"'),
                    Some('\\') => text.push('\\'),
                    Some(other) => {
                        return Err(format!(
                            "`\\{other}` is not a supported escape; double quotes support \\n, \
                             \\t, \\\" and \\\\"
                        ));
                    },
                    None => return Err("the double-quoted value has no closing \"".to_owned()),
                }
            },
            Some(other) => text.push(other),
        }
    }
}

/// Unit tests for the dotenv parser.
#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::{DotenvError, parse};

    /// The variables of `pairs`.
    fn vars(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|&(name, value)| (name.to_owned(), value.to_owned()))
            .collect()
    }

    /// `NAME=value` lines, comments, blank lines, and the `export` prefix.
    #[test]
    fn assignments_comments_blank_lines_and_export() {
        let text = "# a comment\n\nPLAIN=value\n  INDENTED=yes  \nexport EXPORTED=1\nexport\tTABBED=2\n   \
                    # indented comment\nEMPTY=\n_UNDER_9=x y  z\nexport=not a prefix\nURL=http://h/p#frag\r\n";
        assert_eq!(
            parse(text),
            Ok(vars(&[
                ("PLAIN", "value"),
                ("INDENTED", "yes"),
                ("EXPORTED", "1"),
                ("TABBED", "2"),
                ("EMPTY", ""),
                ("_UNDER_9", "x y  z"),
                ("export", "not a prefix"),
                ("URL", "http://h/p#frag"),
            ]))
        );
    }

    /// Single quotes keep every character literally, escapes and `#` included.
    #[test]
    fn single_quotes_are_literal() {
        assert_eq!(
            parse("A='  spaced  '\nB='back\\n\\slash \"q\" # not a comment'\nC=''\n"),
            Ok(vars(&[
                ("A", "  spaced  "),
                ("B", "back\\n\\slash \"q\" # not a comment"),
                ("C", ""),
            ]))
        );
    }

    /// Double quotes support `\n`, `\t`, `\"`, and `\\`.
    #[test]
    fn double_quotes_support_each_escape() {
        assert_eq!(
            parse(
                "NL=\"a\\nb\"\nTAB=\"a\\tb\"\nQUOTE=\"say \\\"hi\\\"\"\nSLASH=\"c:\\\\dir\"\nALL=\" \
                 \\n\\t\\\"\\\\ \"\nSINGLE=\"it's\"\n"
            ),
            Ok(vars(&[
                ("NL", "a\nb"),
                ("TAB", "a\tb"),
                ("QUOTE", "say \"hi\""),
                ("SLASH", "c:\\dir"),
                ("ALL", " \n\t\"\\ "),
                ("SINGLE", "it's"),
            ]))
        );
    }

    /// `${VAR}` and `$VAR` stay as written, in every form, even when the variable
    /// is set earlier in the file.
    #[test]
    fn there_is_no_expansion() {
        assert_eq!(
            parse("HOME=/home/me\nA=${HOME}/x\nB=\"$HOME/y\"\nC='${HOME}'\n"),
            Ok(vars(&[
                ("HOME", "/home/me"),
                ("A", "${HOME}/x"),
                ("B", "$HOME/y"),
                ("C", "${HOME}"),
            ]))
        );
    }

    /// A later assignment of the same name wins.
    #[test]
    fn the_later_assignment_wins() {
        assert_eq!(parse("A=1\nA=2\n"), Ok(vars(&[("A", "2")])));
    }

    /// Every line that is not blank, not a comment, and not an assignment in a
    /// supported form is an error that names its line (PI6).
    #[test]
    fn syntax_errors_name_the_line() {
        for (text, line, part) in [
            ("A=1\njust words\n", 2, "expected NAME=value"),
            ("A=1\n\n=value\n", 3, "is not a valid variable name"),
            ("1A=x\n", 1, "`1A` is not a valid variable name"),
            ("A B=x\n", 1, "`A B` is not a valid variable name"),
            ("A =x\n", 1, "with no space before ="),
            ("export A\n", 1, "expected NAME=value"),
            ("A='open\n", 1, "no closing '"),
            ("A=\"open\n", 1, "no closing \""),
            ("A=\"end\\\n", 1, "no closing \""),
            ("A=\"bad \\x\"\n", 1, "`\\x` is not a supported escape"),
            (
                "A='x' trailing\n",
                1,
                "unexpected `trailing` after the closing quote",
            ),
            ("A=\"x\" # comment\n", 1, "unexpected `# comment`"),
        ] {
            let Err(DotenvError {
                line: found,
                message,
            }) = parse(text)
            else {
                panic!("{text:?} must be a syntax error");
            };
            assert_eq!(found, line, "{text:?}: {message}");
            assert!(message.contains(part), "{part:?} in {message}");
        }
        assert_eq!(
            parse("A=1\nbad\n").map_err(|error| error.to_string()),
            Err(
                "line 2: expected NAME=value, a comment starting with #, or a blank line, not \
                 `bad`"
                    .to_owned()
            )
        );
    }
}
