//! 构建脚本：把 `assets/skills/**` 内嵌进二进制。
//!
//! 生成 `$OUT_DIR/embedded_skills.rs`，其中是一个
//! `pub static EMBEDDED_SKILL_FILES: &[(&str, &[u8])]`：相对 `assets/skills` 的
//! POSIX 路径 → 文件内容（`include_bytes!`）。运行时按路径首段分组，即得到
//! 随二进制分发的「扩展技能」目录。
//!
//! 之所以走 build.rs 而不是在源码里逐个 `embedded!()`：技能是**带子文件的目录树**
//! （`scripts/`、`GLOSSARY.md`、`HTML-REPORT.md` 等），只搬 `SKILL.md` 会把技能弄坏
//! （正文里的相对路径引用会指向不存在的文件）。

use std::{
    env, fs,
    path::{Path, PathBuf},
};

fn main() {
    #[cfg(target_os = "windows")]
    set_windows_info();

    embedded_skill_files();
}

#[cfg(target_os = "windows")]
fn set_windows_info() {
    _ = embed_resource::compile("./windows/icon.rc", embed_resource::NONE);
}

/// 扫描 `assets/skills/**`，生成 `EMBEDDED_SKILL_FILES` 静态表到 `$OUT_DIR`。
fn embedded_skill_files() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let skills_dir = manifest_dir.join("assets").join("skills");

    let mut files: Vec<(String, PathBuf)> = Vec::new();
    collect(&skills_dir, &skills_dir, &mut files);
    // 排序保证生成结果稳定（同一份源码在任何机器上产出逐字节一致的表）
    files.sort();

    let mut out = String::from(
        "/// 内嵌扩展技能文件表：(相对 `assets/skills` 的 POSIX 路径, 文件内容)。\n\
         /// 运行时按路径首段分组，得到随二进制分发的扩展技能目录；\n\
         /// 表由 `build.rs` 从 `assets/skills/**` 生成，新增技能无需改代码。\n\
         pub static EMBEDDED_SKILL_FILES: &[(&str, &[u8])] = &[\n",
    );
    for (rel, abs) in &files {
        out.push_str(&format!(
            "    ({:?}, include_bytes!({:?})),\n",
            rel,
            abs.to_string_lossy()
        ));
    }
    out.push_str("];\n");

    let dest = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR")).join("embedded_skills.rs");
    fs::write(&dest, out).expect("write embedded_skills.rs");
}

/// 递归收集 `dir` 下的普通文件，`rel` 为相对 `root` 的 POSIX 路径。
///
/// 对每个目录与文件都发 `cargo:rerun-if-changed`（目录 mtime 不递归，必须逐项发），
/// 保证新增/删除技能文件能触发重新构建。符号链接按 `symlink_metadata` 判定：
/// 只跟随目录与普通文件，其它类型跳过。
fn collect(root: &Path, dir: &Path, out: &mut Vec<(String, PathBuf)>) {
    println!("cargo:rerun-if-changed={}", dir.display());
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(|e| e.file_name());

    for entry in entries {
        let path = entry.path();
        let Ok(meta) = fs::metadata(&path) else {
            continue;
        };
        if meta.is_dir() {
            collect(root, &path, out);
        } else if meta.is_file() {
            println!("cargo:rerun-if-changed={}", path.display());
            let Ok(rel) = path.strip_prefix(root) else {
                continue;
            };
            let rel = rel
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/");
            out.push((rel, path));
        }
    }
}
