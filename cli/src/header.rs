//! 洛雪脚本头注释。只看前 4000 个字符，规则与觅音 `parseScriptHeader` 一致。

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptHeader {
    pub name: String,
    pub description: String,
    pub version: String,
    pub author: String,
    pub homepage: String,
}

pub fn parse_script_header(code: &str) -> ScriptHeader {
    let head: String = code.chars().take(4000).collect();
    ScriptHeader {
        name: tag(&head, "name"),
        description: tag(&head, "description"),
        version: tag(&head, "version"),
        author: tag(&head, "author"),
        homepage: tag(&head, "homepage"),
    }
}

/// 对齐觅音 `parseScriptHeader`：`@key` 后必须是空白，大小写不敏感。
/// `@namespace` 不会被当成 `@name`。值停在换行或 `*`。
fn tag(head: &str, key: &str) -> String {
    let lower = head.to_ascii_lowercase();
    let needle = format!("@{key}").to_ascii_lowercase();
    let mut from = 0;
    while let Some(rel) = lower[from..].find(&needle) {
        let start = from + rel;
        let after = start + needle.len();
        let Some(next) = lower[after..].chars().next() else {
            break;
        };
        if next.is_whitespace() {
            let value: String = head[after..]
                .trim_start()
                .chars()
                .take_while(|c| *c != '\n' && *c != '\r' && *c != '*')
                .collect();
            return value.trim().to_string();
        }
        from = after;
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_lx_header_tags() {
        let code = r#"
/**
 * @name 测试源
 * @version 1.2.3
 * @author someone
 * @description 用于单测
 * @homepage https://example.com
 */
const x = 1;
"#;
        let h = parse_script_header(code);
        assert_eq!(h.name, "测试源");
        assert_eq!(h.version, "1.2.3");
        assert_eq!(h.author, "someone");
        assert_eq!(h.description, "用于单测");
        assert_eq!(h.homepage, "https://example.com");
    }

    #[test]
    fn tag_match_is_case_insensitive_and_ignores_later_body() {
        let mut code = String::from("// @Name 头部名字\n// @VERSION 9\n");
        code.push_str(&"x".repeat(5000));
        code.push_str("\n// @name 不该读到\n");
        let h = parse_script_header(&code);
        assert_eq!(h.name, "头部名字");
        assert_eq!(h.version, "9");
    }

    #[test]
    fn does_not_treat_namespace_as_name() {
        let h = parse_script_header("// @namespace nope\n// @name real\n");
        assert_eq!(h.name, "real");
    }
}
