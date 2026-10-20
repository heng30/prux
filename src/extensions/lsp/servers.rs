//! LSP 服务器发现：文件类型 → 语言 → 服务器命令 → `PATH` 探测。
//!
//! 表里的服务器都是「装了就按命令名找得到」的常见实现，参数按各家要求给死
//! （多数要求显式 `--stdio`）。表里没有的语言直接返回 `None`，由调用方报「不支持」。

use std::path::{Path, PathBuf};

/// 一个语言服务器的启动描述。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerSpec {
    /// 可执行文件名（同时用于 `PATH` 探测与错误提示）。
    pub command: &'static str,
    /// 启动参数。
    pub args: &'static [&'static str],
    /// 传给 `didOpen` 的 LSP `languageId`。
    pub language_id: &'static str,
    /// 向上查找工作区根时认的标记文件（任一存在即可）。
    pub root_markers: &'static [&'static str],
    /// 找不到命令时展示给用户的安装提示。
    pub install_hint: &'static str,
}

/// 内置服务器表（按 `language_id` 匹配）。
pub const SERVERS: &[ServerSpec] = &[
    ServerSpec {
        command: "rust-analyzer",
        args: &[],
        language_id: "rust",
        root_markers: &["Cargo.toml"],
        install_hint: "rustup component add rust-analyzer",
    },
    ServerSpec {
        command: "clangd",
        args: &[],
        language_id: "cpp",
        root_markers: &[
            "compile_commands.json",
            "compile_flags.txt",
            "CMakeLists.txt",
            "Cargo.toml",
        ],
        install_hint: "install clangd from your package manager or llvm.org",
    },
    ServerSpec {
        command: "clangd",
        args: &[],
        language_id: "c",
        root_markers: &[
            "compile_commands.json",
            "compile_flags.txt",
            "CMakeLists.txt",
            "Makefile",
        ],
        install_hint: "install clangd from your package manager or llvm.org",
    },
    ServerSpec {
        command: "typescript-language-server",
        args: &["--stdio"],
        language_id: "typescript",
        root_markers: &["tsconfig.json", "package.json", "jsconfig.json"],
        install_hint: "npm install -g typescript-language-server typescript",
    },
    ServerSpec {
        command: "typescript-language-server",
        args: &["--stdio"],
        language_id: "typescriptreact",
        root_markers: &["tsconfig.json", "package.json", "jsconfig.json"],
        install_hint: "npm install -g typescript-language-server typescript",
    },
    ServerSpec {
        command: "typescript-language-server",
        args: &["--stdio"],
        language_id: "javascript",
        root_markers: &["jsconfig.json", "package.json", "tsconfig.json"],
        install_hint: "npm install -g typescript-language-server typescript",
    },
    ServerSpec {
        command: "typescript-language-server",
        args: &["--stdio"],
        language_id: "javascriptreact",
        root_markers: &["jsconfig.json", "package.json", "tsconfig.json"],
        install_hint: "npm install -g typescript-language-server typescript",
    },
    ServerSpec {
        command: "pyright-langserver",
        args: &["--stdio"],
        language_id: "python",
        root_markers: &[
            "pyproject.toml",
            "setup.py",
            "setup.cfg",
            "requirements.txt",
        ],
        install_hint: "npm install -g pyright",
    },
    ServerSpec {
        command: "gopls",
        args: &[],
        language_id: "go",
        root_markers: &["go.mod", "go.work"],
        install_hint: "go install golang.org/x/tools/gopls@latest",
    },
    ServerSpec {
        command: "jdtls",
        args: &[],
        language_id: "java",
        root_markers: &[
            "pom.xml",
            "build.gradle",
            "build.gradle.kts",
            "settings.gradle",
        ],
        install_hint: "install eclipse.jdt.ls from your package manager",
    },
    ServerSpec {
        command: "ruby-lsp",
        args: &[],
        language_id: "ruby",
        root_markers: &["Gemfile", ".ruby-version"],
        install_hint: "gem install ruby-lsp",
    },
    ServerSpec {
        command: "lua-language-server",
        args: &[],
        language_id: "lua",
        root_markers: &[".luarc.json", ".git"],
        install_hint: "install lua-language-server from github.com/LuaLS/lua-language-server",
    },
    ServerSpec {
        command: "csharp-ls",
        args: &[],
        language_id: "csharp",
        root_markers: &["*.csproj", "*.sln"],
        install_hint: "dotnet tool install --global csharp-ls",
    },
    ServerSpec {
        command: "intelephense",
        args: &["--stdio"],
        language_id: "php",
        root_markers: &["composer.json"],
        install_hint: "npm install -g intelephense",
    },
];

/// 按文件扩展名推断 LSP `languageId`；不在表里的扩展名返回 `None`。
pub fn language_id_for(path: &Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "rs" => "rust",
        "c" | "h" => "c",
        "cc" | "cpp" | "cxx" | "hpp" | "hh" | "hxx" => "cpp",
        "ts" | "mts" | "cts" => "typescript",
        "tsx" => "typescriptreact",
        "js" | "mjs" | "cjs" => "javascript",
        "jsx" => "javascriptreact",
        "py" | "pyi" => "python",
        "go" => "go",
        "java" => "java",
        "rb" => "ruby",
        "lua" => "lua",
        "cs" => "csharp",
        "php" => "php",
        _ => return None,
    })
}

/// 按 `languageId` 取服务器描述；表里没有时返回 `None`。
pub fn server_for(language_id: &str) -> Option<&'static ServerSpec> {
    SERVERS.iter().find(|s| s.language_id == language_id)
}

/// 目录里是否存在该服务器的任一标记文件。
///
/// 标记项里含 `*` 的按扩展名匹配（如 `*.csproj`），其余按文件名精确匹配。
pub fn has_root_marker(dir: &Path, spec: &ServerSpec) -> bool {
    spec.root_markers.iter().any(|marker| {
        if let Some(suffix) = marker.strip_prefix("*.") {
            std::fs::read_dir(dir)
                .map(|entries| {
                    entries.flatten().any(|entry| {
                        entry
                            .path()
                            .extension()
                            .and_then(|e| e.to_str())
                            .is_some_and(|e| e.eq_ignore_ascii_case(suffix))
                    })
                })
                .unwrap_or(false)
        } else {
            dir.join(marker).exists()
        }
    })
}

/// 从文件所在目录向上找工作区根：第一个含标记文件的祖先目录；
/// 都找不到时退回文件所在目录。
pub fn workspace_root(file: &Path, spec: &ServerSpec) -> PathBuf {
    let start = file.parent().unwrap_or(Path::new("."));
    for dir in start.ancestors() {
        if has_root_marker(dir, spec) {
            return dir.to_path_buf();
        }
    }
    start.to_path_buf()
}

/// 从 `cwd` 开始按服务器表顺序找能识别的项目根；找不到返回 `None`。
pub fn detect_workspace(cwd: &Path) -> Option<(&'static ServerSpec, PathBuf)> {
    for spec in SERVERS {
        for dir in cwd.ancestors() {
            if has_root_marker(dir, spec) {
                return Some((spec, dir.to_path_buf()));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::AgentDirGuard;

    #[test]
    fn language_ids_cover_common_extensions() {
        assert_eq!(language_id_for(Path::new("a.rs")), Some("rust"));
        assert_eq!(language_id_for(Path::new("a.tsx")), Some("typescriptreact"));
        assert_eq!(language_id_for(Path::new("a.hpp")), Some("cpp"));
        assert_eq!(language_id_for(Path::new("a.py")), Some("python"));
        assert_eq!(language_id_for(Path::new("a.unknown")), None);
    }

    #[test]
    fn every_language_has_a_server() {
        for ext in [
            "rs", "c", "cpp", "ts", "tsx", "js", "jsx", "py", "go", "java", "rb", "lua", "cs",
            "php",
        ] {
            let lang = language_id_for(Path::new(&format!("a.{ext}"))).expect(ext);
            assert!(server_for(lang).is_some(), "no server for {lang}");
        }
    }

    #[test]
    fn workspace_root_walks_up_to_marker() {
        let _guard = AgentDirGuard::temp();
        let root = std::env::temp_dir().join(format!("prux-lsp-root-{}", std::process::id()));
        let nested = root.join("src").join("deep");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(root.join("Cargo.toml"), "[package]").unwrap();

        let spec = server_for("rust").unwrap();
        assert_eq!(
            workspace_root(&nested.join("main.rs"), spec),
            root.canonicalize().unwrap_or(root.clone())
        );

        let _ = std::fs::remove_dir_all(&root);
    }
}
