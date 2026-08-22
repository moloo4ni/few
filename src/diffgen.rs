use similar::{ChangeTag, TextDiff};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine {
    pub sign: char,
    pub text: String,
}

pub fn line_diff(old: &str, new: &str) -> Vec<DiffLine> {
    let diff = TextDiff::from_lines(old, new);
    let mut out = Vec::new();
    for change in diff.iter_all_changes() {
        match change.tag() {
            ChangeTag::Delete => out.push(DiffLine {
                sign: '-',
                text: change.value().trim_end_matches('\n').to_owned(),
            }),
            ChangeTag::Insert => out.push(DiffLine {
                sign: '+',
                text: change.value().trim_end_matches('\n').to_owned(),
            }),
            ChangeTag::Equal => {}
        }
    }
    out
}

pub fn stats(lines: &[DiffLine]) -> (usize, usize) {
    let added = lines.iter().filter(|l| l.sign == '+').count();
    let removed = lines.iter().filter(|l| l.sign == '-').count();
    (added, removed)
}

pub fn looks_binary(bytes: &[u8]) -> bool {
    bytes[..bytes.len().min(8192)].contains(&0)
}

pub fn human_size(n: u64) -> String {
    if n < 1024 {
        format!("{n}b")
    } else if n < 1024 * 1024 {
        format!("{}kb", (n + 512) / 1024)
    } else {
        format!("{}mb", (n + 512 * 1024) / (1024 * 1024))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creation_all_plus() {
        let d = line_diff("", "a\nb\n");
        assert_eq!(d.len(), 2);
        assert!(d.iter().all(|l| l.sign == '+'));
        let (a, r) = stats(&d);
        assert_eq!((a, r), (2, 0));
    }

    #[test]
    fn deletion_all_minus() {
        let d = line_diff("x\ny\n", "");
        assert!(d.iter().all(|l| l.sign == '-'));
    }

    #[test]
    fn mixed_edit() {
        let d = line_diff("fn a() {}\n", "fn b() {}\n");
        assert_eq!(stats(&d), (1, 1));
        assert_eq!(d[0].sign, '-');
        assert_eq!(d[0].text, "fn a() {}");
        assert_eq!(d[1].sign, '+');
    }

    #[test]
    fn binary_detection() {
        assert!(looks_binary(&[104, 101, 0, 108]));
        assert!(!looks_binary(b"plain text file"));
    }

    #[test]
    fn sizes() {
        assert_eq!(human_size(24), "24b");
        assert_eq!(human_size(24576), "24kb");
    }
}
