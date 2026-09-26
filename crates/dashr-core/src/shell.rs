//! Shell quoting for the command typed into the chat pane.
//!
//! `herdr pane run` types a command line into the chat pane's shell, so the
//! agent command has to survive that shell intact. On Unix that shell is
//! POSIX: every argument is single quoted and a single quote inside becomes
//! `'\''`. On Windows it is PowerShell, where a single quote inside single
//! quotes is written twice (DEC-038).

/// Quotes one argument for a POSIX shell.
pub fn quote(argument: &str) -> String {
    if !argument.is_empty()
        && argument
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./=:,@%+".contains(c))
    {
        return argument.to_owned();
    }
    format!("'{}'", argument.replace('\'', r"'\''"))
}

/// Joins an argv into one POSIX shell command line.
pub fn join_posix(argv: &[String]) -> String {
    argv.iter()
        .map(|argument| quote(argument))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Quotes one argument for PowerShell.
pub fn quote_powershell(argument: &str) -> String {
    if !argument.is_empty()
        && argument
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./=:\\".contains(c))
        && !argument.starts_with('-')
    {
        return argument.to_owned();
    }
    format!("'{}'", argument.replace('\'', "''"))
}

/// Joins an argv into one PowerShell command line. The call operator makes
/// PowerShell run the first word even when it had to be quoted.
pub fn join_powershell(argv: &[String]) -> String {
    let words: Vec<String> = argv
        .iter()
        .map(|argument| quote_powershell(argument))
        .collect();
    format!("& {}", words.join(" "))
}

/// Joins an argv for the shell of the platform dashr runs on.
pub fn join(argv: &[String]) -> String {
    if cfg!(windows) {
        join_powershell(argv)
    } else {
        join_posix(argv)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_words_are_left_alone() {
        assert_eq!(quote("claude"), "claude");
        assert_eq!(quote("/dev/shm/x/mcp.json"), "/dev/shm/x/mcp.json");
    }

    #[test]
    fn everything_else_is_single_quoted() {
        assert_eq!(quote(""), "''");
        assert_eq!(quote("a b"), "'a b'");
        assert_eq!(quote("it's $HOME `x`"), r"'it'\''s $HOME `x`'");
        assert_eq!(
            join_posix(&["claude".into(), "hello; rm -rf /".into()]),
            "claude 'hello; rm -rf /'"
        );
    }

    #[test]
    fn powershell_doubles_single_quotes_and_uses_the_call_operator() {
        assert_eq!(quote_powershell("claude"), "claude");
        assert_eq!(quote_powershell(r"C:\Temp\mcp.json"), r"C:\Temp\mcp.json");
        assert_eq!(quote_powershell("--mcp-config"), "'--mcp-config'");
        assert_eq!(
            quote_powershell("it's $env:HOME `x`"),
            "'it''s $env:HOME `x`'"
        );
        assert_eq!(
            join_powershell(&["claude".into(), "the human's prompt; rm".into()]),
            "& claude 'the human''s prompt; rm'"
        );
    }
}
