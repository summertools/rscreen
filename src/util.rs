//! Small shared helpers (display width handling, fuzzy matching, session names).

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Truncate a string to `max` display columns, appending an ellipsis when it
/// does not fit. Wide characters are never split.
pub fn truncate_display(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if s.width() <= max {
        return s.to_string();
    }
    if max == 1 {
        return "…".to_string();
    }
    let mut out = String::new();
    let mut width = 0;
    for c in s.chars() {
        let w = c.width().unwrap_or(0);
        if width + w > max - 1 {
            break;
        }
        out.push(c);
        width += w;
    }
    out.push('…');
    out
}

/// Pad a string with spaces to exactly `width` display columns (truncating if
/// necessary).
pub fn pad_display(s: &str, width: usize) -> String {
    let mut out = truncate_display(s, width);
    let current = out.width();
    if current < width {
        out.push_str(&" ".repeat(width - current));
    }
    out
}

/// Center a string within `width` display columns.
pub fn center_display(s: &str, width: usize) -> String {
    let s = truncate_display(s, width);
    let w = s.width();
    if w >= width {
        return s;
    }
    let left = (width - w) / 2;
    let right = width - w - left;
    format!("{}{}{}", " ".repeat(left), s, " ".repeat(right))
}

/// Very small fuzzy matcher for the command palette. Returns a score (higher is
/// better) if all query characters appear in order.
pub fn fuzzy_score(query: &str, candidate: &str) -> Option<i32> {
    if query.is_empty() {
        return Some(0);
    }
    let q: Vec<char> = query.chars().flat_map(|c| c.to_lowercase()).collect();
    let c: Vec<char> = candidate.chars().flat_map(|c| c.to_lowercase()).collect();
    let mut qi = 0;
    let mut score = 0i32;
    let mut last_match: Option<usize> = None;
    for (ci, ch) in c.iter().enumerate() {
        if qi < q.len() && *ch == q[qi] {
            score += 10;
            if let Some(last) = last_match {
                if ci == last + 1 {
                    score += 15; // consecutive matches are valuable
                } else {
                    score -= (ci - last - 1).min(8) as i32;
                }
            }
            if ci == 0 || !c[ci - 1].is_alphanumeric() {
                score += 20; // word-start match
            }
            last_match = Some(ci);
            qi += 1;
        }
    }
    if qi == q.len() {
        // Prefer shorter candidates.
        score -= (c.len() as i32) / 4;
        Some(score)
    } else {
        None
    }
}

/// Validate a session name. Names become socket file names, so keep them
/// portable and short.
pub fn validate_session_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("session name must not be empty".into());
    }
    if name.len() > 40 {
        return Err("session name must be at most 40 bytes".into());
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err("session name may contain only letters, digits, '-' and '_'".into());
    }
    Ok(())
}

/// Friendly session names, used when the user does not pass `-S`.
pub fn random_session_name() -> String {
    const ADJECTIVES: &[&str] = &[
        "amber", "azure", "brisk", "calm", "cobalt", "coral", "crisp", "dusk", "ember", "faint",
        "fern", "frost", "gentle", "golden", "hazel", "indigo", "ivory", "jade", "lunar", "maple",
        "misty", "moss", "navy", "nimble", "opal", "pearl", "quiet", "rapid", "rosy", "rustic",
        "sage", "silver", "solar", "swift", "tidal", "umber", "velvet", "vivid", "willow", "zesty",
    ];
    const NOUNS: &[&str] = &[
        "badger", "beacon", "canyon", "cedar", "comet", "cove", "dawn", "delta", "dune", "eagle",
        "falcon", "field", "forest", "fox", "glade", "harbor", "heron", "island", "lake", "lark",
        "mesa", "moon", "otter", "peak", "pine", "rain", "reef", "ridge", "river", "robin",
        "shore", "sky", "sparrow", "star", "stone", "storm", "sun", "tide", "vale", "wave",
    ];
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as usize)
        .unwrap_or(0);
    let pid = std::process::id() as usize;
    let a = ADJECTIVES[(nanos ^ pid) % ADJECTIVES.len()];
    let n = NOUNS[(nanos / 7 + pid) % NOUNS.len()];
    format!("{}-{}", a, n)
}

/// Human-friendly "time ago" formatting.
pub fn time_ago(seconds: u64) -> String {
    if seconds < 60 {
        "just now".to_string()
    } else if seconds < 3600 {
        format!("{}m ago", seconds / 60)
    } else if seconds < 86_400 {
        format!("{}h ago", seconds / 3600)
    } else {
        format!("{}d ago", seconds / 86_400)
    }
}

/// Visual width of a string.
pub fn width(s: &str) -> usize {
    s.width()
}

/// Remove terminal control characters from text that will be printed outside
/// the frame renderer (errors, warnings, list output). Newlines and tabs stay
/// readable; ESC and C1 controls cannot inject terminal escapes.
pub fn terminal_safe(s: &str) -> String {
    s.chars()
        .filter(|c| *c == '\n' || *c == '\t' || !c.is_control())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncates_with_ellipsis() {
        assert_eq!(truncate_display("hello world", 5), "hell…");
        assert_eq!(truncate_display("hi", 5), "hi");
        assert_eq!(truncate_display("中文测试", 5), "中文…");
    }

    #[test]
    fn pads_to_width() {
        assert_eq!(pad_display("ab", 4), "ab  ");
        assert_eq!(pad_display("abcde", 3), "ab…");
    }

    #[test]
    fn fuzzy() {
        assert!(fuzzy_score("nw", "New window").is_some());
        assert!(fuzzy_score("xyz", "New window").is_none());
        assert!(fuzzy_score("", "anything").is_some());
    }

    #[test]
    fn terminal_safe_strips_escapes() {
        let s = terminal_safe("bad \x1b]2;title\x07 path\nnext");
        assert!(!s.contains('\x1b'));
        assert!(!s.contains('\x07'));
        assert!(s.contains("next"));
    }

    #[test]
    fn names() {
        assert!(validate_session_name("my-session_2").is_ok());
        assert!(validate_session_name("bad name").is_err());
        assert!(validate_session_name("").is_err());
        let n = random_session_name();
        assert!(validate_session_name(&n).is_ok());
    }
}
