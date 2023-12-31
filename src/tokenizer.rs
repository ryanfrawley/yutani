
pub fn tokenize(str: &str) -> Vec<String> {
    enum State {
        Whitespace,
        Token,
        Backtick,
        SingleQuote,
        DoubleQuote,
        Variable,
    }

    let mut tokens: Vec<String> = Vec::new();
    let mut state: State = State::Whitespace;
    let mut token = String::new();
    let mut escape = false;
    for c in str.chars() {
        if escape {
            escape = false;
            println!("pushed esc char: {}", c);
            token.push(c);
            continue;
        }

        match state {
            State::SingleQuote => {
                match c {
                    '\'' => state = State::Token,
                    '$' => state = State::Variable,
                    '\\' => escape = true,
                    _ => token.push(c),
                };
                continue;
            },
            State::DoubleQuote => {
                match c {
                    '"' => state = State::Token,
                    '$' => state = State::Variable,
                    '\\' => escape = true,
                    _ => token.push(c),
                };
                continue;
            },
            State::Backtick => {
                if c == '`' {
                    // TODO: Parse backtick contents as new input and pipe output to current
                    // token
                    state = State::Token;
                }
                continue;
            }
            State::Whitespace => {
                match c {
                    _ if c.is_whitespace() => (),
                    '\'' => state = State::SingleQuote,
                    '"' => state = State::DoubleQuote,
                    '`' => state = State::Backtick,
                    '$' => state = State::Variable,
                    '\\' => {
                        escape = true;
                        state = State::Token;
                    }
                    _ => {
                        state = State::Token;
                        token.push(c);
                    },
                }
            },
            State::Token => {
                match c {
                    _ if c.is_whitespace() => {
                        tokens.push(token);
                        token = String::new();
                        state = State::Whitespace;
                    },
                    '\'' => {
                        state = State::SingleQuote;
                    },
                    '"' => {
                        state = State::DoubleQuote;
                    },
                    '`' => {
                        state = State::Backtick;
                    },
                    '\\' => {
                        escape = true;
                        state = State::Token;
                    }
                    _ => token.push(c),
                };
            },
            State::Variable => {
                if c.is_whitespace() {
                    state = State::Whitespace;
                }
            }
        };
    }

    println!("token: {}", token);

    match state {
        State::Whitespace => (),
        State::Token | State::Variable => tokens.push(token),
        State::Backtick => panic!("unterminated backtick"),
        State::DoubleQuote => panic!("unterminated double quote"),
        State::SingleQuote => panic!("unterminated single quote"),
    }
    tokens
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty() {
        let tokens = tokenize("");
        assert_eq!(tokens, [] as [String; 0]);
    }

    #[test]
    fn basic() {
        let tokens = tokenize("hello world");
        assert_eq!(tokens, vec!["hello", "world"]);
    }

    #[test]
    fn whitespace() {
        let tokens = tokenize("  white    space  ");
        assert_eq!(tokens, vec!["white", "space"]);
    }

    #[test]
    fn dquote() {
        let tokens = tokenize("hello \"world\"");
        assert_eq!(tokens, vec!["hello", "world"]);
    }

    #[test]
    fn dquote_whitespace() {
        let tokens = tokenize("h\"ello  worl\"d");
        assert_eq!(tokens, vec!["hello  world"]);
    }

    #[test]
    fn dquote_multiple() {
        let tokens = tokenize("h\"e\"llo  \"worl\"d");
        assert_eq!(tokens, vec!["hello", "world"]);
    }

    #[test]
    fn squote() {
        let tokens = tokenize("hello 'world'");
        assert_eq!(tokens, vec!["hello", "world"]);
    }

    #[test]
    fn escape_backslash() {
        let tokens = tokenize("\\\\escape");
        assert_eq!(tokens, vec!["\\escape"]);
    }

    #[test]
    fn escape_squote() {
        let tokens = tokenize("\\'test");
        assert_eq!(tokens, vec!["'test"]);
    }

    #[test]
    fn escape_dquote1() {
        let tokens = tokenize("\\\"test");
        assert_eq!(tokens, vec!["\"test"]);
    }

    #[test]
    fn escape_dquote2() {
        let tokens = tokenize("test\\\"");
        assert_eq!(tokens, vec!["test\""]);
    }

    #[test]
    fn escape_dquote_wrapped_dquote() {
        let tokens = tokenize("\"te\\\"st\"");
        assert_eq!(tokens, vec!["te\"st"]);
    }
}
