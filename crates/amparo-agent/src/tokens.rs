//! Token accounting and the cost line (M8 W1).
//!
//! The loop streams inference (`complete_chat_stream`) and no real token
//! count survives — exact billing belongs to the provider. The cost line
//! is therefore an *estimate* and says so: `chars / 4`, the standard
//! approximation, applied uniformly across turns and providers. The
//! method is stated on every rendered line, so the number is a claim
//! with its evidence attached, never a fiction.

/// Estimate the token count of `text` with the standard `chars / 4`
/// approximation: empty input is zero tokens, and unicode counts by
/// character (most tokenizers charge multibyte input at roughly a
/// quarter of its character count).
pub fn estimate_tokens(text: &str) -> usize {
    text.chars().count().div_ceil(4)
}

/// Render the per-run cost line — `None` when `rate` is `None` (the
/// operator turned the line off; counts still appear in the report).
/// The line states its method: the `~`, the `chars/4` basis, and the
/// rate. Cost = `tokens × rate / 1_000_000`, dollars, two decimals.
pub fn format_cost_line(tokens_estimated: usize, rate: Option<f64>) -> Option<String> {
    let rate = rate?;
    let dollars = tokens_estimated as f64 * rate / 1_000_000.0;
    Some(format!(
        "~${dollars:.2} in inference (estimate, chars/4, ${rate}/1M tokens)"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimate_tokens_empty_is_zero() {
        assert_eq!(estimate_tokens(""), 0);
    }

    #[test]
    fn estimate_tokens_counts_chars_not_bytes() {
        // Four ASCII chars → one estimated token.
        assert_eq!(estimate_tokens("abcd"), 1);
        // Multibyte input counts by character: "é" is 2 bytes but 1 char,
        // and four chars still estimate one token.
        assert_eq!(estimate_tokens("éééé"), 1);
        // A mixed string: 10 chars → ceil(10/4) = 3.
        assert_eq!(estimate_tokens("abcdef日本de"), 3);
        // Rounding goes up, never down.
        assert_eq!(estimate_tokens("a"), 1);
    }

    #[test]
    fn cost_line_formats_with_method_stated() {
        // 13,500 estimated tokens at $3/1M → $0.0405 → "~$0.04".
        let line = format_cost_line(13_500, Some(3.0)).expect("rate renders a line");
        assert_eq!(
            line,
            "~$0.04 in inference (estimate, chars/4, $3/1M tokens)"
        );
        // The rate prints without trailing zeros; a fractional rate too.
        let line = format_cost_line(260_000, Some(2.5)).expect("rate renders a line");
        assert_eq!(
            line,
            "~$0.65 in inference (estimate, chars/4, $2.5/1M tokens)"
        );
    }

    #[test]
    fn cost_line_honors_none_rate() {
        assert_eq!(format_cost_line(13_500, None), None);
    }
}
