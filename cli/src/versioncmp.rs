use std::cmp::Ordering;

/// 比较 `@version`。按数字段比较，`1.10` 高于 `1.9`。没有数字时退回字符串比较。
pub fn cmp_version(a: &str, b: &str) -> Ordering {
    let pa = numeric_parts(a);
    let pb = numeric_parts(b);
    if pa.is_empty() && pb.is_empty() {
        return a.cmp(b);
    }
    let n = pa.len().max(pb.len());
    for i in 0..n {
        let da = pa.get(i).copied().unwrap_or(0);
        let db = pb.get(i).copied().unwrap_or(0);
        if da != db {
            return da.cmp(&db);
        }
    }
    a.cmp(b)
}

fn numeric_parts(s: &str) -> Vec<u64> {
    let mut out = Vec::new();
    let mut cur: Option<u64> = None;
    for c in s.chars() {
        if c.is_ascii_digit() {
            let d = u64::from(c.to_digit(10).unwrap());
            cur = Some(cur.unwrap_or(0).saturating_mul(10).saturating_add(d));
        } else if let Some(n) = cur.take() {
            out.push(n);
        }
    }
    if let Some(n) = cur {
        out.push(n);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numeric_version_order() {
        assert_eq!(cmp_version("1.10.0", "1.9.0"), Ordering::Greater);
        assert_eq!(cmp_version("v2", "v1.9"), Ordering::Greater);
        assert_eq!(cmp_version("1.2.3", "1.2.3"), Ordering::Equal);
        assert_eq!(cmp_version("0.1", "1.0"), Ordering::Less);
    }
}
