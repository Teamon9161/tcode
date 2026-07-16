//! Best-effort LaTeX → Unicode linearization for terminal display.
//!
//! The contract is conservative: every rewrite is token-local, and any
//! construct this module does not recognize passes through verbatim —
//! the output is never less readable than the TeX source it replaces.
//! No 2D typesetting: fractions flatten to `a/b`, scripts convert only
//! when every character has a Unicode super-/subscript form.

/// Linearize one math expression (the content between `$…$` / `$$…$$`).
pub fn prettify(tex: &str) -> String {
    let mut out = String::new();
    let mut chars = tex.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' => command(&mut chars, &mut out),
            '^' => script(&mut chars, &mut out, superscript, '^'),
            '_' => script(&mut chars, &mut out, subscript, '_'),
            _ => out.push(c),
        }
    }
    out
}

type Chars<'a> = std::iter::Peekable<std::str::Chars<'a>>;

/// A `\command`, just consumed past its backslash.
fn command(chars: &mut Chars, out: &mut String) {
    let mut name = String::new();
    while let Some(&c) = chars.peek() {
        if c.is_ascii_alphabetic() {
            name.push(c);
            chars.next();
        } else {
            break;
        }
    }
    if name.is_empty() {
        // Control symbols: `\,`/`\;`/`\ ` are thin spaces, `\!` is a
        // negative space, `\{`/`\}` escape literal braces.
        match chars.next() {
            Some(',') | Some(';') | Some(' ') => out.push(' '),
            Some('!') => {}
            Some(c) => out.push(c),
            None => out.push('\\'),
        }
        return;
    }
    match name.as_str() {
        "frac" | "dfrac" | "tfrac" => frac(chars, out),
        "sqrt" => sqrt(chars, out),
        // Upright-text wrappers: the content is the message.
        "text" | "textrm" | "textit" | "mathrm" | "mathit" | "operatorname" => match group(chars) {
            Some(body) => out.push_str(&prettify(&body)),
            None => {
                out.push('\\');
                out.push_str(&name);
            }
        },
        "mathbb" => match group(chars).as_deref().and_then(blackboard) {
            Some(sym) => out.push(sym),
            None => out.push_str("\\mathbb"),
        },
        // Sizing/spacing that has no linear equivalent: drop the command,
        // keep the delimiter or gap it decorated.
        "left" | "right" | "big" | "Big" | "bigg" | "Bigg" => {}
        "quad" | "qquad" => out.push(' '),
        _ => match symbol(&name) {
            Some(sym) => out.push_str(sym),
            // Function names read as their own text: `\sin x` → `sin x`.
            None if FUNCTION_NAMES.contains(&name.as_str()) => out.push_str(&name),
            None => {
                out.push('\\');
                out.push_str(&name);
            }
        },
    }
}

const FUNCTION_NAMES: [&str; 29] = [
    "sin", "cos", "tan", "cot", "sec", "csc", "arcsin", "arccos", "arctan", "sinh", "cosh", "tanh",
    "log", "ln", "lg", "exp", "lim", "sup", "inf", "max", "min", "arg", "det", "gcd", "deg", "dim",
    "ker", "Pr", "mod",
];

/// `\frac{a}{b}` → `a/b`, with parentheses around compound operands.
fn frac(chars: &mut Chars, out: &mut String) {
    let Some(num) = group(chars) else {
        out.push_str("\\frac");
        return;
    };
    let Some(den) = group(chars) else {
        out.push_str("\\frac{");
        out.push_str(&num);
        out.push('}');
        return;
    };
    out.push_str(&parenthesized(&prettify(&num)));
    out.push('/');
    out.push_str(&parenthesized(&prettify(&den)));
}

fn sqrt(chars: &mut Chars, out: &mut String) {
    // Optional index: `\sqrt[3]{x}` → `³√x` when the index has a
    // superscript form, `3√x` otherwise.
    if chars.peek() == Some(&'[') {
        chars.next();
        let mut index = String::new();
        for c in chars.by_ref() {
            if c == ']' {
                break;
            }
            index.push(c);
        }
        let index = prettify(&index);
        match index.chars().map(superscript).collect::<Option<String>>() {
            Some(sup) => out.push_str(&sup),
            None => out.push_str(&index),
        }
    }
    out.push('√');
    if let Some(body) = group(chars) {
        out.push_str(&parenthesized(&prettify(&body)));
    }
}

/// A `^`/`_` script, just consumed past its marker. Converts only when
/// every character of the (prettified) argument has a script form;
/// otherwise the marker and argument stay in linear TeX.
fn script(chars: &mut Chars, out: &mut String, map: fn(char) -> Option<char>, marker: char) {
    let (arg, braced) = match chars.peek() {
        Some(&'{') => match group(chars) {
            Some(body) => (body, true),
            None => {
                out.push(marker);
                return;
            }
        },
        Some(_) => (chars.next().unwrap().to_string(), false),
        None => {
            out.push(marker);
            return;
        }
    };
    let pretty = prettify(&arg);
    match pretty.chars().map(map).collect::<Option<String>>() {
        Some(converted) => out.push_str(&converted),
        None if braced => out.push_str(&format!("{marker}{{{pretty}}}")),
        None => {
            out.push(marker);
            out.push_str(&pretty);
        }
    }
}

/// One balanced `{…}` group, or None (input untouched) when the next
/// char is not `{` or the braces never close.
fn group(chars: &mut Chars) -> Option<String> {
    if chars.peek() != Some(&'{') {
        return None;
    }
    chars.next();
    let mut depth = 1usize;
    let mut body = String::new();
    for c in chars.by_ref() {
        match c {
            '{' => {
                depth += 1;
                body.push(c);
            }
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(body);
                }
                body.push(c);
            }
            _ => body.push(c),
        }
    }
    None
}

/// A fraction operand keeps its meaning without parentheses only when
/// it reads as one token.
fn parenthesized(s: &str) -> String {
    if s.chars().any(|c| " +-−±∓·×÷/=,".contains(c)) {
        format!("({s})")
    } else {
        s.to_string()
    }
}

fn blackboard(letter: &str) -> Option<char> {
    Some(match letter {
        "R" => 'ℝ',
        "N" => 'ℕ',
        "Z" => 'ℤ',
        "Q" => 'ℚ',
        "C" => 'ℂ',
        "E" => '𝔼',
        "P" => 'ℙ',
        _ => return None,
    })
}

fn superscript(c: char) -> Option<char> {
    Some(match c {
        '0' => '⁰',
        '1' => '¹',
        '2' => '²',
        '3' => '³',
        '4' => '⁴',
        '5' => '⁵',
        '6' => '⁶',
        '7' => '⁷',
        '8' => '⁸',
        '9' => '⁹',
        '+' => '⁺',
        '-' | '−' => '⁻',
        '=' => '⁼',
        '(' => '⁽',
        ')' => '⁾',
        'a' => 'ᵃ',
        'b' => 'ᵇ',
        'c' => 'ᶜ',
        'd' => 'ᵈ',
        'e' => 'ᵉ',
        'f' => 'ᶠ',
        'g' => 'ᵍ',
        'h' => 'ʰ',
        'i' => 'ⁱ',
        'j' => 'ʲ',
        'k' => 'ᵏ',
        'l' => 'ˡ',
        'm' => 'ᵐ',
        'n' => 'ⁿ',
        'o' => 'ᵒ',
        'p' => 'ᵖ',
        'r' => 'ʳ',
        's' => 'ˢ',
        't' => 'ᵗ',
        'u' => 'ᵘ',
        'v' => 'ᵛ',
        'w' => 'ʷ',
        'x' => 'ˣ',
        'y' => 'ʸ',
        'z' => 'ᶻ',
        'T' => 'ᵀ',
        _ => return None,
    })
}

fn subscript(c: char) -> Option<char> {
    Some(match c {
        '0' => '₀',
        '1' => '₁',
        '2' => '₂',
        '3' => '₃',
        '4' => '₄',
        '5' => '₅',
        '6' => '₆',
        '7' => '₇',
        '8' => '₈',
        '9' => '₉',
        '+' => '₊',
        '-' | '−' => '₋',
        '=' => '₌',
        '(' => '₍',
        ')' => '₎',
        'a' => 'ₐ',
        'e' => 'ₑ',
        'h' => 'ₕ',
        'i' => 'ᵢ',
        'j' => 'ⱼ',
        'k' => 'ₖ',
        'l' => 'ₗ',
        'm' => 'ₘ',
        'n' => 'ₙ',
        'o' => 'ₒ',
        'p' => 'ₚ',
        'r' => 'ᵣ',
        's' => 'ₛ',
        't' => 'ₜ',
        'u' => 'ᵤ',
        'v' => 'ᵥ',
        'x' => 'ₓ',
        _ => return None,
    })
}

/// Symbol-for-symbol commands. Function names (`\sin`, `\log`, …) map to
/// their bare text so `\sin x` reads `sin x`.
fn symbol(name: &str) -> Option<&'static str> {
    Some(match name {
        // Greek, lower then upper.
        "alpha" => "α",
        "beta" => "β",
        "gamma" => "γ",
        "delta" => "δ",
        "epsilon" | "varepsilon" => "ε",
        "zeta" => "ζ",
        "eta" => "η",
        "theta" => "θ",
        "vartheta" => "ϑ",
        "iota" => "ι",
        "kappa" => "κ",
        "lambda" => "λ",
        "mu" => "μ",
        "nu" => "ν",
        "xi" => "ξ",
        "pi" => "π",
        "rho" => "ρ",
        "sigma" => "σ",
        "varsigma" => "ς",
        "tau" => "τ",
        "upsilon" => "υ",
        "phi" => "ϕ",
        "varphi" => "φ",
        "chi" => "χ",
        "psi" => "ψ",
        "omega" => "ω",
        "Gamma" => "Γ",
        "Delta" => "Δ",
        "Theta" => "Θ",
        "Lambda" => "Λ",
        "Xi" => "Ξ",
        "Pi" => "Π",
        "Sigma" => "Σ",
        "Upsilon" => "Υ",
        "Phi" => "Φ",
        "Psi" => "Ψ",
        "Omega" => "Ω",
        // Binary operators.
        "pm" => "±",
        "mp" => "∓",
        "times" => "×",
        "div" => "÷",
        "cdot" => "·",
        "ast" => "∗",
        "star" => "⋆",
        "circ" => "∘",
        "bullet" => "•",
        "oplus" => "⊕",
        "ominus" => "⊖",
        "otimes" => "⊗",
        "odot" => "⊙",
        // Relations.
        "le" | "leq" => "≤",
        "ge" | "geq" => "≥",
        "ne" | "neq" => "≠",
        "approx" => "≈",
        "equiv" => "≡",
        "sim" => "∼",
        "simeq" => "≃",
        "cong" => "≅",
        "propto" => "∝",
        "ll" => "≪",
        "gg" => "≫",
        "prec" => "≺",
        "succ" => "≻",
        "perp" => "⊥",
        "parallel" => "∥",
        "mid" => "∣",
        "vdash" => "⊢",
        "models" => "⊨",
        // Sets and logic.
        "in" => "∈",
        "notin" => "∉",
        "ni" => "∋",
        "subset" => "⊂",
        "supset" => "⊃",
        "subseteq" => "⊆",
        "supseteq" => "⊇",
        "cup" => "∪",
        "cap" => "∩",
        "setminus" => "∖",
        "emptyset" | "varnothing" => "∅",
        "forall" => "∀",
        "exists" => "∃",
        "nexists" => "∄",
        "neg" | "lnot" => "¬",
        "land" | "wedge" => "∧",
        "lor" | "vee" => "∨",
        "implies" => "⟹",
        "iff" => "⟺",
        // Arrows.
        "to" | "rightarrow" => "→",
        "leftarrow" | "gets" => "←",
        "Rightarrow" => "⇒",
        "Leftarrow" => "⇐",
        "leftrightarrow" => "↔",
        "Leftrightarrow" => "⇔",
        "mapsto" => "↦",
        "uparrow" => "↑",
        "downarrow" => "↓",
        "longrightarrow" => "⟶",
        "hookrightarrow" => "↪",
        // Big operators.
        "sum" => "∑",
        "prod" => "∏",
        "int" => "∫",
        "iint" => "∬",
        "iiint" => "∭",
        "oint" => "∮",
        "bigcup" => "⋃",
        "bigcap" => "⋂",
        // Delimiters.
        "langle" => "⟨",
        "rangle" => "⟩",
        "lceil" => "⌈",
        "rceil" => "⌉",
        "lfloor" => "⌊",
        "rfloor" => "⌋",
        // Miscellany.
        "infty" => "∞",
        "partial" => "∂",
        "nabla" => "∇",
        "angle" => "∠",
        "ell" => "ℓ",
        "hbar" => "ℏ",
        "Re" => "ℜ",
        "Im" => "ℑ",
        "aleph" => "ℵ",
        "wp" => "℘",
        "prime" => "′",
        "dots" | "ldots" => "…",
        "cdots" => "⋯",
        "vdots" => "⋮",
        "ddots" => "⋱",
        "therefore" => "∴",
        "because" => "∵",
        "degree" => "°",
        "top" => "⊤",
        "bot" => "⊥",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::prettify;

    #[test]
    fn symbols_scripts_and_fractions_linearize() {
        assert_eq!(prettify("E = mc^2"), "E = mc²");
        assert_eq!(
            prettify("\\alpha_i \\leq \\sum_{j=1}^{n} \\beta_j^2"),
            "αᵢ ≤ ∑ⱼ₌₁ⁿ βⱼ²"
        );
        assert_eq!(prettify("\\frac{a}{b} = c"), "a/b = c");
        assert_eq!(prettify("\\frac{x+1}{2}"), "(x+1)/2");
        assert_eq!(prettify("\\sqrt{x+1}"), "√(x+1)");
        assert_eq!(prettify("x \\in \\mathbb{R}^n"), "x ∈ ℝⁿ");
        assert_eq!(prettify("\\sin x + \\pi"), "sin x + π");
        assert_eq!(prettify("\\left( \\frac{a}{b} \\right)"), "( a/b )");
    }

    #[test]
    fn unknown_constructs_pass_through_verbatim() {
        assert_eq!(
            prettify("\\begin{matrix} a & b \\end{matrix}"),
            "\\begin{matrix} a & b \\end{matrix}"
        );
        // An unconvertible script keeps its marker and braces.
        assert_eq!(prettify("x^{\\alpha+1}"), "x^{α+1}");
        assert_eq!(prettify("a_{xy}"), "a_{xy}");
    }
}
