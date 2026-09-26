//! POSIX shell quoting.
//!
//! `herdr pane run` types a command line into the chat pane's shell, so the
//! agent command has to survive that shell intact. Every argument is single
//! quoted; a single quote inside becomes `'\''`.

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

/// Joins an argv into one shell command line.
pub fn join(argv: &[String]) -> String {
    argv.iter()
        .map(|argument| quote(argument))
        .collect::<Vec<_>>()
        .join(" ")
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
            join(&["claude".into(), "hello; rm -rf /".into()]),
            "claude 'hello; rm -rf /'"
        );
    }
}
