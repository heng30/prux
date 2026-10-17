//! 环境变量展开：把配置里写的 `${VAR}` 换成运行时取值。

/// 展开字符串里的 `${VAR}`：变量未定义时替换为空串，未闭合的 `${` 连同其后内容原样保留。
///
/// 给「不经过 shell 的取值」用——`std::process::Command` 的命令与参数、配置里的路径、URL、
/// 请求头都不会被解释器展开，变量只能自己替。
pub fn expand(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut rest = value;

    while let Some(start) = rest.find("${") {
        output.push_str(&rest[..start]);
        let tail = &rest[start + 2..];

        let Some(end) = tail.find('}') else {
            output.push_str(&rest[start..]);
            return output;
        };

        output.push_str(&std::env::var(&tail[..end]).unwrap_or_default());
        rest = &tail[end + 1..];
    }

    output.push_str(rest);
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 已定义的变量被替换，未定义的替换为空串，未闭合的 `${` 原样保留；同一串里出现多次也全替换。
    #[test]
    fn expands_defined_variables_and_keeps_the_rest() {
        unsafe { std::env::set_var("PRUX_UTILS_ENV_TEST_VALUE", "value") };
        assert_eq!(expand("a-${PRUX_UTILS_ENV_TEST_VALUE}-b"), "a-value-b");
        assert_eq!(
            expand("${PRUX_UTILS_ENV_TEST_VALUE}${PRUX_UTILS_ENV_TEST_VALUE}"),
            "valuevalue"
        );
        assert_eq!(expand("x${PRUX_UTILS_ENV_TEST_MISSING}y"), "xy");
        assert_eq!(expand("tail ${unclosed"), "tail ${unclosed");
        assert_eq!(expand("no variables here"), "no variables here");
        assert_eq!(expand(""), "");
    }
}
