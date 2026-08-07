//! What somebody typed in the address bar, as a URL.
//!
//! Its own module because it is the one piece of the browser pane that has
//! nothing to do with a window. A `WebContentsView` and a Tauri child webview
//! share no API at all, but they need the same answer to "is `localhost:5173`
//! a host or a search" — and that answer has five tests attached, so the one
//! thing that must not happen is a second copy of it in JavaScript.
//!
//! Reached from the Electron shell as the `resolve_url` command: a backend
//! command rather than a `browser_*` verb, because deciding what a string means
//! needs no view. See `AGENTS.md` rule 9h.

/// What someone typed in the address bar, as a URL.
///
/// The whole of the guesswork, kept pure so it can be tested without a window.
/// Three rules, in order:
///
///  - An explicit scheme is honoured, including `file:` and `about:`.
///  - A loopback host gets **`http`**, not `https`. This is the case the
///    feature exists for — a dev server on `localhost:5173` is plain HTTP, and
///    defaulting it to `https` produces a TLS error page for the single most
///    common thing anyone will type in here.
///  - Anything else that looks like a host gets `https`.
///
/// A bare word is an error rather than a search: this app has no search
/// provider, and quietly sending what someone typed to one would be sending it
/// somewhere they did not name.
pub fn to_url(input: &str) -> Result<String, String> {
    let text = input.trim();
    if text.is_empty() {
        return Err("type an address".into());
    }
    if let Some((scheme, _)) = text.split_once("://") {
        if scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '-')
        {
            return Ok(text.to_string());
        }
    }
    if text.starts_with("about:") || text.starts_with("file:") || text.starts_with("data:") {
        return Ok(text.to_string());
    }

    // Past the explicit schemes, whitespace means this is not an address — and
    // saying so is the whole job. Without this, `127.0.0.1: 12587` gets a
    // scheme prepended and comes back as `http://127.0.0.1: 12587`, which no
    // URL parser accepts: the browser then reports its own `ERR_INVALID_URL`
    // over a string this function built. A refusal names what is wrong with
    // what was typed; an error code names what is wrong with what we sent.
    // (A full URL with a space in it — a `data:` document, a path — already
    // returned above, so nothing legitimate reaches here.)
    if text.chars().any(char::is_whitespace) {
        return Err(format!(
            "'{text}' is not an address — it has a space in it."
        ));
    }

    let host = text.split(['/', '?', '#']).next().unwrap_or(text);
    let name = host.split(':').next().unwrap_or(host);
    let loopback = name.eq_ignore_ascii_case("localhost")
        || name == "127.0.0.1"
        || name == "::1"
        || name == "[::1]";
    if loopback {
        return Ok(format!("http://{text}"));
    }
    // A dot or a port is what separates "a host" from "a word someone typed".
    if name.contains('.') || host.contains(':') {
        return Ok(format!("https://{text}"));
    }
    Err(format!(
        "'{text}' is not an address. Type a host (example.com), a loopback address (localhost:5173) or a full URL."
    ))
}

/// The command form. `to_url` borrows; the registry hands arguments over by
/// value, and one line here is cheaper than a signature bent to suit a macro.
pub fn resolve_url(input: String) -> Result<String, String> {
    to_url(&input)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_explicit_scheme_is_left_alone() {
        assert_eq!(
            to_url("https://github.com/x").unwrap(),
            "https://github.com/x"
        );
        assert_eq!(to_url("http://example.com").unwrap(), "http://example.com");
        assert_eq!(to_url("about:blank").unwrap(), "about:blank");
        assert_eq!(to_url("file:///tmp/x.html").unwrap(), "file:///tmp/x.html");
    }

    /// The case the browser pane was asked for. `https` here would put a TLS
    /// error page in front of the single most common thing typed into it.
    #[test]
    fn a_dev_server_is_plain_http() {
        assert_eq!(to_url("localhost:5173").unwrap(), "http://localhost:5173");
        assert_eq!(
            to_url("127.0.0.1:8080/app").unwrap(),
            "http://127.0.0.1:8080/app"
        );
        assert_eq!(to_url("LOCALHOST:3000").unwrap(), "http://LOCALHOST:3000");
        // …and a real host still is not.
        assert_eq!(to_url("github.com").unwrap(), "https://github.com");
    }

    #[test]
    fn a_host_gets_https_and_keeps_its_path() {
        assert_eq!(
            to_url("docs.rs/tauri/latest?q=1#frag").unwrap(),
            "https://docs.rs/tauri/latest?q=1#frag"
        );
    }

    /// Not a search box. Sending what someone typed to a search provider would
    /// be sending it somewhere they did not name.
    #[test]
    fn a_bare_word_is_refused_rather_than_searched() {
        let refusal = to_url("how do i center a div").unwrap_err();
        assert!(refusal.contains("is not an address"), "{refusal}");
        assert!(to_url("   ").is_err());
    }

    /// Something that looks like a host but has a space in it is refused here
    /// rather than turned into a URL nothing can load.
    ///
    /// Found by accident: a synthetic keystroke put a space in the address bar,
    /// and what came back on screen was Chromium's `ERR_INVALID_URL` — an error
    /// about a string *this function produced*, which tells the person typing
    /// nothing about what they typed.
    #[test]
    fn an_address_with_a_space_in_it_is_refused_not_prefixed() {
        assert!(to_url("127.0.0.1: 12587").is_err());
        assert!(to_url("example .com").is_err());
        // …but a full URL is honoured before this ever applies, spaces and all.
        assert_eq!(
            to_url("data:text/html,<p>a b</p>").unwrap(),
            "data:text/html,<p>a b</p>"
        );
    }
}
