//! `alias` and `unalias`.
//!
//! The parser expands aliases while it reads each chunk, with the table the shell
//! passes to `Parser::set_aliases` (`Vp::aliases`, when `Vp::expand_aliases` is on);
//! these builtins only edit and print the table. The work is done by pure functions
//! that return what to write in order, so stdout and stderr interleave as in bash.

use psh_syntax::Aliases;

use crate::state::{R, Vp};

/// `alias [-p] [name[=value] ...]`.
pub(crate) async fn alias(vp: &mut Vp, args: &[Vec<u8>]) -> R {
    let mut outcome = alias_args(&mut vp.aliases, &args[1..]);
    if vp.opts.get(crate::options::SetOpt::Posix)
        && !args[1..]
            .iter()
            .take_while(|a| a.starts_with(b"-") && *a != b"--")
            .any(|a| a.contains(&b'p'))
    {
        for o in &mut outcome.output {
            if let Output::Stdout(line) = o
                && let Some(rest) = line.strip_prefix(b"alias ")
            {
                *line = rest.strip_prefix(b"-- ").unwrap_or(rest).to_vec();
            }
        }
    }
    emit(vp, "alias", outcome).await
}

/// `unalias [-a] name ...`.
pub(crate) async fn unalias(vp: &mut Vp, args: &[Vec<u8>]) -> R {
    let outcome = unalias_args(&mut vp.aliases, &args[1..]);
    emit(vp, "unalias", outcome).await
}

async fn emit(vp: &mut Vp, name: &str, outcome: Outcome) -> R {
    let mut status = outcome.status;
    for o in outcome.output {
        match o {
            Output::Stdout(b) => {
                if super::out(vp, name, &b).await? != 0 {
                    status = 1;
                }
            }
            Output::Error(m) => vp.error(&m).await,
            Output::Usage(u) => vp.error_raw(u.as_bytes()).await,
        }
    }
    Ok(status)
}

/// Something to write, in order.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Output {
    /// Bytes for stdout.
    Stdout(Vec<u8>),
    /// A message bash prints with its `NAME: line N: ` prefix.
    Error(String),
    /// A usage line bash prints as is (it ends in a newline).
    Usage(&'static str),
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Outcome {
    pub status: i32,
    pub output: Vec<Output>,
}

const ALIAS_USAGE: &str = "alias: usage: alias [-p] [name[=value] ... ]\n";
const UNALIAS_USAGE: &str = "unalias: usage: unalias [-a] name [name ...]\n";

/// `alias [-p] [name[=value] ...]` (the arguments after the command name).
pub(crate) fn alias_args(table: &mut Aliases, args: &[Vec<u8>]) -> Outcome {
    let mut output = Vec::new();
    let mut status = 0;
    let mut print_all = args.is_empty();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if a == b"--" {
            i += 1;
            break;
        }
        if a.len() < 2 || a[0] != b'-' {
            break;
        }
        for &c in &a[1..] {
            if c == b'p' {
                print_all = true;
            } else {
                output.push(Output::Error(format!(
                    "alias: -{}: invalid option",
                    char::from(c)
                )));
                output.push(Output::Usage(ALIAS_USAGE));
                return Outcome { status: 2, output };
            }
        }
        i += 1;
    }
    if print_all {
        for (name, value) in table.iter() {
            output.push(Output::Stdout(definition(name, value)));
        }
    }
    for a in &args[i..] {
        match a.iter().position(|&c| c == b'=') {
            Some(eq) if eq > 0 => {
                let (name, value) = (&a[..eq], &a[eq + 1..]);
                if valid_name(name) {
                    table.set(name, value);
                } else {
                    output.push(Output::Error(format!(
                        "alias: `{}': invalid alias name",
                        String::from_utf8_lossy(name)
                    )));
                    status = 1;
                }
            }
            _ => match table.get(a) {
                Some(value) => output.push(Output::Stdout(definition(a, value))),
                None => {
                    output.push(Output::Error(format!(
                        "alias: {}: not found",
                        String::from_utf8_lossy(a)
                    )));
                    status = 1;
                }
            },
        }
    }
    Outcome { status, output }
}

/// `unalias [-a] name ...`.
pub(crate) fn unalias_args(table: &mut Aliases, args: &[Vec<u8>]) -> Outcome {
    let mut output = Vec::new();
    let mut i = 0;
    let mut all = false;
    while i < args.len() {
        let a = &args[i];
        if a == b"--" {
            i += 1;
            break;
        }
        if a.len() < 2 || a[0] != b'-' {
            break;
        }
        for &c in &a[1..] {
            if c == b'a' {
                all = true;
            } else {
                output.push(Output::Error(format!(
                    "unalias: -{}: invalid option",
                    char::from(c)
                )));
                output.push(Output::Usage(UNALIAS_USAGE));
                return Outcome { status: 2, output };
            }
        }
        i += 1;
    }
    if all {
        table.clear();
        return Outcome { status: 0, output };
    }
    if i == args.len() {
        output.push(Output::Usage(UNALIAS_USAGE));
        return Outcome { status: 2, output };
    }
    let mut status = 0;
    for name in &args[i..] {
        if !table.remove(name) {
            output.push(Output::Error(format!(
                "unalias: {}: not found",
                String::from_utf8_lossy(name)
            )));
            status = 1;
        }
    }
    Outcome { status, output }
}

/// Bash refuses `/`, `$`, `` ` ``, `=`, blanks, metacharacters and quotes in alias names.
fn valid_name(name: &[u8]) -> bool {
    !name.iter().any(|c| {
        matches!(
            c,
            b'/' | b'$'
                | b'`'
                | b'='
                | b' '
                | b'\t'
                | b'\n'
                | b'|'
                | b'&'
                | b';'
                | b'('
                | b')'
                | b'<'
                | b'>'
                | b'\\'
                | b'\''
                | b'"'
        )
    })
}

/// `alias name='value'` (with `--` before names that start with `-`), as `alias` prints
/// a definition: the value single-quoted, each `'` as `'\''`.
pub(crate) fn definition(name: &[u8], value: &[u8]) -> Vec<u8> {
    let mut out = b"alias ".to_vec();
    if name.first() == Some(&b'-') {
        out.extend_from_slice(b"-- ");
    }
    out.extend_from_slice(name);
    // A lone quote is printed as backslash-quote.
    if value == b"'" {
        out.extend_from_slice(b"=\\'\n");
        return out;
    }
    out.extend_from_slice(b"='");
    for &c in value {
        if c == b'\'' {
            out.extend_from_slice(b"'\\''");
        } else {
            out.push(c);
        }
    }
    out.extend_from_slice(b"'\n");
    out
}
