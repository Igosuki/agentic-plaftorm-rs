use crate::{Error, Result};

/// Expands `${VAR}` and `${VAR:-default}`; a missing variable without a default is an error.
pub fn expand_env(input: &str) -> Result<String> {
    expand_with(input, |name| std::env::var(name).ok())
}

fn expand_with(input: &str, lookup: impl Fn(&str) -> Option<String>) -> Result<String> {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after.find('}').ok_or(Error::UnterminatedExpansion)?;
        let expr = &after[..end];
        let (name, default) = match expr.split_once(":-") {
            Some((n, d)) => (n, Some(d)),
            None => (expr, None),
        };
        match lookup(name) {
            Some(v) if !(v.is_empty() && default.is_some()) => out.push_str(&v),
            _ => match default {
                Some(d) => out.push_str(d),
                None => return Err(Error::MissingEnv(name.to_owned())),
            },
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(name: &str) -> Option<String> {
        match name {
            "A" => Some("alpha".into()),
            "EMPTY" => Some(String::new()),
            _ => None,
        }
    }

    #[test]
    fn plain_and_set() {
        assert_eq!(expand_with("x ${A} y", env).unwrap(), "x alpha y");
        assert_eq!(expand_with("no vars", env).unwrap(), "no vars");
    }

    #[test]
    fn default_used_when_unset_or_empty() {
        assert_eq!(expand_with("${B:-d:1}", env).unwrap(), "d:1");
        assert_eq!(expand_with("${EMPTY:-d}", env).unwrap(), "d");
        assert_eq!(expand_with("${A:-d}", env).unwrap(), "alpha");
        assert_eq!(expand_with("${B:-}", env).unwrap(), "");
    }

    #[test]
    fn missing_without_default_errors() {
        assert!(matches!(expand_with("${B}", env), Err(Error::MissingEnv(n)) if n == "B"));
    }

    #[test]
    fn unterminated_errors() {
        assert!(matches!(
            expand_with("${A", env),
            Err(Error::UnterminatedExpansion)
        ));
    }
}
