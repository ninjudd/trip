pub fn inserted_lines(old: &str, new: &str) -> Vec<String> {
    use std::collections::HashMap;

    let old_lines: Vec<&str> = old.lines().map(|l| l.trim()).collect();
    let new_lines: Vec<&str> = new.lines().map(|l| l.trim()).collect();

    let m = old_lines.len();
    let n = new_lines.len();
    let mut dp = vec![vec![0u32; n + 1]; m + 1];
    for i in 1..=m {
        for j in 1..=n {
            if old_lines[i - 1] == new_lines[j - 1] {
                dp[i][j] = dp[i - 1][j - 1] + 1;
            } else {
                dp[i][j] = dp[i - 1][j].max(dp[i][j - 1]);
            }
        }
    }

    let mut inserted = Vec::new();
    let mut deleted = Vec::new();
    let mut i = m;
    let mut j = n;
    while i > 0 && j > 0 {
        if old_lines[i - 1] == new_lines[j - 1] {
            i -= 1;
            j -= 1;
        } else if dp[i - 1][j] >= dp[i][j - 1] {
            deleted.push(old_lines[i - 1].to_string());
            i -= 1;
        } else {
            inserted.push(new_lines[j - 1].to_string());
            j -= 1;
        }
    }
    while j > 0 {
        inserted.push(new_lines[j - 1].to_string());
        j -= 1;
    }

    inserted.reverse();
    deleted.reverse();

    // Post-process: remove insertions that exactly match a deletion (modifications)
    let mut del_counts: HashMap<&str, usize> = HashMap::new();
    for d in &deleted {
        *del_counts.entry(d.as_str()).or_default() += 1;
    }
    inserted
        .into_iter()
        .filter(|line| {
            if let Some(count) = del_counts.get_mut(line.as_str()) {
                if *count > 0 {
                    *count -= 1;
                    return false;
                }
            }
            true
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_insertion() {
        let old = "line1\nline2";
        let new = "line1\nline2\nline3";
        assert_eq!(inserted_lines(old, new), vec!["line3"]);
    }

    #[test]
    fn modification_shows_new_value() {
        // Changed lines show up as insertions (the old value is filtered
        // as a deletion, but the new value is different so it stays)
        let old = "header\nstatus: 50%\nfooter";
        let new = "header\nstatus: 75%\nfooter";
        assert_eq!(inserted_lines(old, new), vec!["status: 75%"]);
    }

    #[test]
    fn exact_duplicate_suppressed() {
        // Line that exists in both old and new at different positions
        // should not appear as an insertion
        let old = "top\nshared line\nbottom";
        let new = "shared line\nbottom\nnew stuff";
        assert_eq!(inserted_lines(old, new), vec!["new stuff"]);
    }

    #[test]
    fn trailing_whitespace_ignored() {
        let old = "hello  \nworld";
        let new = "hello\nworld\nnew line";
        assert_eq!(inserted_lines(old, new), vec!["new line"]);
    }

    #[test]
    fn whitespace_only_lines_match() {
        let old = "header\n   \nfooter";
        let new = "header\n\nfooter\nnew";
        assert_eq!(inserted_lines(old, new), vec!["new"]);
    }

    #[test]
    fn scrolling_does_not_duplicate() {
        let old = "line1\nline2\nline3\nCurrent state\nstatus bar";
        let new = "line3\nCurrent state\nThe working tree has changes.\nstatus bar";
        assert_eq!(inserted_lines(old, new), vec!["The working tree has changes."]);
    }

    #[test]
    fn scrolling_with_whitespace_variance() {
        let old = "line1\nline2\n \nCurrent state\nstatus";
        let new = "line2\n\nCurrent state\nnew content\nstatus";
        assert_eq!(inserted_lines(old, new), vec!["new content"]);
    }

    #[test]
    fn empty_old() {
        assert_eq!(inserted_lines("", "line1\nline2"), vec!["line1", "line2"]);
    }

    #[test]
    fn identical_screens() {
        assert_eq!(inserted_lines("a\nb\nc", "a\nb\nc"), Vec::<String>::new());
    }

    #[test]
    fn real_screen_scrolling_no_duplicate_heading() {
        // Reduced version of real Claude Code screens 19→20
        // "Current state" exists in both, content scrolls, new lines appear below it
        let old = "\
item 5
item 6
item 7
item 8
item 9

---

Current state

status bar old
mode indicator";

        let new = "\
item 7
item 8
item 9

---
Current state

The working tree has uncommitted changes.

status bar new
mode indicator new";

        let result = inserted_lines(old, new);
        assert!(
            !result.iter().any(|l| l.contains("Current state")),
            "Current state should not appear as insertion, got: {:?}",
            result
        );
        assert!(result.iter().any(|l| l.contains("uncommitted")));
    }
}
