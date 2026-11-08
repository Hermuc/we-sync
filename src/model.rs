//! Wallpaper Engine 壁纸模型与扫描。

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Wallpaper {
    /// 创意工坊 ID（目录名）
    pub id: String,
    pub title: String,
    /// 主媒体文件（mp4）
    pub source: PathBuf,
    pub size: u64,
}

impl Wallpaper {
    pub fn ext(&self) -> String {
        self.source
            .extension()
            .map(|e| e.to_string_lossy().to_lowercase())
            .unwrap_or_else(|| "mp4".into())
    }

    /// 按模板生成手机端文件名。
    pub fn remote_name(&self, template: &str) -> String {
        let ext = self.ext();
        let title = crate::util::sanitize_file_name(&self.title, 60);
        let stem = template
            .replace("{title}", &title)
            .replace("{id}", &self.id)
            .replace("{ext}", &ext);
        let name = crate::util::sanitize_file_name(&stem, 100);
        if name.to_lowercase().ends_with(&format!(".{ext}")) {
            name
        } else {
            format!("{name}.{ext}")
        }
    }
}

/// project.json 里我们关心的字段。
#[derive(Debug, Deserialize)]
struct ProjectJson {
    #[serde(default)]
    file: String,
    #[serde(default)]
    title: String,
}

/// 扫描工坊目录下所有壁纸（只保留存在可复制媒体文件的条目）。
pub fn scan(dir: &Path) -> Vec<Wallpaper> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        if !ft.is_dir() {
            continue;
        }
        let id = entry.file_name().to_string_lossy().to_string();
        if !id.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        if let Some(w) = read_wallpaper(&entry.path(), &id) {
            out.push(w);
        }
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

/// 读取单个壁纸目录；project.json 缺失或文件不存在时返回 None。
pub fn read_wallpaper(dir: &Path, id: &str) -> Option<Wallpaper> {
    let project_path = dir.join("project.json");
    let text = std::fs::read_to_string(&project_path).ok()?;
    let proj: ProjectJson = serde_json::from_str(&text).ok()?;

    let mut candidates: Vec<PathBuf> = Vec::new();
    if !proj.file.is_empty() {
        candidates.push(dir.join(&proj.file));
    }
    if candidates.iter().all(|p| !p.is_file()) {
        // 兜底：目录里的第一个 mp4
        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.flatten() {
                let p = e.path();
                if p.extension()
                    .map(|x| x.eq_ignore_ascii_case("mp4"))
                    .unwrap_or(false)
                {
                    candidates.push(p);
                    break;
                }
            }
        }
    }

    let source = candidates.into_iter().find(|p| p.is_file())?;
    let size = std::fs::metadata(&source).map(|m| m.len()).unwrap_or(0);
    if size == 0 {
        return None;
    }
    let title = if proj.title.trim().is_empty() {
        id.to_string()
    } else {
        proj.title.clone()
    };
    Some(Wallpaper {
        id: id.to_string(),
        title,
        source,
        size,
    })
}

/// 体积过滤：区间 [min, max]，0 表示不限。
pub fn passes_size_filter(size: u64, min: u64, max: u64) -> bool {
    size >= min && size <= max
}

/// 只保留 mp4（按实际文件扩展名）。
pub fn is_mp4(w: &Wallpaper) -> bool {
    w.ext() == "mp4"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_bounds() {
        assert!(passes_size_filter(10, 5, 100));
        assert!(!passes_size_filter(4, 5, 100));
        assert!(!passes_size_filter(101, 5, 100));
        assert!(passes_size_filter(999, 0, u64::MAX));
    }

    #[test]
    fn sanitize_and_template() {
        let w = Wallpaper {
            id: "123".into(),
            title: "a/b:c*?".into(),
            source: PathBuf::from("x.mp4"),
            size: 1,
        };
        let n = w.remote_name("{title} [{id}].{ext}");
        assert_eq!(n, "a_b_c__ [123].mp4");
    }
}
