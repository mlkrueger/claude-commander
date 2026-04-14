//! Input sanitization for MCP write tools.
//!
//! [`sanitize_prompt_text`] is the single entry point used by the
//! `send_prompt` MCP tool to normalize driver-supplied text before it
//! reaches a target PTY. The policy (see `docs/pr-review-pr8.md` and
//! `docs/plans/phase-5-mcp-write.md` §Task 1) is:
//!
//! 1. Reject inputs larger than [`MAX_PROMPT_BYTES`] (16 KB) up front.
//! 2. Strip ANSI CSI (`ESC[…<final>`) and OSC (`ESC]…BEL` / `ESC]…ESC\`)
//!    sequences using the battle-tested [`crate::pty::response_boundary::ansi_strip`].
//! 3. Strip control chars `< 0x20` except `\n` and `\t`.
//! 4. Normalize `\r` and `\r\n` to `\n`.
//! 5. Reject inputs that are empty after the transformation.
//!
//! The function returns `Ok(clean)` on success or `Err(reason)` where
//! `reason` is a human-readable string suitable for surfacing in an
//! MCP `CallToolResult::error`.

/// Maximum accepted size of a sanitized prompt, in bytes. The check is
/// applied to the raw input (pre-strip) so a pathological 1 MB blob of
/// pure ANSI escapes is still rejected cheaply.
pub(crate) const MAX_PROMPT_BYTES: usize = 16 * 1024;

/// Maximum accepted length of a sanitized session label, in chars.
/// Labels appear in the TUI session list, modal prompts, and logs —
/// 64 is comfortably wider than the default session-list column and
/// short enough that a malicious driver can't spam a multi-kilobyte
/// label through the modal.
pub(crate) const MAX_LABEL_CHARS: usize = 64;

/// Phase 6 Task 3 — sanitize a driver-supplied session label.
///
/// Policy:
/// 1. Strip ANSI CSI/OSC escape sequences.
/// 2. Drop ASCII control chars (`< 0x20` and `0x7f`).
/// 3. Whitelist: ASCII alphanumeric, space, and `-_./:`.
/// 4. Truncate to [`MAX_LABEL_CHARS`] chars.
/// 5. Reject empty post-sanitization.
///
/// Returns `Ok(clean)` on success or `Err(reason)` suitable for a
/// tool-level error payload.
pub(crate) fn sanitize_label(input: &str) -> Result<String, String> {
    // Strip ANSI first so any escape-sequence interior bytes are
    // then filtered by the control/whitelist pass below. Reusing
    // the pty helper keeps escape parsing in one place.
    let stripped = crate::pty::response_boundary::ansi_strip(input);

    let mut out = String::with_capacity(stripped.len().min(MAX_LABEL_CHARS));
    // Track kept-char count explicitly instead of calling
    // `out.chars().count()` on every loop iteration — the old shape
    // was O(n²) over the input. Not a practical perf concern at the
    // 64-char limit but misleading as an idiom (pr-review
    // -phase-6-tasks-3-to-7.md §D1).
    let mut char_count: usize = 0;
    for ch in stripped.chars() {
        if char_count >= MAX_LABEL_CHARS {
            break;
        }
        // Drop ASCII control (C0 and DEL).
        if (ch as u32) < 0x20 || ch as u32 == 0x7f {
            continue;
        }
        let allowed =
            ch.is_ascii_alphanumeric() || ch == ' ' || matches!(ch, '-' | '_' | '.' | '/' | ':');
        if allowed {
            out.push(ch);
            char_count += 1;
        }
        // Non-whitelisted chars (emoji, non-ASCII letters, punctuation
        // outside the allowed set) are silently dropped rather than
        // rejecting the whole label — matches the prompt sanitizer's
        // "strip and hope" stance.
    }

    let trimmed = out.trim();
    if trimmed.is_empty() {
        return Err("label is empty after sanitization".to_string());
    }
    Ok(trimmed.to_string())
}

/// Apply the phase-5 sanitization policy to driver-supplied text.
///
/// See the module docs for the policy specification.
pub(crate) fn sanitize_prompt_text(input: &str) -> Result<String, String> {
    if input.len() > MAX_PROMPT_BYTES {
        return Err(format!(
            "text too large: {} bytes (max {})",
            input.len(),
            MAX_PROMPT_BYTES
        ));
    }

    // Strip ANSI CSI/OSC first — reuses the tested helper in the pty
    // module so escape parsing stays in one place.
    let stripped = crate::pty::response_boundary::ansi_strip(input);

    // Walk the stripped text char-by-char, normalizing line endings
    // and dropping disallowed controls. We materialize `\r\n` → `\n`
    // by skipping the `\n` that follows a `\r`.
    let mut out = String::with_capacity(stripped.len());
    let mut prev_was_cr = false;
    for ch in stripped.chars() {
        match ch {
            '\r' => {
                out.push('\n');
                prev_was_cr = true;
                continue;
            }
            '\n' => {
                if !prev_was_cr {
                    out.push('\n');
                }
            }
            '\t' => out.push('\t'),
            c if c.is_control() => {}
            c => out.push(c),
        }
        prev_was_cr = false;
    }

    if out.trim().is_empty() {
        return Err("text is empty after sanitization".to_string());
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_plain_text() {
        assert_eq!(sanitize_prompt_text("hello world").unwrap(), "hello world");
    }

    #[test]
    fn preserves_newline_and_tab() {
        assert_eq!(sanitize_prompt_text("a\tb\nc").unwrap(), "a\tb\nc",);
    }

    #[test]
    fn normalizes_cr_to_lf() {
        assert_eq!(sanitize_prompt_text("a\rb").unwrap(), "a\nb");
    }

    #[test]
    fn normalizes_crlf_to_lf() {
        // `\r\n` should collapse to a single `\n`, not `\n\n`.
        assert_eq!(sanitize_prompt_text("a\r\nb").unwrap(), "a\nb");
    }

    #[test]
    fn strips_bell_and_other_c0_controls() {
        // BEL (0x07), SOH (0x01), STX (0x02) all dropped; surrounding
        // printable content preserved.
        assert_eq!(
            sanitize_prompt_text("hi\x07\x01\x02there").unwrap(),
            "hithere",
        );
    }

    #[test]
    fn strips_csi_color_escape() {
        assert_eq!(
            sanitize_prompt_text("hello\x1b[31mred\x1b[0m").unwrap(),
            "hellored",
        );
    }

    #[test]
    fn strips_osc_title_escape() {
        // OSC terminated by BEL.
        assert_eq!(
            sanitize_prompt_text("pre\x1b]0;window title\x07post").unwrap(),
            "prepost",
        );
        // OSC terminated by ST (ESC \).
        assert_eq!(
            sanitize_prompt_text("pre\x1b]0;window title\x1b\\post").unwrap(),
            "prepost",
        );
    }

    #[test]
    fn rejects_empty_input() {
        assert!(sanitize_prompt_text("").is_err());
    }

    #[test]
    fn rejects_input_of_only_controls() {
        // After stripping \x01 \x02 \x03 the buffer is empty.
        let err = sanitize_prompt_text("\x01\x02\x03").unwrap_err();
        assert!(err.contains("empty"), "err = {err}");
    }

    #[test]
    fn rejects_over_16kb() {
        let big = "a".repeat(MAX_PROMPT_BYTES + 1);
        let err = sanitize_prompt_text(&big).unwrap_err();
        assert!(err.contains("too large"), "err = {err}");
    }

    #[test]
    fn accepts_exactly_16kb() {
        let exact = "a".repeat(MAX_PROMPT_BYTES);
        let clean = sanitize_prompt_text(&exact).unwrap();
        assert_eq!(clean.len(), MAX_PROMPT_BYTES);
    }

    // ------------------------------------------------------------------
    // Phase 6 Task 3 — `sanitize_label` policy pins.
    // ------------------------------------------------------------------

    #[test]
    fn label_accepts_plain_ascii() {
        assert_eq!(sanitize_label("worker-01").unwrap(), "worker-01");
    }

    #[test]
    fn label_accepts_whitelist_punctuation() {
        assert_eq!(sanitize_label("a-b_c.d/e:f 1").unwrap(), "a-b_c.d/e:f 1");
    }

    #[test]
    fn label_strips_ansi_escape() {
        assert_eq!(sanitize_label("\x1b[31mhot\x1b[0m").unwrap(), "hot");
    }

    #[test]
    fn label_strips_control_chars() {
        assert_eq!(sanitize_label("a\x01b\x07c").unwrap(), "abc");
    }

    #[test]
    fn label_drops_non_whitelisted_chars() {
        // Emoji and `!` are dropped; surrounding ASCII survives.
        assert_eq!(sanitize_label("hi!\u{1f600}there").unwrap(), "hithere");
    }

    #[test]
    fn label_truncates_to_max_chars() {
        let big = "a".repeat(MAX_LABEL_CHARS + 10);
        let clean = sanitize_label(&big).unwrap();
        assert_eq!(clean.chars().count(), MAX_LABEL_CHARS);
    }

    #[test]
    fn label_rejects_empty() {
        assert!(sanitize_label("").is_err());
    }

    #[test]
    fn label_rejects_all_stripped() {
        // After dropping emoji + ANSI there's nothing left.
        assert!(sanitize_label("\x1b[31m\u{1f600}\x1b[0m").is_err());
    }
}
